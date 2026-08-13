//! `POST /api/v1/session-log/trace` — append-only session trace handler.
//!
//! ## Contract
//!
//! | Method | Path                          | Auth       | Body                       |
//! |--------|-------------------------------|------------|----------------------------|
//! | POST   | `/api/v1/session-log/trace`   | bearer JWT | `Json<SessionTraceRequest>`|
//!
//! ## HTTP codes
//!
//! | Code | Reason |
//! |------|--------|
//! | 200  | Trace inserted. Body: `SessionTraceResponse { id, session_id }`. |
//! | 401  | Unauthenticated, or non-`BearerToken` context (mTLS/Studio rejected). |
//! | 403  | ACL Write denied on `{tenant_id}/session-log`. |
//! | 422  | Field out of bounds: `action_type` empty or >64, `intent` >200, `target` >512, |
//! |      | `outcome` ∉ {success,failure,partial}, invalid `ref`, `session_id` not a ULID, |
//! |      | body `tenant_id` ≠ JWT, `ts_ms` < 0 or > now+60 s. |
//! | 503  | `session_trace` store not wired in `AppState`. |
//! | 500  | SQLite insert failure. |
//!
//! ## Security invariants
//!
//! - `agent_id` = JWT `sub` (destructured from `TrustContext::BearerToken`),
//!   NEVER read from the body. `tenant_id` for ACL = JWT (never from the body).
//! - Field bounds are enforced server-side → 422 on overflow.
//! - `session_id` = server-generated ULID when omitted; ULID format validated when provided.
//! - Insert is synchronously awaited (not fire-and-forget) — reliability guarantee.
//!
//! ## Append-only
//!
//! This handler performs INSERT only — no UPDATE/DELETE path per record.

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::trust::TrustContext;
use ulid::Ulid;

use crate::api_v1::dto::{SessionTraceRequest, SessionTraceResponse};
use crate::session_trace_store::SessionTraceRow;
use crate::state::AppState;

/// Maximum length of the `intent` field.
const MAX_INTENT: usize = 200;
/// Maximum length of the `target` field.
const MAX_TARGET: usize = 512;
/// Maximum length of the `action_type` field.
const MAX_ACTION: usize = 64;
/// Exact length of a ULID (Crockford base32, 26 characters).
const ULID_LEN: usize = 26;
/// Allowed values for the `outcome` field.
const OUTCOMES: [&str; 3] = ["success", "failure", "partial"];
/// Tolerated clock drift on a future `ts_ms` (60 s) before rejecting with 422.
const MAX_TS_DRIFT_MS: i64 = 60_000;

