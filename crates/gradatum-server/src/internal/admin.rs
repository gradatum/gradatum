//! Endpoints admin internes (F-100 incrément 1.6) — delete on-demand = archivage.
//!
//! ## Invariant fondateur (F-100, `decisions/01KXAP7Z61`)
//!
//! > Le delete (archivage) et la destruction physique de l'archive ne peuvent JAMAIS
//! > arriver par accident, et JAMAIS par la main des agents — uniquement le système
//! > (GC) ou l'opérateur (CLI).
//!
//! Ces endpoints vivent sur le listener loopback interne, gardés par le middleware
//! `admin_auth` (loopback + token admin dédié, distinct du token worker). Ils ne sont
//! JAMAIS montés sur le routeur public ni exposés en MCP. La seule porte d'entrée
//! opérateur est la CLI `gradatum-admin`, qui appelle ces endpoints en loopback.
//!
//! ## Autorité admin
//!
//! Le handler construit une **identité admin synthétique** ([`admin_trust`]) — pleine
//! autorité (ACL par-tenant bypassée en aval, voir `api_v1::delete::vault_delete_core`),
//! mais la garde PROTECTED_DELETE reste active au choke point de cascade. L'identité sert
//! aussi à tracer le tombstone (`deleted_by`).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gradatum_core::error::GradatumError;
use gradatum_core::scope::AgentId;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    VaultArchivesListRequest, VaultArchivesPurgeRequest, VaultArchivesPurgeResult,
    VaultArchivesRestoreRequest, VaultArchivesRestoreResult, VaultDeleteRequest,
};

use crate::api_v1::delete::{delete_error_response, vault_delete_core};
use crate::api_v1::logic::{archive_entry_to_dto, list_archives_core};
use crate::internal::CODE_VAULT_PREFIX;
use crate::state::AppState;

/// `sub` de l'identité admin synthétique — tracé dans le tombstone (`deleted_by`).
const ADMIN_SUBJECT: &str = "operator-admin";

/// Construit l'identité admin synthétique pour un tenant donné.
///
/// `tenant_id` = tenant cible (aligné sur le body pour passer `effective_tenant`).
/// L'ACL par-tenant n'est pas évaluée en aval (pleine autorité opérateur, bypass
/// explicite documenté) ; la garde PROTECTED_DELETE reste active.
fn admin_trust(tenant_id: &str) -> TrustContext {
    TrustContext::BearerToken {
        kid: "admin".to_owned(),
        aud: "gradatum".to_owned(),
        // Identité d'agent constante, définie en dur côté serveur (jamais un input
        // client) → `new` sans validation. `serde(transparent)` → wire inchangé.
        sub: AgentId::new(ADMIN_SUBJECT),
        scopes: vec!["service".to_owned(), "write".to_owned()],
        // Frontière : champ principal typé `TenantId` (Task 3). `tenant_id` param `&str`
        // (aligné sur le body) → `.into()` via `From<&str>`, byte-identical.
        tenant_id: tenant_id.into(),
        jti: None,
    }
}

/// `POST /internal/v1/admin/delete` — delete on-demand = ARCHIVAGE (F-100 1.6).
///
/// Corps : [`VaultDeleteRequest`] (dry-run par défaut ; réel = `dry_run=false` +
/// `confirm_ulids=[note_id]`). Réponse : 200 + `DeletePreview` (dry) ou `DeleteResult`
/// (réel). Autorisation : middleware admin (loopback + token admin) en amont.
pub(crate) async fn handle_admin_delete(
    State(state): State<AppState>,
    Json(req): Json<VaultDeleteRequest>,
) -> Response {
    // Lot A1 : `tenant_id` optionnel. Chemin ADMIN loopback (pleine autorité opérateur) —
    // le body EST la source du tenant cible, pas un client hostile ; omis → "main" (défaut
    // opérateur historique, hors threat-model client A1).
    let trust = admin_trust(req.tenant_id.as_ref().map_or("main", |t| t.as_str()));
    match vault_delete_core(&state, &trust, req).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => delete_error_response(&e).into_response(),
    }
}

