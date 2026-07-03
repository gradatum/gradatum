//! Tests E2E vault_search — filtre temporel F-65 (`from_ms`/`to_ms` + `anchor_ms` dans les hits).
//!
//! Couvre :
//! 1. `from_ms_to_ms_fields_accepted` — champs acceptés sans erreur 400.
//! 2. `from_ms_greater_than_to_ms_is_400` — from_ms > to_ms → 400 InvalidInput.
//! 3. `temporal_filter_restricts_results` — `from_ms`/`to_ms` filtrent les notes hors fenêtre.
//! 4. `anchor_ms_present_in_hits` — chaque hit expose `anchor_ms`.
//! 5. `no_bounds_returns_all_results` — sans bornes, backward-compat : tous les hits retournés.
//! 6. `temporal_filter_semantic_path_exercised` — filtre temporel sur chemin sémantique
//!    (embedder non-Noop, `semantic_hits` non-vide, branche `logic.rs:~309` atteinte).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::{AnchorSrc, Index, TemporalEntry};
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "temporal-tester"
read_patterns  = ["main/*", "main/main", "*/decisions", "decisions/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-temporal"
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

/// Embedder déterministe NON-Noop — force l'entrée dans la branche sémantique (F-65 T6).
///
/// Retourne `EmbedBackend::Http` (≠ Noop) pour que `logic.rs:~221` entre dans la branche
/// sémantique. Les vecteurs `[1.0, 0.0, …]` ont une norme non-nulle (cosine = 1.0 contre
/// eux-mêmes) → `search_semantic_inner` retourne les notes pré-seedées.
/// L'`embedder_id` doit correspondre à celui utilisé dans `seed_note_embedding`.
struct DeterministicEmbedder;

const DET_EMBEDDER_ID: &str = "det-test-f65-v1";
const DET_DIM: u16 = 8;

#[async_trait]
impl Embedder for DeterministicEmbedder {
    fn embedder_id(&self) -> &str {
        DET_EMBEDDER_ID
    }
    fn dim(&self) -> u16 {
        DET_DIM
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Vecteur unitaire fixe — norme = 1.0, cosine similarity = 1.0 avec lui-même.
        let mut v = vec![0.0f32; DET_DIM as usize];
        v[0] = 1.0;
        Ok(v)
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let v: Vec<f32> = {
            let mut x = vec![0.0f32; DET_DIM as usize];
            x[0] = 1.0;
            x
        };
        Ok(texts.iter().map(|_| v.clone()).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        // NON Noop → active la branche sémantique dans vault_search_impl.
        EmbedBackend::Http
    }
}

async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL temporal");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test temporal"),
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
            "temporal-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT temporal")
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

/// Seeds a note with FTS + temporal_index entry via public API.
///
/// Note: all test IDs must use only Crockford base32 chars (0-9, A-Z excl. I, L, O, U).
async fn seed_with_temporal(idx: &Arc<SqliteIndex>, id: &str, anchor_ms: i64) {
    // Seed note + FTS (public API).
    idx.seed_note_with_created(id, "decisions", "temporal filter test token", anchor_ms)
        .await
        .expect("seed_note_with_created temporal");

    // Seed temporal_index entry (public API).
    let entry = TemporalEntry {
        note_id: id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry seed");
}

// Note: IDs use only Crockford base32 chars (0-9, A-Z excl. I, L, O, U).
// Valid chars: 0123456789ABCDEFGHJKMNPQRSTVWXYZ
// IDs must be exactly 26 chars.
const ID_T3_A: &str = "01HX000000000000000F65ANCK"; // anchor_ms = 1_000_000 (old)
const ID_T3_B: &str = "01HX000000000000000F65RECS"; // anchor_ms = 7_000_000 (recent)
const ID_T4: &str = "01HX000000000000000F65T4XX";
const ID_T5_A: &str = "01HX000000000000000F65T5AX";
const ID_T5_B: &str = "01HX000000000000000F65T5BX";

// F-65 T1 : champs from_ms/to_ms acceptés sans 400.
#[tokio::test]
async fn from_ms_to_ms_fields_accepted() {
    let (app, state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal filter test token",
            "tenant_id": "main",
            "from_ms": 0i64,
            "to_ms": 9_999_999_999_999i64
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    // Doit retourner 200 (les champs sont connus) — pas 400 "unknown field".
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "from_ms/to_ms ne doivent pas produire 400 unknown field"
    );
}

// F-65 T2 : from_ms > to_ms → 400 InvalidInput.
#[tokio::test]
async fn from_ms_greater_than_to_ms_is_400() {
    let (app, state, _idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal filter test token",
            "tenant_id": "main",
            "from_ms": 2_000_000i64,
            "to_ms":   1_000_000i64
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "from_ms > to_ms doit produire 400"
    );
}

// F-65 T3 : filtre temporel restreint les résultats — note hors fenêtre exclue.
#[tokio::test]
async fn temporal_filter_restricts_results() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    // Note ancienne (anchor_ms = 1_000_000) — hors fenêtre [5_000_000, 9_000_000]
    seed_with_temporal(&idx, ID_T3_A, 1_000_000).await;
    // Note récente (anchor_ms = 7_000_000) — dans la fenêtre
    seed_with_temporal(&idx, ID_T3_B, 7_000_000).await;

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal filter test token",
            "tenant_id": "main",
            "from_ms": 5_000_000i64,
            "to_ms":   9_000_000i64
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "status non-200. resp=?");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items temporal filter");

    let ids: Vec<&str> = items.iter().filter_map(|it| it["path"].as_str()).collect();

    assert!(
        !ids.iter().any(|p| p.contains(ID_T3_A)),
        "note ancienne doit être exclue (anchor_ms=1_000_000 < from_ms=5_000_000). items={json}"
    );
    assert!(
        ids.iter().any(|p| p.contains(ID_T3_B)),
        "note récente doit être incluse (anchor_ms=7_000_000 ∈ [5_000_000, 9_000_000]). items={json}"
    );
}

