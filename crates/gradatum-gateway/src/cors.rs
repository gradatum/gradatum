//! CORS origin allowlist module.
//!
//! Allowlist configured in `[server] allowed_origins` of the TOML file.
//!
//! Behavior:
//! - `allowed_origins = []` (default): no CORS configured — no CORS headers added.
//!   Suitable for purely internal deployments (loopback, closed LAN).
//! - `allowed_origins = ["http://localhost:3000"]`: specific origins allowed.
//! - `allowed_origins = ["*"]`: permissive — accepted but not recommended in production.
//!
//! Security: avoid `["*"]` when the gateway is internet-facing with `bearer_token_env`
//! configured — cross-origin requests could exfiltrate data via CSRF if authentication
//! is carried by a cookie rather than a bearer token.

use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

/// Builds a `CorsLayer` from the list of allowed origins.
///
/// Empty `allowed_origins` → no CORS layer (returns `None`).
/// `["*"]` → permissive mode (not recommended in production).
/// Specific list → `AllowOrigin::list(...)`.
///
/// Methods and headers are unrestricted (GET, POST, OPTIONS / all headers).
/// Security relies on the origin allowlist, not on header restrictions.
pub fn build_cors_layer(allowed_origins: &[String]) -> Option<CorsLayer> {
    if allowed_origins.is_empty() {
        return None;
    }

    // Permissive mode when the list contains only "*".
    if allowed_origins == ["*"] {
        return Some(
            CorsLayer::new()
                .allow_origin(AllowOrigin::any())
                .allow_methods(AllowMethods::any())
                .allow_headers(AllowHeaders::any()),
        );
    }

    // Strict allowlist: only the listed origins are permitted.
    // Filters out empty strings and non-HTTP entries before parsing as HeaderValue.
    let origins: Vec<_> = allowed_origins
        .iter()
        .filter(|s| !s.is_empty() && (s.starts_with("http://") || s.starts_with("https://")))
        .filter_map(|s| s.parse::<axum::http::HeaderValue>().ok())
        .collect();

    if origins.is_empty() {
        tracing::warn!("allowed_origins non-empty but no valid origin parsed — CORS disabled");
        return None;
    }

    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(AllowMethods::any())
            .allow_headers(AllowHeaders::any()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_origins_returns_none() {
        let layer = build_cors_layer(&[]);
        assert!(layer.is_none(), "aucun layer CORS attendu pour liste vide");
    }

    #[test]
    fn test_star_returns_some() {
        let layer = build_cors_layer(&["*".to_string()]);
        assert!(layer.is_some(), "layer CORS permissif attendu pour '*'");
    }

    #[test]
    fn test_specific_origin_returns_some() {
        let layer = build_cors_layer(&["http://localhost:3000".to_string()]);
        assert!(
            layer.is_some(),
            "layer CORS attendu pour origine spécifique"
        );
    }

    #[test]
    fn test_invalid_origin_skipped() {
        // Une origine invalide (vide) doit être ignorée.
        let layer = build_cors_layer(&["".to_string()]);
        // "" n'est pas une HeaderValue valide → origins vide → None.
        assert!(
            layer.is_none(),
            "layer CORS ne doit pas être créé pour origine invalide seule"
        );
    }
}
