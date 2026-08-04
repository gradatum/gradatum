//! Delete on-demand = **archivage** (F-100 incrément 1.6) — cœur d'orchestration admin.
//!
//! # Invariant fondateur (F-100, `decisions/01KXAP7Z61`)
//!
//! > Le delete (archivage) et la destruction physique de l'archive ne peuvent JAMAIS
//! > arriver par accident, et JAMAIS par la main des agents — uniquement le système
//! > (GC) ou l'opérateur (CLI).
//!
//! Ce module porte le **cœur d'orchestration** (`vault_delete_core`) du delete
//! on-demand. Il n'est **PAS** exposé publiquement : ni route HTTP publique, ni outil
//! MCP. Sa seule porte d'entrée est l'endpoint admin interne
//! (`POST /internal/v1/admin/delete`, namespace loopback + token admin dédié), lui-même
//! appelé par la CLI `gradatum-admin delete`. Le cœur reste partagé (zéro duplication) ;
//! seule la surface d'appel a changé (arbitrage Tech Lead — Option A, single-owner DB).
//!
//! # Autorisation admin (bypass ACL explicite, documenté)
//!
//! L'autorisation vit **en amont**, dans le middleware admin
//! (`internal/admin_auth.rs` : loopback obligatoire + token admin dédié). L'opérateur a
//! **pleine autorité** : l'ACL par-tenant n'est donc PAS évaluée ici (bypass explicite).
//! Le [`TrustContext`] passé est une **identité admin synthétique** (traçabilité du
//! tombstone `deleted_by`), pas un JWT client.
//!
//! # Garde PROTECTED_DELETE — s'applique QUAND MÊME à l'admin
//!
//! Une note en section [`Section::PROTECTED_DELETE`] (gouvernance : `agent-issues`,
//! `council`, `project-map`, `identity`, `decisions`, `reasoning`) reste
//! **insupprimable même par l'opérateur via cette API** → **403 Forbidden**. La garde
//! est portée par le choke point `cascade_delete_note` (system-wide) : l'exceptionnel
//! manuel (édition disque) reste hors API, comme acté.
//!
//! # Dry-run + confirmation (mono-note)
//!
//! Exécution réelle : `dry_run=false` **et** `confirm_ulids == [note_id]`. Toute autre
//! valeur → **400**. Borne mono-note : un delete ne cible jamais plus d'une note.
//!
//! # Archivage réversible + tombstone durable + backlinks
//!
//! En mode réel, un **tombstone durable append-only** est écrit AVANT la cascade
//! (précondition dure : pas de mutation si le tombstone échoue). Le `.md` + `.history`
//! sont **déplacés** sous `.archive/` (réversible via restore / GC 60 j), la note sort
//! des index. `DeleteResult.archived_path` porte le chemin d'archive ; `backup` le
//! contenu pré-archivage ; `backlinks_orphaned` les liens entrants devenus orphelins.
//!
//! # Idempotence
//!
//! Archiver une note inexistante est un no-op succès (200, `deleted=false`).

use gradatum_core::audit::http::HttpAuditEvent;
use gradatum_core::{
    error::GradatumError, identity::NoteId, section::Section, trust::TrustContext,
};
use gradatum_dto::{DeletePreview, DeleteResult, DeletedNoteBackup, VaultDeleteRequest};
use ulid::Ulid;

use crate::api_v1::tenant_guard::effective_write_vault;
use crate::api_v1::write::actor_from_trust;
use crate::internal::persist::{VaultDisposition, cascade_delete_note};
use crate::state::AppState;

// ── Cœur d'orchestration admin (appelé par l'endpoint interne / la CLI) ─────────

