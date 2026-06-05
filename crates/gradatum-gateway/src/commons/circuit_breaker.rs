//! Circuit breaker per-provider — vendoring inline.
//!
//! Source originale : bibliothèque partagée privée (module circuit_breaker).
//! Adapté pour gradatum-gateway : annotations utoipa retirées.
//!
//! ## États
//!
//! - **Closed** (normal) : toutes les requêtes passent, les échecs s'accumulent.
//! - **Open** (coupé) : toutes les requêtes sont rejetées immédiatement.
//!   Après `cooldown`, passe en HalfOpen.
//! - **HalfOpen** (sondage) : une seule requête de test est autorisée.
//!   Succès → Closed, échec → retour Open.
//!
//! ## Politique d'échec
//!
//! Seules les erreurs temporaires côté provider comptent : Network, Timeout,
//! RateLimited, UpstreamError.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::commons::error::LlmError;

/// État courant du circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Configuration du circuit breaker.
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

/// Circuit breaker per-provider.
///
/// Usage typique via `Arc<CircuitBreaker>` pour partage entre threads.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    /// Construit un circuit breaker avec la configuration donnée. État initial : `Closed`.
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

    /// Retourne l'état courant du circuit breaker.
    pub fn state(&self) -> CircuitState {
        let mut inner = self
            .inner
            .lock()
            .expect("circuit breaker mutex poisoned — process should restart");
        self.evaluate_state_transition(&mut inner);
        inner.state
    }

    /// Retourne `true` si une requête doit être autorisée.
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

    /// Enregistre un succès.
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

    /// Enregistre un échec et ajuste l'état en conséquence.
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

    /// Retourne le nombre d'échecs actifs dans la fenêtre.
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
        if inner.state == CircuitState::Open {
            if let Some(opened_at) = inner.opened_at {
                if opened_at.elapsed() >= self.config.cooldown {
                    inner.state = CircuitState::HalfOpen;
                    inner.half_open_probe_in_flight = false;
                }
            }
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

/// Identifiant d'un provider.
pub type ProviderId = String;

/// Registry de circuit breakers par provider.
///
/// Thread-safe via `Mutex<HashMap<...>>`.
#[derive(Debug, Default)]
pub struct CircuitBreakerRegistry {
    breakers: Mutex<HashMap<ProviderId, CircuitBreaker>>,
    config: CircuitBreakerConfig,
}

impl CircuitBreakerRegistry {
    /// Construit un registry vide avec la configuration donnée.
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
