//! Circuit breaker decorator pour tout backend `Chat`.
//!
//! ## Comportement
//!
//! - Après `max_consecutive_failures` échecs consécutifs → circuit ouvert.
//! - Pendant le cooldown : toute tentative retourne `ChatError::CircuitOpen` sans appeler
//!   le backend interne.
//! - Après le cooldown : la prochaine requête passe (probe). Si elle réussit → circuit fermé.
//!   Si elle échoue → compteur réinitialisé à 1, le cooldown redémarre.
//! - Un succès remet le compteur de failures à zéro (même sans ouverture préalable).
//!
//! ## Thread safety
//!
//! `failures` et `open_until_ms` sont des atomics — le decorator est `Send + Sync`.
//!
//! Spec ref : plan T07 sous-tâche T07c.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use gradatum_core::note::Note;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

/// Timestamp courant en millisecondes depuis UNIX_EPOCH.
///
/// # Panics
///
/// Ne panique pas sur les systèmes où l'horloge est correctement initialisée
/// (antérieure à UNIX_EPOCH est théoriquement impossible sur un système avec horloge correcte).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Decorator circuit breaker wrappant n'importe quel `Chat`.
///
/// # Exemple
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
    /// Nombre d'échecs consécutifs nécessaires pour ouvrir le circuit.
    max_consecutive_failures: u32,
    /// Durée pendant laquelle le circuit reste ouvert.
    cooldown: Duration,
    /// Compteur d'échecs consécutifs en cours.
    failures: AtomicU32,
    /// Epoch ms jusqu'à laquelle le circuit est ouvert (0 = fermé).
    open_until_ms: AtomicU64,
}

impl<C: Chat> CircuitBreakerChat<C> {
    /// Crée un circuit breaker avec les valeurs par défaut :
    /// - seuil : 3 failures
    /// - cooldown : 5 minutes
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            max_consecutive_failures: 3,
            cooldown: Duration::from_secs(300),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
        }
    }

    /// Change le nombre d'échecs consécutifs avant ouverture.
    #[must_use]
    pub fn with_threshold(self, n: u32) -> Self {
        Self {
            max_consecutive_failures: n,
            ..self
        }
    }

    /// Change la durée de cooldown.
    #[must_use]
    pub fn with_cooldown(self, d: Duration) -> Self {
        Self {
            cooldown: d,
            ..self
        }
    }

    /// Indique si le circuit est actuellement ouvert (cooldown actif).
    pub fn is_open(&self) -> bool {
        let open_until = self.open_until_ms.load(Ordering::Relaxed);
        open_until > 0 && now_ms() < open_until
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
                    let until = now_ms() + self.cooldown.as_millis() as u64;
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
