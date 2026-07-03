//! Tests F-74 v0.7.4 — Validation serveur occurred_at (Task 2).
//!
//! ## Couverture
//!
//! - occurred_at invalide → 400 BadRequest (fail-fast avant enqueue)
//! - occurred_at YYYY-MM-DD valide → 202 Accepted
//! - occurred_at ISO 8601 complet valide → 202 Accepted
//! - absent (backward-compat) → 202 Accepted
//!
//! ## Harness
//!
//! Tower `oneshot` — pas de spawn réseau, validation HTTP seule.
//! Les tests E2E dispatch (anchor_src/anchor_ms) sont dans
//! `gradatum-worker/tests/curate_temporal_anchor.rs` (pattern handle_curate).
//!
//! ## Architecture
//!
//! `vault_write` HTTP → job enqueué dans `SqliteQueueStore` (apalis) — séparé du
//! `SqliteQueue` legacy que lit `Dispatcher::run_once`. Les tests E2E doivent donc
//! appeler `handle_curate` directement depuis le crate worker.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Router, body::Body, http::Request, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_queue::SqliteQueue;
use gradatum_server::api_v1;
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tower::util::ServiceExt as _;

// ── ACL ──────────────────────────────────────────────────────────────────────

const WRITE_ACL: &str = r#"
[[consumer]]
identity = "main-agent"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Trust middleware stub ─────────────────────────────────────────────────────

/// Extrait la valeur brute du Bearer header comme `sub` (pas de validation JWT).
async fn trust_stub(
    mut req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    let trust = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .map(|t| TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: t.to_string(),
            scopes: vec!["read".into(), "write".into()],
            tenant_id: "main".into(),
        })
        .unwrap_or(TrustContext::Unauthenticated);
    req.extensions_mut().insert(trust);
    next.run(req).await
}

// ── Helper ────────────────────────────────────────────────────────────────────

async fn build_write_app() -> (Router, TempDir) {
    let tmp = TempDir::new().expect("TempDir temporal_anchor_e2e");
    let vault = Arc::new(
        Vault::create(
            &tmp.path().join("vault"),
            gradatum_core::scope::VaultId::new("main"),
        )
        .await
        .expect("Vault::create — invariant test fixture"),
    );
    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory — invariant test fixture"),
    );
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool — invariant test fixture");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations — invariant test fixture");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(WRITE_ACL).expect("ACL temporal_anchor valide");

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(queue as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(
            Arc::clone(&job_store) as Arc<dyn gradatum_core::QueueStore>,
            jobs_pool,
        )
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    (app, tmp)
}

async fn post_vault_write(app: Router, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .uri("/api/v1/vault_write")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", "Bearer main-agent")
        .body(Body::from(
            serde_json::to_vec(&body).expect("sérialisation body"),
        ))
        .expect("construction requête");
    app.oneshot(req)
        .await
        .expect("oneshot vault_write")
        .status()
}

// ════════════════════════════════════════════════════════════════════════════
// Task 2 — Validation serveur occurred_at
// ════════════════════════════════════════════════════════════════════════════

/// occurred_at non parseable → 400 InvalidInput.
///
/// Garantit le fail-fast serveur avant enqueue : la valeur invalide est rejetée
/// AVANT que le job soit enfilé dans la queue.
#[tokio::test]
async fn vault_write_invalid_occurred_at_returns_400() {
    let (app, _tmp) = build_write_app().await;
    let status = post_vault_write(
        app,
        serde_json::json!({
            "title": "Note test occurred_at invalide",
            "body":  "corps test",
            "tenant_id": "main",
            "occurred_at": "pas-une-date"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "occurred_at invalide doit retourner 400"
    );
}

/// occurred_at valide (YYYY-MM-DD) → 202 Accepted.
#[tokio::test]
async fn vault_write_valid_occurred_at_date_returns_202() {
    let (app, _tmp) = build_write_app().await;
    let status = post_vault_write(
        app,
        serde_json::json!({
            "title": "Note test occurred_at valide",
            "body":  "corps test",
            "tenant_id": "main",
            "occurred_at": "2026-01-15"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "occurred_at valide YYYY-MM-DD doit retourner 202"
    );
}

/// occurred_at valide (ISO 8601 complet) → 202 Accepted.
#[tokio::test]
async fn vault_write_valid_occurred_at_iso8601_returns_202() {
    let (app, _tmp) = build_write_app().await;
    let status = post_vault_write(
        app,
        serde_json::json!({
            "title": "Note test occurred_at ISO8601",
            "body":  "corps test",
            "tenant_id": "main",
            "occurred_at": "2026-01-15T10:00:00Z"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "occurred_at valide ISO8601 doit retourner 202"
    );
}

/// Backward-compat : sans occurred_at → 202 (comportement historique inchangé).
#[tokio::test]
async fn vault_write_without_occurred_at_returns_202() {
    let (app, _tmp) = build_write_app().await;
    let status = post_vault_write(
        app,
        serde_json::json!({
            "title": "Note test sans occurred_at",
            "body":  "corps test",
            "tenant_id": "main"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "absence d'occurred_at doit retourner 202 (backward-compat)"
    );
}