/// `POST /internal/v1/admin/archives/list` — listing du registre d'archives (admin).
///
/// Réutilise le cœur de listing (filtres → registre → DTO) SANS ACL (pleine autorité
/// opérateur, gate = loopback + token admin). Symétrique au listing public en lecture
/// seule, mais accessible avec le token admin (la CLI ne détient pas de JWT).
pub(crate) async fn handle_admin_archives_list(
    State(state): State<AppState>,
    Json(req): Json<VaultArchivesListRequest>,
) -> Response {
    match list_archives_core(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => delete_error_response(&e).into_response(),
    }
}

/// `POST /internal/v1/admin/archives/purge` — purge à la demande (dry-run + confirm).
///
/// Détruit une archive AVANT l'échéance de rétention. Mono-note, double confirmation
/// (comme le delete). Jamais exposé publiquement ni en MCP.
pub(crate) async fn handle_admin_archives_purge(
    State(state): State<AppState>,
    Json(req): Json<VaultArchivesPurgeRequest>,
) -> Response {
    match admin_archives_purge_core(&state, req).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => delete_error_response(&e).into_response(),
    }
}

/// Orchestration de la purge admin (validation confirm + résolution + destruction).
async fn admin_archives_purge_core(
    state: &AppState,
    req: VaultArchivesPurgeRequest,
) -> Result<VaultArchivesPurgeResult, GradatumError> {
    // Mode réel — confirm_ulids doit valoir EXACTEMENT [note_id] (borne mono-note),
    // validé AVANT toute résolution (400 déterministe même si l'archive n'existe pas).
    if !req.dry_run && req.confirm_ulids.as_slice() != std::slice::from_ref(&req.note_id) {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids must equal exactly [note_id] ([\"{}\"]) — received {} ULID(s).",
            req.note_id,
            req.confirm_ulids.len()
        )));
    }

    // Résoudre l'archive active (métadonnées du preview / de la trace).
    let archive = state.vault.get_active_archive(&req.note_id).await?;

    if req.dry_run {
        return Ok(VaultArchivesPurgeResult {
            note_id: req.note_id,
            dry_run: true,
            purged: false,
            archive: archive.map(archive_entry_to_dto),
        });
    }

    // Mode réel : destruction physique + marquage gc_at (idempotent si aucune archive).
    let purged = state.vault.purge_archive_by_id(&req.note_id).await?;
    Ok(VaultArchivesPurgeResult {
        note_id: req.note_id,
        dry_run: false,
        purged,
        archive: archive.map(archive_entry_to_dto),
    })
}

/// `POST /internal/v1/admin/archives/restore` — restauration en quarantaine (dry-run + confirm).
///
/// Ramène une archive active dans le vault en statut **`pending-review`** (quarantaine :
/// non visible/live, re-entrée pipeline curateur). Mono-note, double confirmation (comme
/// le delete/purge). Jamais exposé publiquement ni en MCP. Renvoie **409** si l'ULID est
/// déjà occupé dans l'index, **404** si aucune archive active.
pub(crate) async fn handle_admin_archives_restore(
    State(state): State<AppState>,
    Json(req): Json<VaultArchivesRestoreRequest>,
) -> Response {
    match admin_archives_restore_core(&state, req).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => delete_error_response(&e).into_response(),
    }
}

