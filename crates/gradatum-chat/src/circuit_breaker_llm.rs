//! `CircuitBreaker<B>` for `LlmBackend` backends.
//!
//! Distinct from [`crate::circuit_breaker::CircuitBreakerChat`],
//! which wraps the `Chat` trait. This module targets the `LlmBackend` trait.
//!
//! ## States
//!
//! ```text
//! Closed ──[5 failures / 60s window]──► Open (30s)
//!                                            │
//!                                     [timeout expired]
//!                                            │
//!                                            ▼
//!                                       HalfOpen ──[2 successes]──► Closed
//!                                            │
//!                                     [1 failure]
//!                                            │
//!                                       Open (60s) ──► Open (120s) ──► Open (300s)
//! ```
//!
//! ## Transparent fallback
//!
//! In the `Open` state, requests are redirected to `HeuristicBackend` without
//! propagating an error. The caller always receives a valid `CuratorDecision`.
//!
//! ## Thread safety
//!
//! - `open_until`: `AtomicI64` (epoch seconds timestamp)
//! - `open_count`: `AtomicU32` (open count for exponential backoff)
//! - `consecutive_successes`: `AtomicU32` (HalfOpen → Closed transition)
//! - `probe_inflight`: `AtomicBool` (at most one concurrent probe in HalfOpen)
//! - `failures`: `tokio::sync::Mutex<VecDeque<Instant>>` (sliding window)
//!

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::backend::{CuratorDecision, LlmBackend, LlmError};
use crate::heuristic_routing::HeuristicBackend;

/// Circuit breaker configuration.
#[derive(Debug, Clone)]
pub struct CircuitConfig {
    /// Number of failures in the window required to open the circuit.
    pub failure_threshold: u32,
    /// Sliding window for counting failures.
    pub failure_window: Duration,
    /// Successive open durations (exponential backoff).
    /// `[30s, 60s, 120s, 300s]` — the last value is used for all subsequent openings.
    pub open_durations: Vec<Duration>,
    /// Number of consecutive successes in HalfOpen required to close the circuit.
    pub success_threshold: u32,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            open_durations: vec![
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(300),
            ],
            success_threshold: 2,
        }
    }
}

/// Circuit breaker wrapping an `LlmBackend` with fallback to `HeuristicBackend`.
///
/// # Example
///
/// ```rust,no_run
/// use gradatum_chat::circuit_breaker_llm::{CircuitBreaker, CircuitConfig};
/// use gradatum_chat::heuristic_routing::HeuristicBackend;
/// use gradatum_chat::openai_compat::OpenAiCompatBackend;
/// use secrecy::SecretString;
/// use std::sync::Arc;
///
/// let backend = OpenAiCompatBackend::new(
///     "http://127.0.0.1:8080".to_string(),
///     "qwen3-0.6b".to_string(),
///     SecretString::new("".to_string().into()),
/// );
/// let cb = CircuitBreaker::new(backend, Arc::new(HeuristicBackend), CircuitConfig::default());
/// ```
pub struct CircuitBreaker<B> {
    inner: B,
    fallback: Arc<HeuristicBackend>,
    config: CircuitConfig,
    /// Sliding window of failure timestamps.
    failures: Mutex<VecDeque<Instant>>,
    /// Epoch-seconds timestamp until which the circuit remains open (0 = closed).
    open_until: AtomicI64,
    /// Number of consecutive successes since entering HalfOpen.
    consecutive_successes: AtomicU32,
    /// Number of successive openings (for exponential backoff).
    open_count: AtomicU32,
    /// `true` if a probe is already in flight (prevents concurrent probes).
    probe_inflight: AtomicBool,
    /// Injectable clock offset in ms — always 0 in production.
    ///
    /// `now_ms()` returns the real epoch. Tests advance logical time via
    /// [`advance_test_clock`] instead of `tokio::time::sleep`, eliminating
    /// flaky assertions on tight cooldown boundaries. Cost: one `load(Relaxed)`.
    time_offset_ms: AtomicI64,
}

impl<B: LlmBackend> CircuitBreaker<B> {
    /// Creates a circuit breaker with the given configuration.
    pub fn new(inner: B, fallback: Arc<HeuristicBackend>, config: CircuitConfig) -> Self {
        Self {
            inner,
            fallback,
            config,
            failures: Mutex::new(VecDeque::new()),
            open_until: AtomicI64::new(0),
            consecutive_successes: AtomicU32::new(0),
            open_count: AtomicU32::new(0),
            probe_inflight: AtomicBool::new(false),
            time_offset_ms: AtomicI64::new(0),
        }
    }

