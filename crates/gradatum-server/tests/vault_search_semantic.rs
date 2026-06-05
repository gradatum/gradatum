//! Tests E2E vault_search — fusion RRF BM25 + semantic (Phase 2.x.2 alpha.11 Task 10).
//!
//! Couvre :
//! 1. `vault_search_noop_embedder_returns_bm25_only` — avec Noop → BM25 seul, pas de panique.
//! 2. `vault_search_fake_embedder_calls_semantic` — avec fake embedder → fusion RRF, items non vide.
//! 3. `vault_search_semantic_embed_error_fallback_to_bm25` — erreur embed → dégradation gracieuse BM25.
//!
//! # Architecture des tests
//!
//! - `FakeEmbedder` : embedder de test `EmbedBackend::Http` qui retourne un vecteur connu.
//! - `ErrorEmbedder` : embedder de test qui retourne toujours une erreur `embed()`.
//! - `AppState` construit manuellement avec injection `with_embedder` + seed SqliteIndex.
//! - ACL autorisant `search-tester` en lecture.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
// VectorStore (Étape 0.2a) : insert_note_embedding accessible via dyn Index sans import.
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL autorisant `search-tester` en lecture sur tous les loci.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "search-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

// ── Fake embedders ────────────────────────────────────────────────────────────

/// Embedder de test : retourne un vecteur non-nul ([1.0, 0.0, ...] dim=8).
///
/// `backend_kind()` = `Http` → non-Noop → active le path sémantique dans vault_search.
/// `embedder_id()` = `"test-embedder"` → cohérent avec les embeddings insérés en DB.
struct FakeEmbedder;

#[async_trait]
impl Embedder for FakeEmbedder {
    fn embedder_id(&self) -> &str {
        "test-embedder"
    }

    fn dim(&self) -> u16 {
        8
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = vec![0.0f32; 8];
        v[0] = 1.0;
        Ok(v)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|_| {
                let mut v = vec![0.0f32; 8];
                v[0] = 1.0;
                v
            })
            .collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        // Http = non-Noop → active le path sémantique.
        EmbedBackend::Http
    }
}

/// Embedder de test : retourne une erreur sur `embed()`.
///
/// Permet de tester la dégradation gracieuse : si `embed()` échoue,
/// `vault_search` doit retourner les résultats BM25 seuls (pas un 500).
struct ErrorEmbedder;

#[async_trait]
impl Embedder for ErrorEmbedder {
    fn embedder_id(&self) -> &str {
        "error-embedder"
    }

    fn dim(&self) -> u16 {
        8
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Err(EmbedError::Embed("embed error simulée pour test".into()))
    }

    async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Embed(
            "embed_batch error simulée pour test".into(),
        ))
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http // non-Noop → tente embed() → échoue → dégradation gracieuse
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Construit un `(Router, AppState, Arc<SqliteIndex>)` avec JWT+ACL configurés et un embedder injecté.
///
/// L'`Arc<SqliteIndex>` concret est retourné pour permettre les appels à `seed_note` /
/// `seed_note_with_fts` (méthodes pub concrètes, hors trait `IndexStore`).
/// `state.search` et le router partagent le même `Arc<SqliteIndex>` via coercion dyn.
async fn build_app_with_embedder(
    embedder: Arc<dyn Embedder>,
) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{middleware, Router};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL)
        .expect("preset ACL search-tester valide — invariant statique");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_search_semantic"),
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

