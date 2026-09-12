//! Fleet heartbeat ingress (F-156 P6b) — the one deliberately non-loopback surface.
//!
//! The fleet members (server, worker, gateway, engines) POST a
//! [`gradatum_dto::HeartbeatRequest`] here; the handler feeds the single write boundary
//! [`crate::registry_store::RegistryStore::register`]. Unlike the metrics/internal
//! listeners (loopback-only, where the bind *is* the access control), this listener binds a
//! **LAN address** on purpose. Its protection is therefore explicit and layered:
//!
//! 1. **Token** — a dedicated bearer (`X-Gradatum-Heartbeat`), a secret **distinct** from
//!    the internal (`:19092`) worker/admin tokens, sourced from env `GRADATUM_HEARTBEAT_TOKEN`
//!    (never in config). Enforced by `heartbeat_auth_middleware` as a `route_layer` — the
//!    check is a middleware, not an ad-hoc test inside the handler (fail-closed if no token
//!    is configured).
//! 2. **Body cap** — `HEARTBEAT_BODY_LIMIT` (8 KiB), far below the generic 512 KiB/4 MiB
//!    limits: a heartbeat is a few hundred bytes.
//! 3. **Rate limit** — a per-source token bucket, reusing [`gradatum_warden::WardenLayer`]
//!    (the same governor the public router uses), sized to tolerate the whole fleet's 15 s
//!    cadence while blocking a flood.
//! 4. **Store guards** — the allowlist, field caps and entry cap of P6a, enforced inside
//!    `register`.
//! 5. **Observability** — every rejection increments
//!    `gradatum_registry_heartbeat_rejected_total{reason}` (six bounded motives); an accepted
//!    beat increments `gradatum_registry_heartbeat_accepted_total`. A rejection is never
//!    silent.
//! 6. *(deploy-gate, not code)* an nftables allowlist restricting the source to the fleet
//!    hosts + a `pct config` firewall rule — provisioned by the operator, documented in
//!    the P6b deliverable.
//!
//! The read projection (`GET /registry`, loopback) is untouched — it stays on the metrics
//! side-channel (P6a).

use std::net::SocketAddr;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;
use tracing::{info, warn};

use gradatum_dto::HeartbeatRequest;
use gradatum_warden::{WardenConfig, WardenLayer};

use crate::metrics::HeartbeatRejectLabel;
use crate::registry_store::{Registration, RegistryError};
use crate::state::AppState;

/// Route path of the heartbeat ingress.
pub const HEARTBEAT_ROUTE: &str = "/internal/v1/registry/heartbeat";

/// Header carrying the heartbeat bearer token — distinct from `X-Gradatum-Internal`.
const AUTH_HEADER: &str = "X-Gradatum-Heartbeat";

/// Dedicated body cap for a heartbeat (8 KiB) — a heartbeat is a few hundred bytes; this is
/// far below the generic internal (4 MiB) / public (512 KiB) limits (DoS cap, ADN 5).
const HEARTBEAT_BODY_LIMIT: usize = 8 * 1024;

/// Per-source rate limit (requests/minute). Sized to tolerate the whole fleet beating at the
/// 15 s cadence from behind a single NAT'd IP (e.g. the three upstream inference engines =
/// 12/min) plus
/// retries/backoff, while blocking a flood.
const RATE_LIMIT_PER_MINUTE: u32 = 60;

/// Burst allowance for the per-source rate limit.
const RATE_LIMIT_BURST: u32 = 20;

/// Increments the rejection metric for `reason` and returns `status`.
///
/// The single choke point that guarantees a rejection is never silent (condition 5).
fn reject(state: &AppState, reason: &'static str, status: StatusCode) -> Response {
    state
        .metrics
        .registry_heartbeat_rejected
        .get_or_create(&HeartbeatRejectLabel { reason })
        .inc();
    status.into_response()
}

/// Extracts the bearer token from `X-Gradatum-Heartbeat: Bearer <token>`.
fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTH_HEADER)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Wall-clock epoch-ms, saturating (never panics on a clock before/after the epoch).
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Auth middleware — dedicated heartbeat token, constant-time compare, fail-closed.
///
/// Deliberately does **not** restrict to loopback (this is the LAN-facing surface). A request
/// with no configured token, a missing/malformed header, or a wrong token (including the
/// internal/admin token, which is a different secret) is rejected `401` and counted
/// `unauthorized`.
async fn heartbeat_auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let Some(token) = state.heartbeat_ingress_token.as_ref() else {
        warn!("heartbeat ingress: token not configured — fail-closed rejection");
        return reject(&state, "unauthorized", StatusCode::UNAUTHORIZED);
    };
    let Some(provided) = extract_bearer(req.headers()).map(str::to_owned) else {
        warn!(ip = %addr.ip(), "heartbeat ingress: {AUTH_HEADER} header absent or malformed");
        return reject(&state, "unauthorized", StatusCode::UNAUTHORIZED);
    };

    let expected = token.expose_secret().as_bytes();
    let got = provided.as_bytes();
    // ConstantTimeEq requires equal lengths; the token min-length is public-by-design
    // (validated at boot), so a length mismatch leaking timing carries no extra information.
    let ok = expected.len() == got.len() && bool::from(expected.ct_eq(got));
    if !ok {
        warn!(ip = %addr.ip(), "heartbeat ingress: invalid token");
        return reject(&state, "unauthorized", StatusCode::UNAUTHORIZED);
    }

    next.run(req).await
}

