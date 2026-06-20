//! Warm-up state and backend self-reporting for the supervisor.
//!
//! [`HealthState`] is thread-safe via an atomic — clonable via `Arc`.
//! The `/health` endpoint exposes a [`HealthSnapshot`] JSON.
//!
//! ## Backend self-report
//!
//! With the `llama-server` supervisor, the "compiled backend" is no longer fixed at
//! Rust binary build time — it depends on the spawned `llama-server` subprocess.
//! The `backend` field returns `"llama-server"` (the actual runtime identifier).
//!
//! ## States
//!
//! - `starting`: the supervisor is waiting for `llama-server` to be ready.
//! - `ok`: `llama-server` responded HTTP 200 on `/health`.
//! - `unhealthy`: restart budget exhausted — the gateway fallback takes over.
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

/// JSON snapshot exposed by `/health`.
#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    /// `"ok"` when ready, `"starting"` during startup, `"unhealthy"` when the restart budget is exhausted.
    pub status: &'static str,
    /// Alias of the loaded model.
    pub model: String,
    /// Backend runtime — `"llama-server"`.
    pub backend: &'static str,
    /// Warm-up state: `"loading"`, `"ready"`, or `"unhealthy"`.
    pub warm_up_state: &'static str,
}

/// Internal states encoded as `u8` for the atomic.
mod state {
    pub const STARTING: u8 = 0;
    pub const READY: u8 = 1;
    pub const UNHEALTHY: u8 = 2;
}

/// Warm-up state shared between the supervisor and the handlers.
pub struct HealthState {
    model: String,
    state: AtomicU8,
}

/// Returns the supervised runtime backend identifier.
///
/// Returns `"llama-server"` (the native llama.cpp binary being supervised).
/// Distinct from Ollama — this is the raw `llama-server` binary from llama.cpp.
pub const fn compiled_backend() -> &'static str {
    "llama-server"
}

impl HealthState {
    /// Creates a new warm-up state for the given `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.into(),
            state: AtomicU8::new(state::STARTING),
        }
    }

    /// Transitions to the `ready` state — called after `llama-server` responds HTTP 200
    /// on its `/health` endpoint.
    pub fn set_ready(&self) {
        self.state.store(state::READY, Ordering::SeqCst);
    }

    /// Transitions to the `unhealthy` state — called when the restart budget is exhausted.
    ///
    /// The gateway detects this state and switches to its fallback.
    pub fn set_unhealthy(&self) {
        self.state.store(state::UNHEALTHY, Ordering::SeqCst);
    }

    /// Returns `true` if the state is `ready`.
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::SeqCst) == state::READY
    }

    /// Returns a snapshot of the current state.
    pub fn snapshot(&self) -> HealthSnapshot {
        let s = self.state.load(Ordering::SeqCst);
        let (status, warm_up_state) = match s {
            state::READY => ("ok", "ready"),
            state::UNHEALTHY => ("unhealthy", "unhealthy"),
            _ => ("starting", "loading"),
        };
        HealthSnapshot {
            status,
            model: self.model.clone(),
            backend: compiled_backend(),
            warm_up_state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_transitions_warmup() {
        let h = HealthState::new("qwen3-4b");
        assert_eq!(h.snapshot().warm_up_state, "loading");
        h.set_ready();
        let s = h.snapshot();
        assert_eq!(s.warm_up_state, "ready");
        assert_eq!(s.model, "qwen3-4b");
    }

    #[test]
    fn health_reports_llama_server_backend() {
        // PIVOT v2 : backend = "llama-server" (plus de feature compile-time)
        assert_eq!(compiled_backend(), "llama-server");
        assert_eq!(HealthState::new("x").snapshot().backend, "llama-server");
    }

    #[test]
    fn health_status_field() {
        let h = HealthState::new("test");
        assert_eq!(h.snapshot().status, "starting");
        h.set_ready();
        assert_eq!(h.snapshot().status, "ok");
    }

    #[test]
    fn health_unhealthy_state() {
        let h = HealthState::new("test");
        h.set_unhealthy();
        let s = h.snapshot();
        assert_eq!(s.status, "unhealthy");
        assert_eq!(s.warm_up_state, "unhealthy");
        assert!(!h.is_ready(), "unhealthy ≠ ready");
    }

    #[test]
    fn health_is_ready_only_when_ready() {
        let h = HealthState::new("test");
        assert!(!h.is_ready(), "starting n'est pas ready");
        h.set_ready();
        assert!(h.is_ready(), "ready est ready");
        h.set_unhealthy();
        assert!(!h.is_ready(), "unhealthy n'est pas ready");
    }
}
