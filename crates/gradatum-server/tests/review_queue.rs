//! Tests E2E `GET /api/v1/review` — file de revue (F-37 S1.2).
//!
//! Couvre :
//! 1. `review_unauthenticated_is_401` — auth obligatoire.
//! 2. `review_lists_pending_review_and_staging` — les deux statuts apparaissent,
//!    avec provenance ; les autres statuts (Live/Draft) sont exclus.
//! 3. `review_bad_cursor_is_400` — cursor non-ULID rejeté.

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
identity = "review-tester"
read_patterns  = ["main/*", "main/review", "reference/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-review"
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
            .expect("SqliteIndex::open_in_memory() — invariant test review_queue"),
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
            "review-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

fn get_review(token: Option<&str>, query: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(format!("/api/v1/review{query}"))
        .method("GET");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::empty()).unwrap()
}

// S1.2-1 : sans token → 401.
#[tokio::test]
async fn review_unauthenticated_is_401() {
    let (app, _state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let resp = app.oneshot(get_review(None, "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// S1.2-2 : liste PendingReview + Staging avec provenance ; exclut Live.
#[tokio::test]
async fn review_lists_pending_review_and_staging() {
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    // Seed 3 notes : PendingReview (distilled), Staging (legacy), Live (exclue).
    let id_pr = Ulid::new().to_string();
    let id_st = Ulid::new().to_string();
    let id_live = Ulid::new().to_string();

    idx.seed_note_with_status(
        &id_pr,
        Section::Decisions,
        "note pending review",
        NoteStatus::PendingReview,
        Some("distilled"),
    )
    .await
    .expect("seed pending-review");
    idx.seed_note_with_status(
        &id_st,
        Section::Reference,
        "note staging legacy",
        NoteStatus::Staging,
        None,
    )
    .await
    .expect("seed staging");
    idx.seed_note_with_status(
        &id_live,
        Section::Reference,
        "note live",
        NoteStatus::Live,
        None,
    )
    .await
    .expect("seed live");

    let resp = app.oneshot(get_review(Some(&token), "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let items = json["items"].as_array().expect("items");
    let ulids: Vec<&str> = items.iter().filter_map(|i| i["ulid"].as_str()).collect();

    assert!(
        ulids.contains(&id_pr.as_str()),
        "PendingReview attendu. body={json}"
    );
    assert!(
        ulids.contains(&id_st.as_str()),
        "Staging attendu. body={json}"
    );
    assert!(
        !ulids.contains(&id_live.as_str()),
        "Live ne doit PAS apparaître. body={json}"
    );
    assert_eq!(json["total"].as_u64(), Some(2), "total = 2 (pr + staging)");

    // Provenance distilled visible sur la note PendingReview.
    let pr_item = items
        .iter()
        .find(|i| i["ulid"].as_str() == Some(id_pr.as_str()))
        .expect("item pr");
    assert_eq!(pr_item["provenance"].as_str(), Some("distilled"));
    assert_eq!(pr_item["status"].as_str(), Some("pending-review"));

    let st_item = items
        .iter()
        .find(|i| i["ulid"].as_str() == Some(id_st.as_str()))
        .expect("item staging");
    assert_eq!(st_item["status"].as_str(), Some("staging"));
}

// S1.2-3 : cursor non-ULID → 400.
#[tokio::test]
async fn review_bad_cursor_is_400() {
    let (app, state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let resp = app
        .oneshot(get_review(Some(&token), "?cursor=not-a-ulid"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