    /// Returns the current timestamp in milliseconds since `UNIX_EPOCH`, plus the
    /// injectable clock offset (0 in production).
    ///
    /// Defined as an instance method (not an associated function) so that
    /// [`Self::time_offset_ms`] is applied, allowing tests to advance logical time
    /// deterministically via [`advance_test_clock`].
    fn now_ms(&self) -> i64 {
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        real + self.time_offset_ms.load(Ordering::Relaxed)
    }

    /// Advances the circuit breaker's logical clock by `ms` milliseconds.
    ///
    /// Reserved for tests: allows crossing cooldown boundaries without
    /// `tokio::time::sleep`, ensuring deterministic, flake-free state-transition
    /// tests. Does not affect the failure sliding window (`Instant`-based),
    /// only the Open/HalfOpen state that relies on epoch ms.
    #[doc(hidden)]
    pub fn advance_test_clock(&self, ms: i64) {
        self.time_offset_ms.fetch_add(ms, Ordering::Relaxed);
    }

    /// Returns `true` if the circuit is currently open.
    pub fn is_open(&self) -> bool {
        let until = self.open_until.load(Ordering::Relaxed);
        until > 0 && self.now_ms() < until
    }

    /// Returns `true` if the circuit is in the HalfOpen state (timeout expired, not yet closed).
    pub fn is_half_open(&self) -> bool {
        let until = self.open_until.load(Ordering::Relaxed);
        until > 0 && self.now_ms() >= until
    }

    /// Opens (or re-opens) the circuit with exponential backoff.
    ///
    /// Increments `open_count` and selects the corresponding open duration.
    ///
    /// Emits an explicit WARN log on the transition to Open, including the backend name,
    /// the previous state, the next state, and the cooldown duration.
    fn trip_open(&self) {
        let count = self.open_count.fetch_add(1, Ordering::Relaxed) as usize;
        let dur = self
            .config
            .open_durations
            .get(count.min(self.config.open_durations.len() - 1))
            .copied()
            .unwrap_or(Duration::from_secs(300));

        let until = self.now_ms() + dur.as_millis() as i64;

        // Déterminer l'état précédent pour le WARN log (B-04)
        let old_state = if self.is_half_open() {
            "HalfOpen"
        } else {
            "Closed"
        };

        self.open_until.store(until, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);

        // B-04 : WARN explicite sur transition → Open (Closed→Open ou HalfOpen→Open)
        tracing::warn!(
            backend = self.inner.name(),
            old_state = old_state,
            new_state = "Open",
            cooldown_secs = dur.as_secs(),
            open_count = count + 1,
            "circuit_state transition: LLM fallback active to heuristic"
        );
    }

    async fn record_failure(&self) {
        let now = Instant::now();
        let mut failures = self.failures.lock().await;

        // Expirer les failures hors de la fenêtre glissante
        while let Some(&front) = failures.front() {
            if now.duration_since(front) > self.config.failure_window {
                failures.pop_front();
            } else {
                break;
            }
        }

        failures.push_back(now);

        if failures.len() as u32 >= self.config.failure_threshold {
            failures.clear();
            drop(failures); // libérer le lock avant trip
            self.trip_open();
        }
    }

    /// Records a success. If in HalfOpen and `success_threshold` is reached, closes the circuit.
    ///
    /// Emits a WARN log on the HalfOpen → Closed transition to signal
    /// LLM backend recovery (returning to LLM mode from heuristic fallback).
    fn record_success(&self) {
        // Nettoie les failures si possible (best-effort, pas bloquant)
        if let Ok(mut f) = self.failures.try_lock() {
            f.clear();
        }

        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;

        // Transition HalfOpen → Closed (B-04 : WARN log)
        if successes >= self.config.success_threshold && self.is_half_open() {
            self.open_until.store(0, Ordering::Relaxed);
            self.open_count.store(0, Ordering::Relaxed);
            self.consecutive_successes.store(0, Ordering::Relaxed);
            // B-04 : WARN log sur transition HalfOpen → Closed (LLM backend récupéré)
            tracing::warn!(
                backend = self.inner.name(),
                old_state = "HalfOpen",
                new_state = "Closed",
                cooldown_secs = 0,
                "circuit_state transition: LLM backend recovered, heuristic fallback deactivated"
            );
        }
    }
}

#[async_trait]
impl<B: LlmBackend> LlmBackend for CircuitBreaker<B> {
    fn name(&self) -> &'static str {
        "circuit_breaker"
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        // Circuit Open → fallback transparent
        if self.is_open() {
            tracing::debug!(
                backend = self.inner.name(),
                "circuit open → fallback heuristic"
            );
            return self.fallback.classify(system, user).await;
        }

