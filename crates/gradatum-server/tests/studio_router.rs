//! Tests E2E du routeur studio `/ui/*` — V2 sécu + fallback SPA #6 (F-37 post-audit).
//!
//! Couvre :
//! 1. `studio_serves_asset_with_security_headers` — un asset existant est servi avec
//!    CSP + nosniff + Referrer-Policy + Permissions-Policy.
//! 2. `studio_spa_fallback_serves_index_on_deep_link` — une route SPA inconnue
//!    (/ui/notes/x) sert index.html (deep-link/refresh OK).
//! 3. `studio_missing_bundle_returns_404` — bundle absent (ui_dir sans index.html) →
//!    404 propre conservé.

use std::fs;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_server::studio::build_studio_router;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Construit un routeur studio sur un ui_dir tmpdir contenant index.html + un asset.
fn router_with_bundle(dir: &std::path::Path) -> axum::Router {
    fs::write(
        dir.join("index.html"),
        "<!doctype html><html><body>STUDIO_INDEX</body></html>",
    )
    .expect("write index.html");
    fs::create_dir_all(dir.join("assets")).expect("mkdir assets");
    fs::write(dir.join("assets").join("app.js"), "console.log('app')").expect("write asset");
    build_studio_router::<()>(dir).with_state(())
}

#[tokio::test]
async fn studio_serves_asset_with_security_headers() {
    let dir = tempfile::TempDir::new().unwrap();
    let app = router_with_bundle(dir.path());

    let req = Request::builder()
        .uri("/ui/assets/app.js")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "asset existant servi");

    let h = resp.headers();
    assert!(
        h.get("content-security-policy").is_some(),
        "CSP doit être présente"
    );
    assert_eq!(
        h.get("x-content-type-options").map(|v| v.as_bytes()),
        Some(&b"nosniff"[..]),
        "X-Content-Type-Options: nosniff"
    );
    // V2 sécu — durcissement complémentaire.
    assert_eq!(
        h.get("referrer-policy").map(|v| v.as_bytes()),
        Some(&b"no-referrer"[..]),
        "Referrer-Policy: no-referrer"
    );
    assert_eq!(
        h.get("permissions-policy").map(|v| v.as_bytes()),
        Some(&b"geolocation=(), microphone=(), camera=()"[..]),
        "Permissions-Policy verrouillée"
    );
}

#[tokio::test]
async fn studio_spa_fallback_serves_index_on_deep_link() {
    let dir = tempfile::TempDir::new().unwrap();
    let app = router_with_bundle(dir.path());

    // Deep-link / refresh sur une route client-side qui n'existe pas comme fichier.
    let req = Request::builder()
        .uri("/ui/notes/01ABCDEF")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback SPA : route inconnue → 200 (index.html), pas 404"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("STUDIO_INDEX"),
        "le fallback doit servir index.html (contenu attendu), got: {body_str}"
    );

    // Les headers de sécu s'appliquent aussi au fallback.
    // (re-test via une nouvelle requête car oneshot a consommé app)
}

#[tokio::test]
async fn studio_missing_bundle_returns_404() {
    // ui_dir vide (pas d'index.html) → bundle absent.
    let dir = tempfile::TempDir::new().unwrap();
    let app = build_studio_router::<()>(dir.path()).with_state(());

    let req = Request::builder()
        .uri("/ui/notes/x")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "bundle absent (pas d'index.html) → 404 propre conservé"
    );
}
