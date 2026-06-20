//! Circuit breaker decorator for any `Chat` backend.
//!
//! ## Behaviour
//!
//! - After `max_consecutive_failures` consecutive failures → circuit opens.
//! - During cooldown: every attempt returns `ChatError::CircuitOpen` without
//!   calling the inner backend.
//! - After cooldown: the next request passes (probe). On success → circuit closes.
//!   On failure → counter resets to 1 and the cooldown restarts.
//! - A success resets the failure counter to zero (even without a prior opening).
//!
//! ## Thread safety
//!
//! `failures` and `open_until_ms` are atomics — the decorator is `Send + Sync`.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use gradatum_core::note::Note;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

/// Returns the current timestamp in milliseconds since `UNIX_EPOCH` (real clock).
fn real_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Circuit breaker decorator wrapping any `Chat` backend.
///
/// # Example
///
/// ```rust,no_run
/// use gradatum_chat::{circuit_breaker::CircuitBreakerChat, http::HttpChat};
/// use std::time::Duration;
///
/// let protected = CircuitBreakerChat::new(HttpChat::default())
///     .with_threshold(5)
///     .with_cooldown(Duration::from_secs(120));
/// ```
pub struct CircuitBreakerChat<C: Chat> {
    inner: C,
    /// Number of consecutive failures required to open the circuit.
    max_consecutive_failures: u32,
    /// Duration for which the circuit remains open.
    cooldown: Duration,
    /// Current consecutive failure counter.
    failures: AtomicU32,
    /// Epoch ms until which the circuit is open (0 = closed).
    open_until_ms: AtomicU64,
    /// Injectable clock offset in ms — always 0 in production. Cost: one `load(Relaxed)`.
    time_offset_ms: AtomicI64,
}

impl<C: Chat> CircuitBreakerChat<C> {
    /// Creates a circuit breaker with default values:
    /// - threshold: 3 failures
    /// - cooldown: 5 minutes
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            max_consecutive_failures: 3,
            cooldown: Duration::from_secs(300),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
            time_offset_ms: AtomicI64::new(0),
        }
    }

    /// Returns the current timestamp in ms, plus the injectable clock offset (0 in production).
    fn now_ms(&self) -> u64 {
        let offset = self.time_offset_ms.load(Ordering::Relaxed);
        (real_now_ms() as i64 + offset).max(0) as u64
    }

    /// Advances the logical clock by `ms` milliseconds (reserved for tests).
    ///
    /// Allows crossing the cooldown boundary without `tokio::time::sleep`,
    /// ensuring deterministic, flake-free tests under runner contention.
    #[doc(hidden)]
    pub fn advance_test_clock(&self, ms: i64) {
        self.time_offset_ms.fetch_add(ms, Ordering::Relaxed);
    }

    /// Sets the number of consecutive failures before the circuit opens.
    #[must_use]
    pub fn with_threshold(self, n: u32) -> Self {
        Self {
            max_consecutive_failures: n,
            ..self
        }
    }

    /// Sets the cooldown duration.
    #[must_use]
    pub fn with_cooldown(self, d: Duration) -> Self {
        Self {
            cooldown: d,
            ..self
        }
    }

    /// Returns `true` if the circuit is currently open (cooldown active).
    pub fn is_open(&self) -> bool {
        let open_until = self.open_until_ms.load(Ordering::Relaxed);
        open_until > 0 && self.now_ms() < open_until
    }
}

#[async_trait]
impl<C: Chat> Chat for CircuitBreakerChat<C> {
    async fn classify_curator(
        &self,
        note: &Note,
        ctx: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        // Vérification circuit ouvert
        if self.is_open() {
            return Err(ChatError::CircuitOpen);
        }

        match self.inner.classify_curator(note, ctx).await {
            Ok(verdict) => {
                // Succès → remise à zéro du compteur et fermeture du circuit
                self.failures.store(0, Ordering::Relaxed);
                self.open_until_ms.store(0, Ordering::Relaxed);
                Ok(verdict)
            }
            Err(e) => {
                let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= self.max_consecutive_failures {
                    let until = self.now_ms() + self.cooldown.as_millis() as u64;
                    self.open_until_ms.store(until, Ordering::Relaxed);
                    // Remet le compteur à zéro pour que la probe après cooldown parte proprement
                    self.failures.store(0, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }

    fn backend_kind(&self) -> ChatBackend {
        self.inner.backend_kind()
    }
}
