//! Tests E2E — exclusion par défaut de la section `snapshot` dans `vault_search` (F-246).
//!
//! # Cas de la carte (les deux)
//!
//! 1. `search_without_section_filter_excludes_snapshot` — sans filtre de section, aucune
//!    note de section `snapshot` ne remonte (ni via BM25, ni via le chemin sémantique),
//!    et `corpus_match_count` ne les compte pas (parité décompte F-162).
//! 2. `search_filtered_on_snapshot_section_returns_all` — avec `section=snapshot`
//!    (filtre explicite), TOUTES les captures remontent : exclusion par défaut ≠
//!    inaccessibilité.
//!
//! # Embedder de test
//!
//! Le `ConstEmbedder` (vecteur constant) force chaque note embeddée à matcher le chemin
//! sémantique. Sans le filtre d'exclusion sémantique (point 2 du brief), une capture
//! `snapshot` remonterait dans la fusion RRF via le bras sémantique même si le bras BM25
//! est filtré — le test 1 prouve donc les DEUX bras (lexical + sémantique).

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
read_patterns  = ["**"]
write_patterns = []
"#;

/// Embedder de test : vecteur constant non-nul → cosine ≈ 1 pour toute note ayant ce
/// même embedding. Force la remontée de TOUTES les notes embeddées dans le chemin
/// sémantique (le pire cas pour un défaut d'exclusion sémantique).
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

/// Seed une note dans une section donnée AVEC son embedding (le chemin sémantique la
/// voit). `body` porte le mot de requête commun à toutes les notes du corpus.
async fn seed_embedded(state: &AppState, idx: &SqliteIndex, id: &str, section: &str, body: &str) {
    idx.seed_note_with_fts_vault(id, "main", section, None, body)
        .await
        .expect("seed note");
    let note_id = NoteId(Ulid::from_string(id).expect("ulid"));
    let mut emb = vec![0.0f32; 8];
    emb[0] = 1.0;
    state
        .search
        .insert_note_embedding("main", &note_id, "test-embedder", 8, &emb)
        .await
        .expect("insert embedding");
}

/// POST `/api/v1/vault_search` avec le body donné → `(status, json)`.
async fn post_search(
    app: &axum::Router,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

/// Le `path` d'un item est `{section}/{ulid}`.
fn item_paths(json: &serde_json::Value) -> Vec<String> {
    json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect()
}

/// Cas 1 de la carte — sans filtre de section, aucune note `snapshot` ne remonte
/// (ni BM25 ni sémantique), et `corpus_match_count` ne les compte pas non plus.
#[tokio::test]
async fn search_without_section_filter_excludes_snapshot() {
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

    let id_ref_1 = Ulid::generate().to_string();
    let id_ref_2 = Ulid::generate().to_string();
    let id_snap_1 = Ulid::generate().to_string();
    let id_snap_2 = Ulid::generate().to_string();

    // Deux notes visibles par défaut + deux captures brutes (section `snapshot`).
    seed_embedded(
        &state,
        &idx,
        &id_ref_1,
        "reference",
        "f246exclusion corpus alpha",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_ref_2,
        "decisions",
        "f246exclusion corpus beta",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_snap_1,
        "snapshot",
        "f246exclusion raw capture",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_snap_2,
        "snapshot",
        "f246exclusion raw capture bis",
    )
    .await;

    let body = serde_json::json!({
        "query": "f246exclusion",
        "limit": 10,
        "tenant_id": "main",
        "include_corpus_count": true,
    });
    let (status, json) = post_search(&app, &token, body).await;
    assert_eq!(status, StatusCode::OK, "vault_search 200 attendu: {json}");

    let paths = item_paths(&json);

    // Les deux notes hors `snapshot` remontent.
    assert!(
        paths.iter().any(|p| p.contains(&id_ref_1)),
        "note reference doit figurer sans filtre de section. paths={paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.contains(&id_ref_2)),
        "note decisions doit figurer sans filtre de section. paths={paths:?}"
    );

    // Aucune capture `snapshot` ne remonte (ni BM25, ni sémantique).
    assert!(
        !paths.iter().any(|p| p.contains(&id_snap_1)),
        "capture snapshot 1 NE doit PAS figurer sans filtre de section (F-246). paths={paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains(&id_snap_2)),
        "capture snapshot 2 NE doit PAS figurer sans filtre de section (F-246). paths={paths:?}"
    );

    // `corpus_match_count` suit la MÊME exclusion : 2 notes visibles comptées, pas 4.
    assert_eq!(
        json["corpus_match_count"], 2,
        "le décompte ne doit PAS annoncer de correspondances invisibles (F-162). json={json}"
    );
}

/// Cas 2 de la carte — `section=snapshot` (filtre explicite) rend TOUTES les captures.
#[tokio::test]
async fn search_filtered_on_snapshot_section_returns_all() {
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

    let id_ref_1 = Ulid::generate().to_string();
    let id_snap_1 = Ulid::generate().to_string();
    let id_snap_2 = Ulid::generate().to_string();

    seed_embedded(
        &state,
        &idx,
        &id_ref_1,
        "reference",
        "f246exclusion corpus alpha",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_snap_1,
        "snapshot",
        "f246exclusion raw capture",
    )
    .await;
    seed_embedded(
        &state,
        &idx,
        &id_snap_2,
        "snapshot",
        "f246exclusion raw capture bis",
    )
    .await;

    let body = serde_json::json!({
        "query": "f246exclusion",
        "limit": 10,
        "tenant_id": "main",
        "section": "snapshot",
        "include_corpus_count": true,
    });
    let (status, json) = post_search(&app, &token, body).await;
    assert_eq!(status, StatusCode::OK, "vault_search 200 attendu: {json}");

    let paths = item_paths(&json);

    // Toutes les captures remontent — l'exclusion par défaut n'est pas une
    // inaccessibilité.
    assert!(
        paths.iter().any(|p| p.contains(&id_snap_1)),
        "capture snapshot 1 doit figurer avec section=snapshot. paths={paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.contains(&id_snap_2)),
        "capture snapshot 2 doit figurer avec section=snapshot. paths={paths:?}"
    );

    // La note d'une autre section n'est pas dans le filtre explicite.
    assert!(
        !paths.iter().any(|p| p.contains(&id_ref_1)),
        "note reference NE doit PAS figurer avec section=snapshot. paths={paths:?}"
    );

    // Décompte cohérent : les captures demandées explicitement sont comptées.
    assert_eq!(
        json["corpus_match_count"], 2,
        "le décompte doit compter les captures demandées explicitement. json={json}"
    );
}
