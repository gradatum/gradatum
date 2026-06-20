//! Static router for the gradatum studio (`/ui/*`).
//!
//! Serves the React/Vite bundle without authentication (LAN: the JS is public,
//! API calls carry the Bearer JWT). Security hardening:
//! - **Strict CSP** (`Content-Security-Policy`): `style-src 'self'`
//!   without global `unsafe-inline`; self-hosted fonts.
//!   `style-src-attr 'unsafe-inline'` for dynamic CSS custom properties.
//!   `frame-ancestors 'none'` (modern browsers).
//! - `X-Frame-Options: DENY` (legacy browsers — defense in depth alongside CSP).
//! - `X-Content-Type-Options: nosniff`.
//! - `Referrer-Policy: no-referrer` (no URL leakage).
//! - `Permissions-Policy: geolocation=(), microphone=(), camera=()`.
//!
//! **SPA fallback**: any unresolved `/ui/*` route (deep-link, refresh on
//! `/ui/notes/x`) serves `index.html` → the React client router takes over.
//! `ServeFile` itself returns a **clean 404** if `index.html` is absent (bundle
//! not deployed) → the "absent bundle → clean 404" contract is preserved.

use std::path::Path;

use axum::Router;
use http::{HeaderName, HeaderValue, header::X_CONTENT_TYPE_OPTIONS};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
};

/// Builds the studio sub-router for `/ui/*` from the bundle directory.
///
/// The returned router is intended to be merged into the main router without
/// going through the JWT authentication middleware.
///
/// # Side effects
/// None at construction time (file reads are deferred to the request
/// via `ServeDir`/`ServeFile`).
pub fn build_studio_router<S>(ui_dir: &Path) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // CSP durcie — D3.1 + D3.2 :
    // - style-src 'self' : 'unsafe-inline' global retiré (C34 soldé) ; seules les
    //   feuilles CSS bundlées par Vite (hash filename) sont autorisées.
    // - style-src-attr 'unsafe-inline' : permet les attributs style="" individuels
    //   (CSS custom properties dynamiques : --stat-dot-color, --chip-color, etc.
    //   + overrides locaux mineurs sur boutons). Directement sur l'attribut uniquement,
    //   n'autorise pas les blocs <style> inline.
    // - font-src 'self' : Google Fonts retirés (C35 soldé) ; polices self-hosted
    //   (@fontsource woff2/woff bundlées par Vite).
    // - fonts.googleapis.com + fonts.gstatic.com retirés de style-src et font-src.
    let csp_name = HeaderName::from_static("content-security-policy");
    let csp_value = HeaderValue::from_static(
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self'; \
         style-src-attr 'unsafe-inline'; \
         font-src 'self'; \
         connect-src 'self'; \
         img-src 'self' data:; \
         frame-ancestors 'none'",
    );
    let xcto_value = HeaderValue::from_static("nosniff");

    // durcissement complémentaire sur /ui/*.
    let referrer_policy_name = HeaderName::from_static("referrer-policy");
    let referrer_policy_value = HeaderValue::from_static("no-referrer");
    let permissions_policy_name = HeaderName::from_static("permissions-policy");
    let permissions_policy_value =
        HeaderValue::from_static("geolocation=(), microphone=(), camera=()");
    // X-Frame-Options: DENY — défense en profondeur complémentaire au CSP `frame-ancestors 'none'`.
    // Couvre les navigateurs legacy qui ne supportent pas CSP frame-ancestors (IE11, Safari <10).
    let x_frame_options_name = HeaderName::from_static("x-frame-options");
    let x_frame_options_value = HeaderValue::from_static("DENY");

    // #6 — Fallback SPA : index.html servi pour toute route /ui/* non résolue
    // (deep-link, refresh). `.fallback()` (idiome SPA tower-http) sert le fichier
    // configuré pour toute route non matchée, en ignorant le path de la requête.
    // `ServeFile` renvoie lui-même un 404 propre si index.html est absent (bundle
    // non déployé) → le contrat "bundle absent → 404 propre" est préservé.
    let index_html = ui_dir.join("index.html");
    let studio_service = ServeDir::new(ui_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(index_html));

    Router::new()
        .nest_service("/ui", studio_service)
        .layer(SetResponseHeaderLayer::overriding(csp_name, csp_value))
        .layer(SetResponseHeaderLayer::overriding(
            X_CONTENT_TYPE_OPTIONS,
            xcto_value,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            referrer_policy_name,
            referrer_policy_value,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            permissions_policy_name,
            permissions_policy_value,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            x_frame_options_name,
            x_frame_options_value,
        ))
}
