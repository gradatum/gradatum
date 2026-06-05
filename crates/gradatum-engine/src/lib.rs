//! # gradatum-engine
//!
//! Superviseur Rust d'un sous-process `llama-server` natif — axum OpenAI-compat.
//!
//! ## Architecture PIVOT v2
//!
//! `gradatum-engine` est un superviseur qui :
//! 1. **spawn** `llama-server` via `tokio::process::Command` (jamais via shell).
//! 2. **wait-ready** : poll `GET /health` enfant jusqu'à 200.
//! 3. **proxy** : handlers `/v1/chat/completions` + `/v1/embeddings` → reqwest vers l'enfant.
//! 4. **supervise** : restart-on-failure borné + shutdown gracieux.
//!
//! L'engine ne charge plus de modèle lui-même (zéro duplication VRAM/RAM).
//!
//! ## Stability
//!
//! `0.x` — aucune garantie de stabilité API. Voir `docs/internal/`.
//!
//! ## Feature gates
//!
//! - `serve` : compile le serveur axum + superviseur llama-server.
//!
//! Sans feature : crate stub (seule `VERSION` est exposée).
//!
//! ## Anti-cycle invariant
//!
//! `gradatum-engine` peut dépendre de `gradatum-core` et `gradatum-dto`.
//! `gradatum-core` et `gradatum-dto` ne doivent JAMAIS dépendre de `gradatum-engine`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Version du crate (depuis `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "serve")]
pub mod config;

#[cfg(feature = "serve")]
pub mod error;

#[cfg(feature = "serve")]
pub mod health;

#[cfg(feature = "serve")]
pub mod metrics;

#[cfg(feature = "serve")]
pub mod runtime;

#[cfg(feature = "serve")]
pub mod server;

#[cfg(feature = "serve")]
pub mod sink;

#[cfg(feature = "serve")]
pub mod supervisor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
