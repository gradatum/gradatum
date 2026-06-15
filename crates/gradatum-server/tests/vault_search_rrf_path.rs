//! Tests E2E vault_search — Path RRF complet BM25 + semantic.
//!
//!
//! Couvre :
//! 1. `rrf_path_two_notes_bm25_only_returns_both` — BM25 seul (Noop) sur 2 notes.
//! 2. `rrf_path_semantic_only_note_returned` — note absente BM25 ramenée via semantic.
//! 3. `rrf_path_preserves_title_and_snippet_after_fusion` — enrichissement post-fusion.
//! 4. `rrf_path_score_within_expected_range_k60` — score RRF k=60 plausible.
//! 5. `rrf_path_semantic_only_hit_enriched_title_section` — régression v0.3.5 :
//!    hit sémantique-only obtient `title` non-null et `section` non-vide
//!    (passe d'enrichissement batch `get_titles_sections` après fusion RRF).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::identity::NoteId;
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
// VectorStore (Étape 0.2a) : insert_note_embedding accessible via dyn Index sans import.
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
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

/// Embedder qui retourne un vecteur fixe : utilisé pour activer le path semantic
/// avec un embedding déterministe alignable avec ceux insérés en DB.
struct FixedEmbedder {
    vec: Vec<f32>,
}

#[async_trait]
impl Embedder for FixedEmbedder {
    fn embedder_id(&self) -> &str {
        "fixed-test"
    }

    fn dim(&self) -> u16 {
        self.vec.len() as u16
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.vec.clone())
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.vec.clone()).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http // non-Noop → active path semantic
    }
}

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-rrf"
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
async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{middleware, Router};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_search_rrf_path"),
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

/// Test 1 : 2 notes BM25 + Noop embedder → les 2 notes retournées avec score RRF.
#[tokio::test]
async fn rrf_path_two_notes_bm25_only_returns_both() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let id_a = Ulid::new().to_string();
    let id_b = Ulid::new().to_string();

    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(&id_a, "reference", "alpha gradatum search rrf test note A")
        .await
        .expect("seed A");
    idx.seed_note_with_fts(&id_b, "reference", "alpha gradatum search rrf test note B")
        .await
        .expect("seed B");

    let req = search_req("alpha", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");

    assert!(items.len() >= 2, "doit retourner les 2 notes. body={json}");
    // Tous les paths contiennent un id ULID parmi {id_a, id_b}.
    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.iter().any(|p| p.contains(&id_a)), "id_a absent");
    assert!(paths.iter().any(|p| p.contains(&id_b)), "id_b absent");
}

