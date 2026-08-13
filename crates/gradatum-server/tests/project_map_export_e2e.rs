//! Tests E2E `GET /api/v1/project-map/export-features`.
//!
//! Couvre :
//! 1. `export_features_unauthenticated_is_401` — auth obligatoire.
//! 2. `export_features_authenticated_acl_deny_is_403` — ACL sans grant → 403.
//! 3. `export_features_empty_section_returns_empty_array` — section vide → `[]`.
//! 4. `export_features_returns_sorted_features` — tri F-XX numérique + exclusion dropped.
//! 5. `export_features_include_dropped_true_exposes_dropped` — param `include_dropped=true`.
//! 6. `export_features_invalid_bool_param_is_400` — param invalide `include_dropped=foo` → 400.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

// ── Preset ACL ───────────────────────────────────────────────────────────────

const TEST_ACL: &str = r#"
[[consumer]]
identity = "pm-tester"
read_patterns  = ["main/*", "main/project-map"]
write_patterns = []
"#;

const DENY_ACL: &str = r#"
[[consumer]]
identity = "pm-tester"
read_patterns  = []
write_patterns = []
"#;

// ── Embedder noop ─────────────────────────────────────────────────────────────

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-pm"
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

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn build_app(acl_preset: &str) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(acl_preset).expect("preset ACL valide");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test project_map"),
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

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "pm-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT — invariant test")
}

fn get_export(token: Option<&str>, query: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(format!("/api/v1/project-map/export-features{query}"))
        .method("GET");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder
        .body(Body::empty())
        .expect("builder — invariant test")
}

// ── Corps note project-map minimale (wikilinks typés forcés) ─────────────────

fn pm_body_released(feature: &str, version: &str, title: &str) -> (String, String) {
    (
        format!(
            "[[feature:{feature}]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/{version}]]"
        ),
        title.to_string(),
    )
}

fn pm_body_planned_backlog(feature: &str, title: &str) -> (String, String) {
    (
        format!(
            "[[feature:{feature}]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/backlog]]"
        ),
        title.to_string(),
    )
}

fn pm_body_dropped(feature: &str, version: &str, title: &str) -> (String, String) {
    (
        format!(
            "[[feature:{feature}]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
             [[release:dropped]] [[version:gradatum/{version}]]"
        ),
        title.to_string(),
    )
}

// ── Seed helper ───────────────────────────────────────────────────────────────

async fn seed(idx: &SqliteIndex, body: &str) {
    let id = Ulid::generate().to_string();
    idx.seed_note(&id, "project-map", body)
        .await
        .expect("seed note project-map");
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

// 1. Sans token → 401.
#[tokio::test]
async fn export_features_unauthenticated_is_401() {
    let (app, _state, _idx) = build_app(TEST_ACL).await;
    let resp = app.oneshot(get_export(None, "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// 2. Token valide mais ACL deny → 403.
#[tokio::test]
async fn export_features_authenticated_acl_deny_is_403() {
    let (app, state, _idx) = build_app(DENY_ACL).await;
    let token = sign(&state);
    let resp = app.oneshot(get_export(Some(&token), "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// 3. Section project-map vide → 200 + tableau vide.
#[tokio::test]
async fn export_features_empty_section_returns_empty_array() {
    let (app, state, _idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    let resp = app.oneshot(get_export(Some(&token), "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(entries.is_empty(), "section vide → tableau vide");
}

// 4. Trois cartes seedées : F-37 released, F-50 backlog, F-99 dropped.
//    Sans include_dropped → F-37 + F-50 triés, F-99 exclue.
//    F-50 backlog → version = "vX.Y.Z" (Règle A).
#[tokio::test]
async fn export_features_returns_sorted_features_excluding_dropped() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);

    // Seeder dans l'ordre inverse intentionnellement (pour vérifier le tri).
    let (body_f99, _) = pm_body_dropped("F-99", "0.6.0", "F-99 annulée");
    let (body_f50, _) = pm_body_planned_backlog("F-50", "F-50 future");
    let (body_f37, _) = pm_body_released("F-37", "0.6.3", "F-37 livrée");

    seed(&idx, &body_f99).await;
    seed(&idx, &body_f50).await;
    seed(&idx, &body_f37).await;

    let resp = app.oneshot(get_export(Some(&token), "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();

    // F-99 (dropped) exclue par défaut.
    assert_eq!(entries.len(), 2, "dropped exclue par défaut : {entries:?}");

    // Tri numérique F-37 avant F-50.
    assert_eq!(entries[0]["feature"], "F-37", "tri numérique attendu");
    assert_eq!(entries[0]["release"], "released");
    assert_eq!(entries[0]["version"], "v0.6.3");
    // seed_note ne popule pas la colonne title → "" via unwrap_or_default.

    assert_eq!(entries[1]["feature"], "F-50");
    assert_eq!(entries[1]["release"], "planned");
    // Règle A : backlog → sentinel "vX.Y.Z"
    assert_eq!(
        entries[1]["version"], "vX.Y.Z",
        "backlog → sentinel vX.Y.Z (Règle A)"
    );
}

// 5. include_dropped=true expose les cartes dropped.
#[tokio::test]
async fn export_features_include_dropped_exposes_dropped_cards() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);

    let (body_f10, _) = pm_body_released("F-10", "0.5.0", "F-10 released");
    let (body_f51, _) = pm_body_dropped("F-51", "0.6.0", "F-51 dropped");

    seed(&idx, &body_f10).await;
    seed(&idx, &body_f51).await;

    // Par défaut : F-51 dropped exclue.
    let resp = app
        .clone()
        .oneshot(get_export(Some(&token), ""))
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let entries_default: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        entries_default.len(),
        1,
        "dropped exclue par défaut : {entries_default:?}"
    );
    assert_eq!(entries_default[0]["feature"], "F-10");

    // include_dropped=true : F-51 incluse.
    let resp2 = app
        .oneshot(get_export(Some(&token), "?include_dropped=true"))
        .await
        .unwrap();
    let bytes2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let entries_all: Vec<serde_json::Value> = serde_json::from_slice(&bytes2).unwrap();
    assert_eq!(
        entries_all.len(),
        2,
        "dropped incluse avec include_dropped=true : {entries_all:?}"
    );

    // Tri : F-10 avant F-51.
    assert_eq!(entries_all[0]["feature"], "F-10");
    assert_eq!(entries_all[1]["feature"], "F-51");
    assert_eq!(entries_all[1]["release"], "dropped");
}

// 6. V-04 : param bool invalide → 400 (rejet serde Axum Query).
//
// Axum rejette `?include_dropped=foo` avec 400 Bad Request car le Query extractor
// ne peut pas désérialiser "foo" en bool. Ce test prouve le comportement de rejet.
// Note : Axum retourne 400 avant d'atteindre le handler (rejet extractor-level).
// Le comportement est donc indépendant de l'auth — on peut envoyer sans token
// pour tester ça, mais le middleware auth s'exécute avant l'extractor Query.
// On passe un token valide pour isoler le rejet du Query extractor.
#[tokio::test]
async fn export_features_invalid_bool_param_is_400() {
    let (app, state, _idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    let resp = app
        .oneshot(get_export(Some(&token), "?include_dropped=foo"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "param bool invalide doit retourner 400"
    );
}
