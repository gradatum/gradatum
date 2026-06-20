//! Tests E2E vault_search — exposition du champ `title` dans la réponse.
//!
//!
//! Couvre :
//! 1. `vault_search_response_includes_title_when_db_has_title` — title présent dans JSON.
//! 2. `vault_search_response_title_null_when_no_h1` — title=null si pas de H1.
//! 3. `vault_search_response_preserves_snippet_alongside_title` — snippet + title coexistent.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::identity::NoteId;
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

/// Embedder Noop pour ces tests (pas besoin de semantic ici, on teste juste BM25 + DTO).
struct NoopEmbedderTest;

#[async_trait]
impl Embedder for NoopEmbedderTest {
    fn embedder_id(&self) -> &str {
        "noop-title"
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
/// L'`Arc<SqliteIndex>` concret est retourné pour `seed_note_with_fts` (méthode pub concrète, hors trait).
/// `state.search` et le router partagent le même `Arc<SqliteIndex>` via coercion dyn.
/// `upsert_note_title` étant dans `DocumentStore` (trait production), elle reste accessible
/// via `state.search.upsert_note_title(...)`.
async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL)
        .expect("preset ACL search-tester valide — invariant statique");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_search_title"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopEmbedderTest));
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

fn search_req(query: &str, token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 5,
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

/// Test 1 : note avec title H1 → réponse JSON expose `title`.
#[tokio::test]
async fn vault_search_response_includes_title_when_db_has_title() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let note_id = Ulid::new().to_string();
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(
        &note_id,
        "reference",
        "# Titre alpha11 patch1\n\nbody contenu alpha11 patch1 search test",
    )
    .await
    .expect("seed_note_with_fts");

    // Persister title via upsert_note_title (méthode DocumentStore — accessible via state.search).
    let nid = NoteId(Ulid::from_string(&note_id).unwrap());
    state
        .search
        .upsert_note_title(&nid, "Titre alpha11 patch1")
        .await
        .expect("upsert_note_title");

    let req = search_req("alpha11", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    assert!(
        !items.is_empty(),
        "items doit contenir la note. body={json}"
    );

    let item = &items[0];
    assert!(
        item.as_object().unwrap().contains_key("title"),
        "champ 'title' doit exister dans la réponse. item={item}"
    );
    assert_eq!(
        item["title"].as_str(),
        Some("Titre alpha11 patch1"),
        "title doit refléter la valeur DB. item={item}"
    );
}

/// Test 2 : note sans title (pas de H1 extrait) → champ `title` présent mais null.
#[tokio::test]
async fn vault_search_response_title_null_when_no_h1() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let note_id = Ulid::new().to_string();
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(&note_id, "reference", "body sans h1 alpha11 patch1 zzz")
        .await
        .expect("seed_note_with_fts");
    // Volontairement pas d'upsert_note_title : title = NULL en DB.

    let req = search_req("zzz", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    assert!(!items.is_empty());

    let item = &items[0];
    assert!(
        item.as_object().unwrap().contains_key("title"),
        "champ 'title' doit exister (même null) — Option<String> sérialise en null. item={item}"
    );
    assert!(
        item["title"].is_null(),
        "title doit être null si pas de H1. item={item}"
    );
}

/// Test 3 : title et snippet coexistent dans la réponse.
#[tokio::test]
async fn vault_search_response_preserves_snippet_alongside_title() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let note_id = Ulid::new().to_string();
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(
        &note_id,
        "reference",
        "# Coexist title and snippet\n\ncontent body alpha11 coexist patch1",
    )
    .await
    .expect("seed_note_with_fts");

    let nid = NoteId(Ulid::from_string(&note_id).unwrap());
    state
        .search
        .upsert_note_title(&nid, "Coexist title and snippet")
        .await
        .expect("upsert_note_title");

    let req = search_req("coexist", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    assert!(!items.is_empty());

    let item = &items[0];
    assert_eq!(
        item["title"].as_str(),
        Some("Coexist title and snippet"),
        "title doit être présent. item={item}"
    );
    assert!(
        item["snippet"].is_string(),
        "snippet doit aussi être présent. item={item}"
    );
}

/// Test 4 (P2-R1) : note avec `title = ""` persisté en DB → vault_search rend `title=null`.
///
/// Ferme l'invariant `vault_read.title == vault_search.title` by-construction :
/// vault_read filtre `.filter(!trim().is_empty())` ; vault_search doit faire de même.
/// Un `title = ""` en colonne (possible sur notes legacy ou bug backfill) ne doit
/// jamais remonter comme `Some("")` dans aucun des deux endpoints.
///
/// Setup : note avec body qui MATCHE la query (BM25) + `upsert_note_title("", ...)` → `title=""`.
/// Attendu : `items[0].title` est `null` (JSON null), pas `""`.
#[tokio::test]
async fn vault_search_empty_title_in_db_becomes_null() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let note_id = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &note_id,
        "reference",
        // Body sans H1 + mot-clé unique pour éviter collisions avec autres tests.
        "contenu emptytitle xzqfoo invariant p2r1 search",
    )
    .await
    .expect("seed_note_with_fts");

    // Persister title = "" en DB (simule une note legacy ou bug backfill).
    let nid = NoteId(Ulid::from_string(&note_id).unwrap());
    state
        .search
        .upsert_note_title(&nid, "")
        .await
        .expect("upsert_note_title avec chaine vide");

    let req = search_req("emptytitle xzqfoo", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    assert!(
        !items.is_empty(),
        "la note doit apparaître dans les résultats. body={json}"
    );

    let item = &items[0];
    // Invariant P2-R1 : title="" en DB doit remonter comme null, pas comme "".
    assert!(
        item["title"].is_null(),
        "title vide en DB doit être null dans vault_search (invariant P2-R1). item={item}"
    );
}

/// Test 5 (P2-R1 non-régression) : titre whitespace-only (`"   "`) → null.
///
/// Complète le test 4 pour les titres constitués uniquement d'espaces.
/// Le filtre `.filter(|s| !s.trim().is_empty())` couvre les deux cas.
#[tokio::test]
async fn vault_search_whitespace_title_in_db_becomes_null() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let note_id = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &note_id,
        "reference",
        "contenu whitespacetitle xzqbar invariant p2r1 search",
    )
    .await
    .expect("seed_note_with_fts");

    let nid = NoteId(Ulid::from_string(&note_id).unwrap());
    state
        .search
        .upsert_note_title(&nid, "   ")
        .await
        .expect("upsert_note_title whitespace");

    let req = search_req("whitespacetitle xzqbar", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    assert!(
        !items.is_empty(),
        "la note doit apparaître dans les résultats. body={json}"
    );

    let item = &items[0];
    assert!(
        item["title"].is_null(),
        "title whitespace-only en DB doit être null dans vault_search (invariant P2-R1). item={item}"
    );
}
