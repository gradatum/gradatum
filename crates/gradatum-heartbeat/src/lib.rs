//! Fire-and-forget fleet heartbeat client.
//!
//! Every binary of the fleet (`gradatum-server`, `gradatum-worker`, `gradatum-gateway`,
//! and each `gradatum-engine`) uses this crate to announce its liveness to the registry
//! ingress with a periodic POST of a [`gradatum_dto::HeartbeatRequest`].
//!
//! ## Contract — a heartbeat NEVER degrades the service it reports on
//!
//! The whole client runs in a detached tokio task ([`spawn`]). It is:
//!
//! - **fire-and-forget**: the caller never awaits it; errors are never propagated;
//! - **non-blocking**: a slow or unreachable ingress cannot stall the caller — each POST
//!   carries a short timeout (`REQUEST_TIMEOUT`) and runs off the caller's path;
//! - **panic-free**: no `unwrap`/`expect`/`panic` on the network path;
//! - **backoff-bounded**: consecutive failures widen the interval up to `MAX_BACKOFF`,
//!   so a down ingress is never hammered (precedent: the buzz-acp restart loop);
//! - **log-throttled**: a persistent outage logs once at onset and then only on state
//!   transitions, not on every missed beat.
//!
//! ## Opt-in by environment (never hardcoded)
//!
//! [`spawn_from_env`] reads the target URL and the bearer token from the environment
//! (`GRADATUM_HEARTBEAT_URL`, `GRADATUM_HEARTBEAT_TOKEN`), mirroring the worker's existing
//! `GRADATUM_INTERNAL_*` convention. The URL absent ⇒ no heartbeat, no error (opt-in). The
//! URL set but the token missing ⇒ fail-safe: the client refuses to emit an unauthenticated
//! heartbeat (logs a warning, emits nothing).

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use gradatum_dto::HeartbeatRequest;

/// Environment variable carrying the heartbeat ingress URL (opt-in switch).
pub const ENV_URL: &str = "GRADATUM_HEARTBEAT_URL";
/// Environment variable carrying the heartbeat bearer token.
pub const ENV_TOKEN: &str = "GRADATUM_HEARTBEAT_TOKEN";

/// Header carrying the bearer token, distinct from the internal API header.
const AUTH_HEADER: &str = "X-Gradatum-Heartbeat";

/// Default heartbeat cadence: 15 s (≪ the 45 s registry TTL — tolerates two missed beats).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);

/// Per-request timeout — a stuck ingress can never stall the beat loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the failure backoff (defense against hammering a down ingress).
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Static parameters of one heartbeat producer.
///
/// `service` MUST match one of the registry's allowlisted names exactly
/// (`server` | `worker` | `gateway` | `engine-agent-main` | `engine-curator` |
/// `engine-embed`) — any other value is silently rejected `UnknownService` at the store.
pub struct HeartbeatConfig {
    /// Target ingress URL (full path, e.g. `http://192.0.2.10:19095/internal/v1/registry/heartbeat`).
    pub url: String,
    /// Bearer token (scope: heartbeat). Wrapped, never logged.
    pub token: SecretString,
    /// Fleet service name — an exact allowlist match (see the struct docs).
    pub service: String,
    /// Binary version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Build commit SHA (public), or `None`.
    pub build_sha: Option<String>,
    /// Heartbeat cadence.
    pub interval: Duration,
}

/// Maps a compile-time `BUILD_SHA` string to `Option<String>` (empty ⇒ `None`).
///
/// Shared by the four binaries so the "empty means unknown" rule lives in one place.
#[must_use]
pub fn sha_opt(sha: &str) -> Option<String> {
    let trimmed = sha.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Spawns a heartbeat producer from the environment, or returns `None` when disabled.
///
/// - `GRADATUM_HEARTBEAT_URL` absent/empty ⇒ `None` (opt-in: no task, no error).
/// - URL set but `GRADATUM_HEARTBEAT_TOKEN` absent/empty ⇒ `None` + a warning (fail-safe:
///   never emit an unauthenticated heartbeat).
/// - both present ⇒ `Some(handle)` for the detached beat loop.
#[must_use]
pub fn spawn_from_env(
    service: impl Into<String>,
    version: impl Into<String>,
    build_sha: Option<String>,
) -> Option<JoinHandle<()>> {
    let url = std::env::var(ENV_URL)
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let token = match std::env::var(ENV_TOKEN) {
        Ok(t) if !t.trim().is_empty() => SecretString::from(t),
        _ => {
            warn!(
                "{ENV_URL} is set but {ENV_TOKEN} is missing/empty — fleet heartbeat disabled \
                 (fail-safe: never send an unauthenticated heartbeat)"
            );
            return None;
        }
    };
    Some(spawn(HeartbeatConfig {
        url,
        token,
        service: service.into(),
        version: version.into(),
        build_sha,
        interval: DEFAULT_INTERVAL,
    }))
}

/// Spawns the detached beat loop for an explicit configuration.
///
/// Never blocks the caller and never returns an error — a client build failure is logged
/// and the task exits (the service keeps running without a heartbeat).
#[must_use]
pub fn spawn(cfg: HeartbeatConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "fleet heartbeat: HTTP client build failed — heartbeat disabled");
                return;
            }
        };
        info!(
            service = %cfg.service,
            url = %cfg.url,
            interval_secs = cfg.interval.as_secs(),
            "fleet heartbeat producer started"
        );
        run_loop(&client, &cfg).await;
    })
}