/// Test 2 : RRF path semantic-only — note absente de BM25 mais avec embedding aligné
/// est quand même retournée via la branche semantic.
///
/// Setup :
/// - id_a : body match BM25 (token "rrfsemonly") + embedding [0,1,...] (orthogonal).
/// - id_b : body NE match PAS BM25 (token absent) + embedding [1,0,...] (cosine=1).
/// Query embedding [1,0,...] → semantic retourne id_b en premier.
///
/// Vérification : le path RRF complet expose id_b dans la réponse, prouvant que
/// la branche semantic est bien câblée et que `rrf_fuse` agrège les ids hors BM25.
#[tokio::test]
async fn rrf_path_semantic_only_note_returned() {
    let mut q = vec![0.0f32; 8];
    q[0] = 1.0;
    let fixed = Arc::new(FixedEmbedder { vec: q.clone() });
    let (app, state, idx) = build_app(fixed).await;
    let token = sign(&state);

    let id_a = Ulid::new().to_string();
    let id_b = Ulid::new().to_string();

    // A : match BM25 sur "rrfsemonly" — embedding orthogonal.
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(&id_a, "reference", "rrfsemonly content note A")
        .await
        .expect("seed A");
    let nid_a = NoteId(Ulid::from_string(&id_a).unwrap());
    let mut emb_a = vec![0.0f32; 8];
    emb_a[1] = 1.0;
    state
        .search
        .insert_note_embedding(&nid_a, "fixed-test", 8, &emb_a)
        .await
        .expect("insert emb A");

    // B : pas de "rrfsemonly" dans le body — BM25 ne le trouve PAS.
    // Embedding aligné → semantic le ramène avec cosine=1.
    idx.seed_note_with_fts(&id_b, "reference", "completely different body content xyz")
        .await
        .expect("seed B");
    let nid_b = NoteId(Ulid::from_string(&id_b).unwrap());
    state
        .search
        .insert_note_embedding(&nid_b, "fixed-test", 8, &q)
        .await
        .expect("insert emb B");

    let req = search_req("rrfsemonly", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(items.len() >= 2, "doit retourner les 2 notes. body={json}");

    // id_b doit être présent même s'il ne match PAS BM25 (preuve que la branche
    // semantic est câblée et fusionne correctement).
    assert!(
        items
            .iter()
            .any(|i| i["path"].as_str().unwrap().contains(&id_b)),
        "id_b doit apparaître via path semantic (BM25 ne le trouve pas). body={json}"
    );

    // Score RRF doit être borné raisonnablement : 2/(k=60) max ≈ 0.0333 sup.
    for item in items {
        let score = item["score"].as_f64().unwrap();
        assert!(
            (0.0..0.1).contains(&score),
            "score RRF k=60 doit être dans [0, 0.1[. score={score}, item={item}"
        );
    }
}

/// Test 3 : path RRF préserve title + snippet sur la note enrichie post-fusion.
#[tokio::test]
async fn rrf_path_preserves_title_and_snippet_after_fusion() {
    let mut q = vec![0.0f32; 8];
    q[0] = 1.0;
    let fixed = Arc::new(FixedEmbedder { vec: q.clone() });
    let (app, state, idx) = build_app(fixed).await;
    let token = sign(&state);

    let id = Ulid::new().to_string();
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(
        &id,
        "reference",
        "# RRF Title preservation\n\nbody preservetitle alpha11 rrf",
    )
    .await
    .expect("seed");

    let nid = NoteId(Ulid::from_string(&id).unwrap());
    // upsert_note_title : méthode DocumentStore (trait production) → accessible via state.search.
    state
        .search
        .upsert_note_title(&nid, "RRF Title preservation")
        .await
        .expect("upsert title");
    state
        .search
        .insert_note_embedding(&nid, "fixed-test", 8, &q)
        .await
        .expect("insert emb");

    let req = search_req("preservetitle", &token, 5);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty());

    let item = &items[0];
    assert_eq!(
        item["title"].as_str(),
        Some("RRF Title preservation"),
        "title doit être préservé après fusion RRF. item={item}"
    );
    assert!(
        item["snippet"].is_string() && !item["snippet"].as_str().unwrap().is_empty(),
        "snippet doit être préservé après fusion RRF. item={item}"
    );
}

