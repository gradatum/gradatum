//! Middleware d'authentification pour l'API interne.
//!
//! ## Double garde
//!
//! 1. `remote_addr` doit être loopback IPv4 (127.x.x.x) ou IPv6 (::1).
//! 2. Header `X-Gradatum-Internal: Bearer <token>` — comparaison constant-time.
//!
//! ## ADN 5 / ANSSI R23
//!
//! Token wrappé `SecretString`, jamais loggué, comparaison via `subtle::ConstantTimeEq`.

use std::net::IpAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::state::AppState;

/// Middleware vérifiant l'authentification de l'API interne.
///
/// Refuse toute requête dont l'adresse source n'est pas loopback OU dont le
/// header `X-Gradatum-Internal` ne correspond pas au token configuré.
///
/// ## Comportement
///
/// Le token est extrait de `AppState.internal_api_token`. Si absent (listener
/// désactivé), toute requête est rejetée avec 401.
///
/// Jamais de log du token — uniquement un log de l'IP en cas de rejet.
pub(crate) async fn internal_auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // Garde 0 : token configuré (protection fail-closed si router spawné sans token).
    let token = match &state.internal_api_token {
        Some(t) => t.clone(),
        None => {
            warn!("internal API: token not configured — fail-closed rejection");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Garde 1 : loopback uniquement.
    let is_loopback = match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    };

    if !is_loopback {
        warn!(ip = %addr.ip(), "internal API: rejecting non-loopback address");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Garde 2 : token constant-time, extrait des headers de la requête.
    let provided = match extract_bearer_token(req.headers()) {
        Some(t) => t.to_string(), // clone nécessaire avant move de req
        None => {
            warn!("internal API: X-Gradatum-Internal header absent or malformed");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let expected_bytes = token.expose_secret().as_bytes();
    let provided_bytes = provided.as_bytes();

    // Comparer en temps constant.
    // subtle::ConstantTimeEq n'est défini que pour des slices de même longueur.
    //
    // ## Longueur publique-par-design (V5 mitigation leak-longueur)
    //
    // Si les longueurs diffèrent, rejet immédiat (timing différent du cas ct_eq).
    // Cette différence de timing est acceptable car la longueur minimale du token
    // (MIN_INTERNAL_TOKEN_LEN = 32) est documentée publiquement dans la config
    // et validée au boot (validate_internal_token dans main.rs).
    // Un attaquant connaît donc la longueur cible — le timing sur longueur n'apporte
    // pas d'info supplémentaire. La comparaison ct_eq protège le contenu exact.
    let lengths_match = expected_bytes.len() == provided_bytes.len();
    let bytes_match = if lengths_match {
        bool::from(expected_bytes.ct_eq(provided_bytes))
    } else {
        false
    };

    if !bytes_match {
        warn!("internal API: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await
}

/// Extrait le token depuis `X-Gradatum-Internal: Bearer <token>`.
///
/// Retourne `None` si le header est absent, malformé ou ne commence pas par `Bearer `.
fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let val = headers.get("X-Gradatum-Internal")?.to_str().ok()?;
    val.strip_prefix("Bearer ")
}
