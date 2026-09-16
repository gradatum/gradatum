//! F-296 reliquat 1 — the inert `pattern` filter of `vault_list` is removed.
//!
//! Wire-contract test: a `POST /api/v1/vault_list` body still carrying `pattern` must be
//! rejected at deserialization by `#[serde(deny_unknown_fields)]`. Because the JSON is
//! syntactically valid and only the field is unknown, serde raises a *data* error, which
//! axum 0.8 maps to **422 Unprocessable Entity** (never 400 — that status is reserved for
//! JSON *syntax* errors). The rejection body carries the field name.
//!
//! # Test server
//!
//! A minimal Axum server is started on an ephemeral port with `AppState::default()` (empty
//! ACL, default-deny) and the same trust-extraction stub as the production wiring. The
//! deserialization rejection happens in the `Json` extractor, before the handler body and
//! therefore before the ACL gate — so the removed-field rejection is observable regardless
//! of authorization outcome.

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_server::state::AppState;
use reqwest::StatusCode;

/// Starts an Axum test server on an ephemeral port and returns its address.
async fn start_test_server() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::api_v1;

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        let trust = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .map(|token| TrustContext::BearerToken {
                kid: "test-kid".to_string(),
                aud: "gradatum".to_string(),
                sub: token.into(),
                scopes: vec!["read".to_string()],
                tenant_id: "main".into(),
                jti: None,
            })
            .unwrap_or(TrustContext::Unauthenticated);
        req.extensions_mut().insert(trust);
        next.run(req).await
    }

    let state = AppState::default();
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port — must succeed on localhost");
    let addr = listener
        .local_addr()
        .expect("obtain local address — listener is active");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server stopped cleanly");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("HTTP client build — no custom TLS")
}

/// A body carrying the removed `pattern` field is rejected at deserialization (422),
/// before the handler and its ACL gate, and the body names the unknown field.
#[tokio::test]
async fn vault_list_with_removed_pattern_field_is_rejected_422() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_list"))
        .bearer_auth("test-token-stub")
        .json(&serde_json::json!({ "pattern": "x" }))
        .send()
        .await
        .expect("vault_list request carrying the removed `pattern` field");
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown field must be rejected at deserialization (422), not accepted"
    );
    let body = resp.text().await.expect("response body");
    assert!(
        body.contains("pattern"),
        "the rejection body names the unknown field (deny_unknown_fields message) — got: {body}"
    );
}
