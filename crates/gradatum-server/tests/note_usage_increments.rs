//! Tests d'intégration E2E — incréments d'usage per-note (F-110).
//!
//! Couvre les incréments sur les read-paths `vault_read` et `vault_search` :
//! - `vault_read` d'une note existante → 1 `read`.
//! - `vault_search` limit 5 → 5 `search-hit` + 3 `search-hit-top3` (rangs 1-3).
//! - `vault_search` 2 résultats → 2 `search-hit` + 2 `search-hit-top3` (< 3 → tous top3).
//! - `vault_read` note absente (404) → aucun incrément (best-effort, succès only).
//! - INVARIANT 1 spec : réponse `vault_search` byte-identique store câblé vs non câblé.
//!
//! L'accumulateur (`state.note_usage_accumulators`) est un `Arc` partagé entre le router
//! et `env.state` : `swap()` après une requête observe les incréments enregistrés.

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::identity::NoteId;
use gradatum_server::note_usage_store::{
    KIND_READ, KIND_SEARCH_HIT, KIND_SEARCH_HIT_TOP3, NoteUsageStore,
};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

use helpers::{
    NoopBackend, TEST_ACL, build_app, call_vault_read, call_vault_read_raw, seed_notes, sign_token,
};

/// Effectue `POST /api/v1/vault_search` et retourne la `Response` brute.
async fn vault_search_raw(
    app: axum::Router,
    token: &str,
    query: &str,
    limit: u32,
) -> axum::http::Response<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": limit,
        "tenant_id": "main",
    });
    let req = Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.expect("vault_search oneshot")
}

/// Compte les entrées de l'accumulateur pour un `kind` donné.
fn count_kind(
    batch: &std::collections::HashMap<(String, String, String), (u64, i64)>,
    kind: &str,
) -> usize {
    batch.iter().filter(|((_, _, k), _)| k == kind).count()
}

#[tokio::test]
async fn vault_read_increments_read_once() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let nid = env
        .write_note_with_h1("Note Usage Read", "contenu de test lisible")
        .await;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read doit réussir");
    assert_eq!(resp["path"].as_str(), Some(nid.to_string().as_str()));

    let batch = env.state.note_usage_accumulators.swap();
    assert_eq!(
        batch.len(),
        1,
        "exactement une clé enregistrée. batch={batch:?}"
    );
    let (count, _ms) = batch
        .get(&("main".into(), nid.to_string(), KIND_READ.into()))
        .copied()
        .expect("clé (main, id, read) présente");
    assert_eq!(count, 1, "1 incrément `read` sur la note servie");
}

#[tokio::test]
async fn vault_search_limit5_increments_5_hits_3_top3() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    // 6 notes matchent « alpha beta » ; limit 5 → 5 résultats.
    seed_notes(&env, 6).await;

    let resp = vault_search_raw(env.app.clone(), &token, "alpha beta", 5).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["items"].as_array().map(|a| a.len()),
        Some(5),
        "5 items attendus. body={json}"
    );

    let batch = env.state.note_usage_accumulators.swap();
    assert_eq!(
        count_kind(&batch, KIND_SEARCH_HIT),
        5,
        "5 search-hit (1 par note)"
    );
    assert_eq!(
        count_kind(&batch, KIND_SEARCH_HIT_TOP3),
        3,
        "3 search-hit-top3 (rangs 1-3)"
    );
    // Chaque hit est une note distincte comptée une seule fois.
    for ((_, _, k), (count, _)) in &batch {
        if k == KIND_SEARCH_HIT || k == KIND_SEARCH_HIT_TOP3 {
            assert_eq!(*count, 1, "chaque note comptée une fois par requête");
        }
    }
}

