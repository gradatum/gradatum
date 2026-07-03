//! Tests E2E — `GET /api/v1/system/metrics/{catalog,timeseries}` (v0.7.5 Slice 2a F-85).
//!
//! Couvre :
//! 1. `timeseries_unauthenticated_is_401` — auth obligatoire.
//! 2. `timeseries_unknown_series_is_400` — série hors allowlist → 400.
//! 3. `timeseries_from_ge_to_is_400` — plage invalide (from_ms >= to_ms) → 400.
//! 4. `catalog_lists_stub_families_marked_uninstrumented` — stubs curator/llm toujours présents.
//! 5. `compute_bucket_ms_no_downsample_when_under_max` — test unitaire compute_bucket_ms.
//! 6. `compute_bucket_ms_rounds_up_to_minute_multiple` — test unitaire compute_bucket_ms.
//! 7. `timeseries_extreme_range_is_400` — plage i64::MIN..i64::MAX → 400 (C1 security-reviewer).
//! 8. `compute_bucket_ms_no_overflow_on_huge_span` — totalité pour i64::MAX (C1bis security-reviewer).

use std::sync::{Arc, LazyLock};

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

// ---------------------------------------------------------------------------
// Seed JWT déterministe pour authed_request (32 bytes fixes — tests uniquement)
// ---------------------------------------------------------------------------

/// Seed Ed25519 fixe : 32 octets identiques dans build_app() et AUTHED_TOKEN.
/// Même seed → même keypair → tokens mutuellement vérifiables.
const TEST_SEED: &[u8; 32] = b"metrics-test-seed-32bytes-padded";

/// Token JWT pré-signé — réutilisé par `authed_request` sans état partagé.
static AUTHED_TOKEN: LazyLock<String> = LazyLock::new(|| {
    JwtService::from_signing_bytes(
        TEST_SEED,
        "metrics-test-kid".to_string(),
        "gradatum".to_string(),
        3600,
        86_400,
    )
    .expect("static JwtService for metrics endpoint tests")
    .sign(
        "metrics-tester",
        &["read".to_string()],
        TokenScope::Service,
        "main",
    )
    .expect("sign static test token")
});

// ---------------------------------------------------------------------------
// ACL autorisant le consommateur de test (miroir dashboard.rs)
// ---------------------------------------------------------------------------

const TEST_ACL: &str = r#"
[[consumer]]
identity = "metrics-tester"
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
        "noop-metrics"
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
// Helper : construit le router de test + index en mémoire
// Retourne `Router` directement — le token est dans AUTHED_TOKEN.
// ---------------------------------------------------------------------------

