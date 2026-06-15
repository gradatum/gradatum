//! Handler for `POST /api/v1/event-log` — gateway `QaEvent` ingestion.
//!
//! ## Contract
//!
//! | Method | Path                | Auth       | Body                    |
//! |--------|---------------------|------------|-------------------------|
//! | POST   | `/api/v1/event-log` | bearer JWT | `Json<Vec<QaEventDto>>` |
//!
//! ## HTTP status codes
//!
//! | Code | Reason |
//! |------|--------|
//! | 200  | Batch accepted. Body: `EventLogResponse { accepted_count, status: "accepted" }`. |
//! | 400  | Invalid RFC 3339 timestamp in a batch event. |
//! | 401  | Unauthenticated (bearer absent or invalid). |
//! | 403  | ACL denied (consumer not authorised on `{tenant_id}/event-log`). |
//! | 413  | Batch > 1 000 events (anti-DoS protection). |
//! | 422  | Text field exceeds the maximum allowed length (`MAX_FIELD_LEN`). |
//! | 503  | `event_log` not wired in `AppState` (server configuration issue). |
//! | 500  | SQLite insert error. |
//!
//! ## ACL identity
//!
//! The gateway presents a JWT with `tenant_id="main"` (api-key → `/auth/exchange`).
//! The ACL must grant Write on the locus `main/event-log` for the consumer.
//! Example `bearer.toml`:
//! ```toml
//! [[consumer]]
//! identity = "gateway"
//! write_patterns = ["main/event-log"]
//! ```
//!
//! ## Append-only
//!
//! This handler does ONLY INSERT — no UPDATE/DELETE path per record.
//! Immutability-by-construction guarantees append-only semantics without `AclOp::Append`.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::audit::http::{HttpAuditActor, HttpAuditEvent};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{EventLogResponse, QaEventDto};
use ulid::Ulid;

use crate::event_log_store::EventLogError;
use crate::state::AppState;

/// Maximum number of `QaEvent`s per POST (anti-DoS protection).
///
/// Requests exceeding this limit return 413 Payload Too Large.
/// The gateway flushes in batches of 10 — 1 000 is a very conservative ceiling.
const MAX_BATCH_SIZE: usize = 1000;

/// Maximum length of free-text fields in a `QaEventDto`.
///
/// Protects against an authenticated client sending oversized fields.
/// Affected fields: `route`, `model_alias`, `provider`, `feature_id`, `model_used`.
/// Violation returns 422 Unprocessable Entity (client error, not server).
///
/// The `timestamp` field is bounded separately by `MAX_TIMESTAMP_LEN` (a valid RFC 3339
/// value fits in < 64 chars — generous headroom for extended time zones).
const MAX_FIELD_LEN: usize = 1024;

/// Maximum length of the `timestamp` field (RFC 3339 string).
///
/// A valid RFC 3339 timestamp fits in < 40 chars. 64 chars provides ample headroom
/// while blocking multi-KB field injection.
const MAX_TIMESTAMP_LEN: usize = 64;

