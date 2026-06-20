//! Per-provider circuit breaker — inline vendored.
//!
//! Original source: private shared library (`circuit_breaker` module).
//! Adapted for gradatum-gateway: utoipa annotations removed.
//!
//! ## States
//!
//! - **Closed** (normal): all requests pass through; failures accumulate.
//! - **Open** (tripped): all requests are rejected immediately.
//!   After `cooldown`, transitions to `HalfOpen`.
//! - **HalfOpen** (probing): a single test request is allowed.
//!   Success → `Closed`; failure → back to `Open`.
//!
//! ## Failure policy
//!
//! Only transient provider-side errors count: `Network`, `Timeout`,
//! `RateLimited`, `UpstreamError`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::commons::error::LlmError;

/// Current state of the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Circuit breaker configuration.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    pub threshold: u32,
    pub window: Duration,
    pub cooldown: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            threshold: 5,
            window: Duration::from_secs(60),
            cooldown: Duration::from_secs(30),
        }
    }
}

impl CircuitBreakerConfig {
    pub fn aggressive() -> Self {
        Self {
            threshold: 3,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: CircuitState,
    failure_timestamps: Vec<Instant>,
    opened_at: Option<Instant>,
    half_open_probe_in_flight: bool,
}

/// Per-provider circuit breaker.
///
/// Typically shared across threads via `Arc<CircuitBreaker>`.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    /// Builds a circuit breaker with the given configuration. Initial state: `Closed`.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                failure_timestamps: Vec::new(),
                opened_at: None,
                half_open_probe_in_flight: false,
            }),
        }
    }

    /// Returns the current state of the circuit breaker.
    pub fn state(&self) -> CircuitState {
        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        self.evaluate_state_transition(&mut inner);
        inner.state
    }

    /// Returns `true` if a request should be allowed through.
    pub fn should_allow(&self) -> bool {
        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        self.evaluate_state_transition(&mut inner);

        match inner.state {
            CircuitState::Closed => true,
            CircuitState::Open => false,
            CircuitState::HalfOpen => {
                if inner.half_open_probe_in_flight {
                    false
                } else {
                    inner.half_open_probe_in_flight = true;
                    true
                }
            }
        }
    }

    /// Records a successful request.
    pub fn record_success(&self) {
        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        match inner.state {
            CircuitState::Closed => {
                inner.failure_timestamps.clear();
            }
            CircuitState::HalfOpen => {
                inner.state = CircuitState::Closed;
                inner.failure_timestamps.clear();
                inner.opened_at = None;
                inner.half_open_probe_in_flight = false;
            }
            CircuitState::Open => {}
        }
    }

    /// Records a failure and adjusts the state accordingly.
    pub fn record_failure(&self, error: &LlmError) {
        if !Self::counts_as_circuit_failure(error) {
            return;
        }

        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        let now = Instant::now();

        match inner.state {
            CircuitState::Closed => {
                inner.failure_timestamps.push(now);
                self.evict_stale_failures(&mut inner, now);

                if inner.failure_timestamps.len() >= self.config.threshold as usize {
                    inner.state = CircuitState::Open;
                    inner.opened_at = Some(now);
                    inner.failure_timestamps.clear();
                }
            }
            CircuitState::HalfOpen => {
                inner.state = CircuitState::Open;
                inner.opened_at = Some(now);
                inner.half_open_probe_in_flight = false;
                inner.failure_timestamps.clear();
            }
            CircuitState::Open => {
                inner.opened_at = Some(now);
            }
        }
    }

    /// Returns the number of active failures within the current window.
    pub fn failure_count(&self) -> usize {
        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        let now = Instant::now();
        self.evict_stale_failures(&mut inner, now);
        inner.failure_timestamps.len()
    }

    fn counts_as_circuit_failure(error: &LlmError) -> bool {
        matches!(
            error,
            LlmError::Network { .. }
                | LlmError::Timeout { .. }
                | LlmError::RateLimited { .. }
                | LlmError::UpstreamError { .. }
        )
    }

    fn evaluate_state_transition(&self, inner: &mut Inner) {
        if inner.state == CircuitState::Open
            && let Some(opened_at) = inner.opened_at
            && opened_at.elapsed() >= self.config.cooldown
        {
            inner.state = CircuitState::HalfOpen;
            inner.half_open_probe_in_flight = false;
        }
    }

    fn evict_stale_failures(&self, inner: &mut Inner, now: Instant) {
        inner
            .failure_timestamps
            .retain(|&ts| now.duration_since(ts) < self.config.window);
    }
}

// ---------------------------------------------------------------------------
// Registry per-provider
// ---------------------------------------------------------------------------

/// Provider identifier type alias.
pub type ProviderId = String;

/// Registry of per-provider circuit breakers.
///
/// Thread-safe via `Mutex<HashMap<...>>`.
#[derive(Debug, Default)]
pub struct CircuitBreakerRegistry {
    breakers: Mutex<HashMap<ProviderId, CircuitBreaker>>,
    config: CircuitBreakerConfig,
}

impl CircuitBreakerRegistry {
    /// Builds an empty registry with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            breakers: Mutex::new(HashMap::new()),
            config,
        }
    }

    pub fn should_allow(&self, provider_id: &str) -> bool {
        let mut map = self.breakers.lock().expect("registry mutex poisoned");
        let cb = map
            .entry(provider_id.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.config));
        cb.should_allow()
    }

    pub fn record_success(&self, provider_id: &str) {
        let mut map = self.breakers.lock().expect("registry mutex poisoned");
        let cb = map
            .entry(provider_id.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.config));
        cb.record_success();
    }

    pub fn record_failure(&self, provider_id: &str, error: &LlmError) {
        let mut map = self.breakers.lock().expect("registry mutex poisoned");
        let cb = map
            .entry(provider_id.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.config));
        cb.record_failure(error);
    }

    pub fn state(&self, provider_id: &str) -> CircuitState {
        let mut map = self.breakers.lock().expect("registry mutex poisoned");
        match map.get_mut(provider_id) {
            Some(cb) => cb.state(),
            None => CircuitState::Closed,
        }
    }
}
