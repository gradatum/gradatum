//! Handler for `POST /api/v1/capture` — ingestion de lignes BRUTES en notes `snapshot`.
//!
//! ## Contract
//!
//! | Method | Path                | Auth       | Body                  |
//! |--------|---------------------|------------|-----------------------|
//! | POST   | `/api/v1/capture`   | bearer JWT | `Json<Vec<String>>`   |
//!
//! ## HTTP status codes
//!
//! | Code | Reason |
//! |------|--------|
//! | 200  | Batch accepté (enqueué). Body: `EventLogResponse { accepted_count, status: "accepted" }`. |
//! | 401  | Non authentifié (bearer absent ou invalide). |
//! | 403  | ACL refusée (consumer non autorisé sur `{tenant_id}/snapshot`). |
//! | 413  | Batch > `MAX_BATCH_SIZE` lignes (anti-DoS). |
//! | 422  | Une ligne dépasse `MAX_FIELD_LEN` caractères. |
//! | 500  | Erreur d'enqueue du job (store en erreur). |
//!
//! ## Principe directeur — la capture ne juge pas
//!
//! L'appelant ne fournit QUE le contenu de la ligne : aucune classification, aucun
//! choix de section, aucune composition de titre, aucun anti-doublon. Le serveur :
//!
//! - force la section `snapshot` via `section_hint` (admission directe du curateur —
//!   pas de re-classification, pas de recomposition du corps) ;
//! - compose le titre mécaniquement (horodatage + discriminant ULID) ;
//! - enque un job `Curate` par ligne, dont le worker dérive le statut `live`
//!   (`outcome_to_status(Admitted)`), qui est vectorisable ET visible par défaut
//!   (`is_embeddable_default`) — la chaîne curate→embed produit le vecteur.
//!
//! ## Append-only
//!
//! Chaque ligne devient une note NEW dans la section `snapshot`. Aucune mise à jour,
//! aucune déduplication — c'est la matière première que le pipeline de distillation
//! (pièce suivante) traitera.
//!
//! ## Async par construction
//!
//! Le 200 signifie « batch accepté dans la file » : les notes se matérialisent quand
//! le worker traite les jobs `Curate` puis `Embed`. L'écriture réelle est asynchrone
//! (même chemin que `vault_write`).

use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use chrono::Utc;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::audit::http::HttpAuditEvent;
use gradatum_core::section::Section;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{EventLogResponse, VaultWriteRequest};
use ulid::Ulid;

use crate::api_v1::logic::{effective_author, err_to_status, locus_for_section};
use crate::api_v1::tenant_guard::{effective_write_vault, write_scope_allowed};
use crate::api_v1::write::{actor_from_trust, build_curate_job_record};
use crate::state::AppState;

/// Nombre maximal de lignes par POST (anti-DoS protection).
///
/// Miroir du `MAX_BATCH_SIZE` d'`event_log` — 1 000 lignes est un plafond très
/// conservateur pour un flush de capture par lot.
const MAX_BATCH_SIZE: usize = 1000;

/// Longueur maximale d'une ligne brute capturée.
///
/// Miroir du `MAX_FIELD_LEN` d'`event_log`. Une ligne unique de plus de 1 024
/// caractères est hors contrat de capture (retour 422, erreur client).
const MAX_FIELD_LEN: usize = 1024;