/// `POST /api/v1/event-log`
///
/// Ingests a batch of `QaEventDto` records sent by the gateway (fire-and-forget).
///
/// # Pipeline
///
/// 1. Authentication check (`trust.is_authenticated()`) → 401.
/// 2. Extract `tenant_id` from the JWT (`TrustContext::BearerToken`).
/// 3. ACL Write evaluation on locus `{tenant_id}/event-log` → 403.
/// 4. Batch size check (≤ 1 000) → 413.
/// 5. Insert via `EventLogStore::insert_batch` (single transaction) → 500 on error.
/// 6. Emit `event_log_ingest` audit event (outcome `accepted`).
/// 7. 200 OK + `EventLogResponse { accepted_count }`.
///
/// # Side effects
///
/// Inserts N rows into the `event_log` table (append-only). Emits a JSONL audit event.
///
/// # Durability
///
/// If the endpoint returns 500, the events are lost (best-effort gateway).
/// This is not a fatal error — the LLM request is never blocked by the hook.
pub async fn post_event_log(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(events): Json<Vec<QaEventDto>>,
) -> Result<Json<EventLogResponse>, StatusCode> {
    let request_id = extract_request_id(&headers);

    // 1. Authentification obligatoire.
    if !trust.is_authenticated() {
        emit_audit_failure(&state, &trust, "main", &request_id, "unauthenticated").await;
        return Err(StatusCode::UNAUTHORIZED);
    }

    // 2. tenant_id depuis JWT — jamais du body (D10 multi-tenancy invariant).
    //    P0 cross-tenant (Lot 3) : un contexte sans tenant (Mtls/Studio) ne doit PAS
    //    être masqué en "main" dans l'audit trail. None → 403 explicite (audit
    //    "__unknown__" pour ne pas falsifier le tenant réel). Bearer non-main est
    //    déjà refusé en amont par le middleware (Lot 2).
    let tenant_id = match trust.tenant_id() {
        Some(t) => t.to_owned(),
        None => {
            emit_audit_failure(&state, &trust, "__unknown__", &request_id, "no_tenant").await;
            return Err(StatusCode::FORBIDDEN);
        }
    };

    // 3. ACL Write sur le locus event-log du tenant.
    let locus = format!("{tenant_id}/event-log");
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_audit_failure(&state, &trust, &tenant_id, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    // 4. Protection taille batch.
    if events.len() > MAX_BATCH_SIZE {
        tracing::warn!(
            count = events.len(),
            max = MAX_BATCH_SIZE,
            "event_log batch trop grand — 413"
        );
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // 4b. Validation longueur des champs texte (F2).
    //
    // Un client authentifié malveillant pourrait envoyer des champs de plusieurs MB.
    // On valide AVANT l'insert pour retourner 422 (erreur client) et non 500.
    // Chaque champ dépasse MAX_FIELD_LEN → 422 Unprocessable Entity.
    for event in &events {
        let fields: &[(&str, &str)] = &[
            ("route", &event.route),
            ("model_alias", &event.model_alias),
            ("provider", &event.provider),
            ("timestamp", &event.timestamp),
        ];
        for (name, value) in fields {
            let limit = if *name == "timestamp" {
                MAX_TIMESTAMP_LEN
            } else {
                MAX_FIELD_LEN
            };
            if value.len() > limit {
                tracing::warn!(
                    field = name,
                    len = value.len(),
                    max = limit,
                    "event_log champ trop long — 422"
                );
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
        }
        // Champs Option<String> (feature_id, model_used, agent_id) — bornés à MAX_FIELD_LEN.
        for (name, opt) in [
            ("feature_id", &event.feature_id),
            ("model_used", &event.model_used),
            ("agent_id", &event.agent_id),
        ] {
            if let Some(v) = opt {
                if v.len() > MAX_FIELD_LEN {
                    tracing::warn!(
                        field = name,
                        len = v.len(),
                        max = MAX_FIELD_LEN,
                        "event_log champ optionnel trop long — 422"
                    );
                    return Err(StatusCode::UNPROCESSABLE_ENTITY);
                }
            }
        }
    }

    // 5. Récupérer le store (None = non câblé en prod → 503).
    let event_log = match &state.event_log {
        Some(store) => store,
        None => {
            tracing::error!("event_log non câblé dans AppState — vérifier with_event_log_path");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    // 6. Insertion (1 transaction).
    //
    // F1 : `EventLogError::BadTimestamp` est une erreur CLIENT (timestamp malformé
    // fourni dans le batch) → 400 Bad Request.
    // Toute autre erreur est une erreur SERVEUR (SQLite, mutex) → 500.
    // Un 500 sur timestamp invalide induirait le client en erreur (retry inutile)
    // et constitue un vecteur DoS via accumulation de logs d'erreur.
    let accepted_count =
        event_log
            .insert_batch(&tenant_id, &events)
            .await
            .map_err(|e| match e {
                EventLogError::BadTimestamp(ref ts) => {
                    tracing::warn!(timestamp = %ts, "event_log timestamp invalide — 400");
                    StatusCode::BAD_REQUEST
                }
                _ => {
                    tracing::error!(error = %e, "event_log insert_batch échoué");
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            })?;

    // 7. Audit (best-effort — non fatal).
    emit_audit_success(
        &state,
        &trust,
        &tenant_id,
        &locus,
        &request_id,
        accepted_count,
    )
    .await;

    Ok(Json(EventLogResponse {
        accepted_count,
        status: "accepted".to_string(),
    }))
}

// ── Helpers d'audit ───────────────────────────────────────────────────────────

/// Extracts the `request_id` from the `X-Request-ID` header, or generates a fresh ULID.
///
/// # Validation
///
/// The header value is validated: ASCII graphic characters only (no space),
/// length ≤ 128 chars. If absent or non-conforming, a ULID is generated.
///
/// This filtering prevents CR/LF/ANSI injection into structured logs.
fn extract_request_id(headers: &HeaderMap) -> String {
    headers
        .get("X-Request-ID")
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()))
        .map(|s| s.to_owned())
        .unwrap_or_else(|| Ulid::new().to_string())
}

/// Builds an `HttpAuditActor` from the `TrustContext`.
fn actor_from_trust(trust: &TrustContext) -> HttpAuditActor {
    match trust {
        TrustContext::BearerToken { kid, sub, aud, .. } => HttpAuditActor {
            kid: kid.clone(),
            sub: sub.clone(),
            aud: aud.clone(),
        },
        TrustContext::Mtls { cn, .. } => HttpAuditActor {
            kid: format!("mtls:{cn}"),
            sub: cn.clone(),
            aud: "gradatum".into(),
        },
        TrustContext::Studio { user, .. } => HttpAuditActor {
            kid: "studio".into(),
            sub: user.clone(),
            aud: "gradatum-studio".into(),
        },
        TrustContext::Unauthenticated => HttpAuditActor {
            kid: String::new(),
            sub: String::new(),
            aud: String::new(),
        },
    }
}

/// Emits an `auth_failure` audit event (401/403) — best-effort, non-fatal.
async fn emit_audit_failure(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    request_id: &str,
    reason: &str,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "auth_failure".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: format!("{tenant_id}/event-log"),
        note_id: None,
        content_hash: None,
        outcome: "denied".into(),
        curator: Some(serde_json::json!({ "reason": reason })),
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, reason = reason, "audit event_log auth_failure échoué");
    }
}

/// Emits an `event_log_ingest` audit event (outcome `accepted`) — best-effort, non-fatal.
async fn emit_audit_success(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    locus: &str,
    request_id: &str,
    count: usize,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "event_log_ingest".into(),
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
        tracing::warn!(error = %e, "audit event_log_ingest échoué — non fatal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request},
        Router,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use gradatum_acl_policy::AclEngine;
    use gradatum_auth::jwt::JwtService;
    use gradatum_core::trust::TrustContext;

    use crate::event_log_store::EventLogStore;

    /// Construit un `QaEventDto` minimal pour les tests.
    fn make_dto(has_tokens: bool) -> QaEventDto {
        QaEventDto {
            route: "/v1/chat/completions".to_owned(),
            model_alias: "alias-test".to_owned(),
            provider: "test-provider".to_owned(),
            status_code: 200,
            latency_ms: 42,
            timestamp: "2026-06-01T12:00:00Z".to_owned(),
            feature_id: None,
            model_used: None,
            tokens_input: if has_tokens { Some(100) } else { None },
            tokens_output: if has_tokens { Some(50) } else { None },
            cost_usd: None,
            agent_id: None,
        }
    }

    /// Construit un `TrustContext::BearerToken` de test pour tenant "main".
    fn trust_main() -> TrustContext {
        TrustContext::BearerToken {
            kid: "test-kid".to_owned(),
            aud: "gradatum".to_owned(),
            sub: "gateway".to_owned(),
            scopes: vec!["service".to_owned()],
            tenant_id: "main".to_owned(),
        }
    }

    /// Preset ACL accordant Write sur `main/event-log` au consumer "gateway".
    fn acl_with_event_log_write() -> AclEngine {
        let preset = r#"
[[consumer]]
identity = "gateway"
read_patterns = []
write_patterns = ["main/event-log"]
sees_personal_classified = false
token_hash = "placeholder"
"#;
        AclEngine::from_preset_str(preset).expect("preset ACL valide")
    }

    /// Preset ACL refusant tout accès au consumer "gateway".
    fn acl_deny_all() -> AclEngine {
        AclEngine::from_preset_str("").expect("preset vide valide")
    }

    /// Construit un routeur de test avec event_log injecté.
    async fn test_router(acl: AclEngine, trust: TrustContext) -> Router {
        use axum::routing::post;

        let jwt = JwtService::new_ephemeral();
        let event_log_store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory event_log");

        let state =
            crate::state::AppState::with_jwt_and_acl(jwt, acl).with_event_log(event_log_store);

        Router::new()
            .route("/api/v1/event-log", post(post_event_log))
            .layer(axum::Extension(trust))
            .with_state(state)
    }

    /// Effectue un POST JSON et retourne (status, body_vec).
    async fn post_json(router: Router, body: impl serde::Serialize) -> (StatusCode, Vec<u8>) {
        let body_bytes = serde_json::to_vec(&body).expect("sérialisation JSON");
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/api/v1/event-log")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .expect("construction requête");

        let response = router.oneshot(req).await.expect("appel router");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("lecture body")
            .to_bytes()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn post_authenticated_batch_3_returns_200_and_accepted_count() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let events = vec![make_dto(true), make_dto(false), make_dto(true)];

        let (status, body) = post_json(router, events).await;
        assert_eq!(status, StatusCode::OK, "doit retourner 200");

        let resp: EventLogResponse = serde_json::from_slice(&body).expect("parse EventLogResponse");
        assert_eq!(resp.accepted_count, 3, "doit accepter 3 events");
        assert_eq!(resp.status, "accepted");
    }

    #[tokio::test]
    async fn post_unauthenticated_returns_401() {
        // TrustContext::Unauthenticated → 401.
        let router = test_router(acl_with_event_log_write(), TrustContext::Unauthenticated).await;
        let events = vec![make_dto(false)];

        let (status, _) = post_json(router, events).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "non-authentifié → 401");
    }

    #[tokio::test]
    async fn post_acl_deny_returns_403() {
        // ACL vide → DenyImplicit pour tout.
        let router = test_router(acl_deny_all(), trust_main()).await;
        let events = vec![make_dto(false)];

        let (status, _) = post_json(router, events).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "ACL deny → 403");
    }

    #[tokio::test]
    async fn post_batch_over_1000_returns_413() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        // 1001 events → 413.
        let events: Vec<QaEventDto> = (0..=1000).map(|_| make_dto(false)).collect();

        let (status, _) = post_json(router, events).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "batch > 1000 → 413");
    }

    #[tokio::test]
    async fn post_tokens_none_accepted() {
        // Streaming event sans tokens → doit être accepté (champs None).
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let events = vec![make_dto(false)]; // tokens_input/output = None

        let (status, body) = post_json(router, events).await;
        assert_eq!(status, StatusCode::OK);
        let resp: EventLogResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(resp.accepted_count, 1);
    }

    #[tokio::test]
    async fn post_empty_batch_returns_200_count_0() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let events: Vec<QaEventDto> = vec![];

        let (status, body) = post_json(router, events).await;
        assert_eq!(status, StatusCode::OK);
        let resp: EventLogResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(resp.accepted_count, 0);
    }

    // ── Tests sécurité F1/F2 ─────────────────────────────────────────────────

    /// F1 — Un timestamp RFC3339 invalide doit retourner 400, pas 500.
    ///
    /// Un 500 induirait le client en erreur (retry inutile) et constitue un
    /// vecteur DoS via accumulation de logs d'erreur serveur.
    #[tokio::test]
    async fn post_invalid_timestamp_returns_400_not_500() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let mut event = make_dto(false);
        event.timestamp = "NOT-A-DATE".to_owned();
        let events = vec![event];

        let (status, _) = post_json(router, events).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "timestamp invalide → 400 (pas 500)"
        );
    }

    /// F2 — Un champ `route` dépassant MAX_FIELD_LEN doit retourner 422.
    ///
    /// Protège contre un client authentifié qui enverrait des champs de plusieurs MB.
    #[tokio::test]
    async fn post_field_over_max_len_returns_422() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let mut event = make_dto(false);
        // 1025 chars > MAX_FIELD_LEN (1024).
        event.route = "x".repeat(1025);
        let events = vec![event];

        let (status, _) = post_json(router, events).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "champ route > 1024 chars → 422"
        );
    }

    /// F2 — Un champ `timestamp` dépassant MAX_TIMESTAMP_LEN (64) doit retourner 422.
    ///
    /// Borne le timestamp AVANT de tenter le parse RFC3339 (validation rapide client).
    #[tokio::test]
    async fn post_timestamp_over_max_ts_len_returns_422() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let mut event = make_dto(false);
        // 65 chars > MAX_TIMESTAMP_LEN (64).
        event.timestamp = "2026-06-01T12:00:00Z".to_owned() + &"x".repeat(45);
        let events = vec![event];

        let (status, _) = post_json(router, events).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "timestamp > 64 chars → 422"
        );
    }

    // ── Tests agent_id ────────────────────────────────────────────────────────

    /// POST avec agent_id présent → 200 accepted.
    ///
    /// Vérifie que le handler accepte les events avec agent_id et retourne 200.
    #[tokio::test]
    async fn post_with_agent_id_returns_200() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let mut event = make_dto(false);
        event.agent_id = Some("example-agent".to_owned());
        let events = vec![event];

        let (status, body) = post_json(router, events).await;
        assert_eq!(status, StatusCode::OK, "event avec agent_id → 200");
        let resp: EventLogResponse = serde_json::from_slice(&body).expect("parse EventLogResponse");
        assert_eq!(resp.accepted_count, 1, "doit accepter 1 event");
        assert_eq!(resp.status, "accepted");
    }

    /// POST sans agent_id (champ absent) → 200 accepted (rétrocompat).
    ///
    /// Vérifie que les clients sans agent_id sont acceptés sans régression.
    #[tokio::test]
    async fn post_without_agent_id_returns_200_retrocompat() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        // make_dto retourne agent_id = None → champ omis dans la sérialisation JSON
        // (QaEvent gateway utilise skip_serializing_if = "Option::is_none")
        let events = vec![make_dto(false)];

        let (status, body) = post_json(router, events).await;
        assert_eq!(status, StatusCode::OK, "event sans agent_id → 200");
        let resp: EventLogResponse = serde_json::from_slice(&body).expect("parse EventLogResponse");
        assert_eq!(resp.accepted_count, 1);
    }

    /// F2 — agent_id > MAX_FIELD_LEN (1024) → 422.
    ///
    /// Cohérent avec la borne feature_id / model_used.
    #[tokio::test]
    async fn post_agent_id_over_max_field_len_returns_422() {
        let router = test_router(acl_with_event_log_write(), trust_main()).await;
        let mut event = make_dto(false);
        // 1025 chars > MAX_FIELD_LEN (1024).
        event.agent_id = Some("a".repeat(1025));
        let events = vec![event];

        let (status, _) = post_json(router, events).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_id > 1024 chars → 422"
        );
    }
}
