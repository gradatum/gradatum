//! Middleware d'authentification pour l'API admin (F-100 incrément 1.6).
//!
//! ## Double garde — distincte du worker
//!
//! 1. `remote_addr` doit être loopback IPv4 (127.x.x.x) ou IPv6 (::1).
//! 2. Header `X-Gradatum-Admin: Bearer <token>` — comparaison constant-time contre
//!    `AppState.admin_api_token` (token **distinct** du token worker).
//!
//! ## Séparation des rôles (invariant fondateur F-100)
//!
//! Les endpoints `/internal/v1/admin/*` (delete / restore / purge) sont gardés par CE
//! middleware, sur le même listener loopback que l'API worker mais avec un token
//! séparé. Le worker ne détient que le token worker → il ne peut PAS atteindre la
//! surface de mutation admin. La destruction/archivage ne passe jamais par la main des
//! agents ni du worker : uniquement l'opérateur (CLI, ce token) ou le GC (interne).
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

/// Middleware vérifiant l'authentification de l'API admin.
///
/// Refuse toute requête dont l'adresse source n'est pas loopback OU dont le header
/// `X-Gradatum-Admin` ne correspond pas au token admin configuré. Si le token admin
/// est absent (`None`), toute requête est rejetée avec 401 (fail-closed).
///
/// Jamais de log du token — uniquement un log de l'IP en cas de rejet.
pub(crate) async fn admin_auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // Garde 0 : token admin configuré (fail-closed si absent).
    let token = match &state.admin_api_token {
        Some(t) => t.clone(),
        None => {
            warn!("admin API: token not configured — fail-closed rejection");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Garde 1 : loopback uniquement.
    let is_loopback = match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    };
    if !is_loopback {
        warn!(ip = %addr.ip(), "API admin : rejet adresse non-loopback");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Garde 2 : token constant-time, extrait du header `X-Gradatum-Admin`.
    let provided = match extract_bearer_token(req.headers()) {
        Some(t) => t.to_string(),
        None => {
            warn!("admin API: X-Gradatum-Admin header absent or malformed");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let expected_bytes = token.expose_secret().as_bytes();
    let provided_bytes = provided.as_bytes();

    // Longueur publique-par-design (identique à l'API worker) : rejet si longueurs
    // diffèrent, ct_eq sinon — protège le contenu exact du token.
    let lengths_match = expected_bytes.len() == provided_bytes.len();
    let bytes_match = if lengths_match {
        bool::from(expected_bytes.ct_eq(provided_bytes))
    } else {
        false
    };

    if !bytes_match {
        warn!("admin API: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await
}

/// Extrait le token depuis `X-Gradatum-Admin: Bearer <token>`.
///
/// Retourne `None` si le header est absent, malformé ou ne commence pas par `Bearer `.
fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let val = headers.get("X-Gradatum-Admin")?.to_str().ok()?;
    val.strip_prefix("Bearer ")
}
