//! Tests E2E `GET /api/v1/dashboard` — agrégat d'observabilité (F-37 S1.3).
//!
//! Couvre :
//! 1. `dashboard_unauthenticated_is_401` — auth obligatoire (vs /health unauth).
//! 2. `dashboard_aggregates_note_counts` — notes_by_status reflète les statuts seedés
//!    (tolérant legacy `downgraded`), forgotten_count, WAL n/a (omis), queue_depth 0.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "dash-tester"
read_patterns  = ["main/*", "main/dashboard", "reference/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-dash"
    }
    fn dim(&self) -> u16 {
        8
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0f32; 8])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{middleware, Router};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test dashboard"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(embedder);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx)
}

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "dash-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

fn get_dashboard(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri("/api/v1/dashboard").method("GET");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

// S1.3-1 : sans token → 401 (le dashboard est derrière auth, contrairement à /health).
#[tokio::test]
async fn dashboard_unauthenticated_is_401() {
    let (app, _state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let resp = app.oneshot(get_dashboard(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// S1.3-2 : agrège les comptes de notes par statut (tolérant legacy).
#[tokio::test]
async fn dashboard_aggregates_note_counts() {
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    // 2 live, 1 pending-review, 1 staging.
    for _ in 0..2 {
        idx.seed_note_with_status(
            &Ulid::new().to_string(),
            Section::Reference,
            "live note",
            NoteStatus::Live,
            None,
        )
        .await
        .expect("seed live");
    }
    idx.seed_note_with_status(
        &Ulid::new().to_string(),
        Section::Decisions,
        "pr note",
        NoteStatus::PendingReview,
        None,
    )
    .await
    .expect("seed pr");
    idx.seed_note_with_status(
        &Ulid::new().to_string(),
        Section::Reference,
        "staging note",
        NoteStatus::Staging,
        None,
    )
    .await
    .expect("seed staging");

    let resp = app.oneshot(get_dashboard(Some(&token))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let nbs = json["notes_by_status"]
        .as_object()
        .expect("notes_by_status");
    assert_eq!(nbs.get("live").and_then(|v| v.as_u64()), Some(2), "2 live");
    assert_eq!(
        nbs.get("pending-review").and_then(|v| v.as_u64()),
        Some(1),
        "1 pending-review"
    );
    assert_eq!(
        nbs.get("staging").and_then(|v| v.as_u64()),
        Some(1),
        "1 staging"
    );

    // forgotten_count présent (0 ici), queue_depth présent (0, store noop).
    assert_eq!(json["forgotten_count"].as_u64(), Some(0));
    assert_eq!(json["queue_depth"].as_u64(), Some(0));

    // WAL non mesurable (index in-memory, wal_path None) → champ OMIS ("n/a"), JAMAIS 0.
    assert!(
        json.get("wal_size_bytes").is_none(),
        "wal_size_bytes doit être omis (n/a) en in-memory, pas 0. body={json}"
    );

    // jobs_by_status présent (map vide avec NoopQueueStore).
    assert!(json["jobs_by_status"].is_object(), "jobs_by_status objet");
}