/// Validates the ULID format: 26 ASCII alphanumeric characters (Crockford base32).
///
/// Format validation only (length + alphabet) — no strict Crockford checksum.
/// Sufficient to bound input and prevent injection.
fn is_ulid_shape(s: &str) -> bool {
    s.len() == ULID_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Validates the `ref` field: ULID-26 | sha 7–40 hex | `section/ULID`.
fn valid_ref(s: &str) -> bool {
    let sha = |x: &str| (7..=40).contains(&x.len()) && x.bytes().all(|b| b.is_ascii_hexdigit());
    if is_ulid_shape(s) || sha(s) {
        return true;
    }
    matches!(s.split_once('/'), Some((sec, id)) if !sec.is_empty() && sec.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') && is_ulid_shape(id))
}

/// `POST /api/v1/session-log/trace` — see module documentation.
///
/// # Side effects
///
/// Inserts one row into `session_trace` (append-only). Insert is synchronously awaited.
pub async fn post_session_trace(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<SessionTraceRequest>,
) -> Result<(StatusCode, Json<SessionTraceResponse>), StatusCode> {
    // 1. Authentification obligatoire.
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // 2. C-SA1 : agent_id = JWT sub (destructuring), tenant_id = JWT — jamais du body.
    //    Seul BearerToken porte `sub`/`tenant_id` → mTLS/Studio refusés (401).
    let (agent_id, tenant_id) = match &trust {
        // Frontière : `tenant_id` typé `TenantId` (Task 3) ; comparé au `req.tenant_id`
        // (String, DTO) et passé à `insert_trace(&str)` — `.as_str().to_owned()` byte-identical.
        TrustContext::BearerToken { sub, tenant_id, .. } => {
            // `sub` typé `AgentId` ; `SessionTraceRow.agent_id` reste `String`
            // (frontière DTO) — `.as_str().to_owned()` byte-identical.
            (sub.as_str().to_owned(), tenant_id.as_str().to_owned())
        }
        _ => return Err(StatusCode::UNAUTHORIZED),
    };

    // 2bis. tenant_id du body : explicitness uniquement. S'il est fourni ET diffère
    //       du tenant_id du JWT → 422 (pas de divergence silencieuse client/serveur).
    //       L'identité ACL reste TOUJOURS le tenant_id du JWT (C-SA1 inchangé).
    // Lot A1 : `tenant_id` du body est optionnel. Omis (`None`) → aucune divergence
    // possible (le tenant est celui du JWT). Fourni ET divergent → 422 (explicitness).
    if let Some(body_tenant) = req.tenant_id.as_ref()
        && body_tenant.as_str() != tenant_id
    {
        tracing::warn!(
            body_tenant = %body_tenant,
            jwt_tenant = %tenant_id,
            "session_trace: body tenant_id ≠ JWT → 422"
        );
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // 3. ACL Write sur le locus session-log du tenant (du JWT).
    let locus = format!("{tenant_id}/session-log");
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        tracing::warn!(locus = %locus, "session_trace: ACL Write deny");
        return Err(StatusCode::FORBIDDEN);
    }
    // C3a (EX-C3a-1) : à ON, l'append de trace est une écriture — scope write exigé.
    if !crate::api_v1::tenant_guard::write_scope_allowed(&state, &trust) {
        return Err(StatusCode::FORBIDDEN);
    }

    // 4. C-SA2 : bornes enforced server-side → 422.
    if req.action_type.is_empty() || req.action_type.len() > MAX_ACTION {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if req.intent.as_ref().is_some_and(|s| s.len() > MAX_INTENT) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if req.target.as_ref().is_some_and(|s| s.len() > MAX_TARGET) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if req
        .outcome
        .as_ref()
        .is_some_and(|s| !OUTCOMES.contains(&s.as_str()))
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if req.r#ref.as_ref().is_some_and(|s| !valid_ref(s)) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // ts_ms client-controlled : borner à [0, now + 60s] (dérive horloge tolérée)
    // pour rejeter les valeurs aberrantes (négatives ou très futures). now = serveur.
    let now_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    if req.ts_ms < 0 || req.ts_ms > now_ms + MAX_TS_DRIFT_MS {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // 5. C-SA6 : session_id ULID — server-gen si omis, format validé si fourni.
    let session_id = match req.session_id {
        Some(s) => {
            if !is_ulid_shape(&s) {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            s
        }
        None => Ulid::generate().to_string(),
    };

    // 6. Récupérer le store (None = non câblé → 503).
    let store = match &state.session_trace {
        Some(s) => s,
        None => {
            tracing::error!("session_trace not wired in AppState — check with_session_trace_path");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    let row = SessionTraceRow {
        session_id: session_id.clone(),
        agent_id,
        ts_ms: req.ts_ms,
        action_type: req.action_type,
        target: req.target,
        intent: req.intent,
        outcome: req.outcome,
        marker: None, // Tier 2 = Phase 1b
        ref_: req.r#ref,
    };

    // 7. C-SA7 : insert synchrone awaité (fiable, pas fire-and-forget).
    let id = store.insert_trace(&tenant_id, &row).await.map_err(|e| {
        tracing::error!(error = %e, "session_trace insert failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((
        StatusCode::OK,
        Json(SessionTraceResponse { id, session_id }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{self, Request},
        routing::post,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use gradatum_acl_policy::AclEngine;
    use gradatum_auth::jwt::JwtService;

    use crate::session_trace_store::SessionTraceStore;

    /// `TrustContext::BearerToken` de test pour tenant "main", sub "claude-code".
    fn trust_main() -> TrustContext {
        TrustContext::BearerToken {
            kid: "test-kid".to_owned(),
            aud: "gradatum".to_owned(),
            sub: "claude-code".into(),
            scopes: vec!["service".to_owned()],
            tenant_id: "main".into(),
            jti: None,
        }
    }

    /// Preset ACL accordant Write sur `main/session-log` au consumer "claude-code".
    fn acl_with_write() -> AclEngine {
        let preset = r#"
[[consumer]]
identity = "claude-code"
read_patterns = []
write_patterns = ["main/session-log"]
sees_personal_classified = false
token_hash = "placeholder"
"#;
        AclEngine::from_preset_str(preset).expect("preset ACL valide")
    }

    /// Preset ACL refusant tout accès.
    fn acl_deny_all() -> AclEngine {
        AclEngine::from_preset_str("").expect("preset vide valide")
    }

    /// Routeur de test avec session_trace injecté.
    async fn test_router(acl: AclEngine, trust: TrustContext) -> Router {
        let jwt = JwtService::new_ephemeral();
        let store = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory session_trace");
        let state = crate::state::AppState::with_jwt_and_acl(jwt, acl).with_session_trace(store);

        Router::new()
            .route("/api/v1/session-log/trace", post(post_session_trace))
            .layer(axum::Extension(trust))
            .with_state(state)
    }

    /// POST JSON → (status, body_vec).
    async fn post_json(router: Router, body: serde_json::Value) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/api/v1/session-log/trace")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("ser JSON")))
            .expect("build req");
        let response = router.oneshot(req).await.expect("oneshot");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn trace_requires_auth() {
        let router = test_router(acl_with_write(), TrustContext::Unauthenticated).await;
        let (status, _) =
            post_json(router, serde_json::json!({"ts_ms":1,"action_type":"plan"})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn trace_inserts_and_server_gens_session_id() {
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, body) = post_json(
            router,
            serde_json::json!({"ts_ms":1000,"action_type":"deploy","intent":"x","outcome":"success"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "doit insérer → 200");
        let b: serde_json::Value = serde_json::from_slice(&body).expect("parse resp");
        assert!(b["id"].as_i64().unwrap() > 0, "rowid > 0");
        // ULID server-gen (C-SA6) : 26 chars.
        assert_eq!(
            b["session_id"].as_str().unwrap().len(),
            26,
            "session_id ULID server-gen 26 chars"
        );
    }

    #[tokio::test]
    async fn trace_accepts_valid_session_id() {
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, body) = post_json(
            router,
            serde_json::json!({
                "ts_ms":1,"action_type":"plan",
                "session_id":"01HQ0000000000000000000000"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let b: serde_json::Value = serde_json::from_slice(&body).expect("parse");
        assert_eq!(
            b["session_id"].as_str().unwrap(),
            "01HQ0000000000000000000000",
            "session_id fourni doit être préservé"
        );
    }

    #[tokio::test]
    async fn trace_rejects_bad_session_id() {
        // C-SA6 : session_id fourni hors format ULID → 422.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","session_id":"not-a-ulid"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_oversized_intent() {
        // C-SA2.
        let router = test_router(acl_with_write(), trust_main()).await;
        let big = "x".repeat(201);
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","intent":big}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_oversized_target() {
        // C-SA2.
        let router = test_router(acl_with_write(), trust_main()).await;
        let big = "x".repeat(513);
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","target":big}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_bad_outcome() {
        // C-SA2 enum.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","outcome":"bogus"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_empty_action_type() {
        // C-SA2.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(router, serde_json::json!({"ts_ms":1,"action_type":""})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_oversized_action_type() {
        // C-SA2.
        let router = test_router(acl_with_write(), trust_main()).await;
        let big = "x".repeat(65);
        let (status, _) = post_json(router, serde_json::json!({"ts_ms":1,"action_type":big})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_rejects_bad_ref() {
        // C-SA2 : ref non sha/ulid/section-ulid → 422.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","ref":"not a ref!"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn trace_accepts_valid_sha_ref() {
        // ref sha7 valide → 200.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"deploy","ref":"a9982a8"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn trace_rejects_agent_id_in_body() {
        // C-SA1 : deny_unknown_fields → un agent_id injecté dans le body est rejeté
        // au désérialisation (422 via le ModelRejection JSON d'axum).
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":1,"action_type":"plan","agent_id":"evil"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_id dans le body → 422 (deny_unknown_fields, C-SA1)"
        );
    }

    #[tokio::test]
    async fn trace_rejects_divergent_tenant_id_in_body() {
        // Fix #1 (A01) : tenant_id body ≠ JWT → 422 (pas de divergence silencieuse).
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({
                "ts_ms":1,"action_type":"plan","tenant_id":"other"
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "tenant_id body ≠ JWT → 422"
        );
    }

    #[tokio::test]
    async fn trace_accepts_matching_tenant_id_in_body() {
        // Fix #1 : tenant_id body == JWT ("main") → accepté (explicitness).
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({
                "ts_ms":1,"action_type":"plan","tenant_id":"main"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "tenant_id body == JWT → 200");
    }

    #[tokio::test]
    async fn trace_rejects_ref_with_path_traversal_in_section() {
        // Fix #2 (A03) : section avec `../` (charset non-[a-z0-9_-]) → 422.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({
                "ts_ms":1,"action_type":"plan",
                "ref":"../../evil/01HQ0000000000000000000000"
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "ref section avec ../ → 422"
        );
    }

    #[tokio::test]
    async fn trace_accepts_valid_section_ref() {
        // Fix #2 : section charset valide ([a-z0-9_-]) + ULID → 200.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({
                "ts_ms":1,"action_type":"plan",
                "ref":"decisions/01HQ0000000000000000000000"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "section/ULID valide → 200");
    }

    #[tokio::test]
    async fn trace_rejects_negative_ts_ms() {
        // Fix #3 (A04) : ts_ms négatif → 422.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) =
            post_json(router, serde_json::json!({"ts_ms":-1,"action_type":"plan"})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "ts_ms négatif → 422"
        );
    }

    #[tokio::test]
    async fn trace_rejects_far_future_ts_ms() {
        // Fix #3 : ts_ms an 316887 (très futur, > now + 60s) → 422.
        let router = test_router(acl_with_write(), trust_main()).await;
        let (status, _) = post_json(
            router,
            serde_json::json!({"ts_ms":9_999_999_999_999_i64,"action_type":"plan"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "ts_ms futur lointain → 422"
        );
    }

    #[tokio::test]
    async fn trace_acl_deny_403() {
        let router = test_router(acl_deny_all(), trust_main()).await;
        let (status, _) =
            post_json(router, serde_json::json!({"ts_ms":1,"action_type":"plan"})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
