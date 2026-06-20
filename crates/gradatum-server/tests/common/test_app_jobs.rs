//! Helpers de construction routeur/state pour les tests E2E.
//!
//! Fournit deux constructeurs :
//! - [`build_with_flag`] : construit un `axum::Router` de test (flag JWT retiré).
//! - [`build_with_state`] : retourne `(axum::Router, AppState)` pour permettre
//!   la seed directe de notes via `state.search.seed_note(...)`.
//!
//! Le middleware `auth_middleware` est inclus — requis par les handlers write.rs
//! qui extraient `Extension<TrustContext>`. Les handlers notes.rs n'extraient pas
//! TrustContext, mais la présence du middleware est sans effet négatif.

use axum::{Router, middleware};
use gradatum_server::state::AppState;

/// Construit un routeur de test.
///
/// Utilise `AppState::default()` — index in-memory, queue placeholder, ACL deny-all.
/// Le middleware `auth_middleware` est actif — requis pour `Extension<TrustContext>`.
///
/// # Usage
///
/// ```ignore
/// let app = common::test_app_jobs::build_with_flag().await;
/// ```
#[allow(dead_code)] // API utilitaire — peut être utilisée dans d'autres tests du crate.
pub async fn build_with_flag() -> Router {
    let (app, _state) = build_with_state().await;
    app
}

/// Construit un routeur de test et retourne également l'`AppState` associé.
///
/// Permet la seed directe de notes dans l'index in-memory via `state.search.seed_note(...)`.
/// Le routeur partage le même `AppState` (cloné via `Arc` sous le capot).
///
/// # Usage
///
/// ```ignore
/// let (app, state) = common::test_app_jobs::build_with_state().await;
/// state.search.seed_note("01KR...", "reference", "body").await.unwrap();
/// let resp = app.oneshot(req).await.unwrap();
/// ```
///
/// # Panics
///
/// Panique si le `SqliteIndex` in-memory ne peut pas être initialisé — impossible
/// en conditions normales.
pub async fn build_with_state() -> (Router, AppState) {
    let state = AppState::default();

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state)
}