/// Test 5 — Régression v0.3.5 : hit sémantique-only enrichi avec title + section.
///
/// Setup :
/// - id_a : body match BM25 (token "enrich425") + embedding orthogonal → BM25 hit, enrichi via bm25_map.
/// - id_b : body NE match PAS BM25 + embedding aligné query → semantic-only hit.
///   La note id_b a un `title` et une `section` en base. AVANT le fix, `title` était `null`
///   et `section` était `""` dans la réponse. APRÈS le fix, `title` et `section` sont remplis.
///
/// Ce test ÉCHOUE sur le code v0.3.4 (sans la passe `get_titles_sections`) et PASSE sur v0.3.5.
#[tokio::test]
async fn rrf_path_semantic_only_hit_enriched_title_section() {
    let mut q = vec![0.0f32; 8];
    q[0] = 1.0;
    let fixed = Arc::new(FixedEmbedder { vec: q.clone() });
    let (app, state, idx) = build_app(fixed).await;
    let token = sign(&state);

    let id_a = Ulid::new().to_string();
    let id_b = Ulid::new().to_string();

    // id_a : match BM25, embedding orthogonal (ne sera pas le top semantic).
    idx.seed_note_with_fts(&id_a, "reference", "enrich425 bm25-only note alpha beta")
        .await
        .expect("seed A");
    let nid_a = NoteId(Ulid::from_string(&id_a).unwrap());
    let mut emb_a = vec![0.0f32; 8];
    emb_a[1] = 1.0; // orthogonal à q=[1,0,...] → faible cosine
    state
        .search
        .insert_note_embedding(&nid_a, "fixed-test", 8, &emb_a)
        .await
        .expect("insert emb A");

    // id_b : NE match PAS BM25 (token absent), embedding aligné → semantic-only.
    // On lui donne un title ET une section explicites pour vérifier l'enrichissement.
    idx.seed_note_with_fts(&id_b, "decisions", "completely-different-body-no-token-xyz")
        .await
        .expect("seed B");
    let nid_b = NoteId(Ulid::from_string(&id_b).unwrap());
    state
        .search
        .upsert_note_title(&nid_b, "Titre enrichissement sémantique")
        .await
        .expect("upsert title B");
    state
        .search
        .insert_note_embedding(&nid_b, "fixed-test", 8, &q) // aligné → cosine=1
        .await
        .expect("insert emb B");

    let req = search_req("enrich425", &token, 10);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");

    // id_b doit être présent (semantic-only).
    let item_b = items
        .iter()
        .find(|i| i["path"].as_str().unwrap_or("").contains(&id_b))
        .unwrap_or_else(|| panic!("id_b (semantic-only) absent de la réponse. items={items:?}"));

    // Régression v0.3.5 : title non-null + section non-vide pour le hit sémantique-only.
    assert_eq!(
        item_b["title"].as_str(),
        Some("Titre enrichissement sémantique"),
        "hit sémantique-only doit avoir title enrichi depuis notes (fix v0.3.5). item={item_b}"
    );
    let section_b = item_b["path"]
        .as_str()
        .expect("path")
        .split('/')
        .next()
        .unwrap_or("");
    assert_eq!(
        section_b, "decisions",
        "hit sémantique-only doit avoir section 'decisions' (pas 'main' fallback vide). item={item_b}"
    );

    // Snippet sémantique-only : `null` attendu (pas de match FTS5 pour générer un extrait).
    assert!(
        item_b["snippet"].is_null(),
        "snippet sémantique-only doit être null (pas de match FTS). item={item_b}"
    );
}

/// Test 4 : RRF score k=60 — score retourné est cohérent avec la formule.
///
/// Avec 1 note matchant BM25 (rang 0) et semantic (rang 0) → score RRF brut = 2/(60+0) ≈ 0.0333.
/// Le score retourné est `composite = rrf × (1+0.2×R) × (1+0.1×P)`.
/// Sur une note fraîchement créée (R≈1.0) sans backlinks (P=0), composite = rrf × 1.2 ≈ 0.040.
#[tokio::test]
async fn rrf_path_score_within_expected_range_k60() {
    let mut q = vec![0.0f32; 8];
    q[0] = 1.0;
    let fixed = Arc::new(FixedEmbedder { vec: q.clone() });
    let (app, state, idx) = build_app(fixed).await;
    let token = sign(&state);

    let id = Ulid::new().to_string();
    // seed_note_with_fts : méthode concrète SqliteIndex (hors trait).
    idx.seed_note_with_fts(
        &id,
        "reference",
        "scoretest alpha11 rrf k60 unique-token-xyz",
    )
    .await
    .expect("seed");
    let nid = NoteId(Ulid::from_string(&id).unwrap());
    state
        .search
        .insert_note_embedding(&nid, "fixed-test", 8, &q)
        .await
        .expect("insert emb");

    let req = search_req("unique-token-xyz", &token, 5);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items");
    assert!(!items.is_empty(), "doit contenir 1 note. body={json}");

    let score = items[0]["score"].as_f64().expect("score number");
    // k=60 : 2/(60+0) = 0.03333 (RRF brut) → boost composite max 1.32 → < 0.045.
    // Sur une note nouvellement seedée (R~1.0, P=0) le score se trouve typiquement
    // autour de 0.040 (= 0.0333 × 1.2). On élargit la borne pour absorber f32+seed timing.
    assert!(
        (0.030..0.05).contains(&score),
        "score doit être dans [0.030, 0.05] (RRF k=60 × composite ≤ 1.32). score={score}"
    );
}