/// Orchestration de la restauration admin (validation confirm + résolution + restauration).
async fn admin_archives_restore_core(
    state: &AppState,
    req: VaultArchivesRestoreRequest,
) -> Result<VaultArchivesRestoreResult, GradatumError> {
    // Mode réel — confirm_ulids doit valoir EXACTEMENT [note_id] (borne mono-note),
    // validé AVANT toute résolution (400 déterministe même si l'archive n'existe pas).
    if !req.dry_run && req.confirm_ulids.as_slice() != std::slice::from_ref(&req.note_id) {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids must equal exactly [note_id] ([\"{}\"]) — received {} ULID(s).",
            req.note_id,
            req.confirm_ulids.len()
        )));
    }

    // Résoudre l'archive active (métadonnées du preview / de la trace).
    let archive = state.vault.get_active_archive(&req.note_id).await?;

    if req.dry_run {
        return Ok(VaultArchivesRestoreResult {
            note_id: req.note_id,
            dry_run: true,
            restored: false,
            status: None,
            restored_path: None,
            archive: archive.map(archive_entry_to_dto),
        });
    }

    // Mode réel : restauration (409 si ULID occupé, 404 si aucune archive active — mappés
    // par `restore_archive_by_id` → `Conflict` / `NoteNotFound`).
    let outcome = state.vault.restore_archive_by_id(&req.note_id).await?;
    Ok(VaultArchivesRestoreResult {
        note_id: req.note_id,
        dry_run: false,
        restored: true,
        status: Some(outcome.status.to_string()),
        restored_path: Some(outcome.restored_path),
        archive: archive.map(archive_entry_to_dto),
    })
}

// ── Cycle de vie des vaults (C2, F-18, EX-C2-4) ──────────────────────────────

/// Vault racine — jamais suspendable ni supprimable (le suspendre briquerait le
/// déploiement : plus aucun grant actif, refus global immédiat).
const ROOT_VAULT: &str = "main";

/// Valide le `vault_id` d'une requête lifecycle (parse-don't-validate, P2-a).
///
/// `Box<Response>` : variante Err rare (400) — évite de gonfler le Result du chemin
/// nominal (clippy::result_large_err).
fn parse_lifecycle_vault(
    req: &gradatum_dto::VaultLifecycleRequest,
) -> Result<String, Box<Response>> {
    match gradatum_core::scope::VaultId::parse(req.vault_id.as_str()) {
        Ok(v) => Ok(v.as_str().to_owned()),
        Err(e) => Err(Box::new(
            (StatusCode::BAD_REQUEST, format!("invalid vault_id: {e}")).into_response(),
        )),
    }
}

