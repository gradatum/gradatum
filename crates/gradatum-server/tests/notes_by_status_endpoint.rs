//! Tests E2E — `GET /api/v1/notes/by-status` (F-85 bug drill-down "Deprecated vide").
//!
//! Couvre :
//! 1. `notes_by_status_unauthenticated_401` — auth obligatoire.
//! 2. `notes_by_status_no_acl_403` — JWT valide sans droits ACL → 403.
//! 3. `notes_by_status_bad_status_400` — statut vide/inconnu → 400.
//! 4. `notes_by_status_ok_lists_downgraded` — 200 + notes downgraded listées.

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

// ---------------------------------------------------------------------------
// ACL authorisant le consommateur de test (miroir dashboard — même structure
// que TEST_ACL dans system_traces_endpoint.rs).
// ---------------------------------------------------------------------------

const TEST_ACL: &str = r#"
[[consumer]]
identity = "notes-status-tester"
read_patterns  = ["main/*", "main/dashboard", "reference/*"]
write_patterns = []
"#;

// ---------------------------------------------------------------------------
// Embedder noop (zéro I/O, zéro réseau)
// ---------------------------------------------------------------------------

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-notes-status"
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

// ---------------------------------------------------------------------------
// Helper : construit le router de test sans notes seedées.
// Utilisé pour les tests 401 / 403 / 400 (retour avant accès index SQLite).
// ---------------------------------------------------------------------------

async fn build_app() -> (axum::Router, AppState) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL notes-status");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test notes-status"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state)
}

// ---------------------------------------------------------------------------
// Helper : construit le router de test AVEC SqliteIndex partagé pour seed.
// Utilisé pour le test 200 qui doit insérer des notes downgraded avant la requête.
//
// Note de design : `seed_note` + `downgrade_note` sont des méthodes `pub` sur
// `SqliteIndex` — on garde un `Arc<SqliteIndex>` avant de caster en `dyn Index`
// pour pouvoir appeler ces helpers de seed depuis le test.
// ---------------------------------------------------------------------------

async fn build_app_with_index() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL notes-status");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test notes-status"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
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

// ---------------------------------------------------------------------------
// Helper : signe un JWT pour le consommateur de test autorisé
// ---------------------------------------------------------------------------

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "notes-status-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT notes-status-tester")
}

// ---------------------------------------------------------------------------
// Helper : construit une requête GET /api/v1/notes/by-status
// ---------------------------------------------------------------------------

fn get_by_status_req(token: Option<&str>, query: &str) -> Request<Body> {
    let uri = if query.is_empty() {
        "/api/v1/notes/by-status".to_string()
    } else {
        format!("/api/v1/notes/by-status?{query}")
    };
    let mut b = Request::builder().uri(uri).method("GET");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sans token → 401 (endpoint derrière auth, même groupe que /system/traces).
#[tokio::test]
async fn notes_by_status_unauthenticated_401() {
    let (app, _state) = build_app().await;
    let resp = app
        .oneshot(get_by_status_req(None, "status=downgraded"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Consumer authentifié mais inconnu du preset TEST_ACL → 403 Forbidden.
///
/// L'AclEngine évalue `main/dashboard` (locus ACL du handler) — un consumer
/// absent du preset retourne `AclDecision::Deny`.
#[tokio::test]
async fn notes_by_status_no_acl_403() {
    let (app, state) = build_app().await;

    // Consumer inconnu du preset TEST_ACL → AclEngine::evaluate retourne Deny.
    let no_access_token = state
        .jwt
        .sign(
            "no-dashboard-access",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT no-dashboard-access");

    let resp = app
        .oneshot(get_by_status_req(
            Some(&no_access_token),
            "status=downgraded",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// `status` hors allowlist → 400 ; `status` vide → 400.
#[tokio::test]
async fn notes_by_status_bad_status_400() {
    let (app, state) = build_app().await;
    let token = sign(&state);

    // Statut inconnu → rejet global (parse_status_csv retourne None).
    let resp = app
        .clone()
        .oneshot(get_by_status_req(Some(&token), "status=bogus"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "statut inconnu doit retourner 400"
    );

    // Statut vide (chaîne CSV vide, résultat vec vide) → 400.
    let resp2 = app
        .oneshot(get_by_status_req(Some(&token), "status="))
        .await
        .unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::BAD_REQUEST,
        "status= vide doit retourner 400"
    );
}

/// Seed 2 notes downgraded → GET status=downgraded → 200 + entries présentes.
///
/// Vérifie :
/// - HTTP 200 avec token valide.
/// - `entries` contient exactement 2 éléments (les 2 notes seedées).
/// - Chaque entry a les champs `ulid`, `section`, `status`, `snippet`, `modified_at`.
/// - `status` de chaque entry vaut `"downgraded"`.
/// - `total` == 2.
#[tokio::test]
async fn notes_by_status_ok_lists_downgraded() {
    let (app, state, idx) = build_app_with_index().await;
    let token = sign(&state);

    // Seed 2 notes en "live" puis downgrade.
    // `seed_note` + `downgrade_note` sont les méthodes publiques de SqliteIndex.
    let id_a = "01AAAAAAAAAAAAAAAAAAAAAAAA";
    let id_b = "01BBBBBBBBBBBBBBBBBBBBBBBB";

    idx.seed_note(id_a, "decisions", "note A downgraded pour test")
        .await
        .expect("seed_note A");
    idx.seed_note(id_b, "reference", "note B downgraded pour test")
        .await
        .expect("seed_note B");

    let note_id_a = NoteId(id_a.parse().expect("ULID valide id_a"));
    let note_id_b = NoteId(id_b.parse().expect("ULID valide id_b"));

    idx.downgrade_note(
        &gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new("main"),
        ),
        &note_id_a,
        "test: drill-down fix",
        None,
    )
    .await
    .expect("downgrade_note A");
    idx.downgrade_note(
        &gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new("main"),
        ),
        &note_id_b,
        "test: drill-down fix",
        None,
    )
    .await
    .expect("downgrade_note B");

    // Requête GET /api/v1/notes/by-status?status=downgraded
    let resp = app
        .oneshot(get_by_status_req(Some(&token), "status=downgraded"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "doit retourner 200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("réponse JSON valide");

    // Vérification `total` et `entries`.
    let total = json["total"].as_u64().expect("total doit être u64");
    assert_eq!(total, 2, "total doit être 2 (2 notes downgraded seedées)");

    let entries = json["entries"]
        .as_array()
        .expect("entries doit être tableau");
    assert_eq!(
        entries.len(),
        2,
        "entries doit contenir exactement 2 éléments"
    );

    // Chaque entry doit avoir les champs attendus et status == "downgraded".
    for entry in entries {
        assert!(
            entry["ulid"].as_str().is_some(),
            "champ 'ulid' manquant ou non-string"
        );
        assert!(
            entry["section"].as_str().is_some(),
            "champ 'section' manquant ou non-string"
        );
        assert_eq!(
            entry["status"].as_str().unwrap_or(""),
            "downgraded",
            "status doit être 'downgraded'"
        );
        assert!(
            entry["snippet"].as_str().is_some(),
            "champ 'snippet' manquant ou non-string"
        );
        assert!(
            entry["modified_at"].as_str().is_some(),
            "champ 'modified_at' manquant ou non-string"
        );
    }
}
