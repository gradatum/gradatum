//! `FallbackEmbedder<P, F>` — decorator primary + fallback avec circuit breaker.
//!
//! ## Circuit breaker
//!
//! Après `max_consecutive_failures` échecs consécutifs du primary, le circuit
//! s'ouvre pour une durée `cooldown` : pendant ce temps, seul le fallback est
//! appelé (le primary est bypassé entièrement). Sur un succès du primary, le
//! compteur est remis à zéro et le circuit se referme immédiatement.
//!
//! ## Atomics
//!
//! `failures` et `open_until_ms` sont des atomics `Relaxed` — la cohérence
//! exacte n'est pas critique (un cycle manqué de circuit-open est acceptable),
//! on évite ainsi le coût d'un Mutex pour ce hot path.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

/// Embedder decorator : essaie `primary`, bascule sur `fallback` si échec.
///
/// # Type parameters
///
/// - `P` : embedder primaire (ex : `HttpEmbedder`)
/// - `F` : embedder de secours (ex : `FastEmbedCpu` ou `Noop`)
pub struct FallbackEmbedder<P: Embedder, F: Embedder> {
    primary: P,
    fallback: F,
    /// Nombre d'échecs consécutifs avant ouverture du circuit.
    max_consecutive_failures: u32,
    /// Durée pendant laquelle le circuit reste ouvert après déclenchement.
    cooldown: Duration,
    /// Compteur d'échecs consécutifs courant.
    failures: AtomicU32,
    /// Timestamp (ms epoch) jusqu'auquel le circuit est ouvert. 0 = fermé.
    open_until_ms: AtomicU64,
}

impl<P: Embedder, F: Embedder> FallbackEmbedder<P, F> {
    /// Crée un `FallbackEmbedder` avec les valeurs par défaut :
    /// - seuil = 3 échecs consécutifs
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

    /// Remplace le seuil d'ouverture du circuit (nombre d'échecs consécutifs).
    #[must_use]
    pub fn with_threshold(mut self, n: u32) -> Self {
        self.max_consecutive_failures = n;
        self
    }

    /// Remplace la durée de cooldown après ouverture du circuit.
    #[must_use]
    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    /// Retourne le timestamp courant en millisecondes depuis l'epoch UNIX.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Retourne `true` si le circuit est actuellement ouvert (primary bypassé).
    fn circuit_open(&self) -> bool {
        Self::now_ms() < self.open_until_ms.load(Ordering::Relaxed)
    }

    /// Enregistre un échec du primary. Ouvre le circuit si le seuil est atteint.
    fn record_failure(&self) {
        let f = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if f >= self.max_consecutive_failures {
            let until = Self::now_ms() + self.cooldown.as_millis() as u64;
            self.open_until_ms.store(until, Ordering::Relaxed);
        }
    }

    /// Enregistre un succès du primary. Remet le compteur à zéro (circuit fermé).
    fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl<P: Embedder, F: Embedder> Embedder for FallbackEmbedder<P, F> {
    /// Identifiant du primary.
    fn embedder_id(&self) -> &str {
        self.primary.embedder_id()
    }

    /// Dimension du primary (doit correspondre au fallback — validé par l'appelant).
    fn dim(&self) -> u16 {
        self.primary.dim()
    }

    /// Tente l'embedding via le primary. Si le circuit est ouvert ou si le primary
    /// échoue, bascule sur le fallback.
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

    /// Tente le batch via le primary. Si le circuit est ouvert ou si le primary
    /// échoue, bascule sur le fallback.
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
