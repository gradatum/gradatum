//! Erreurs du crate `gradatum-chat`.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.13.

use thiserror::Error;

/// Erreurs possibles lors de l'appel à un backend `Chat`.
#[derive(Debug, Error)]
pub enum ChatError {
    /// Erreur HTTP reqwest (connexion, DNS, TLS…).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// La réponse du LLM ne contient pas de JSON valide parseable.
    #[error("parse failure: {0}")]
    ParseFailure(String),

    /// Le backend a dépassé son timeout.
    #[error("backend timeout")]
    Timeout,

    /// Le circuit breaker est ouvert — cooldown actif.
    #[error("circuit open (cooldown active)")]
    CircuitOpen,

    /// Réponse structurellement invalide (mauvais champ, valeur hors bornes…).
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}
