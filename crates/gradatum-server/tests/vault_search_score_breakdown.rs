//! Tests E2E vault_search — F-37 S1.1 décomposition de score opt-in.
//!
//! Couvre :
//! 1. `score_breakdown_omitted_by_default` — sans `include_scores`, le champ `scores`
//!    est absent (rétrocompat wire totale).
//! 2. `score_breakdown_present_when_opted_in` — avec `include_scores:true`, chaque hit
//!    porte un objet `scores` cohérent (composite == score, rrf_score, in_degree, ranks).

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
identity = "search-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-breakdown"
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
            .expect("SqliteIndex::open_in_memory() — invariant test score_breakdown"),
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

fn search_req(query: &str, token: &str, include_scores: bool) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 10,
        "tenant_id": "main",
        "include_scores": include_scores,
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

async fn seed_one(idx: &Arc<SqliteIndex>) -> String {
    let id = Ulid::new().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_note_with_created(
        &id,
        "reference",
        "content gradatum breakdown query token alpha",
        now_ms,
    )
    .await
    .expect("seed note");
    id
}

// S1.1-1 : Sans include_scores, le champ `scores` est absent de chaque hit.
#[tokio::test]
async fn score_breakdown_omitted_by_default() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let _id = seed_one(&idx).await;

    // Requête SANS include_scores (champ omis → défaut false).
    let body = serde_json::json!({ "query": "breakdown query token", "tenant_id": "main" });
    let req = Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty(), "au moins un hit attendu. body={json}");

    for it in items {
        assert!(
            it.get("scores").is_none(),
            "le champ `scores` doit être absent sans include_scores. hit={it}"
        );
        // Le champ legacy `trust` reste présent (rétrocompat).
        assert_eq!(it["trust"].as_f64(), Some(0.5), "trust legacy = 0.5");
    }
}

// S1.1-2 : Avec include_scores:true, chaque hit porte un objet `scores` cohérent.
#[tokio::test]
async fn score_breakdown_present_when_opted_in() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let _id = seed_one(&idx).await;

    let req = search_req("breakdown query token", &token, true);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty(), "au moins un hit attendu. body={json}");

    for it in items {
        let scores = it
            .get("scores")
            .unwrap_or_else(|| panic!("`scores` attendu avec include_scores. hit={it}"));

        // Champs obligatoires présents.
        let rrf = scores["rrf_score"].as_f64().expect("rrf_score f64");
        let recency = scores["recency_factor"]
            .as_f64()
            .expect("recency_factor f64");
        let pagerank = scores["pagerank_factor"]
            .as_f64()
            .expect("pagerank_factor f64");
        let composite = scores["composite"].as_f64().expect("composite f64");
        assert!(scores.get("in_degree").is_some(), "in_degree attendu");

        // Invariants : facteurs bornés, rrf > 0, composite == score retourné.
        assert!(rrf > 0.0, "rrf_score > 0, got {rrf}");
        assert!(
            recency > 0.0 && recency <= 1.0,
            "recency ∈ (0,1], got {recency}"
        );
        assert!(
            (0.0..=1.0).contains(&pagerank),
            "pagerank ∈ [0,1], got {pagerank}"
        );

        let hit_score = it["score"].as_f64().expect("score f64");
        // `score` est un f32 sérialisé ; comparer à tolérance f32.
        assert!(
            (hit_score - composite).abs() < 1e-4,
            "composite ({composite}) doit égaler score ({hit_score})"
        );

        // La note seedée match BM25 → bm25_rank présent (0-indexé).
        assert!(
            scores.get("bm25_rank").is_some(),
            "bm25_rank attendu pour un hit lexical. scores={scores}"
        );
    }
}
