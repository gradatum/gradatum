//! Inbound bearer authentication middleware.
//!
//! Protects all endpoints except `/health` (always public for monitoring probes).
//!
//! Behavior:
//! - `bearer_token` is `None` in `AppState` → no authentication, open/test mode.
//! - `bearer_token` is `Some(token)` → requires `Authorization: Bearer <token>` on all
//!   endpoints except `/health` and loopback connections (when `trust_localhost = true`).
//! - `/health` is ALWAYS public regardless of configuration.
//! - Loopback bypass relies on `ConnectInfo<SocketAddr>.ip().is_loopback()` — the real
//!   TCP address supplied by the kernel. HTTP headers (`X-Forwarded-For`, `X-Real-IP`) are
//!   intentionally ignored for this decision and cannot grant the bypass.
//!
//! On failure: `401 Unauthorized` with an empty body — no token information is exposed.

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;

use crate::AppState;

/// Axum middleware that validates the inbound bearer token.
///
/// Non-generic body signature — Axum 0.8 requires `Request<Body>` for `Next::run()`.
///
/// The peer address is read from the `ConnectInfo<SocketAddr>` extension injected by
/// `into_make_service_with_connect_info::<SocketAddr>()` at server startup.
/// When the extension is absent (mocked transport without a real socket), the loopback
/// bypass is denied by default (conservative security posture).
///
/// # Side effects
/// - Reads the `Authorization` header and the `ConnectInfo` extension; does not modify the request.
/// - Never logs the token (neither expected nor provided).
pub async fn bearer_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // /health is always public — bypass auth.
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    // No token configured → open mode (local / test).
    let expected = match &state.bearer_token {
        Some(t) => t.clone(), // Arc<SecretString> — clone bon marché (ref count)
        None => return Ok(next.run(req).await),
    };

    // Loopback bypass relies on the real TCP socket peer address.
    // The ConnectInfo extension is injected by into_make_service_with_connect_info.
    // SECURITY: X-Forwarded-For / X-Real-IP headers are intentionally ignored —
    // a remote client can forge them freely. Only the kernel knows the real peer address.
    if state.trust_localhost {
        let peer_ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        if let Some(ip) = peer_ip {
            if ip.is_loopback() {
                return Ok(next.run(req).await);
            }
        }
        // ConnectInfo absent (mocked transport, no real socket) → deny by default.
    }

    // Extract the token from the Authorization header.
    let provided = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_owned());

    match provided {
        // Constant-time comparison — guards against timing oracles.
        // expose_secret() in a short scope: the exposed bytes do not outlive this match arm.
        Some(token) if bool::from(token.as_bytes().ct_eq(expected.expose_secret().as_bytes())) => {
            Ok(next.run(req).await)
        }
        // Token absent or invalid → 401. Never log the provided token.
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware, routing::get, Router};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn make_app(token: Option<&str>, trust_localhost: bool) -> Router {
        use crate::config::{Config, LoggingConfig, ServerConfig};
        use crate::AppState;
        use std::collections::HashMap;

        let config = Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                registry_db: None,
                bearer_token_env: None,
                rate_limit_per_minute: 0,
                circuit_threshold: 5,
                circuit_window_secs: 60,
                circuit_cooldown_secs: 30,
                max_total_tokens: 0,
                trust_localhost,
                enable_slot_passthrough: true,
                allowed_origins: vec![],
                max_tools_per_request: 64,
            },
            logging: LoggingConfig {
                level: "error".to_string(),
            },
            providers: std::collections::BTreeMap::new(),
            aliases: HashMap::new(),
            gateway: HashMap::new(),
            vault_aware: Default::default(),
        };

        let mut state = AppState::for_test(config);
        state.bearer_token = token.map(|t| Arc::new(secrecy::SecretString::from(t.to_string())));

        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state)
    }

    fn make_state_with_trust_localhost(token: &str) -> AppState {
        use crate::config::{Config, LoggingConfig, ServerConfig};
        use crate::AppState;
        use std::collections::HashMap;

        let config = Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                registry_db: None,
                bearer_token_env: None,
                rate_limit_per_minute: 0,
                circuit_threshold: 5,
                circuit_window_secs: 60,
                circuit_cooldown_secs: 30,
                max_total_tokens: 0,
                trust_localhost: true,
                enable_slot_passthrough: true,
                allowed_origins: vec![],
                max_tools_per_request: 64,
            },
            logging: LoggingConfig {
                level: "error".to_string(),
            },
            providers: std::collections::BTreeMap::new(),
            aliases: HashMap::new(),
            gateway: HashMap::new(),
            vault_aware: Default::default(),
        };

        let mut state = AppState::for_test(config);
        state.bearer_token = Some(Arc::new(secrecy::SecretString::from(token.to_string())));
        state
    }

    #[tokio::test]
    async fn test_health_always_public_no_auth_configured() {
        let app = make_app(None, false);
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_always_public_with_auth_configured() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_no_auth_configured_allows_all() {
        let app = make_app(None, false);
        let req = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_correct_token_allows() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Bearer secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_wrong_token_returns_401() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Bearer wrongtoken")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_missing_token_returns_401() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_malformed_auth_header_returns_401() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Basic secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_token_different_length_returns_401() {
        let app = make_app(Some("secret123"), false);
        let req = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let app2 = make_app(Some("secret123"), false);
        let req2 = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Bearer secret123extra")
            .body(Body::empty())
            .unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);
    }

    /// trust_localhost=true + ConnectInfo loopback (127.0.0.1) → bypass sans bearer.
    #[tokio::test]
    async fn test_auth_bypass_when_trust_localhost_and_loopback() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware;
        use tower::ServiceExt;

        let state = make_state_with_trust_localhost("secret123");

        let app = Router::new()
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        let loopback: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let req = Request::builder()
            .uri("/v1/models")
            .extension(ConnectInfo(loopback))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// trust_localhost=true + ConnectInfo remote → 401 sans bearer.
    #[tokio::test]
    async fn test_auth_required_when_trust_localhost_and_remote_peer() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware;
        use tower::ServiceExt;

        let state = make_state_with_trust_localhost("secret123");

        let app = Router::new()
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        let remote: SocketAddr = "10.0.0.1:54321".parse().unwrap();
        let req = Request::builder()
            .uri("/v1/models")
            .extension(ConnectInfo(remote))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// trust_localhost=false + ConnectInfo loopback → 401. Bypass non actif quand false.
    #[tokio::test]
    async fn test_auth_required_when_trust_localhost_false_even_loopback() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware;
        use tower::ServiceExt;

        let mut state = {
            use crate::config::{Config, LoggingConfig, ServerConfig};
            use crate::AppState;
            use std::collections::HashMap;
            let config = Config {
                server: ServerConfig {
                    listen: "127.0.0.1:0".to_string(),
                    registry_db: None,
                    bearer_token_env: None,
                    rate_limit_per_minute: 0,
                    circuit_threshold: 5,
                    circuit_window_secs: 60,
                    circuit_cooldown_secs: 30,
                    max_total_tokens: 0,
                    trust_localhost: false,
                    enable_slot_passthrough: true,
                    allowed_origins: vec![],
                    max_tools_per_request: 64,
                },
                logging: LoggingConfig {
                    level: "error".to_string(),
                },
                providers: std::collections::BTreeMap::new(),
                aliases: HashMap::new(),
                gateway: HashMap::new(),
                vault_aware: Default::default(),
            };
            AppState::for_test(config)
        };
        state.bearer_token = Some(Arc::new(secrecy::SecretString::from(
            "secret123".to_string(),
        )));

        let app = Router::new()
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        let loopback: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let req = Request::builder()
            .uri("/v1/models")
            .extension(ConnectInfo(loopback))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// ANTI-SPOOF : peer_addr remote + header X-Forwarded-For=127.0.0.1
    /// + trust_localhost=true → 401. Le header HTTP ne peut pas accorder le bypass.
    #[tokio::test]
    async fn test_auth_spoof_header_does_not_grant_bypass() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware;
        use tower::ServiceExt;

        let state = make_state_with_trust_localhost("secret123");

        let app = Router::new()
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        let remote: SocketAddr = "10.0.0.1:54321".parse().unwrap();
        let req = Request::builder()
            .uri("/v1/models")
            .header("X-Forwarded-For", "127.0.0.1")
            .header("X-Real-IP", "127.0.0.1")
            .extension(ConnectInfo(remote))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Bearer valide + peer_addr remote → 200.
    #[tokio::test]
    async fn test_valid_bearer_with_remote_peer_allows() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware;
        use tower::ServiceExt;

        let state = make_state_with_trust_localhost("secret123");

        let app = Router::new()
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        let remote: SocketAddr = "10.0.0.1:54321".parse().unwrap();
        let req = Request::builder()
            .uri("/v1/models")
            .header("Authorization", "Bearer secret123")
            .extension(ConnectInfo(remote))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
