//! Tests E2E vault_search — Multi-facteur scoring (alpha.12 Task 13).
//!
//! Couvre :
//! 1. `multifactor_recent_note_outranks_old_note_at_equal_rrf` — recency boost
//! 2. `multifactor_popular_note_outranks_isolated_at_equal_rrf` — pagerank boost
//! 3. `multifactor_scores_are_positive_and_decreasing` — invariant ordre composite

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
        "noop-multifactor"
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

/// Construit un `(Router, AppState, Arc<SqliteIndex>)` partageant le MÊME index in-memory.
///
/// L'`Arc<SqliteIndex>` concret est retourné pour `seed_note_with_created` (méthode pub concrète, hors trait).
/// `state.search` et le router partagent le même `Arc<SqliteIndex>` via coercion dyn.
async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{middleware, Router};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_search_multifactor"),
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

fn search_req(query: &str, token: &str, limit: u32) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": limit,
        "tenant_id": "main"
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

/// Helper : retourne la position 0-indexée de `note_id` dans `items`, ou `usize::MAX` si absent.
fn position_of(items: &[serde_json::Value], note_id: &str) -> usize {
    items
        .iter()
        .position(|i| i["path"].as_str().unwrap_or("").contains(note_id))
        .unwrap_or(usize::MAX)
}

// T13-1 : Note récente remonte vs note ancienne, à RRF égal (BM25 même body).
#[tokio::test]
async fn multifactor_recent_note_outranks_old_note_at_equal_rrf() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let id_a = Ulid::new().to_string();
    let id_b = Ulid::new().to_string();

    // Note A : créée il y a 1 an (low recency) — seed_note_with_created : méthode concrète.
    idx.seed_note_with_created(
        &id_a,
        "reference",
        "content gradatum search alpha multifactor query token",
        now_ms - 365 * 24 * 3_600_000,
    )
    .await
    .expect("seed A");

    // Note B : créée aujourd'hui (high recency) — même contenu → BM25 égal
    idx.seed_note_with_created(
        &id_b,
        "reference",
        "content gradatum search alpha multifactor query token",
        now_ms,
    )
    .await
    .expect("seed B");

    let req = search_req("multifactor query token", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");

    let pos_b = position_of(items, &id_b);
    let pos_a = position_of(items, &id_a);
    assert!(
        pos_b != usize::MAX && pos_a != usize::MAX,
        "les 2 notes doivent être dans la réponse. body={json}"
    );
    assert!(
        pos_b < pos_a,
        "note récente B (pos={pos_b}) doit devancer note ancienne A (pos={pos_a}). body={json}"
    );
}

// T13-2 : Note très liée remonte vs note isolée, à RRF égal (BM25 même body).
#[tokio::test]
async fn multifactor_popular_note_outranks_isolated_at_equal_rrf() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let now_ms = chrono::Utc::now().timestamp_millis();
    // Notes datées d'il y a 7 jours pour neutraliser les diff de recency.
    let seven_days_ago = now_ms - 7 * 24 * 3_600_000;

    let id_c = Ulid::new().to_string();
    let id_d = Ulid::new().to_string();

    // seed_note_with_created : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_created(
        &id_c,
        "reference",
        "gradatum architecture design popularitytest",
        seven_days_ago,
    )
    .await
    .expect("seed C");
    idx.seed_note_with_created(
        &id_d,
        "reference",
        "gradatum architecture design popularitytest",
        seven_days_ago,
    )
    .await
    .expect("seed D");

    // 5 backlinks vers id_d (isolated id_c reste à 0).
    // upsert_link : méthode IndexStore (trait production) → accessible via state.search.
    for i in 0..5 {
        let linker = Ulid::new().to_string();
        idx.seed_note_with_created(&linker, "reference", &format!("linker {i}"), seven_days_ago)
            .await
            .expect("seed linker");
        state
            .search
            .upsert_link("main", &linker, &id_d)
            .await
            .expect("upsert_link");
    }

    let req = search_req("popularitytest architecture design", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");

    let pos_d = position_of(items, &id_d);
    let pos_c = position_of(items, &id_c);
    assert!(
        pos_d != usize::MAX && pos_c != usize::MAX,
        "les 2 notes doivent être dans la réponse. body={json}"
    );
    assert!(
        pos_d < pos_c,
        "note liée D (pos={pos_d}, 5 backlinks) doit devancer note isolée C (pos={pos_c}, 0 backlinks). body={json}"
    );
}

// T13-3 : scores composite retournés sont cohérents (score > 0.0, décroissants)
#[tokio::test]
async fn multifactor_scores_are_positive_and_decreasing() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let now_ms = chrono::Utc::now().timestamp_millis();

    // 2 notes avec body distinct, dates différentes.
    // seed_note_with_created : méthode concrète SqliteIndex (hors trait).
    let id1 = Ulid::new().to_string();
    let id2 = Ulid::new().to_string();
    idx.seed_note_with_created(&id1, "reference", "keyword unique fresh", now_ms)
        .await
        .expect("seed 1");
    idx.seed_note_with_created(
        &id2,
        "reference",
        "keyword unique old aged",
        now_ms - 30 * 24 * 3_600_000,
    )
    .await
    .expect("seed 2");

    let req = search_req("keyword unique", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty(), "doit retourner ≥1 item. body={json}");

    let mut prev_score = f64::MAX;
    for (i, item) in items.iter().enumerate() {
        let s = item["score"].as_f64().expect("score number");
        assert!(s > 0.0, "score[{i}] = {s} doit être > 0.0");
        assert!(
            s <= prev_score + 1e-9,
            "scores doivent être décroissants : item[{i}].score = {s} > prev = {prev_score}"
        );
        prev_score = s;
    }
}