// F-65 T4 : anchor_ms présent dans chaque hit (valeur non-nulle pour notes avec temporal_index).
#[tokio::test]
async fn anchor_ms_present_in_hits() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    let anchor = 3_141_592i64;
    seed_with_temporal(&idx, ID_T4, anchor).await;

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal filter test token",
            "tenant_id": "main"
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "status non-200 pour anchor_ms_present_in_hits"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items anchor_ms");

    let hit = items
        .iter()
        .find(|it| it["path"].as_str().is_some_and(|p| p.contains(ID_T4)))
        .expect("hit doit être présent");

    assert_eq!(
        hit["anchor_ms"].as_i64(),
        Some(anchor),
        "anchor_ms doit valoir {anchor}. hit={hit}"
    );
}

// ── Constantes ULID pour T6 — Crockford base32 strict (pas I/L/O/U), 26 chars. ──
// Chemin sémantique : note dans la fenêtre temporelle.
const ID_SEM_IN: &str = "01HX000000000000000SEM0001";
// Chemin sémantique : note hors fenêtre (anchor_ms < from_ms).
const ID_SEM_OUT: &str = "01HX000000000000000SEM0002";
// Chemin sémantique : note avec embedding SANS entrée temporal_index → exclue (None => false).
const ID_SEM_NOTEMPORAL: &str = "01HX000000000000000SEM0003";

/// Seeds une note FTS + entrée temporal_index + embedding.
///
/// Utilisé par T6 pour garantir que la note apparaît dans `semantic_hits`
/// (`search_semantic_inner` la trouvera grâce au vecteur pré-seedé).
async fn seed_with_temporal_and_embedding(idx: &Arc<SqliteIndex>, id: &str, anchor_ms: i64) {
    idx.seed_note_with_created(
        id,
        "decisions",
        "temporal semantic deterministic test",
        anchor_ms,
    )
    .await
    .expect("seed_note_with_created (temporal+embedding)");

    let entry = TemporalEntry {
        note_id: id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry (T6)");

    // Vecteur identique à ce que DeterministicEmbedder retourne → cosine = 1.0.
    let mut vector = vec![0.0f32; DET_DIM as usize];
    vector[0] = 1.0;
    idx.seed_note_embedding(id, DET_EMBEDDER_ID, DET_DIM, &vector)
        .await
        .expect("seed_note_embedding (T6)");
}

/// Seeds une note FTS + embedding SANS entrée temporal_index.
///
/// Simule le cas d'une note sémantiquement similaire mais sans ancrage temporel :
/// doit être exclue quand des bornes temporelles sont actives (None => false).
async fn seed_with_embedding_no_temporal(idx: &Arc<SqliteIndex>, id: &str, created_ms: i64) {
    idx.seed_note_with_created(
        id,
        "decisions",
        "temporal semantic deterministic test",
        created_ms,
    )
    .await
    .expect("seed_note_with_created (embedding only)");

    // Embedding seedé sans temporal_index entry.
    let mut vector = vec![0.0f32; DET_DIM as usize];
    vector[0] = 1.0;
    idx.seed_note_embedding(id, DET_EMBEDDER_ID, DET_DIM, &vector)
        .await
        .expect("seed_note_embedding (no temporal, T6)");
}

// F-65 T5 : sans bornes, tous les résultats sont retournés (backward-compat).
#[tokio::test]
async fn no_bounds_returns_all_results() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);

    seed_with_temporal(&idx, ID_T5_A, 1_000_000).await;
    seed_with_temporal(&idx, ID_T5_B, 9_000_000).await;

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal filter test token",
            "tenant_id": "main"
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "status non-200 pour no_bounds_returns_all_results"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items no bounds");

    let ids: Vec<&str> = items.iter().filter_map(|it| it["path"].as_str()).collect();

    assert!(
        ids.iter().any(|p| p.contains(ID_T5_A)),
        "note A doit être incluse sans bornes. items={json}"
    );
    assert!(
        ids.iter().any(|p| p.contains(ID_T5_B)),
        "note B doit être incluse sans bornes. items={json}"
    );
}

