//! `CircuitBreaker<B>` pour backends `LlmBackend` — P2.0b.
//!
//! Différent de [`crate::circuit_breaker::CircuitBreakerChat`] (Phase 1)
//! qui wrappait le trait `Chat`. Ce module cible le trait `LlmBackend`.
//!
//! ## États
//!
//! ```text
//! Closed ──[5 failures / 60s window]──► Open (30s)
//!                                            │
//!                                     [timeout expiré]
//!                                            │
//!                                            ▼
//!                                       HalfOpen ──[2 successes]──► Closed
//!                                            │
//!                                     [1 failure]
//!                                            │
//!                                       Open (60s) ──► Open (120s) ──► Open (300s)
//! ```
//!
//! ## Fallback transparent
//!
//! En état `Open`, les requêtes sont redirigées vers `HeuristicBackend` sans
//! propager d'erreur. L'appelant reçoit une `CuratorDecision` valide dans tous les cas.
//!
//! ## Thread safety
//!
//! - `open_until` : `AtomicI64` (timestamp epoch secondes)
//! - `open_count` : `AtomicU32` (compteur d'ouvertures pour backoff exponentiel)
//! - `consecutive_successes` : `AtomicU32` (transition HalfOpen → Closed)
//! - `probe_inflight` : `AtomicBool` (une seule probe simultanée en HalfOpen)
//! - `failures` : `tokio::sync::Mutex<VecDeque<Instant>>` (fenêtre glissante)
//!
//! Spec ref : plan P2.0b §"Step 5.8".

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::backend::{CuratorDecision, LlmBackend, LlmError};
use crate::heuristic_routing::HeuristicBackend;

/// Configuration du circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitConfig {
    /// Nombre de failures dans la fenêtre pour ouvrir le circuit.
    pub failure_threshold: u32,
    /// Fenêtre glissante de comptage des failures.
    pub failure_window: Duration,
    /// Durées d'ouverture successives (backoff exponentiel).
    /// `[30s, 60s, 120s, 300s]` — la dernière valeur est utilisée pour toutes
    /// les ouvertures suivantes.
    pub open_durations: Vec<Duration>,
    /// Nombre de succès consécutifs en HalfOpen pour fermer le circuit.
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

/// Circuit breaker wrappant un `LlmBackend` avec fallback vers `HeuristicBackend`.
///
/// # Exemple
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
    /// Fenêtre glissante des timestamps de failures.
    failures: Mutex<VecDeque<Instant>>,
    /// Timestamp epoch secondes jusqu'auquel le circuit reste ouvert (0 = fermé).
    open_until: AtomicI64,
    /// Nombre de succès consécutifs depuis l'entrée en HalfOpen.
    consecutive_successes: AtomicU32,
    /// Nombre d'ouvertures successives (pour le backoff exponentiel).
    open_count: AtomicU32,
    /// `true` si une probe est déjà en flight (évite les probes simultanées).
    probe_inflight: AtomicBool,
}

impl<B: LlmBackend> CircuitBreaker<B> {
    /// Crée un circuit breaker avec la configuration fournie.
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
        }
    }

    /// Timestamp courant en millisecondes depuis UNIX_EPOCH.
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    /// Indique si le circuit est actuellement ouvert.
    pub fn is_open(&self) -> bool {
        let until = self.open_until.load(Ordering::Relaxed);
        until > 0 && Self::now_ms() < until
    }

    /// Indique si le circuit est en état HalfOpen (timeout expiré, pas encore fermé).
    pub fn is_half_open(&self) -> bool {
        let until = self.open_until.load(Ordering::Relaxed);
        until > 0 && Self::now_ms() >= until
    }

    /// Enregistre une failure et ouvre le circuit si le seuil est atteint.
    /// Ouvre (ou ré-ouvre) le circuit avec backoff exponentiel.
    ///
    /// Incrémente `open_count` et sélectionne la durée d'ouverture correspondante.
    ///
    /// ## Caveat B-04
    ///
    /// Émet un WARN log explicite sur la transition vers Open, avec le nom du backend,
    /// l'état précédent, l'état suivant et la durée de cooldown.
    fn trip_open(&self) {
        let count = self.open_count.fetch_add(1, Ordering::Relaxed) as usize;
        let dur = self
            .config
            .open_durations
            .get(count.min(self.config.open_durations.len() - 1))
            .copied()
            .unwrap_or(Duration::from_secs(300));

        let until = Self::now_ms() + dur.as_millis() as i64;

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

    /// Enregistre un succès. Si en HalfOpen et `success_threshold` atteint → ferme.
    ///
    /// ## Caveat B-04
    ///
    /// Émet un WARN log sur la transition HalfOpen → Closed pour signaler la
    /// récupération du backend LLM (retour au mode LLM depuis heuristique).
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
            open_durations: vec![
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
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

        // Attendre l'expiry du timeout (50ms)
        tokio::time::sleep(Duration::from_millis(100)).await;

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
            open_durations: vec![Duration::from_millis(50)],
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

        // Attendre expiry → HalfOpen
        tokio::time::sleep(Duration::from_millis(100)).await;
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
            open_durations: vec![
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
            ],
            success_threshold: 2,
        };
        let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

        // 1ère ouverture (50ms)
        for _ in 0..5 {
            let _ = cb.classify(S, U).await;
        }
        let first_open_count = cb.open_count.load(Ordering::Relaxed);
        assert_eq!(first_open_count, 1, "première ouverture");

        // Attendre expiry (50ms) → HalfOpen
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(cb.is_half_open());

        // Re-trip en HalfOpen → 2ème ouverture (100ms)
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