/// `POST /internal/v1/admin/vaults/create` — provisionne un vault (EX-C2-4, A7).
///
/// `INSERT OR IGNORE` transactionnel : tenant `active` + self-grant `write`.
/// Idempotent — re-jeu = `changed: false`, toujours 200. Aucune nouvelle table (A5).
///
/// ## Registration runtime du handle (A7, gatée `multi_tenant.enabled`)
///
/// À **flag ON**, en plus des lignes d'index, un `Vault` réel est instancié (adossé au pool
/// `index.db` PARTAGÉ `state.shared_index`, sous-répertoire md sibling `<root>/<vault_id>/`)
/// puis enregistré dans `state.vaults` — le vault devient **résoluble** (`resolve`), plus un
/// fantôme index-only. À **flag OFF** (défaut LIVE), le comportement est **strictement
/// inchangé** : provisioning index-only, aucun handle instancié, registre jamais muté en
/// runtime (byte-identical). Le vault racine `main` (déjà enregistré au boot) n'est jamais
/// ré-instancié.
///
/// ## Atomicité
///
/// L'I/O fallible (instanciation du handle = création du sous-répertoire md) est faite
/// **avant** `provision_vault` : si elle échoue, AUCUNE ligne d'index n'est écrite (pas de
/// tenant orphelin). `provision_vault` étant idempotent, un retry après un échec tardif
/// re-provisionne et re-enregistre sans duplication.
pub(crate) async fn handle_admin_vault_create(
    State(state): State<AppState>,
    Json(req): Json<gradatum_dto::VaultLifecycleRequest>,
) -> Response {
    let vault_id = match parse_lifecycle_vault(&req) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    // ── Garde de registre (lot REG) — AVANT toute I/O ─────────────────────────
    // `provision_vault` porte la garde complète (préfixe + ligne `code_vault`), mais elle
    // s'exécute APRÈS `instantiate_sibling_vault`, qui crée le sous-répertoire md. Un refus
    // tardif laisserait donc un répertoire orphelin sur disque. Le barreau lexical est ici
    // gratuit et couvre le cas réel ; le barreau registre reste en aval, dans l'index.
    if vault_id.starts_with(CODE_VAULT_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "vault_id '{vault_id}' belongs to the code registry ('{CODE_VAULT_PREFIX}') — \
                 a code vault is never provisioned as a data vault"
            ),
        )
            .into_response();
    }

    // Gate byte-identical : registration runtime UNIQUEMENT à flag ON, et jamais pour `main`.
    let register = state.server_config.multi_tenant.enabled && vault_id != ROOT_VAULT;

    // Instanciation du handle AVANT provisioning (I/O fallible en premier → atomicité).
    let pending_handle = if register {
        match instantiate_sibling_vault(&state, &vault_id).await {
            Ok(handle) => Some(handle),
            Err(resp) => return resp,
        }
    } else {
        None
    };

    let changed = match state.search.provision_vault(&vault_id).await {
        Ok(c) => c,
        // Refus de registre (lot REG) : faute de l'appelant, pas de l'infrastructure → 400.
        Err(e @ GradatumError::InvalidInput(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("provision_vault refused: {e}"),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("provision_vault failed: {e}"),
            )
                .into_response();
        }
    };

    if let Some(handle) = pending_handle {
        // `add_vault` : fail-closed identité + idempotent. Refus quasi-inatteignable ici (le
        // handle a été instancié AVEC ce `vault_id`) → 500 explicite, jamais un fantôme.
        if let Err(e) = state
            .vaults
            .add_vault(gradatum_core::scope::VaultId::new(&vault_id), handle)
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("registry insert failed: {e}"),
            )
                .into_response();
        }
    }

    tracing::info!(vault_id = %vault_id, changed, registered = register, "admin: vault provisioned");
    (
        StatusCode::OK,
        Json(gradatum_dto::VaultLifecycleResponse {
            vault_id: vault_id.into(),
            status: gradatum_core::scope::TenantStatus::Active
                .as_db_str()
                .to_owned(),
            changed,
        }),
    )
        .into_response()
}

/// Résout ou instancie le handle `Vault` réel pour `vault_id` (registration runtime, A7).
///
/// **Idempotent** : si le handle est déjà enregistré, le retourne sans ré-instanciation ni
/// I/O. Sinon, délègue l'instanciation à [`AppState::instantiate_vault_handle`] (pool
/// `index.db` partagé + root SSOT du vault racine, logique commune avec le bootstrap boot),
/// en mappant l'erreur `anyhow` (fail-closed : `shared_index`/root absents ou échec I/O —
/// invariant boot rompu) en `500`.
async fn instantiate_sibling_vault(
    state: &AppState,
    vault_id: &str,
) -> Result<Arc<gradatum_vault::Vault>, Response> {
    let vid = gradatum_core::scope::VaultId::new(vault_id);

    // Idempotence : déjà enregistré → réutiliser (aucune ré-instanciation ni I/O).
    if let Some(existing) = state.vaults.get(&vid) {
        return Ok(existing);
    }

    state.instantiate_vault_handle(vid).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("vault handle instantiation failed: {e}"),
        )
            .into_response()
    })
}

/// Cœur commun suspend / soft-delete : garde `main`, flip de statut, idempotence.
async fn vault_status_flip(
    state: &AppState,
    req: &gradatum_dto::VaultLifecycleRequest,
    status: gradatum_core::scope::TenantStatus,
) -> Response {
    let vault_id = match parse_lifecycle_vault(req) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if vault_id == ROOT_VAULT {
        return (
            StatusCode::FORBIDDEN,
            format!("the root vault '{ROOT_VAULT}' cannot be suspended or deleted"),
        )
            .into_response();
    }
    match state.search.set_tenant_status(&vault_id, status).await {
        Ok(Some(changed)) => {
            tracing::info!(vault_id = %vault_id, status = status.as_db_str(), changed,
                "admin: vault status changed");
            (
                StatusCode::OK,
                Json(gradatum_dto::VaultLifecycleResponse {
                    vault_id: vault_id.into(),
                    status: status.as_db_str().to_owned(),
                    changed,
                }),
            )
                .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, format!("unknown vault '{vault_id}'")).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set_tenant_status failed: {e}"),
        )
            .into_response(),
    }
}