/// Outer middleware counting the rejections produced by the *layers* (not the handler):
/// body-cap `413` → `oversized_body`, rate-limit `429` → `rate_limited`.
///
/// The handler counts the semantic motives (allowlist/field/capacity) and the auth
/// middleware counts `unauthorized`; those statuses (`400`/`503`/`401`) are ignored here, so
/// no rejection is double-counted.
async fn heartbeat_reject_metrics_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let resp = next.run(req).await;
    let reason = match resp.status() {
        StatusCode::PAYLOAD_TOO_LARGE => Some("oversized_body"),
        StatusCode::TOO_MANY_REQUESTS => Some("rate_limited"),
        _ => None,
    };
    if let Some(reason) = reason {
        state
            .metrics
            .registry_heartbeat_rejected
            .get_or_create(&HeartbeatRejectLabel { reason })
            .inc();
    }
    resp
}

/// Heartbeat handler — feeds the single write boundary and records the outcome.
///
/// Returns `204 No Content` on success. On a store rejection it maps the typed
/// [`RegistryError`] to a bounded reason + status and counts it. A malformed JSON body is
/// rejected `400` by the extractor before reaching here (with a body explaining why — not
/// silent, but not one of the enumerated attack motives).
async fn handle_heartbeat(
    State(state): State<AppState>,
    Json(hb): Json<HeartbeatRequest>,
) -> Response {
    let reg = Registration {
        service: hb.service,
        version: hb.version,
        build_sha: hb.build_sha,
        pid: hb.pid,
        last_seen: Instant::now(),
        last_seen_ms: now_epoch_ms(),
    };
    match state.registry.register(reg) {
        Ok(()) => {
            state.metrics.registry_heartbeat_accepted.inc();
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            let (reason, status) = match err {
                RegistryError::UnknownService(_) => ("unknown_service", StatusCode::BAD_REQUEST),
                RegistryError::FieldTooLong { .. } => ("field_too_long", StatusCode::BAD_REQUEST),
                RegistryError::CapacityExceeded(_) => ("capacity", StatusCode::SERVICE_UNAVAILABLE),
            };
            warn!(error = %err, reason, "heartbeat ingress: rejected");
            reject(&state, reason, status)
        }
    }
}

/// Builds the per-source rate-limit layer, or `None` if it cannot be constructed.
///
/// The constants are guaranteed valid (`> 0`), so `WardenLayer::new` never fails in practice;
/// a failure is logged and rate limiting is skipped (auth + caps still protect the surface).
fn build_rate_limit_layer() -> Option<WardenLayer> {
    // `WardenConfig` is `#[non_exhaustive]`: build from `Default`, override the driven fields.
    let mut cfg = WardenConfig::default();
    cfg.enabled = true;
    cfg.rate_limit_per_minute = RATE_LIMIT_PER_MINUTE;
    cfg.rate_limit_burst = RATE_LIMIT_BURST;
    // Rate-limit every source uniformly, loopback included (the server's own 15 s beat is far
    // under the limit) — the LAN is the threat surface, so no loopback bypass.
    cfg.bypass_loopback = false;
    match WardenLayer::new(cfg) {
        Ok(layer) => Some(layer),
        Err(e) => {
            warn!(error = %e, "heartbeat ingress: rate-limit layer init failed — proceeding without it");
            None
        }
    }
}

/// Builds the heartbeat ingress router.
///
/// Layer order (outermost → innermost): reject-metrics → rate-limit → auth → body-cap →
/// handler. So a `429` (rate limit) or `413` (body cap) surfaces to the reject-metrics
/// middleware, and the flood is rejected before the token is even checked.
pub fn build_ingress_router(state: AppState) -> Router {
    let route = Router::new()
        .route(HEARTBEAT_ROUTE, post(handle_heartbeat))
        // Auth closest to the handler (a `route_layer` scoped to this route).
        .route_layer(from_fn_with_state(state.clone(), heartbeat_auth_middleware))
        // Dedicated small body cap (enforced at extraction).
        .layer(DefaultBodyLimit::max(HEARTBEAT_BODY_LIMIT));

    // Rate limit wraps (auth + handler); reject-metrics wraps everything.
    let route = match build_rate_limit_layer() {
        Some(layer) => route.layer(layer),
        None => route,
    };

    route
        .layer(from_fn_with_state(
            state.clone(),
            heartbeat_reject_metrics_middleware,
        ))
        .with_state(state)
}

/// Serves the heartbeat ingress on an already-bound listener.
///
/// The bind is performed by the caller (`main.rs`) so a bind failure aborts the boot
/// (visible — an announced capability that cannot bind must never fail silently). This
/// function only drives `axum::serve` with `ConnectInfo` (required by the rate limiter).
///
/// # Errors
///
/// Returns an error if the axum serve loop terminates with one.
pub async fn serve_ingress(
    listener: tokio::net::TcpListener,
    router: Router,
) -> anyhow::Result<()> {
    let actual = listener.local_addr()?;
    info!(addr = %actual, "fleet heartbeat ingress listening (F-156 P6b)");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("heartbeat ingress listener stopped with error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_epoch_ms_is_positive() {
        assert!(now_epoch_ms() > 0, "epoch-ms must be a positive wall clock");
    }

    #[test]
    fn extract_bearer_parses_and_rejects() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTH_HEADER, "Bearer sekret".parse().expect("valid header"));
        assert_eq!(extract_bearer(&headers), Some("sekret"));

        let mut bad = HeaderMap::new();
        bad.insert(AUTH_HEADER, "sekret".parse().expect("valid header"));
        assert_eq!(extract_bearer(&bad), None, "no 'Bearer ' prefix ⇒ None");

        assert_eq!(
            extract_bearer(&HeaderMap::new()),
            None,
            "absent header ⇒ None"
        );
    }
}
