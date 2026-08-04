//! Tests E2E — `GET /api/v1/system/traces` (v0.7.5 Slice 3).
//!
//! Couvre :
//! 1. `get_traces_unauthenticated_401` — auth obligatoire.
//! 2. `get_traces_no_acl_scope_is_403` — JWT valide sans droits ACL → 403.
//! 3. `get_traces_bad_range_400` — `from_ms > to_ms` → 400.
//! 4. `get_traces_bad_cursor_400` — curseur malformé → 400.
//! 5. `get_traces_ok_excludes_context_sent_and_paginates` — 200 + exclusion context-sent
//!    + pagination keyset + round-trip cursor HTTP (P2-3).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::scope::TenantId;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::session_trace_store::{SessionTraceRow, SessionTraceStore};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// ACL authorisant le consommateur de test (miroir dashboard.rs)
// ---------------------------------------------------------------------------

const TEST_ACL: &str = r#"
[[consumer]]
identity = "traces-tester"
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
        "noop-traces"
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
// Helper : construit le router de test sans store session_trace câblé.
// Utilisé pour les tests 401 / 403 / 400 (retour avant accès au store).
// ---------------------------------------------------------------------------

async fn build_app() -> (axum::Router, AppState) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL traces");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test system"),
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
// Helper : construit le router de test AVEC store session_trace câblé.
// Utilisé pour le test 200 qui doit insérer des données avant la requête.
// ---------------------------------------------------------------------------

async fn build_app_with_traces() -> (axum::Router, AppState, SessionTraceStore) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL traces");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test system"),
    );

    let store = SessionTraceStore::open_in_memory()
        .await
        .expect("SessionTraceStore::open_in_memory() — invariant test system");

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        .with_session_trace(store.clone());
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, store)
}

// ---------------------------------------------------------------------------
// Helper : signe un JWT pour le consommateur de test
// ---------------------------------------------------------------------------

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "traces-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT traces-tester")
}

// ---------------------------------------------------------------------------
// Helper : construit une requête GET /api/v1/system/traces
// ---------------------------------------------------------------------------

