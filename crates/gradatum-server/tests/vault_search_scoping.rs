//! Tests E2E F-31 — scoping optionnel `locus` + `vault_id` dans vault_search handler.
//!
//! Couvre :
//! 1. `scoping_locus_filters_results` — requête avec locus ne retourne que les notes
//!    correspondantes (BM25 path).
//! 2. `scoping_locus_absent_returns_all` — sans locus : résultats identiques à avant
//!    (non-régression comportementale).
//! 3. `scoping_unknown_field_rejected` — `deny_unknown_fields` intact après ajout.
//! 4. `scoping_vault_id_empty_returns_400` — validation : vault_id vide → 400.
//! 5. `scoping_vault_id_too_long_returns_400` — validation : vault_id > 128 chars → 400.
//! 6. `scoping_vault_id_cross_vault_fts` — vault_id cross-vault : retourne uniquement
//!    les notes du vault cible.

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
use ulid::Ulid;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL autorisant `search-tester` en lecture sur plusieurs vaults/sections.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "search-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "*/decisions", "*/council",
                  "secondary/*", "secondary/main"]
write_patterns = []
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL)
        .expect("preset ACL — invariant statique test vault_search_scoping");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_search_scoping"),
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
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test")
}

fn search_req_with_locus(query: &str, locus: &str, token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 10,
        "tenant_id": "main",
        "locus": locus
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn search_req_basic(query: &str, token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 10,
        "tenant_id": "main"
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn search_req_json(body: serde_json::Value, token: &str) -> Request<Body> {
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : locus présent → ne retourne que les notes dont le locus commence par
/// le préfixe (BM25 path via Noop embedder).
#[tokio::test]
async fn scoping_locus_filters_results() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_council = Ulid::generate().to_string();
    let id_decisions = Ulid::generate().to_string();

    idx.seed_note_with_fts_vault(
        &id_council,
        "main",
        "council",
        Some("council/art19"),
        "scoping gradatum locus prefixe council",
    )
    .await
    .expect("seed council");

    idx.seed_note_with_fts_vault(
        &id_decisions,
        "main",
        "decisions",
        Some("decisions/2026"),
        "scoping gradatum locus prefixe decisions",
    )
    .await
    .expect("seed decisions");

    let req = search_req_with_locus("scoping", "council/", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");

    // Seule la note council doit figurer
    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p.contains(&id_council)),
        "id_council doit figurer dans les résultats. paths={paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains(&id_decisions)),
        "id_decisions NE doit PAS figurer avec locus=council/. paths={paths:?}"
    );
}

/// Test 2 : sans locus — comportement inchangé (non-régression).
///
/// Les deux notes sont retournées quand aucun filtre locus n'est fourni.
#[tokio::test]
async fn scoping_locus_absent_returns_all() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_a = Ulid::generate().to_string();
    let id_b = Ulid::generate().to_string();

    idx.seed_note_with_fts_vault(
        &id_a,
        "main",
        "council",
        Some("council/a"),
        "locus absent test retourne tout gradatum",
    )
    .await
    .expect("seed a");

    idx.seed_note_with_fts_vault(
        &id_b,
        "main",
        "decisions",
        Some("decisions/b"),
        "locus absent test retourne tout gradatum",
    )
    .await
    .expect("seed b");

    let req = search_req_basic("locus", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");
    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or("").to_string())
        .collect();

    assert!(paths.iter().any(|p| p.contains(&id_a)), "id_a absent");
    assert!(paths.iter().any(|p| p.contains(&id_b)), "id_b absent");
}

/// Test 3 : champ inconnu → 422 (deny_unknown_fields intact après ajout F-31).
#[tokio::test]
async fn scoping_unknown_field_rejected() {
    let (app, state, _) = build_app().await;
    let token = sign(&state);

    let body = serde_json::json!({
        "query": "test",
        "tenant_id": "main",
        "unknown_field_f31": "should_fail"
    });
    let req = search_req_json(body, &token);
    let resp = app.oneshot(req).await.unwrap();

    // deny_unknown_fields → Axum retourne 422 Unprocessable Entity
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "champ inconnu doit retourner 422 — deny_unknown_fields intact"
    );
}

/// Test 4 : vault_id vide → 400 Bad Request.
#[tokio::test]
async fn scoping_vault_id_empty_returns_400() {
    let (app, state, _) = build_app().await;
    let token = sign(&state);

    let body = serde_json::json!({
        "query": "test",
        "tenant_id": "main",
        "vault_id": ""
    });
    let req = search_req_json(body, &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vault_id vide doit retourner 400"
    );
}

/// Test 5 : vault_id > 128 chars → 400 Bad Request.
#[tokio::test]
async fn scoping_vault_id_too_long_returns_400() {
    let (app, state, _) = build_app().await;
    let token = sign(&state);

    let long_vault_id = "a".repeat(129);
    let body = serde_json::json!({
        "query": "test",
        "tenant_id": "main",
        "vault_id": long_vault_id
    });
    let req = search_req_json(body, &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vault_id > 128 chars doit retourner 400"
    );
}