        // Circuit HalfOpen → une seule probe simultanée
        if self.is_half_open() {
            // B-04 : WARN log sur transition Open → HalfOpen (timeout expiré, probe lancée)
            tracing::warn!(
                backend = self.inner.name(),
                old_state = "Open",
                new_state = "HalfOpen",
                cooldown_secs = 0,
                "circuit_state transition: cooldown expired, probing LLM backend"
            );

            // Tente de prendre le slot de probe
            if self
                .probe_inflight
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                // Une probe est déjà en flight → fallback
                tracing::debug!(
                    backend = self.inner.name(),
                    "circuit half-open, probe in flight → fallback"
                );
                return self.fallback.classify(system, user).await;
            }

            let result = self.inner.classify(system, user).await;
            self.probe_inflight.store(false, Ordering::Release);

            match result {
                Ok(d) => {
                    self.record_success();
                    return Ok(d);
                }
                Err(e) if e.counts_for_circuit() => {
                    // Re-trip en HalfOpen : une seule failure ré-ouvre le circuit
                    // immédiatement avec backoff exponentiel.
                    self.trip_open();
                    return self.fallback.classify(system, user).await;
                }
                Err(e) => return Err(e),
            }
        }

        // Circuit Closed → appel normal
        match self.inner.classify(system, user).await {
            Ok(d) => {
                self.record_success();
                Ok(d)
            }
            Err(e) if e.counts_for_circuit() => {
                self.record_failure().await;
                // Fallback transparent après enregistrement
                self.fallback.classify(system, user).await
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CuratorDecision;

    fn ok_decision() -> CuratorDecision {
        CuratorDecision {
            section: "reference".into(),
            tags: vec![],
            wikilinks: vec![],
            duplicate_hint: None,
        }
    }

    struct AlwaysOk;
    struct AlwaysFail;
    struct FailThenOk {
        count: tokio::sync::Mutex<u32>,
        fail_n: u32,
    }

    #[async_trait]
    impl LlmBackend for AlwaysOk {
        fn name(&self) -> &'static str {
            "always_ok"
        }
        fn is_local(&self) -> bool {
            true
        }
        async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
            Ok(ok_decision())
        }
    }

    #[async_trait]
    impl LlmBackend for AlwaysFail {
        fn name(&self) -> &'static str {
            "always_fail"
        }
        fn is_local(&self) -> bool {
            true
        }
        async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
            Err(LlmError::Timeout)
        }
    }

    #[async_trait]
    impl LlmBackend for FailThenOk {
        fn name(&self) -> &'static str {
            "fail_then_ok"
        }
        fn is_local(&self) -> bool {
            true
        }
        async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
            let mut c = self.count.lock().await;
            let n = *c;
            *c += 1;
            if n < self.fail_n {
                Err(LlmError::Timeout)
            } else {
                Ok(ok_decision())
            }
        }
    }

    fn default_config_short_window() -> CircuitConfig {
        CircuitConfig {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            // Cooldowns en SECONDES, pas en millisecondes (anti-flake, 2026-07-29).
            // Le franchissement d'un cooldown est piloté par `advance_test_clock`
            // (horloge logique) ; le NON-franchissement dépend, lui, du wall-clock
            // réel écoulé entre `trip_open()` et l'assertion. Avec 50 ms cette marge
            // était franchie sous instrumentation `llvm-cov` + tests parallèles du
            // même binaire. Rapport 1:2:4:8 conservé : backoff inchangé.
            open_durations: vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(40),
            ],
            success_threshold: 2,
        }
    }

    const S: &str = "system";
    const U: &str = "Classify this note.\nTitle: t\nBody (truncated to 500 chars): b";

    #[tokio::test]
    async fn closed_stays_closed_under_5_successes() {
        let cb = CircuitBreaker::new(
            AlwaysOk,
            Arc::new(HeuristicBackend),
            CircuitConfig::default(),
        );
        for _ in 0..5 {
            cb.classify(S, U).await.unwrap();
        }
        assert!(!cb.is_open(), "circuit ne doit pas s'ouvrir sur 5 succès");
    }

    #[tokio::test]
    async fn closed_to_open_after_5_failures() {
        let cb = CircuitBreaker::new(
            AlwaysFail,
            Arc::new(HeuristicBackend),
            CircuitConfig::default(),
        );
        // 5 failures dans la fenêtre → circuit s'ouvre, fallback retourne Ok
        for _ in 0..5 {
            let r = cb.classify(S, U).await;
            assert!(r.is_ok(), "fallback heuristic doit toujours retourner Ok");
        }
        assert!(cb.is_open(), "circuit doit être ouvert après 5 failures");
    }

    #[tokio::test]
    async fn window_expiry_resets_failure_count() {
        // Fenêtre très courte (50ms) → 4 failures expireront avant la 5e
        let cfg = CircuitConfig {
            failure_threshold: 5,
            failure_window: Duration::from_millis(50),
            open_durations: vec![Duration::from_secs(60)],
            success_threshold: 2,
        };
        let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

        // 4 failures
        for _ in 0..4 {
            let _ = cb.classify(S, U).await;
        }
        // Attendre que la fenêtre expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 4 autres failures (les précédentes ont expiré) → circuit ne doit pas être ouvert
        for _ in 0..4 {
            let _ = cb.classify(S, U).await;
        }
        assert!(
            !cb.is_open(),
            "après expiry de la fenêtre, le circuit ne doit pas être ouvert après 4 nouvelles failures"
        );
    }

    #[tokio::test]
    async fn open_to_halfopen_after_timeout() {
        let cfg = default_config_short_window();
        let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

        // Ouvre le circuit
        for _ in 0..5 {
            let _ = cb.classify(S, U).await;
        }
        assert!(cb.is_open(), "circuit doit être ouvert");

        // Avancer l'horloge logique au-delà du timeout (5s) — déterministe (D2.3)
        cb.advance_test_clock(10_000);

        assert!(
            !cb.is_open(),
            "après timeout, circuit ne doit plus être open"
        );
        assert!(
            cb.is_half_open(),
            "circuit doit être half-open après timeout"
        );
    }

    #[tokio::test]
    async fn halfopen_to_closed_with_2_successes() {
        let cfg = CircuitConfig {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            // Cooldown en secondes — cf. note anti-flake dans `default_config_short_window()`.
            open_durations: vec![Duration::from_secs(5)],
            success_threshold: 2,
        };
        let cb = CircuitBreaker::new(
            FailThenOk {
                count: tokio::sync::Mutex::new(0),
                fail_n: 5,
            },
            Arc::new(HeuristicBackend),
            cfg,
        );

        // Ouvre le circuit
        for _ in 0..5 {
            let _ = cb.classify(S, U).await;
        }
        assert!(cb.is_open());

        // Avancer l'horloge au-delà du cooldown (5s) → HalfOpen — déterministe (D2.3)
        cb.advance_test_clock(10_000);
        assert!(cb.is_half_open());

        // 2 succès → Closed
        cb.classify(S, U).await.unwrap();
        cb.classify(S, U).await.unwrap();

        assert!(!cb.is_open(), "après 2 succès, circuit doit être fermé");
        assert!(
            !cb.is_half_open(),
            "après 2 succès, circuit ne doit plus être half-open"
        );
    }

    #[tokio::test]
    async fn halfopen_to_open_retrp_with_backoff() {
        let cfg = CircuitConfig {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            // Cooldowns en secondes — cf. note anti-flake ci-dessus.
            open_durations: vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(40),
            ],
            success_threshold: 2,
        };
        let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

        // 1ère ouverture (5s)
        for _ in 0..5 {
            let _ = cb.classify(S, U).await;
        }
        let first_open_count = cb.open_count.load(Ordering::Relaxed);
        assert_eq!(first_open_count, 1, "première ouverture");

        // Avancer l'horloge au-delà du cooldown (5s) → HalfOpen — déterministe (D2.3)
        cb.advance_test_clock(10_000);
        assert!(cb.is_half_open());

        // Re-trip en HalfOpen → 2ème ouverture (10s)
        let _ = cb.classify(S, U).await;
        let second_open_count = cb.open_count.load(Ordering::Relaxed);
        assert!(
            second_open_count > first_open_count,
            "open_count doit augmenter à chaque re-trip"
        );
        assert!(cb.is_open(), "circuit doit être réouvert après re-trip");
    }

    #[tokio::test]
    async fn fallback_returns_valid_curator_decision() {
        // Fallback doit retourner une CuratorDecision valide (section non vide)
        let cfg = CircuitConfig {
            failure_threshold: 1,
            failure_window: Duration::from_secs(60),
            open_durations: vec![Duration::from_secs(60)],
            success_threshold: 2,
        };
        let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

        // Ouvre le circuit
        let _ = cb.classify(S, U).await;
        assert!(cb.is_open());

        // Fallback doit retourner une décision valide
        let decision = cb.classify(S, U).await.unwrap();
        assert!(
            !decision.section.is_empty(),
            "fallback heuristic doit retourner une section non vide"
        );
    }
}
