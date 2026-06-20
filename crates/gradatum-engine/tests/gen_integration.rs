//! Tests d'intégration vague-2 — reverse-proxy transparent via routeur engine.
//!
//! Remplace les tests `ProxyBackend` (payload reconstruite) supprimés au profit du
//! forward transparent. Un child-stub axum simule `llama-server`.
//!
//! Compiler avec : `cargo test -p gradatum-engine --features test-utils`
#![cfg(feature = "test-utils")]
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use gradatum_engine::server::EngineServer;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower::ServiceExt;

async fn start_child(resp_body: &'static str) -> (u16, Arc<Mutex<Vec<u8>>>) {
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let cap = captured.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Bytes| {
            let cap = cap.clone();
            async move {
                *cap.lock().await = body.to_vec();
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(resp_body))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, captured)
}

#[tokio::test]
async fn transparent_chat_roundtrip() {
    let (port, captured) =
        start_child("{\"id\":\"x1\",\"choices\":[{\"message\":{\"content\":\"hi\"}}]}").await;
    let app =
        EngineServer::test_app_with_child(format!("http://127.0.0.1:{port}"), 30, 32 * 1024 * 1024);
    let raw = br#"{"messages":[{"role":"user","content":"hi"}],"temperature":0.0}"#;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(raw.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Body forwardé intact
    assert_eq!(captured.lock().await.clone().as_slice(), raw.as_slice());
    // Réponse child renvoyée telle quelle
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["id"], "x1");
}
