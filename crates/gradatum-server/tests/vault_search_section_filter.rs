//! Test E2E régression (C2 v0.4.4) — `section` doit filtrer AUSSI les hits sémantiques.
//!
//! ## Bug d'origine (confirmé empiriquement 2026-06-11)
//!
//! `vault_search` filtrait `section` sur le chemin BM25 (`search_fts_with_snippet`)
//! mais PAS sur le chemin sémantique (`search_semantic` ne reçoit que `locus`).
//! Avec un embedder actif (cas LIVE bge-m3), des notes d'AUTRES sections
//! remontaient en hits sémantique-only dans la fusion RRF malgré
//! `section=lessons-learned`.
//!
//! Ce test reproduit le leak avec un FakeEmbedder qui donne le même vecteur à
//! toutes les notes (cosine ≈ 1 partout) : sans le fix, une note `debug`
//! remonterait sur une requête `section=reference`.

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
read_patterns  = ["main/*", "main/main"]
write_patterns = []
"#;

/// Embedder de test : vecteur constant non-nul → cosine ≈ 1 pour toute note
/// ayant ce même embedding. Force la remontée de TOUTES les notes embeddées
/// dans le chemin sémantique (le pire cas pour le leak de section).
struct ConstEmbedder;

#[async_trait]
impl Embedder for ConstEmbedder {
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
        EmbedBackend::Http
    }
}

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(SqliteIndex::open_in_memory().await.expect("open_in_memory"));

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(ConstEmbedder));
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

async fn seed_embedded(state: &AppState, idx: &SqliteIndex, id: &str, section: &str, body: &str) {
    idx.seed_note_with_fts_vault(id, "main", section, None, body)
        .await
        .expect("seed note");
    let note_id = NoteId(Ulid::from_string(id).expect("ulid"));
    let mut emb = vec![0.0f32; 8];
    emb[0] = 1.0;
    state
        .search
        .insert_note_embedding(&note_id, "test-embedder", 8, &emb)
        .await
        .expect("insert embedding");
}

/// Régression : avec un embedder actif, `section=reference` ne doit JAMAIS
/// retourner une note de la section `debug`, même si le cosine la favorise.
#[tokio::test]
async fn section_filter_excludes_other_sections_in_semantic_path() {
    let (app, state, idx) = build_app().await;
    let token = state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");

    let id_ref = Ulid::new().to_string();
    let id_debug = Ulid::new().to_string();

    // Deux notes, sections différentes, MÊME corpus de mots → match BM25 + cosine.
    seed_embedded(
        &state,
        &idx,
        &id_ref,
        "reference",
        "alpha gradatum sectionleak corpus",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_debug,
        "debug",
        "alpha gradatum sectionleak corpus",
    )
    .await;

    let body = serde_json::json!({
        "query": "sectionleak",
        "limit": 10,
        "tenant_id": "main",
        "section": "reference"
    });
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
    let paths: Vec<String> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect();

    assert!(
        paths.iter().any(|p| p.contains(&id_ref)),
        "note reference doit figurer. paths={paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains(&id_debug)),
        "note debug NE doit PAS figurer avec section=reference (leak sémantique). paths={paths:?}"
    );
}
