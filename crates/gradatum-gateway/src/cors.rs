//! Module CORS whitelist — F-MAJ-1 fix.
//!
//! Remplace `CorsLayer::permissive()` de llm-free-gateway-v2 par une whitelist
//! configurée dans `[server] allowed_origins` du TOML.
//!
//! Comportement :
//! - `allowed_origins = []` (défaut) : pas de CORS configuré — aucun header CORS ajouté.
//!   Convient pour les déploiements purement internes (loopback, LAN fermé).
//! - `allowed_origins = ["http://localhost:3000"]` : origins spécifiques autorisées.
//! - `allowed_origins = ["*"]` : permissif — accepté mais déconseillé en prod.
//!
//! Sécurité : ne jamais utiliser `["*"]` si le gateway est exposé sur internet
//! avec bearer_token_env configuré — les requêtes cross-origin pourraient exfiltrer
//! des données via CSRF si l'auth est portée par un cookie plutôt qu'un Bearer.

use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

/// Construit un `CorsLayer` depuis la liste d'origines autorisées.
///
/// `allowed_origins` vide → aucun layer CORS (retourne `None`).
/// `["*"]` → permissif (équivalent llm-free-gateway-v2 — déconseillé).
/// Liste spécifique → `AllowOrigin::list(...)`.
///
/// Les méthodes et headers sont libéraux (GET, POST, OPTIONS / tous headers).
/// La sécurité repose sur la whitelist des origines, pas sur les headers.
pub fn build_cors_layer(allowed_origins: &[String]) -> Option<CorsLayer> {
    if allowed_origins.is_empty() {
        return None;
    }

    // Mode permissif si la liste contient uniquement "*".
    if allowed_origins == ["*"] {
        return Some(
            CorsLayer::new()
                .allow_origin(AllowOrigin::any())
                .allow_methods(AllowMethods::any())
                .allow_headers(AllowHeaders::any()),
        );
    }

    // Whitelist stricte : seules les origines listées sont autorisées.
    // Filtre les chaînes vides et les entrées non-HTTP avant le parsing HeaderValue.
    let origins: Vec<_> = allowed_origins
        .iter()
        .filter(|s| !s.is_empty() && (s.starts_with("http://") || s.starts_with("https://")))
        .filter_map(|s| s.parse::<axum::http::HeaderValue>().ok())
        .collect();

    if origins.is_empty() {
        tracing::warn!(
            "allowed_origins non vide mais aucune origine valide parsée — CORS désactivé"
        );
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
