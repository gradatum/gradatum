//! Handler for `GET /health` — the gateway's public liveness surface.
//!
//! Returns the shared [`LivenessReport`] (`status`, `version`, `build_sha`) flattened at the
//! top level, plus the list of configured provider *names*. The liveness shape is the single
//! source of truth defined in `gradatum_dto::health`, so a fleet-check reads a uniform
//! `status`/`version`/`build_sha` across every gradatum `/health` surface (server, worker,
//! engine, gateway).
//!
//! ## Status
//!
//! The gateway is an LLM proxy; it performs **no backend probing** on the request path (it
//! never blocks nor fails `/health`). It therefore reports [`HealthStatus::Ok`] whenever the
//! process is serving — the honest liveness verdict for a surface that answers `200` while
//! it is up. Deriving `degraded`/`unhealthy` from provider circuit-breaker state is a
//! deliberate non-goal here (out of P4 scope); the DTO enum is adopted so that a later phase
//! can widen the produced value without a wire break.
//!
//! ## Disclosure
//!
//! Only provider **names** are exposed — never a provider URL, endpoint, IP, or secret. The
//! `build_sha` is a public commit identifier (fleet-check parity, arbitrage 17/08), not
//! sensitive data.
//!
//! Response:
//! ```json
//! { "status": "ok", "version": "2.3.0", "build_sha": "5c7060fd", "providers": ["my-provider"] }
//! ```

use axum::{Json, extract::State};
use gradatum_dto::{HealthStatus, LivenessReport};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Response body for the gateway health endpoint.
///
/// The liveness triple (`status`, `version`, `build_sha`) is the shared [`LivenessReport`],
/// `#[serde(flatten)]`-ed so its keys sit at the top level of the body. `providers` is the
/// gateway-specific addition, preserved for backward compatibility with the `2.2.0` body.
#[derive(Serialize)]
pub struct HealthResponse {
    /// Shared liveness block (`status` + `version` + optional public `build_sha`).
    #[serde(flatten)]
    pub liveness: LivenessReport,
    /// Names of the configured providers (no URL, endpoint, IP, or secret).
    pub providers: Vec<String>,
}

/// Handler for `GET /health`.
///
/// Returns 200 with the shared liveness block plus the configured provider names. `status`
/// is [`HealthStatus::Ok`] whenever the process is serving — the gateway performs no backend
/// probing on the request path.
///
/// `build_sha` is injected at compile time by `build.rs` (`cargo:rustc-env=BUILD_SHA`) and
/// is always present: it falls back to the literal `"unknown"` for a build outside a git
/// checkout, matching the other gradatum producers.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let liveness = LivenessReport::new(
        HealthStatus::Ok,
        env!("CARGO_PKG_VERSION").to_string(),
        // Aligned with `gradatum-server`: always `Some`, value `"unknown"` when git is absent.
        Some(env!("BUILD_SHA").to_string()),
    );
    Json(HealthResponse {
        liveness,
        providers: state.config.provider_names(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the response body exactly as the handler does, for pure wire-shape assertions
    /// (no `AppState` needed). The handler wiring itself is covered end-to-end in
    /// `tests/integration.rs::health_returns_ok`.
    fn sample_body(providers: Vec<String>) -> HealthResponse {
        HealthResponse {
            liveness: LivenessReport::new(
                HealthStatus::Ok,
                env!("CARGO_PKG_VERSION").to_string(),
                Some(env!("BUILD_SHA").to_string()),
            ),
            providers,
        }
    }

    #[test]
    fn status_serializes_via_dto_enum() {
        // The `status` field is the DTO `HealthStatus`, whose `Ok` variant is the stable
        // lowercase wire value `"ok"` — the value P1..P3 consumers already expect.
        let json = serde_json::to_value(sample_body(vec![])).unwrap();
        assert_eq!(json["status"].as_str(), Some("ok"));
    }

    #[test]
    fn body_round_trips_into_shared_liveness_report() {
        // Proof the gateway body IS the shared contract: the serialized body deserializes
        // back into the DTO `LivenessReport` (extra `providers` key ignored by serde).
        let json = serde_json::to_string(&sample_body(vec!["p".to_string()])).unwrap();
        let live: LivenessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(live.status, HealthStatus::Ok);
        assert_eq!(live.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn build_sha_present_and_public() {
        // `build_sha` is always emitted (fleet-check parity). It equals the compile-time
        // `BUILD_SHA` — a real short sha in a git build, `"unknown"` as the robust fallback.
        let json = serde_json::to_value(sample_body(vec![])).unwrap();
        let sha = json["build_sha"]
            .as_str()
            .expect("build_sha must be present");
        assert!(!sha.is_empty(), "build_sha is never an empty string");
        assert_eq!(sha, env!("BUILD_SHA"));
    }

    #[test]
    fn retro_compat_version_and_providers_preserved() {
        // The 2.2.0 keys `version` and `providers` remain, only `build_sha` is additive.
        let json = serde_json::to_value(sample_body(vec!["prov-a".to_string()])).unwrap();
        assert_eq!(json["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
        let providers = json["providers"].as_array().expect("providers array");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].as_str(), Some("prov-a"));
    }
}