async fn build_app() -> axum::Router {
    use axum::{Router, middleware};

    // Même seed que AUTHED_TOKEN → les tokens signés par AUTHED_TOKEN sont valides.
    let jwt = JwtService::from_signing_bytes(
        TEST_SEED,
        "metrics-test-kid".to_string(),
        "gradatum".to_string(),
        3600,
        86_400,
    )
    .expect("JwtService build_app — invariant test system");
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL metrics");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test system"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Helper : requête authentifiée avec le token statique
// ---------------------------------------------------------------------------

fn authed_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method(method)
        .header("authorization", format!("Bearer {}", *AUTHED_TOKEN))
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Helper : requête non authentifiée
// ---------------------------------------------------------------------------

fn unauth_request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method("GET")
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests E2E
// ---------------------------------------------------------------------------

/// Sans token → 401 (l'endpoint timeseries est derrière auth).
#[tokio::test]
async fn timeseries_unauthenticated_is_401() {
    let app = build_app().await;
    let resp = app
        .oneshot(unauth_request(
            "/api/v1/system/metrics/timeseries?series=a&from_ms=0&to_ms=100",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Série hors allowlist → 400 Bad Request.
#[tokio::test]
async fn timeseries_unknown_series_is_400() {
    let app = build_app().await;
    let resp = app
        .oneshot(authed_request(
            "GET",
            "/api/v1/system/metrics/timeseries?series=not.allowed&from_ms=0&to_ms=100",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Plage invalide (from_ms >= to_ms) → 400 Bad Request.
#[tokio::test]
async fn timeseries_from_ge_to_is_400() {
    let app = build_app().await;
    let resp = app
        .oneshot(authed_request(
            "GET",
            "/api/v1/system/metrics/timeseries?series=read_usage.search&from_ms=100&to_ms=100",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Catalog → 200, stubs curator/llm présents et marqués instrumented=false.
#[tokio::test]
async fn catalog_lists_stub_families_marked_uninstrumented() {
    let app = build_app().await;
    let resp = app
        .oneshot(authed_request("GET", "/api/v1/system/metrics/catalog"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["series"].as_array().unwrap();
    // Les stubs curator/llm sont toujours présents, instrumented=false.
    assert!(
        entries
            .iter()
            .any(|e| e["key"] == "curator.decisions" && e["instrumented"] == false),
        "curator.decisions stub absent ou instrumented=true"
    );
}

// ---------------------------------------------------------------------------
// Tests unitaires compute_bucket_ms
// ---------------------------------------------------------------------------

/// 100 points (span 100 min) sous max_points=500 → bucket brut = 60_000 ms (pas de downsample).
#[test]
fn compute_bucket_ms_no_downsample_when_under_max() {
    assert_eq!(
        gradatum_server::api_v1::system::compute_bucket_ms(100 * 60_000, 500),
        60_000
    );
}

/// Span 14 j, max_points=500 → bucket ≥ 60_000, multiple de 60_000, et span/bucket ≤ 500.
#[test]
fn compute_bucket_ms_rounds_up_to_minute_multiple() {
    let span = 14 * 86_400_000_i64;
    let b = gradatum_server::api_v1::system::compute_bucket_ms(span, 500);
    assert!(b >= 60_000);
    assert_eq!(b % 60_000, 0, "multiple de 60s");
    assert!(span / b <= 500, "nb buckets <= max_points");
}

// ---------------------------------------------------------------------------
// Tests sécurité C1 / C1bis (security-reviewer P2 — v0.7.5 Slice 2a)
// ---------------------------------------------------------------------------

/// Plage extrême i64::MIN..i64::MAX → 400 (overflow check + guard MAX_SPAN_MS).
///
/// Sans correctif, `checked_sub` renverrait `None` sur cette plage (overflow i64),
/// ou la soustraction brute produirait une valeur négative en release (wrap) → scan
/// complet de `metric_sample` sans borne. Ce test prouve que la protection est active.
#[tokio::test]
async fn timeseries_extreme_range_is_400() {
    let app = build_app().await;
    let resp = app
        .oneshot(authed_request(
            "GET",
            // from_ms = i64::MIN = -9223372036854775808
            // to_ms   = i64::MAX =  9223372036854775807
            "/api/v1/system/metrics/timeseries?series=read_usage.search&from_ms=-9223372036854775808&to_ms=9223372036854775807",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `compute_bucket_ms(i64::MAX, 500)` ne panique pas et retourne un multiple de 60_000 ≥ 60_000.
///
/// Ce test s'exécute en mode debug (overflow-checks actifs par défaut dans `cargo test`),
/// ce qui est plus strict que le mode release. Il prouve l'absence de panique quel que soit
/// le mode de compilation (C1bis security-reviewer).
#[test]
fn compute_bucket_ms_no_overflow_on_huge_span() {
    let b = gradatum_server::api_v1::system::compute_bucket_ms(i64::MAX, 500);
    assert!(b >= 60_000, "bucket doit être ≥ 60_000, obtenu {b}");
    assert_eq!(
        b % 60_000,
        0,
        "bucket doit être un multiple de 60_000, obtenu {b}"
    );
}

// ---------------------------------------------------------------------------
// Tests durcissement reviewer P2-b — cardinalité séries (v0.7.5 Slice 2a)
// ---------------------------------------------------------------------------

/// 33 séries valides distinctes (> MAX_SERIES=32) → 400 Bad Request.
///
/// Vérifie que la borne de cardinalité après déduplication est active (ADN 5 DoS).
#[tokio::test]
async fn timeseries_too_many_series_is_400() {
    let app = build_app().await;
    // Génère 33 clés valides distinctes : `read_usage.X` est dans l'allowlist
    // (series_meta retourne Some pour tout préfixe `read_usage.*`).
    let keys: Vec<String> = (0..33).map(|i| format!("read_usage.key{i:02}")).collect();
    let series_param = keys.join(",");
    let uri =
        format!("/api/v1/system/metrics/timeseries?series={series_param}&from_ms=0&to_ms=3600000");
    let resp = app.oneshot(authed_request("GET", &uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Clé valide répétée en CSV → déduplication → 200, une seule entrée dans la réponse.
///
/// Vérifie que les doublons sont éliminés avant traitement (pas d'entrée vide en double).
#[tokio::test]
async fn timeseries_duplicate_series_deduplicated() {
    let app = build_app().await;
    let resp = app
        .oneshot(authed_request(
            "GET",
            "/api/v1/system/metrics/timeseries?series=read_usage.search,read_usage.search&from_ms=0&to_ms=3600000",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let series = json["series"].as_array().unwrap();
    let count = series
        .iter()
        .filter(|s| s["key"] == "read_usage.search")
        .count();
    assert_eq!(
        count, 1,
        "la clé dupliquée doit apparaître une seule fois dans la réponse, obtenu {count}"
    );
}
