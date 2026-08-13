//! Tests E2E vault_search — filtre `status` + champ `status` dans les hits (F-37 notes fix).
//!
//! Couvre :
//! 1. `status_field_present_in_hits` — chaque hit expose `status` (valeur SQL brute).
//! 2. `status_filter_restricts_results` — `status=pending-review` ne retourne que les
//!    notes de ce statut (FTS SQL-side).
//! 3. `status_filter_invalid_is_400` — valeur hors liste → 400.
//! 4. `status_filter_downgraded_legacy_accepted` — `downgraded` (legacy SQL) accepté.

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
identity = "status-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-status"
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
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test status_filter"),
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
            "status-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

fn search_req(token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Seed FTS-indexed note with a given status. Body fixed so FTS matches "alpha status".
async fn seed(idx: &Arc<SqliteIndex>, id: &str, status: gradatum_core::status::NoteStatus) {
    use gradatum_core::section::Section;
    idx.seed_note_with_status(
        id,
        Section::Reference,
        "alpha status filter token query body",
        status,
        None,
    )
    .await
    .expect("seed");
}

// S-1 : chaque hit expose `status` (valeur SQL brute), sans filtre.
#[tokio::test]
async fn status_field_present_in_hits() {
    use gradatum_core::status::NoteStatus;
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let id_live = Ulid::generate().to_string();
    seed(&idx, &id_live, NoteStatus::Live).await;

    let req = search_req(
        &token,
        serde_json::json!({ "query": "alpha status token", "tenant_id": "main" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty(), "au moins un hit. body={json}");
    for it in items {
        assert_eq!(
            it["status"].as_str(),
            Some("live"),
            "chaque hit doit exposer status='live'. hit={it}"
        );
    }
}

// S-2 : status=pending-review ne retourne que les notes de ce statut (FTS SQL-side).
#[tokio::test]
async fn status_filter_restricts_results() {
    use gradatum_core::status::NoteStatus;
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let id_live = Ulid::generate().to_string();
    let id_pr = Ulid::generate().to_string();
    seed(&idx, &id_live, NoteStatus::Live).await;
    seed(&idx, &id_pr, NoteStatus::PendingReview).await;

    let req = search_req(
        &token,
        serde_json::json!({ "query": "alpha status token", "tenant_id": "main", "status": "pending-review" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");

    assert!(
        !items.is_empty(),
        "au moins le hit pending-review. body={json}"
    );
    for it in items {
        assert_eq!(
            it["status"].as_str(),
            Some("pending-review"),
            "filtre status=pending-review : aucun autre statut. hit={it}"
        );
        let path = it["path"].as_str().unwrap_or("");
        assert!(
            path.contains(&id_pr),
            "seule la note pending-review doit apparaître. path={path}"
        );
    }
}

// S-3 : status invalide → 400.
#[tokio::test]
async fn status_filter_invalid_is_400() {
    let (app, state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let req = search_req(
        &token,
        serde_json::json!({ "query": "alpha", "tenant_id": "main", "status": "archived" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// S-4 : status=downgraded (legacy SQL hors enum) accepté (200).
#[tokio::test]
async fn status_filter_downgraded_legacy_accepted() {
    let (app, state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let req = search_req(
        &token,
        serde_json::json!({ "query": "alpha", "tenant_id": "main", "status": "downgraded" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "downgraded (legacy SQL) doit être accepté"
    );
}