/// Test 6 (RÉÉCRIT — P0 cross-tenant Lot 4) : le cross-read `vault_id` ≠ "main"
/// n'est PLUS supporté tant que le vault est mono-physique. Tout `vault_id`
/// arbitraire est refusé `403` (l'ancien "warn et continue" était la faille F-31).
#[tokio::test]
async fn scoping_vault_id_cross_vault_fts() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_secondary = Ulid::generate().to_string();

    // Note dans vault "secondary" (ne doit jamais devenir atteignable via cross-read).
    idx.seed_note_with_fts_vault(
        &id_secondary,
        "secondary",
        "decisions",
        None,
        "crossvault isolation gradatum test",
    )
    .await
    .expect("seed secondary");

    // Requête avec vault_id="secondary" — désormais 403 (mono-vault).
    let body = serde_json::json!({
        "query": "crossvault",
        "limit": 10,
        "tenant_id": "main",
        "vault_id": "secondary"
    });
    let req = search_req_json(body, &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "cross-read vault_id='secondary' → 403 (mono-vault, Lot 4)"
    );
}

/// Test 6bis : vault_id="main" explicite reste accepté (zéro breaking pour main).
#[tokio::test]
async fn scoping_vault_id_main_explicit_ok() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_main = Ulid::generate().to_string();
    idx.seed_note_with_fts_vault(
        &id_main,
        "main",
        "decisions",
        None,
        "crossvault isolation gradatum test",
    )
    .await
    .expect("seed main");

    let body = serde_json::json!({
        "query": "crossvault",
        "limit": 10,
        "tenant_id": "main",
        "vault_id": "main"
    });
    let req = search_req_json(body, &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_id='main' explicite → 200 (zéro breaking)"
    );
}

/// Test 7 (B5 audit P0 — régression double-escape) : locus contenant un préfixe réel
/// matche via l'API HTTP complète.
///
/// Régression couverte : avant le fix B5, handlers.rs appelait `escape_like(locus)` PUIS
/// les fonctions sqlite ré-appelaient `escape_like` → double-escape → 0 match.
/// Ce test échoue sur le code pré-fix si locus contient "/" (serait transformé en "\/\/" etc).
///
/// Seed : note avec locus="agent/logs/2026" → recherche locus="agent/" doit la retourner.
#[tokio::test]
async fn scoping_locus_prefix_matches_via_http_no_double_escape() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_agent = Ulid::generate().to_string();
    let id_other = Ulid::generate().to_string();

    // Note dans locus "agent/logs/2026"
    idx.seed_note_with_fts_vault(
        &id_agent,
        "main",
        "council",
        Some("agent/logs/2026"),
        "doublescape prefixe locus gradatum test",
    )
    .await
    .expect("seed agent/logs/2026");

    // Note dans locus "decisions/2026" (hors filtre)
    idx.seed_note_with_fts_vault(
        &id_other,
        "main",
        "decisions",
        Some("decisions/2026"),
        "doublescape prefixe locus gradatum test",
    )
    .await
    .expect("seed decisions/2026");

    // Filtre locus="agent/" via HTTP — doit retourner id_agent uniquement.
    let req = search_req_with_locus("doublescape", "agent/", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");

    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or("").to_string())
        .collect();

    assert!(
        paths.iter().any(|p| p.contains(&id_agent)),
        "id_agent (locus=agent/logs/2026) doit figurer avec locus=agent/. paths={paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains(&id_other)),
        "id_other (locus=decisions/2026) NE doit PAS figurer avec locus=agent/. paths={paths:?}"
    );
}

/// Test 8 (B5 audit P0 — anti-injection) : locus="%" via HTTP ne matche pas les fixtures
/// normales.
///
/// Garantit que le locus "%" est correctement échappé par sqlite (pas par le handler)
/// et n'agit donc pas comme wildcard LIKE global.
#[tokio::test]
async fn scoping_locus_percent_via_http_no_wildcard_match() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let id_normal = Ulid::generate().to_string();

    // Note avec locus normal (sans "%")
    idx.seed_note_with_fts_vault(
        &id_normal,
        "main",
        "council",
        Some("council/art19"),
        "percentwild locus injection gradatum test",
    )
    .await
    .expect("seed council/art19");

    // Recherche avec locus="%" — ne doit PAS retourner la note (locus ne commence pas par "%")
    let req = search_req_with_locus("percentwild", "%", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items array");

    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or("").to_string())
        .collect();

    assert!(
        !paths.iter().any(|p| p.contains(&id_normal)),
        "locus='%' ne doit pas agir comme wildcard LIKE global. paths={paths:?}"
    );
}
