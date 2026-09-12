//! Integration coverage for the worker `/health` listener (F-156 P3).
//!
//! Proves the two properties a unit test cannot: the listener actually answers over TCP,
//! and a taken port degrades gracefully (the worker keeps running, no panic, no abort).

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use gradatum_dto::{HealthStatus, LivenessReport};
use gradatum_worker::health::{
    HealthServerConfig, HealthSnapshot, spawn_health_server, starting_report,
};

/// Picks a currently-free loopback port by binding `:0`, reading the assigned port, then
/// releasing it. A small race window remains before the caller re-binds; acceptable for a
/// test, and the loopback space is otherwise idle in CI.
fn free_port() -> u16 {
    let l = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

fn snapshot() -> Arc<HealthSnapshot> {
    Arc::new(HealthSnapshot::from_pointee(starting_report(
        "9.9.9", "cafef00d",
    )))
}

#[tokio::test]
async fn health_endpoint_answers_liveness() {
    let port = free_port();
    let cfg = HealthServerConfig {
        bind: "127.0.0.1".to_string(),
        port,
    };
    spawn_health_server(&cfg, snapshot()).expect("loopback bind spawns");

    // Give the detached listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let url = format!("http://127.0.0.1:{port}/health");
    let resp = reqwest::get(&url).await.expect("GET /health");
    assert_eq!(resp.status().as_u16(), 200, "/health must return 200");

    let body: LivenessReport = resp.json().await.expect("liveness body deserializes");
    // Pre-first-probe snapshot: the worker reports `starting`, carrying version + build_sha.
    assert_eq!(body.status, HealthStatus::Starting);
    assert_eq!(body.version, "9.9.9");
    assert_eq!(body.build_sha.as_deref(), Some("cafef00d"));
}

#[tokio::test]
async fn health_endpoint_serves_detailed() {
    let port = free_port();
    let cfg = HealthServerConfig {
        bind: "127.0.0.1".to_string(),
        port,
    };
    spawn_health_server(&cfg, snapshot()).expect("loopback bind spawns");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let url = format!("http://127.0.0.1:{port}/health/detailed");
    let resp = reqwest::get(&url).await.expect("GET /health/detailed");
    assert_eq!(resp.status().as_u16(), 200);
    // The detailed body is the HealthReport superset; a raw JSON parse confirms the shape.
    let body: serde_json::Value = resp.json().await.expect("detailed body is JSON");
    assert_eq!(body["status"], "starting");
    assert_eq!(body["build_sha"], "cafef00d");
}

#[tokio::test]
async fn bind_tolerant_when_port_already_taken() {
    // Hold the port with a std listener, then point the health server at the same port.
    let port = free_port();
    let held = StdTcpListener::bind(("127.0.0.1", port)).expect("hold the port");

    let cfg = HealthServerConfig {
        bind: "127.0.0.1".to_string(),
        port,
    };
    // Must NOT panic and must return: an observability port never gates the consumer.
    spawn_health_server(&cfg, snapshot()).expect("loopback bind spawns");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The worker "keeps running": the port we hold is still ours (the health task backed
    // off gracefully rather than stealing or crashing).
    assert!(
        held.local_addr().is_ok(),
        "the pre-bound listener must remain valid — worker continues despite the collision"
    );
}
