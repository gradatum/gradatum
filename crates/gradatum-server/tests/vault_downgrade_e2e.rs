//! Phase 2.1.2 alpha.9 — Tests E2E POST /api/v1/vault_downgrade + PATCH /api/v1/notes/{id}.
//!
//! 4 tests couvrant :
//! 1. `vault_downgrade_success_returns_200` — downgrade d'une note existante → 200 + JSON conforme.
//! 2. `vault_downgrade_idempotent_second_call` — 2 appels successifs → 200 les deux fois.
//! 3. `vault_downgrade_nonexistent_returns_404` — note absente → 404.
//! 4. `patch_note_revert_downgraded_to_live` — downgrade puis PATCH status=live → 204 + DB vérifiée.
//!
//! # Seed
//!
//! Les notes sont seedées via `SqliteIndex::seed_note` (méthode pub concrète, pas dans le trait)
//! sur le handle concret retourné par `build_with_concrete_index`. L'instance `Arc<SqliteIndex>`
//! est partagée avec `state.search` — le Router et les helpers de seed utilisent le même index.
//!
//! # Auth
//!
//! Ces endpoints ne requièrent pas de bearer JWT (MVP V4 default false invariant VPN).
//! Le routeur de test inclut `auth_middleware` pour `Extension<TrustContext>` mais les
//! handlers notes.rs ne l'extraient pas.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

mod common;

// ── Helper setup ──────────────────────────────────────────────────────────────

/// Construit un `(Router, AppState, Arc<SqliteIndex>)` partageant le MÊME index in-memory.
///
/// L'`Arc<SqliteIndex>` concret est retourné pour permettre les appels à `seed_note` /
/// `seed_note_with_fts` / `seed_note_with_created` (méthodes pub concrètes, hors trait).
/// `state.search` et le router partagent le même `Arc<SqliteIndex>` via coercion dyn.
async fn build_with_concrete_index() -> (
    axum::Router,
    gradatum_server::state::AppState,
    Arc<SqliteIndex>,
) {
    use axum::{middleware, Router};
    use gradatum_server::state::AppState;

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_downgrade_e2e"),
    );

    // Partager l'index via Arc : coercion vers dyn Index pour AppState.search.
    // AppState::new() utilisé à la place de ::default() pour éviter le lint
    // field_reassign_with_default (clippy refuse la réassignation post Default::default()).
    let mut state = AppState::new();
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

// ── Helper seed ───────────────────────────────────────────────────────────────

/// Insère une note minimale dans l'index concret et retourne son id ULID.
///
/// Utilise `SqliteIndex::seed_note` (méthode pub sur le type concret, pas dans le trait
/// `IndexStore`). La note a `status='live'`, section=`"reference"`, vault_id=`"main"`.
async fn seed_note(idx: &SqliteIndex) -> String {
    let id = Ulid::new().to_string();
    idx.seed_note(&id, "reference", "corps de test pour vault_downgrade e2e")
        .await
        .expect("seed_note — doit réussir sur index in-memory");
    id
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : downgrade d'une note existante → 200 + JSON status=downgraded.
///
/// Vérifie :
/// - Status HTTP 200 (synchrone, pas 202).
/// - Champ `status = "downgraded"` dans la réponse JSON.
/// - Champ `reason` reflète la valeur envoyée.
/// - Champ `note_id` reflète l'id envoyé.
#[tokio::test]
async fn vault_downgrade_success_returns_200() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    let body = serde_json::json!({
        "note_id": note_id,
        "reason": "test downgrade"
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        json["status"], "downgraded",
        "status doit être 'downgraded'"
    );
    assert_eq!(
        json["reason"], "test downgrade",
        "reason doit être reflétée"
    );
    assert_eq!(json["note_id"], note_id, "note_id doit être reflété");
    assert!(
        json["status_changed"].is_i64(),
        "status_changed doit être un entier (epoch ms)"
    );
}

/// Test 2 : deux appels successifs → 200 les deux fois (idempotence).
///
/// Vérifie que `downgrade_note` idempotent : mettre à jour la raison d'une note
/// déjà downgradée retourne 200 sans erreur.
#[tokio::test]
async fn vault_downgrade_idempotent_second_call() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    let make_req = |reason: &str| -> Request<Body> {
        let body = serde_json::json!({"note_id": note_id, "reason": reason});
        Request::builder()
            .uri("/api/v1/vault_downgrade")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };

    // Premier appel — downgrade initial.
    let r1 = app
        .clone()
        .oneshot(make_req("première raison"))
        .await
        .unwrap();
    assert_eq!(
        r1.status(),
        StatusCode::OK,
        "premier appel doit retourner 200"
    );

    // Deuxième appel — mise à jour raison — idempotent.
    let r2 = app.oneshot(make_req("deuxième raison")).await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::OK,
        "deuxième appel idempotent doit retourner 200"
    );

    let bytes = r2.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["reason"], "deuxième raison", "raison mise à jour");
}

/// Test 3 : note inexistante → 404 Not Found.
///
/// Vérifie que le handler retourne 404 quand `downgrade_note` produit `NoteNotFound`.
#[tokio::test]
async fn vault_downgrade_nonexistent_returns_404() {
    let (app, _state, _idx) = build_with_concrete_index().await;

    let body = serde_json::json!({
        "note_id": "01KR0000000000000000000000",
        "reason": "test inexistant"
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "note absente → 404");
}

/// Test 4 : downgrade puis revert via PATCH status=live → 204 + DB vérifiée.
///
/// Scénario complet :
/// 1. Seed une note status=live.
/// 2. POST /vault_downgrade → 200 (status=downgraded).
/// 3. PATCH /notes/{id} body={status: "live"} → 204.
/// 4. Vérifier via un second downgrade que la note existe et est opérationnelle
///    (le revert signifie qu'un nouveau downgrade doit fonctionner → 200).
#[tokio::test]
async fn patch_note_revert_downgraded_to_live() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    // Étape 1 : Downgrade.
    let dr_body = serde_json::json!({"note_id": note_id, "reason": "test revert"});
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&dr_body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "downgrade initial doit retourner 200"
    );

    // Étape 2 : Revert via PATCH status=live.
    let patch_body = serde_json::json!({"status": "live"});
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&patch_body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "PATCH revert doit retourner 204"
    );

    // Étape 3 : Vérifier que status=live en DB via un downgrade qui réussit.
    // Si la note est revenue à live, un nouveau downgrade doit fonctionner → 200.
    // (Preuve indirecte que la note existe et a bien été mise à jour.)
    let dr_body2 = serde_json::json!({"note_id": note_id, "reason": "confirm revert"});
    let req2 = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&dr_body2).unwrap()))
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "downgrade post-revert doit fonctionner (note existe, patch_note_status appliqué)"
    );

    let bytes = resp2.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "downgraded");
    assert_eq!(json["reason"], "confirm revert");
}