/// Orchestration du delete on-demand = archivage (F-100 1.6). Appelée UNIQUEMENT par
/// l'endpoint admin interne (`handle_admin_delete`), lui-même appelé par la CLI.
///
/// Retourne la valeur JSON de réponse (un [`DeletePreview`] en dry-run, un
/// [`DeleteResult`] en mode réel). L'autorisation (loopback + token admin) est faite en
/// amont par le middleware admin ; `trust` est l'identité admin synthétique servant à la
/// traçabilité (tombstone). L'ACL par-tenant n'est **pas** évaluée (bypass admin explicite,
/// pleine autorité opérateur) — mais la garde PROTECTED_DELETE reste active au choke point.
///
/// # Errors
///
/// - [`GradatumError::Unauthorized`] si le trust n'est pas authentifié (garde défensive).
/// - [`GradatumError::Forbidden`] si le tenant du body diverge de l'identité admin, ou si
///   la note cible est en section `PROTECTED_DELETE`.
/// - [`GradatumError::InvalidInput`] si `note_id` n'est pas un ULID valide ou si
///   `confirm_ulids` ne vaut pas `[note_id]` en mode réel.
/// - [`GradatumError::Storage`] sur échec réel de purge index/redirect (strict).
pub(crate) async fn vault_delete_core(
    state: &AppState,
    trust: &TrustContext,
    req: VaultDeleteRequest,
) -> Result<serde_json::Value, GradatumError> {
    // 1. Garde défensive : l'identité admin synthétique est toujours authentifiée.
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    // 2. Tenant effectif dérivé de l'identité admin (refuse un body divergent).
    //    ACL par-tenant NON évaluée ici : l'autorisation est le gate admin amont
    //    (loopback + token dédié) — pleine autorité opérateur (bypass ACL explicite).
    // C1 (F-63, EX-C1-1/2) : résolution write-scope — l'archivage est une écriture ;
    // à flag ON, même l'identité admin synthétique exige un grant write sur le vault.
    let vault_id = effective_write_vault(state, trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.into_forbidden("tenant admin diverge du body"))?;

    // 3. Valider l'ULID cible.
    let note_ulid = Ulid::from_string(&req.note_id)
        .map_err(|e| GradatumError::InvalidInput(format!("invalid note_id: {e}")))?;
    let note_id_str = req.note_id.as_str();

    // 4. Mode réel — confirm_ulids doit valoir EXACTEMENT [note_id] (borne mono-note).
    //    (P2-1) Validé AVANT tout court-circuit (existence, protégé) : un confirm
    //    malformé est un 400 déterministe même si la note n'existe pas — pas d'ambiguïté
    //    entre « validation » et « idempotence ».
    if !req.dry_run && req.confirm_ulids.as_slice() != std::slice::from_ref(&req.note_id) {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids must equal exactly [note_id] ([\"{}\"]) — received {} ULID(s). \
             A hard-delete is single-note.",
            req.note_id,
            req.confirm_ulids.len()
        )));
    }

    // 5. Résoudre la note dans l'index (existence + section + backup content).
    let record = state.search.get_note(&vault_id, note_id_str).await?;

    let Some(record) = record else {
        // 6a. Note inexistante → no-op idempotent (confirm déjà validé en mode réel).
        if req.dry_run {
            let preview = DeletePreview {
                note_id: req.note_id.clone(),
                exists: false,
                section: String::new(),
                title: None,
                backlinks: Vec::new(),
                dry_run: true,
            };
            return to_value(&preview);
        }
        let result = DeleteResult {
            note_id: req.note_id.clone(),
            deleted: false,
            backlinks_orphaned: Vec::new(),
            backup: None,
            archived_path: None,
        };
        return to_value(&result);
    };

    // 6b. Refus dur si section protégée — AUCUN flag de contournement (1.2b).
    //     S'applique QUAND MÊME à l'admin : une note de gouvernance reste insupprimable
    //     via cette API, même avec pleine autorité opérateur.
    //
    // Court-circuit précoce (avant backlinks/backup) pour une erreur explicite.
    // La protection de fond reste garantie dans `cascade_delete_note`
    // (choke point system-wide) même si ce court-circuit était retiré : la même
    // section y est re-résolue et refusée avant toute mutation.
    if Section::is_protected_delete(&record.section) {
        return Err(GradatumError::Forbidden(format!(
            "protected section: '{}' can never be hard-deleted (PROTECTED_DELETE, no bypass)",
            record.section
        )));
    }

    // 7. Backlinks entrants qui deviendraient orphelins (calculés AVANT toute mutation).
    let backlinks = state.search.backlinks(&vault_id, note_id_str).await?;

    // 8. Dry-run : preview sans mutation.
    if req.dry_run {
        let preview = DeletePreview {
            note_id: req.note_id.clone(),
            exists: true,
            section: record.section.clone(),
            title: record.title.clone(),
            backlinks,
            dry_run: true,
        };
        return to_value(&preview);
    }

    // 9. Backup préalable : capturé depuis le record déjà lu, AVANT la cascade.
    let backup = DeletedNoteBackup {
        section: record.section.clone(),
        title: record.title.clone(),
        body: record.body_text.clone(),
    };

    // 10. Tombstone durable AVANT la cascade (P1-2, crash-safety).
    //     Trace append-only (sink JSONL disque, survit à un crash mid-delete ET à la
    //     suppression de la note) portant : deleted_by (sub de l'identité admin), section, title,
    //     body, content_hash, timestamp. PRÉCONDITION DURE : si l'écriture du tombstone
    //     échoue, la cascade N'EST PAS exécutée — jamais de suppression irréversible sans
    //     trace de récupération durable (guard-data-loss). En prod, `state.audit` est un
    //     JsonlFileSink câblé au boot (`main.rs` → `with_audit_dir(<storage.root>/audit)`),
    //     dont `record` flush sur disque → la précondition dure est réellement armée. Le
    //     NoopAuditSink ne subsiste que sur les états de test qui n'injectent pas de sink
    //     (il retourne Ok sans persister — capacité prouvée par ailleurs via un sink capturant).
    let content_hash = (record.content_hash.len() == 32).then(|| {
        format!(
            "sha256:{}",
            record
                .content_hash
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    });
    let tombstone = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_delete".to_owned(),
        actor: actor_from_trust(trust),
        tenant_id: vault_id.clone(),
        locus: format!("{}/{}", record.section, note_id_str),
        note_id: Some(req.note_id.clone()),
        content_hash,
        outcome: "deleted".to_owned(),
        // `curator` est le sac de métadonnées libre par événement (cf. vault_downgrade
        // qui y met {job_id, duration_ms}). On y consigne le contenu de récupération.
        curator: Some(serde_json::json!({
            "tombstone": {
                "section": &record.section,
                "title": &record.title,
                "body": &record.body_text,
                "deleted_by": trust.subject(),
            }
        })),
        request_id: format!("vault_delete-{note_ulid}"),
    };
    state.audit.record(tombstone).await.map_err(|e| {
        GradatumError::Storage(format!(
            "durable tombstone audit failed — hard-delete aborted (no deletion): {e}"
        ))
    })?;

    // 11. Cascade physique (vault → index strict → redirect). Réutilise la source
    //     unique `cascade_delete_note` (zéro duplication de cascade). Disposition =
    //     ARCHIVAGE (F-100 1.6) : le `.md` + `.history` sont déplacés sous `.archive/`
    //     et inscrits au registre `archive_index`, la note sort TOTALEMENT des index.
    //     `gc_due` = maintenant + rétention configurée (défaut 60 j).
    let retention_days = i64::from(state.server_config.archive.retention_days);
    let gc_due_ms = chrono::Utc::now().timestamp_millis() + retention_days * 86_400_000;
    let disposition = VaultDisposition::Archive {
        // Frontière DTO (`VaultDisposition::archived_by: Option<String>`) : `as_str`
        // est byte-identical, le champ persisté est inchangé.
        archived_by: trust.subject().map(|a| a.as_str().to_owned()),
        gc_due_ms,
    };
    let mut archived_path: Option<String> = None;
    match cascade_delete_note(
        state,
        &vault_id,
        note_id_str,
        NoteId(note_ulid),
        disposition,
    )
    .await
    {
        Ok(outcome) => {
            // Strict : un échec index réel est propagé (contrat F-100 1.2).
            if let Some(e) = outcome.index_error {
                return Err(e);
            }
            // Redirect : non fatal (par conception, un redirect orphelin résout
            // simplement vers une note absente) — journalisé, pas propagé.
            if let Some(e) = outcome.redirect_error {
                tracing::warn!(
                    note_id = %note_id_str,
                    error = %e,
                    "vault_delete: redirect purge failed (non-fatal)"
                );
            }
            archived_path = outcome.archive_path;
        }
        Err(GradatumError::NoteNotFound(_)) => {
            // Drift index/disque : le `.md` était déjà absent alors que l'index le
            // référençait encore. Résoudre le drift en purgeant l'index (strict).
            state
                .search
                .delete_note_from_index(&vault_id, note_id_str)
                .await?;
            if let Err(e) = state
                .search
                .delete_redirect_by_ulid(&vault_id, note_id_str)
                .await
            {
                tracing::warn!(
                    note_id = %note_id_str,
                    error = %e,
                    "vault_delete: redirect purge (drift) failed (non-fatal)"
                );
            }
        }
        Err(e) => return Err(e),
    }

    tracing::info!(
        note_id = %note_id_str,
        section = %record.section,
        orphaned_backlinks = backlinks.len(),
        archived = archived_path.is_some(),
        "vault_delete: note archived (reversible via 60d GC / admin restore)"
    );

    let result = DeleteResult {
        note_id: req.note_id.clone(),
        deleted: true,
        backlinks_orphaned: backlinks,
        backup: Some(backup),
        archived_path,
    };
    to_value(&result)
}

/// Serialises a response DTO into a JSON value (500 on the impossible serde failure).
fn to_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, GradatumError> {
    serde_json::to_value(v)
        .map_err(|e| GradatumError::Storage(format!("response serialization: {e}")))
}

/// Mappe une erreur métier delete → status HTTP + corps JSON d'erreur.
///
/// Réutilisée par l'endpoint admin interne (`internal/admin.rs`). Les erreurs `Storage`
/// sont masquées derrière un message générique (anti-fuite chemin/DB), cohérent avec le
/// mapping MCP.
pub(crate) fn delete_error_response(
    e: &GradatumError,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    let (status, msg) = match e {
        GradatumError::Unauthorized => (StatusCode::UNAUTHORIZED, "not authenticated".to_owned()),
        GradatumError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
        GradatumError::InvalidInput(m) => (StatusCode::BAD_REQUEST, m.clone()),
        GradatumError::NoteNotFound(_) => (StatusCode::NOT_FOUND, "note not found".to_owned()),
        GradatumError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_owned(),
        ),
    };
    (status, axum::Json(serde_json::json!({ "error": msg })))
}