/// `POST /internal/v1/admin/vaults/suspend` — gèle un vault (refus immédiat, réversible).
///
/// Le JOIN `tenants.status='active'` de `tenant_grants` ne retourne plus rien pour ce
/// tenant : middleware ET grants refusent dès la requête suivante.
pub(crate) async fn handle_admin_vault_suspend(
    State(state): State<AppState>,
    Json(req): Json<gradatum_dto::VaultLifecycleRequest>,
) -> Response {
    vault_status_flip(&state, &req, gradatum_core::scope::TenantStatus::Suspended).await
}

/// `POST /internal/v1/admin/vaults/delete` — suppression LOGIQUE (soft-delete, EX-C2-4).
///
/// Même refus immédiat que suspend ; la purge physique des notes est différée au job
/// de purge opérateur ([`handle_admin_vault_purge`]) — A5 : aucun ALTER destructif
/// sur `notes`, réversibilité opérateur tant que la purge n'a pas tourné.
pub(crate) async fn handle_admin_vault_delete(
    State(state): State<AppState>,
    Json(req): Json<gradatum_dto::VaultLifecycleRequest>,
) -> Response {
    vault_status_flip(&state, &req, gradatum_core::scope::TenantStatus::Deleted).await
}

/// `POST /internal/v1/admin/vaults/purge` — purge PHYSIQUE d'un vault soft-deleted.
///
/// Job de purge différée d'EX-C2-4/A5, opérateur-only (invariant F-100 : la destruction
/// physique n'arrive JAMAIS par accident ni par la main des agents). Dry-run par défaut
/// + double confirmation (`confirm_vault_id == vault_id`), lot borné (cap 500/appel).
///
/// Fail-closed : exige `tenants.status = 'deleted'` (**409** sinon — un vault actif ou
/// simplement suspendu n'est JAMAIS purgeable), **404** tenant inconnu, **403** `main`.
///
/// Chaque note passe par le choke point [`cascade_delete_note`] (disposition `Destroy` —
/// pas d'archivage, le vault entier disparaît) ; la garde PROTECTED_DELETE system-wide
/// y reste active → notes protégées comptées `skipped`, le lot continue. La ligne
/// `tenants` reste en place au statut `deleted` (tombstone, trace registre).
pub(crate) async fn handle_admin_vault_purge(
    State(state): State<AppState>,
    Json(req): Json<gradatum_dto::VaultPurgeRequest>,
) -> Response {
    use crate::internal::persist::{VaultDisposition, cascade_delete_note};
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::TenantStatus;
    use ulid::Ulid;

    let vault_id = match gradatum_core::scope::VaultId::parse(req.vault_id.as_str()) {
        Ok(v) => v.as_str().to_owned(),
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid vault_id: {e}")).into_response();
        }
    };
    if vault_id == ROOT_VAULT {
        return (
            StatusCode::FORBIDDEN,
            format!("the root vault '{ROOT_VAULT}' can never be purged"),
        )
            .into_response();
    }
    // Mode réel — confirmation validée AVANT toute résolution (400 déterministe,
    // parité avec la purge d'archives F-100).
    if !req.dry_run
        && req
            .confirm_vault_id
            .as_ref()
            .map(gradatum_core::scope::VaultId::as_str)
            != Some(vault_id.as_str())
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("confirm_vault_id must equal exactly \"{vault_id}\" in real mode"),
        )
            .into_response();
    }
    // Garde fail-closed : uniquement un vault SOFT-DELETED est purgeable.
    match state.search.get_tenant_status(&vault_id).await {
        Ok(Some(TenantStatus::Deleted)) => {}
        Ok(Some(status)) => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "vault '{vault_id}' is '{}' — soft-delete it first (purge requires status 'deleted')",
                    status.as_db_str()
                ),
            )
                .into_response();
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("unknown vault '{vault_id}'")).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_tenant_status failed: {e}"),
            )
                .into_response();
        }
    }

    let limit = req.limit.clamp(1, 500);
    let (ulids, eligible) = match state.search.list_vault_note_ulids(&vault_id, limit).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_vault_note_ulids failed: {e}"),
            )
                .into_response();
        }
    };

    if req.dry_run {
        return (
            StatusCode::OK,
            Json(gradatum_dto::VaultPurgeResponse {
                vault_id: vault_id.into(),
                dry_run: true,
                eligible,
                deleted: 0,
                skipped: 0,
                remaining: eligible,
            }),
        )
            .into_response();
    }

    let mut deleted: u64 = 0;
    let mut skipped: u64 = 0;
    for ulid_str in ulids {
        let Ok(note_id) = Ulid::from_string(&ulid_str).map(NoteId) else {
            // Inatteignable en principe (sentinelles exclues du listing) — SKIP loggé,
            // le lot continue (jamais d'échec global pour une ligne inattendue).
            tracing::warn!(vault_id = %vault_id, note_id = %ulid_str,
                "purge vault: non-ULID id in the listing — SKIP");
            skipped += 1;
            continue;
        };
        match cascade_delete_note(
            &state,
            &vault_id,
            &ulid_str,
            note_id,
            VaultDisposition::Destroy,
        )
        .await
        {
            Ok(outcome) => {
                if let Some(e) = outcome.index_error {
                    tracing::warn!(note_id = %ulid_str, error = %e,
                        "purge vault: delete_note_from_index failed (non-bloquant)");
                }
                if let Some(e) = outcome.redirect_error {
                    tracing::warn!(note_id = %ulid_str, error = %e,
                        "purge vault: delete_redirect_by_ulid failed (non-bloquant)");
                }
                deleted += 1;
            }
            // Section protégée (PROTECTED_DELETE system-wide) : SKIP explicite, jamais
            // de hard-delete — la note reste, comptée dans `skipped`.
            Err(GradatumError::Forbidden(msg)) => {
                tracing::info!(note_id = %ulid_str, %msg,
                    "purge vault: protected note — SKIP, never hard-delete");
                skipped += 1;
            }
            // Ligne d'index sans `.md` (note jamais matérialisée côté vault, ou déjà
            // détruite) : convergence — dé-indexation directe, sinon `remaining` ne
            // tendrait jamais vers 0.
            Err(GradatumError::NoteNotFound(_)) => {
                // `Ok(_)` : la ligne d'index est absente APRÈS l'appel quel que soit le
                // booléen (true = supprimée ici, false = déjà partie) — convergence acquise.
                match state
                    .search
                    .delete_note_from_index(&vault_id, &ulid_str)
                    .await
                {
                    Ok(_) => {
                        if let Err(e) = state
                            .search
                            .delete_redirect_by_ulid(&vault_id, &ulid_str)
                            .await
                        {
                            tracing::warn!(note_id = %ulid_str, error = %e,
                                "purge vault: delete_redirect_by_ulid failed (non-bloquant)");
                        }
                        deleted += 1;
                    }
                    Err(e) => {
                        tracing::warn!(note_id = %ulid_str, error = %e,
                            "purge vault: orphan de-indexation failed — SKIP");
                        skipped += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(note_id = %ulid_str, error = %e,
                    "purge vault: cascade_delete_note failed — SKIP, le lot continue");
                skipped += 1;
            }
        }
    }
    let remaining = eligible.saturating_sub(deleted);
    tracing::info!(vault_id = %vault_id, eligible, deleted, skipped, remaining,
        "admin: soft-deleted vault purge — batch complete");
    (
        StatusCode::OK,
        Json(gradatum_dto::VaultPurgeResponse {
            vault_id: vault_id.into(),
            dry_run: false,
            eligible,
            deleted,
            skipped,
            remaining,
        }),
    )
        .into_response()
}