/// The beat loop: send, then sleep `interval` on success or a bounded backoff on failure.
async fn run_loop(client: &reqwest::Client, cfg: &HeartbeatConfig) {
    let mut consecutive_failures: u32 = 0;
    loop {
        match send_once(client, cfg).await {
            Ok(()) => {
                // Log the RECOVERY transition only (nominal beats stay at debug).
                if consecutive_failures > 0 {
                    info!(
                        service = %cfg.service,
                        after_failures = consecutive_failures,
                        "fleet heartbeat: ingress reachable again"
                    );
                } else {
                    debug!(service = %cfg.service, "fleet heartbeat sent");
                }
                consecutive_failures = 0;
                tokio::time::sleep(cfg.interval).await;
            }
            Err(reason) => {
                // Log the ONSET transition only (throttle a persistent outage).
                if consecutive_failures == 0 {
                    warn!(
                        service = %cfg.service,
                        url = %cfg.url,
                        error = %reason,
                        "fleet heartbeat: ingress unreachable — backing off (subsequent failures throttled)"
                    );
                }
                consecutive_failures = consecutive_failures.saturating_add(1);
                tokio::time::sleep(backoff(cfg.interval, consecutive_failures)).await;
            }
        }
    }
}

/// Exponential backoff on top of the base interval, clamped to [`MAX_BACKOFF`].
fn backoff(interval: Duration, failures: u32) -> Duration {
    // 2^(failures-1) as a multiplier, saturating; clamp to MAX_BACKOFF.
    let shift = failures.saturating_sub(1).min(16);
    let mult = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let scaled = interval.checked_mul(u32::try_from(mult).unwrap_or(u32::MAX));
    scaled.unwrap_or(MAX_BACKOFF).min(MAX_BACKOFF)
}

/// Sends one heartbeat. Returns `Err(String)` (a display reason) on any failure — never panics.
async fn send_once(client: &reqwest::Client, cfg: &HeartbeatConfig) -> Result<(), String> {
    let body = HeartbeatRequest::new(
        cfg.service.clone(),
        cfg.version.clone(),
        cfg.build_sha.clone(),
        std::process::id(),
    );
    let resp = client
        .post(&cfg.url)
        .header(AUTH_HEADER, format!("Bearer {}", cfg.token.expose_secret()))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        // The body may carry a rejection reason; the token is never in the request log.
        Err(format!("ingress returned HTTP {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_opt_maps_empty_to_none() {
        assert_eq!(sha_opt(""), None);
        assert_eq!(sha_opt("   "), None);
        assert_eq!(sha_opt("deadbeef"), Some("deadbeef".to_owned()));
        assert_eq!(sha_opt("  abc123  "), Some("abc123".to_owned()));
    }

    #[test]
    fn backoff_is_bounded_and_grows() {
        let base = Duration::from_secs(15);
        // First failure: base interval, no amplification.
        assert_eq!(backoff(base, 1), base);
        // Grows with failures.
        assert!(backoff(base, 3) > backoff(base, 1));
        // Never exceeds the cap, even for a huge failure count.
        assert!(backoff(base, u32::MAX) <= MAX_BACKOFF);
        assert!(backoff(base, 100) <= MAX_BACKOFF);
    }

    /// An unreachable ingress must yield `Err` — never a panic, never a hang — so the beat
    /// loop can back off instead of taking down the caller (fire-and-forget contract).
    #[tokio::test]
    async fn send_once_errors_without_panic_when_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("client builds");
        let cfg = HeartbeatConfig {
            // Port 1 is not listening → connection refused, fast.
            url: "http://127.0.0.1:1/internal/v1/registry/heartbeat".to_owned(),
            token: SecretString::from("test-token".to_owned()),
            service: "worker".to_owned(),
            version: "2.3.0".to_owned(),
            build_sha: None,
            interval: DEFAULT_INTERVAL,
        };
        let result = send_once(&client, &cfg).await;
        assert!(
            result.is_err(),
            "an unreachable ingress must return Err, not panic"
        );
    }

    /// A build with no configured target must produce no task (opt-in) — verified through
    /// the public helper with both env vars cleared.
    #[tokio::test]
    async fn spawn_from_env_is_none_without_url() {
        // SAFETY of the test: these two vars are owned by this crate and only read here.
        unsafe {
            std::env::remove_var(ENV_URL);
            std::env::remove_var(ENV_TOKEN);
        }
        assert!(spawn_from_env("worker", "2.3.0", None).is_none());
    }
}
