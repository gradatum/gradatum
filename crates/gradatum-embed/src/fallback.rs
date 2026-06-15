//! `FallbackEmbedder<P, F>` — primary + fallback decorator with circuit breaker.
//!
//! ## Circuit breaker
//!
//! After `max_consecutive_failures` consecutive primary failures, the circuit
//! opens for a `cooldown` duration: during that window only the fallback is
//! called (the primary is bypassed entirely). On a primary success, the counter
//! resets and the circuit closes immediately.
//!
//! ## Atomics
//!
//! `failures` and `open_until_ms` use `Relaxed` atomics — exact consistency
//! is not critical (missing one circuit-open cycle is acceptable), avoiding the
//! cost of a `Mutex` on this hot path.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

/// Embedder decorator: tries `primary`, falls back to `fallback` on failure.
///
/// # Type parameters
///
/// - `P`: primary embedder (e.g. `HttpEmbedder`)
/// - `F`: fallback embedder (e.g. `FastEmbedCpu` or `Noop`)
pub struct FallbackEmbedder<P: Embedder, F: Embedder> {
    primary: P,
    fallback: F,
    /// Number of consecutive failures before the circuit opens.
    max_consecutive_failures: u32,
    /// Duration the circuit stays open after tripping.
    cooldown: Duration,
    /// Current consecutive failure counter.
    failures: AtomicU32,
    /// Epoch timestamp (ms) until which the circuit is open. 0 = closed.
    open_until_ms: AtomicU64,
}

impl<P: Embedder, F: Embedder> FallbackEmbedder<P, F> {
    /// Creates a `FallbackEmbedder` with default values:
    /// - threshold = 3 consecutive failures
    /// - cooldown = 5 minutes
    pub fn new(primary: P, fallback: F) -> Self {
        Self {
            primary,
            fallback,
            max_consecutive_failures: 3,
            cooldown: Duration::from_secs(300),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
        }
    }

    /// Overrides the circuit-open threshold (number of consecutive failures).
    #[must_use]
    pub fn with_threshold(mut self, n: u32) -> Self {
        self.max_consecutive_failures = n;
        self
    }

    /// Overrides the cooldown duration after the circuit opens.
    #[must_use]
    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    /// Returns the current timestamp in milliseconds since the UNIX epoch.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Returns `true` if the circuit is currently open (primary bypassed).
    fn circuit_open(&self) -> bool {
        Self::now_ms() < self.open_until_ms.load(Ordering::Relaxed)
    }

    /// Records a primary failure. Opens the circuit if the threshold is reached.
    fn record_failure(&self) {
        let f = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if f >= self.max_consecutive_failures {
            let until = Self::now_ms() + self.cooldown.as_millis() as u64;
            self.open_until_ms.store(until, Ordering::Relaxed);
        }
    }

    /// Records a primary success. Resets the counter and closes the circuit.
    fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl<P: Embedder, F: Embedder> Embedder for FallbackEmbedder<P, F> {
    /// Returns the primary embedder identifier.
    fn embedder_id(&self) -> &str {
        self.primary.embedder_id()
    }

    /// Returns the primary dimension (must match the fallback — validated by the caller).
    fn dim(&self) -> u16 {
        self.primary.dim()
    }

    /// Attempts embedding via the primary. Falls back to the fallback if the circuit
    /// is open or the primary fails.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if self.circuit_open() {
            return self.fallback.embed(text).await;
        }
        match self.primary.embed(text).await {
            Ok(v) => {
                self.record_success();
                Ok(v)
            }
            Err(_) => {
                self.record_failure();
                self.fallback.embed(text).await
            }
        }
    }

    /// Attempts batch embedding via the primary. Falls back to the fallback if the
    /// circuit is open or the primary fails.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.circuit_open() {
            return self.fallback.embed_batch(texts).await;
        }
        match self.primary.embed_batch(texts).await {
            Ok(v) => {
                self.record_success();
                Ok(v)
            }
            Err(_) => {
                self.record_failure();
                self.fallback.embed_batch(texts).await
            }
        }
    }

    fn backend_kind(&self) -> EmbedBackend {
        self.primary.backend_kind()
    }
}