fn get_traces_req(token: Option<&str>, query: &str) -> Request<Body> {
    let uri = if query.is_empty() {
        "/api/v1/system/traces".to_string()
    } else {
        format!("/api/v1/system/traces?{query}")
    };
    let mut b = Request::builder().uri(uri).method("GET");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

// ---------------------------------------------------------------------------
// Helper : insère une trace « normale » via insert_at
// ---------------------------------------------------------------------------

async fn insert_trace(
    store: &SessionTraceStore,
    session: &str,
    agent: &str,
    action: &str,
    ts: i64,
    created: i64,
) -> i64 {
    let row = SessionTraceRow {
        session_id: session.to_owned(),
        agent_id: agent.to_owned(),
        ts_ms: ts,
        action_type: action.to_owned(),
        target: Some("target".into()),
        intent: Some("intent".into()),
        outcome: Some("success".into()),
        marker: None,
        ref_: None,
    };
    store
        .insert_at(&TenantId::new("main"), &row, created)
        .await
        .expect("insert_trace")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sans token → 401 (l'endpoint est derrière auth, même groupe que /dashboard).
#[tokio::test]
async fn get_traces_unauthenticated_401() {
    let (app, _state) = build_app().await;
    let resp = app.oneshot(get_traces_req(None, "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Consumer authentifié sans scope Read sur `main/dashboard` → 403 Forbidden.
#[tokio::test]
async fn get_traces_no_acl_scope_is_403() {
    let (app, state) = build_app().await;

    // Consumer inconnu du preset TEST_ACL → AclEngine::evaluate retourne Deny.
    let no_dash_token = state
        .jwt
        .sign(
            "no-dashboard-access",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT no-dashboard-access");

    let resp = app
        .oneshot(get_traces_req(Some(&no_dash_token), ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// `from_ms > to_ms` → 400 Bad Request.
#[tokio::test]
async fn get_traces_bad_range_400() {
    let (app, state) = build_app().await;
    let token = sign(&state);

    let resp = app
        .oneshot(get_traces_req(Some(&token), "from_ms=200&to_ms=100"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Curseur malformé → 400 Bad Request.
#[tokio::test]
async fn get_traces_bad_cursor_400() {
    let (app, state) = build_app().await;
    let token = sign(&state);

    let resp = app
        .oneshot(get_traces_req(Some(&token), "cursor=pas_un_cursor_valide"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Test 200 : exclusion `context-sent` + pagination + round-trip cursor HTTP (P2-3).
///
/// Vérifie :
/// - HTTP 200 avec token valide + store câblé
/// - `context-sent` absents de la réponse (prérequis dur F-85)
/// - `limit=2` avec 3 traces normales → page 1 = 2 traces, `next_cursor` non null
/// - Round-trip cursor HTTP : page 2 ne contient aucun `id` de page 1 (P2-3)
#[tokio::test]
async fn get_traces_ok_excludes_context_sent_and_paginates() {
    let (app, state, store) = build_app_with_traces().await;
    let token = sign(&state);

    // Seed : 1 context-sent + 3 traces normales (created_at croissant → tri DESC inversé).
    store
        .mark_sent("main", "sess-1", "ulid-A", "snippet", 1000)
        .await
        .expect("mark_sent");
    insert_trace(&store, "sess-1", "agent-x", "plan", 2000, 2).await;
    insert_trace(&store, "sess-1", "agent-x", "decision", 3000, 3).await;
    insert_trace(&store, "sess-1", "agent-x", "verdict", 4000, 4).await;

    // ── Page 1 : limit=2 ──────────────────────────────────────────────────────
    let resp = app
        .clone()
        .oneshot(get_traces_req(Some(&token), "limit=2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "doit retourner 200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON valide page 1");

    let traces_p1 = json["traces"].as_array().expect("traces doit être tableau");
    assert_eq!(
        traces_p1.len(),
        2,
        "page 1 doit contenir exactement 2 traces"
    );

    // Aucune trace context-sent (prérequis dur F-85).
    for t in traces_p1 {
        assert_ne!(
            t["action_type"].as_str().unwrap_or(""),
            "context-sent",
            "context-sent ne doit jamais apparaître dans la réponse"
        );
    }

    // `next_cursor` présent (il reste 1 trace non retournée).
    let next_cursor = json["next_cursor"]
        .as_str()
        .expect("next_cursor doit être non-null");
    assert!(!next_cursor.is_empty(), "next_cursor ne doit pas être vide");

    // IDs de la page 1 pour vérifier l'absence de chevauchement.
    let ids_p1: Vec<i64> = traces_p1
        .iter()
        .map(|t| t["id"].as_i64().expect("id doit être i64"))
        .collect();

    // ── Round-trip cursor HTTP (P2-3) ─────────────────────────────────────────
    let cursor_query = format!("limit=2&cursor={next_cursor}");
    let resp2 = app
        .oneshot(get_traces_req(Some(&token), &cursor_query))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK, "page 2 doit retourner 200");

    let bytes2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let json2: serde_json::Value = serde_json::from_slice(&bytes2).expect("JSON valide page 2");

    let traces_p2 = json2["traces"]
        .as_array()
        .expect("traces page 2 doit être tableau");
    assert!(
        !traces_p2.is_empty(),
        "page 2 doit contenir au moins 1 trace"
    );

    // Aucun chevauchement d'id entre page 1 et page 2.
    for t in traces_p2 {
        let id = t["id"].as_i64().expect("id doit être i64");
        assert!(
            !ids_p1.contains(&id),
            "id={id} apparaît dans page 1 ET page 2 — chevauchement interdit (P2-3)"
        );
        assert_ne!(
            t["action_type"].as_str().unwrap_or(""),
            "context-sent",
            "context-sent ne doit jamais apparaître dans la réponse"
        );
    }

    // Page 2 doit être la dernière (1 trace restante après les 2 de page 1).
    assert!(
        json2["next_cursor"].is_null(),
        "page 2 doit être la dernière — next_cursor doit être null"
    );
}