/// `POST /api/v1/capture`
///
/// Reçoit un lot de lignes brutes et enque un job `Curate` par ligne, avec
/// `section_hint = "snapshot"` (section FORCÉE, jamais suggérée) et un titre
/// mécanique composé côté serveur.
///
/// # Pipeline
///
/// 1. Authentification (`trust.is_authenticated()`) → 401.
/// 2. Tenant d'écriture effectif (`effective_write_vault`) → 403.
/// 3. ACL Write sur `{tenant}/snapshot` → 403. Scope write (EX-C3a-1) → 403.
/// 4. Taille du batch (≤ `MAX_BATCH_SIZE`) → 413.
/// 5. Longueur de chaque ligne (≤ `MAX_FIELD_LEN`) → 422.
/// 6. Author dérivé du credential (`effective_author`, R2 v2.0.0) → 401/400.
/// 7. Enqueue d'un `JobRecord::Curate` par ligne → 500 sur erreur.
/// 8. Émission de l'audit `capture_ingest` (outcome `accepted`).
/// 9. 200 OK + `EventLogResponse { accepted_count, status: "accepted" }`.
///
/// # Side effects
///
/// Enque N jobs `Curate` dans `state.job_store`. Chaque job écrira une note
/// `snapshot` (statut `live`) et enchaînera un job `Embed` (vectorisation).
pub(crate) async fn post_capture(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(lines): Json<Vec<String>>,
) -> Result<Json<EventLogResponse>, StatusCode> {
    let request_id = extract_request_id(&headers);

    // 1. Authentification obligatoire.
    if !trust.is_authenticated() {
        emit_audit_failure(
            &state,
            &trust,
            &unknown_tenant(&trust),
            &request_id,
            "unauthenticated",
        )
        .await;
        return Err(StatusCode::UNAUTHORIZED);
    }

    // 2. Tenant effectif d'écriture (C1/F-63, EX-C1-1/2) — jamais depuis le body.
    let tenant = match effective_write_vault(&state, &trust, None).await {
        Ok(t) => t,
        Err(_) => {
            emit_audit_failure(
                &state,
                &trust,
                &unknown_tenant(&trust),
                &request_id,
                "tenant_deny",
            )
            .await;
            return Err(StatusCode::FORBIDDEN);
        }
    };

    // 3. ACL Write sur le locus de la section cible (`{tenant}/snapshot`).
    let locus = locus_for_section(&tenant, Some(Section::Snapshot.as_str()));
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_audit_failure(&state, &trust, &tenant, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }
    // EX-C3a-1 : à ON, la capture d'une ligne est une écriture — scope write exigé.
    if !write_scope_allowed(&state, &trust) {
        emit_audit_failure(&state, &trust, &tenant, &request_id, "scope_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    // 4. Protection taille batch.
    if lines.len() > MAX_BATCH_SIZE {
        tracing::warn!(
            count = lines.len(),
            max = MAX_BATCH_SIZE,
            "capture batch too large — 413"
        );
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // 4b. Validation longueur des lignes (F2) — AVANT enqueue, erreur client 422.
    for line in &lines {
        if line.len() > MAX_FIELD_LEN {
            tracing::warn!(
                len = line.len(),
                max = MAX_FIELD_LEN,
                "capture line too long — 422"
            );
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    // 5. Author — identité du credential (v2.0.0, Task 10). Fail-closed : jamais
    //    une note sans auteur, jamais une identité déclarée par le client.
    let author = match effective_author(&None, trust.subject()) {
        Ok(a) => a,
        Err(e) => {
            emit_audit_failure(&state, &trust, &tenant, &request_id, "author_unresolved").await;
            return Err(err_to_status(&e));
        }
    };

    // 6. Enqueue un job Curate par ligne.
    //
    // Chaque job porte section_hint="snapshot" (admission directe du curateur, corps
    // intact) et un titre mécanique `snapshot <horodatage> <ULID>`. Le statut final
    // est dérivé par le worker (`Admitted → live`), vectorisable ET visible.
    let mut accepted_count = 0usize;
    for line in &lines {
        let note_id = Ulid::generate();
        let title = compose_snapshot_title(note_id);
        let mut req = VaultWriteRequest::new(title, line.clone());
        req.section_hint = Some(Section::Snapshot.as_str().to_string());
        req.author = Some(author.clone());
        // note_id reste None (création pure) — `build_curate_job_record` reçoit
        // l'ULID pré-alloué en paramètre et le worker l'honore via write_note_with_id.
        let record = build_curate_job_record(&req, note_id, &tenant);
        state.job_store.enqueue(record).await.map_err(|e| {
            tracing::error!(error = %e, "capture enqueue failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        accepted_count += 1;
    }

    // 7. Audit succès (best-effort — non fatal).
    emit_audit_success(&state, &trust, &tenant, &locus, &request_id, accepted_count).await;

    Ok(Json(EventLogResponse {
        accepted_count,
        status: "accepted".to_string(),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Titre mécanique d'une note `snapshot` : horodatage UTC (RFC 3339, millisecondes)
/// + discriminant ULID. Jamais fourni par l'appelant — composé côté serveur.
fn compose_snapshot_title(note_id: Ulid) -> String {
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!("snapshot {now} {note_id}")
}

/// Tenant pour l'audit de refus : le tenant porté par le credential si présent,
/// sinon le marqueur explicite `__unknown__` (ne jamais forger "main").
fn unknown_tenant(trust: &TrustContext) -> String {
    trust
        .tenant_id()
        .map(|t| t.as_str().to_owned())
        .unwrap_or_else(|| "__unknown__".to_string())
}

/// Extrait le `request_id` du header `X-Request-ID`, ou génère un ULID frais.
fn extract_request_id(headers: &HeaderMap) -> String {
    headers
        .get("X-Request-ID")
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()))
        .map(|s| s.to_owned())
        .unwrap_or_else(|| Ulid::generate().to_string())
}

/// Émet un événement d'audit `auth_failure` (401/403) — best-effort, non-fatal.
async fn emit_audit_failure(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    request_id: &str,
    reason: &str,
) {
    let evt = HttpAuditEvent {
        ts: Utc::now(),
        event: "auth_failure".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: format!("{tenant_id}/snapshot"),
        note_id: None,
        content_hash: None,
        outcome: "denied".into(),
        curator: Some(serde_json::json!({ "reason": reason })),
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, reason = reason, "audit capture auth_failure failed");
    }
}

/// Émet un événement d'audit `capture_ingest` (outcome `accepted`) — best-effort.
async fn emit_audit_success(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    locus: &str,
    request_id: &str,
    count: usize,
) {
    let evt = HttpAuditEvent {
        ts: Utc::now(),
        event: "capture_ingest".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: locus.into(),
        note_id: None,
        content_hash: None,
        outcome: "accepted".into(),
        curator: Some(serde_json::json!({ "accepted_count": count })),
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, "audit capture_ingest failed — non fatal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_title_is_mechanical_and_unique() {
        let a = compose_snapshot_title(Ulid::generate());
        let b = compose_snapshot_title(Ulid::generate());
        assert_ne!(a, b, "deux titres mécaniques distincts");
        assert!(a.starts_with("snapshot "), "préfixe section: {a}");
        assert!(a.len() > "snapshot ".len(), "horodatage présent");
    }

    #[test]
    fn unknown_tenant_marks_non_bearer() {
        let t = TrustContext::Unauthenticated;
        assert_eq!(unknown_tenant(&t), "__unknown__");
    }
}