#[tokio::test]
async fn vault_search_2_results_increments_2_top3_2() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    // 2 notes seulement → 2 résultats → top3 = 2 (moins de 3 résultats).
    seed_notes(&env, 2).await;

    let resp = vault_search_raw(env.app.clone(), &token, "alpha beta", 10).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["items"].as_array().map(|a| a.len()),
        Some(2),
        "2 items attendus. body={json}"
    );

    let batch = env.state.note_usage_accumulators.swap();
    assert_eq!(count_kind(&batch, KIND_SEARCH_HIT), 2);
    assert_eq!(
        count_kind(&batch, KIND_SEARCH_HIT_TOP3),
        2,
        "moins de 3 résultats → top3 = nombre réel"
    );
}

#[tokio::test]
async fn vault_read_not_found_increments_nothing() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    // ULID valide mais aucune note associée → chemin d'erreur AVANT l'incrément `read`.
    let ghost = Ulid::new().to_string();

    let resp = call_vault_read_raw(env.app.clone(), &token, &ghost, "main").await;
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "note absente → statut non-200 (erreur, pas de succès)"
    );

    let batch = env.state.note_usage_accumulators.swap();
    assert!(
        batch.is_empty(),
        "aucun incrément sur le chemin d'erreur (best-effort, succès only). batch={batch:?}"
    );
}

/// INVARIANT 1 spec : la réponse `vault_search` est byte-identique que le store
/// `note_usage` soit câblé (`Some`) ou non (`None`).
///
/// Deux states partagent le MÊME vault + index (mêmes ULID) ; seule la présence du
/// store diffère. L'instrumentation per-note ne touche jamais le chemin de réponse :
/// les octets doivent être strictement égaux. Les notes sont créées « maintenant »
/// → `recency_factor ≈ 1.0` (identique en f32 entre les deux requêtes proches).
#[tokio::test]
async fn vault_search_response_identical_with_usage_store_wired_or_not() {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    // Seed 3 notes partagées (mêmes ULID pour les deux states).
    for i in 0..3 {
        let ulid = Ulid::new().to_string();
        let body = format!("# Note alpha beta {i}\nalpha beta contenu partagé {i}");
        index
            .seed_note_with_fts(&ulid, "reference", &body)
            .await
            .expect("seed_note_with_fts");
        let nid = NoteId(Ulid::from_string(&ulid).expect("ULID"));
        index
            .upsert_note_title("main", &nid, &format!("Note alpha beta {i}"))
            .await
            .expect("upsert_note_title");
    }

    // Construit un router (+ token) partageant le vault ; `store` optionnel.
    async fn build(
        vault_registry: Arc<dyn gradatum_vault::Registry>,
        index: Arc<gradatum_index::SqliteIndex>,
        store: Option<NoteUsageStore>,
    ) -> (axum::Router, String) {
        let jwt = JwtService::new_ephemeral();
        let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");
        let mut state = AppState::with_jwt_and_acl(jwt, acl)
            .with_embedder(Arc::new(NoopBackend))
            .with_vault_arc(vault_registry);
        state.search = index;
        state.note_usage = store;
        let token = state
            .jwt
            .sign(
                "alpha13-tester",
                &["read".to_string()],
                TokenScope::Service,
                "main",
            )
            .expect("sign JWT");
        let app = Router::new()
            .nest("/api/v1", gradatum_server::api_v1::router())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                gradatum_server::middleware::auth_middleware,
            ))
            .with_state(state.clone());
        (app, token)
    }

    // State A : store NON câblé.
    let (app_a, token_a) = build(vault_registry.clone(), index.clone(), None).await;
    // State B : store câblé (fichier temp, jamais lu sur le chemin de requête).
    let store = NoteUsageStore::open_or_create(&tmp.path().join("note_usage.db"))
        .await
        .expect("NoteUsageStore::open_or_create");
    let (app_b, token_b) = build(vault_registry.clone(), index.clone(), Some(store)).await;

    let bytes_a = vault_search_raw(app_a, &token_a, "alpha beta", 10)
        .await
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let bytes_b = vault_search_raw(app_b, &token_b, "alpha beta", 10)
        .await
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();

    assert_eq!(
        bytes_a, bytes_b,
        "réponse vault_search byte-identique avec/sans store note_usage câblé (INVARIANT 1)"
    );
}
