//! Tests E2E F-60 L2 — `GET /api/v1/lessons/recall`.
//!
//! Couvre :
//! 1. `recall_returns_lessons_by_class` — recall par classe, payload conforme
//!    (ulid/title/snippet/tags/anchor_ms), section restreinte à lessons-learned.
//! 2. `recall_excludes_codified` — leçon taguée `codified` jamais retournée.
//! 3. `recall_invalid_class_400` — classe hors vocabulaire → 400.
//! 4. `recall_unauthenticated_401` — sans JWT → 401.
//! 5. `recall_default_limit_5` — sans `limit`, défaut 5 appliqué.
//! 6. `recall_latency_under_50ms` — assert latence < 50 ms sur fixtures.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_embed::Noop as NoopEmbedder;
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Preset ACL autorisant `lesson-tester` en lecture sur la section lessons-learned.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "lesson-tester"
read_patterns  = ["main/lessons-learned", "main/*", "main/main"]
write_patterns = []
"#;

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL — invariant test");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory()"),
    );

    let noop = Arc::new(NoopEmbedder::new(8));
    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(noop);
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
            "lesson-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test")
}

fn recall_req(query: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(format!("/api/v1/lessons/recall?{query}"))
        .method("GET");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Test 1 : recall par classe → payload conforme, restreint à lessons-learned.
#[tokio::test]
async fn recall_returns_lessons_by_class() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Leçon taguée `deploy` (le mot n'est PAS dans le corps → match par tag).
    idx.seed_lesson(
        "01KAAAAAAAAAAAAAAAAAAAAAAA",
        "Cutover discipline",
        "deploy release",
        "Toujours health-check avant le basculement.",
        1_700_000_000_000,
    )
    .await
    .expect("seed deploy lesson");

    // Note d'une autre section avec "deploy" dans le corps → exclue par la section.
    idx.seed_note_with_fts("01KBBBBBBBBBBBBBBBBBBBBBBB", "debug", "deploy crashed")
        .await
        .expect("seed debug note");

    let resp = app
        .oneshot(recall_req("class=deploy&limit=5", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let json = body_json(resp).await;
    let items = json["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "seule la leçon lessons-learned doit matcher"
    );

    let it = &items[0];
    assert_eq!(it["ulid"], "01KAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(it["title"], "Cutover discipline");
    assert_eq!(it["anchor_ms"], 1_700_000_000_000_i64);
    let tags: Vec<String> = it["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["deploy".to_string(), "release".to_string()]);
    assert!(
        !it["snippet"].as_str().unwrap().is_empty(),
        "snippet non vide attendu"
    );
}

/// Test 2 : leçon `codified` exclue du recall.
#[tokio::test]
async fn recall_excludes_codified() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    idx.seed_lesson(
        "01KCAAAAAAAAAAAAAAAAAAAAAA",
        "Migration active",
        "migration",
        "Ne jamais modifier une migration appliquée.",
        1_700_000_000_000,
    )
    .await
    .expect("seed active");
    idx.seed_lesson(
        "01KCBBBBBBBBBBBBBBBBBBBBBB",
        "Migration codifiée",
        "migration codified",
        "Leçon déjà intégrée migration.",
        1_700_000_001_000,
    )
    .await
    .expect("seed codified");

    let resp = app
        .oneshot(recall_req("class=migration", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let ulids: Vec<String> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["ulid"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ulids.contains(&"01KCAAAAAAAAAAAAAAAAAAAAAA".to_string()),
        "leçon active présente. ulids={ulids:?}"
    );
    assert!(
        !ulids.contains(&"01KCBBBBBBBBBBBBBBBBBBBBBB".to_string()),
        "leçon codified exclue. ulids={ulids:?}"
    );
}

/// Test 3 : classe hors vocabulaire → 400.
#[tokio::test]
async fn recall_invalid_class_400() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);

    let resp = app
        .oneshot(recall_req("class=not_a_real_class", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "classe inconnue → 400"
    );
}

/// Test 3b : tentative d'injection FTS via class → 400 (vocabulaire fermé).
#[tokio::test]
async fn recall_injection_attempt_400() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);

    // class=deploy OR release — encodé URL → rejeté car != valeur littérale du vocabulaire.
    let resp = app
        .oneshot(recall_req("class=deploy%20OR%20release", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "injection FTS rejetée par validation vocabulaire"
    );
}

/// Test 4 : sans JWT → 401.
#[tokio::test]
async fn recall_unauthenticated_401() {
    let (app, _state, _idx) = build_app().await;

    let resp = app.oneshot(recall_req("class=deploy", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "sans token → 401");
}

/// Test 5 : sans `limit`, défaut 5 appliqué (6 leçons seedées → 5 retournées).
#[tokio::test]
async fn recall_default_limit_5() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    for i in 0..6u8 {
        let id = format!("01KDDDDDDDDDDDDDDDDDDDDDD{i}");
        idx.seed_lesson(
            &id,
            &format!("Leçon archi {i}"),
            "archi",
            "Décision d'architecture documentée.",
            1_700_000_000_000 + i64::from(i),
        )
        .await
        .expect("seed loop");
    }

    let resp = app
        .oneshot(recall_req("class=archi", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        5,
        "défaut limit=5 attendu, got {}",
        items.len()
    );
}

/// Test 6 : latence < 50 ms sur fixtures (assert perf du contrat L2).
///
/// In-memory SQLite, ~20 leçons — le chemin BM25-only doit rester très en deçà
/// de la cible 50 ms. Marge large pour absorber la variance CI.
#[tokio::test]
async fn recall_latency_under_50ms() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    for i in 0..20u8 {
        let id = format!("01KEEEEEEEEEEEEEEEEEEEEE{:02}", i);
        idx.seed_lesson(
            &id,
            &format!("Leçon ci-cd {i}"),
            "ci-cd",
            "Pipeline runner discipline et isolation des jobs.",
            1_700_000_000_000 + i64::from(i),
        )
        .await
        .expect("seed loop");
    }

    let start = std::time::Instant::now();
    let resp = app
        .oneshot(recall_req("class=ci-cd&limit=5", Some(&token)))
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        elapsed.as_millis() < 50,
        "recall doit être < 50ms, mesuré {} ms",
        elapsed.as_millis()
    );
}