/// Construit une requête POST /api/v1/vault_search.
fn vault_search_req(query: &str, token: &str) -> Request<Body> {
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

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : avec Noop embedder → BM25 seul (aucune panique, retour 200).
///
/// Le handler doit détecter `backend_kind() == Noop` et ne PAS appeler `embed()`.
/// Résultat : items BM25 seuls (ou vide si corpus vide), HTTP 200.
#[tokio::test]
async fn vault_search_noop_embedder_returns_bm25_only() {
    use gradatum_embed::Noop as NoopEmbedder;

    let noop = Arc::new(NoopEmbedder::new(8));
    let (app, state, idx) = build_app_with_embedder(noop).await;

    // Signer un JWT pour le consumer "search-tester".
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test");

    // Seed une note pour avoir quelque chose dans BM25.
    let note_id = Ulid::new().to_string();
    idx.seed_note(&note_id, "reference", "gradatum semantic search noop test")
        .await
        .expect("seed_note — invariant test");

    let req = vault_search_req("semantic", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Noop embedder doit retourner 200"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(
        json["items"].is_array(),
        "réponse doit contenir 'items' tableau. body={json}"
    );
    // Corpus vide in-memory sans index FTS peuplé → 0 hits BM25 possible.
    // L'essentiel est l'absence de panique et le 200.
}

/// Test 2 : avec fake embedder (non-Noop) → fusion RRF active, items retournés.
///
/// Setup :
/// - 1 note seedée avec contenu BM25 + embedding `[1.0, 0.0, ...]`.
/// - FakeEmbedder retourne `[1.0, 0.0, ...]` pour la query → cosine ≈ 1.0.
/// - vault_search doit combiner BM25 + semantic → retourner la note.
#[tokio::test]
async fn vault_search_fake_embedder_calls_semantic() {
    use gradatum_core::identity::NoteId;

    let fake = Arc::new(FakeEmbedder);
    let (app, state, idx) = build_app_with_embedder(fake).await;

    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test");

    // Seed une note avec body contenant le terme de recherche.
    let note_id_str = Ulid::new().to_string();
    idx.seed_note(
        &note_id_str,
        "reference",
        "gradatum alpha11 semantic rrf fusion test note",
    )
    .await
    .expect("seed_note — invariant test");

    // Insérer un embedding pour cette note (vecteur [1.0, 0.0, ...] dim=8).
    let ulid = ulid::Ulid::from_string(&note_id_str).unwrap();
    let note_id = NoteId(ulid);
    let emb: Vec<f32> = {
        let mut v = vec![0.0f32; 8];
        v[0] = 1.0;
        v
    };
    state
        .search
        .insert_note_embedding(&note_id, "test-embedder", 8, &emb)
        .await
        .expect("insert_note_embedding — invariant test");

    // vault_search — query "semantic" doit trouver la note via BM25 + semantic.
    let req = vault_search_req("semantic", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fake embedder doit retourner 200"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(
        json["items"].is_array(),
        "réponse doit contenir 'items' tableau. body={json}"
    );

    // La note doit apparaître dans les résultats (BM25 sur "semantic" dans le body
    // + boost sémantique cosine ≈ 1.0).
    let items = json["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "items doit contenir au moins 1 résultat (note avec 'semantic' dans le body). body={json}"
    );

    // Vérifier la structure de chaque item.
    for item in items {
        assert!(
            item["path"].is_string(),
            "chaque item doit avoir un 'path'. item={item}"
        );
        assert!(
            item["score"].is_number(),
            "chaque item doit avoir un 'score'. item={item}"
        );
    }
}

/// Test 3 : erreur `embed()` → dégradation gracieuse vers BM25 seul (pas de 500).
///
/// Si `embed()` retourne une erreur, `vault_search` doit :
/// - Logger un WARN (non vérifiable en test unitaire).
/// - Retourner HTTP 200 avec les résultats BM25 seuls.
/// - Ne PAS retourner 500.
#[tokio::test]
async fn vault_search_embed_error_falls_back_to_bm25() {
    let error_emb = Arc::new(ErrorEmbedder);
    let (app, state, idx) = build_app_with_embedder(error_emb).await;

    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test");

    // Seed une note.
    let note_id = Ulid::new().to_string();
    idx.seed_note(&note_id, "reference", "gradatum alpha11 fallback bm25 only")
        .await
        .expect("seed_note — invariant test");

    let req = vault_search_req("alpha11", &token);
    let resp = app.oneshot(req).await.unwrap();

    // Dégradation gracieuse : 200 (pas 500), même si embed() a échoué.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "erreur embed() doit retourner 200 (dégradation gracieuse), pas 500"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(
        json["items"].is_array(),
        "réponse doit contenir 'items' tableau même avec erreur embed. body={json}"
    );
}