// F-65 T6 : filtre temporel sur chemin sémantique — branche `logic.rs:~309` exercée.
//
// Garantit que le filtre temporal `retain()` s'applique sur les `semantic_hits` NON-vides
// (embedder = DeterministicEmbedder, backend_kind=Http ≠ Noop → branche sémantique active).
//
// Propriétés vérifiées :
// (a) note hors fenêtre → exclue du chemin sémantique ;
// (b) note sans entrée temporal_index → exclue (None => false dans retain) ;
// (c) note dans la fenêtre → incluse.
// Parité F-65 §5 : aucun résultat hors-plage FTS∪ANN dans la réponse finale.
#[tokio::test]
async fn temporal_filter_semantic_path_exercised() {
    // Embedder non-Noop → la branche sémantique est activée dans vault_search_impl.
    let (app, state, idx) = build_app(Arc::new(DeterministicEmbedder)).await;
    let token = sign(&state);

    // Note dans la fenêtre [4_000_000, 8_000_000] avec embedding.
    seed_with_temporal_and_embedding(&idx, ID_SEM_IN, 5_000_000).await;
    // Note hors fenêtre (anchor_ms = 2_000_000 < from_ms = 4_000_000) avec embedding.
    seed_with_temporal_and_embedding(&idx, ID_SEM_OUT, 2_000_000).await;
    // Note avec embedding mais SANS entrée temporal_index → doit être exclue par None=>false.
    seed_with_embedding_no_temporal(&idx, ID_SEM_NOTEMPORAL, 6_000_000).await;

    let req = search_req(
        &token,
        serde_json::json!({
            "query": "temporal semantic deterministic test",
            "tenant_id": "main",
            "from_ms": 4_000_000i64,
            "to_ms":   8_000_000i64
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "T6 : status non-200 — chemin sémantique F-65"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"]
        .as_array()
        .expect("T6 : items absent de la réponse");

    let ids: Vec<&str> = items.iter().filter_map(|it| it["path"].as_str()).collect();

    // (a) Note sémantique hors fenêtre doit être absente de la réponse finale.
    assert!(
        !ids.iter().any(|p| p.contains(ID_SEM_OUT)),
        "T6(a) : note hors fenêtre (anchor_ms=2_000_000 < from_ms=4_000_000) doit être exclue. \
         ids={ids:?}\njson={json}"
    );

    // (b) Note sans temporal_index est exclue (branche None => false dans retain).
    assert!(
        !ids.iter().any(|p| p.contains(ID_SEM_NOTEMPORAL)),
        "T6(b) : note sans temporal_index doit être exclue quand bornes actives. \
         ids={ids:?}\njson={json}"
    );

    // (c) Note dans la fenêtre est incluse (par chemin FTS ou sémantique, ou les deux).
    assert!(
        ids.iter().any(|p| p.contains(ID_SEM_IN)),
        "T6(c) : note dans la fenêtre (anchor_ms=5_000_000 ∈ [4,8]) doit être incluse. \
         ids={ids:?}\njson={json}"
    );
}
