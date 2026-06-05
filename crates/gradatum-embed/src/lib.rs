//! # gradatum-embed
//!
//! Trait `Embedder` + backends HTTP et CPU + decorator `FallbackEmbedder`.
//!
//! ## Architecture
//!
//! ```text
//! Embedder (trait)
//! ├── FastEmbedCpu  — inférence locale ONNX via fastembed (feature = "fastembed-cpu")
//! ├── HttpEmbedder  — appel HTTP OpenAI-compat /v1/embeddings (remote embedder)
//! ├── Noop          — vecteurs nuls (tests / désactivation)
//! └── FallbackEmbedder<P, F>  — decorator circuit-breaker primary→fallback
//! ```
//!
//! ## Feature flags
//!
//! - `fastembed-cpu` (désactivé par défaut) : active `FastEmbedCpu`.
//!   Requiert un ONNX Runtime compatible installé ou téléchargé par fastembed.
//!
//! ## Anti-cycle invariant
//!
//! `gradatum-embed` MUST NOT depend on `gradatum-engine`.
//! `gradatum-engine` PEUT dépendre de `gradatum-embed` (adapters locaux).
//!
//! ## Stability
//!
//! `0.x` — aucune garantie de stabilité API.
//! See [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod embedder_trait;
pub mod error;
pub mod fallback;
#[cfg(feature = "fastembed-cpu")]
pub mod fastembed_cpu;
pub mod http;
pub mod noop;

// Re-exports publics — API principale du crate.
pub use embedder_trait::{EmbedBackend, Embedder};
pub use error::EmbedError;
pub use fallback::FallbackEmbedder;
#[cfg(feature = "fastembed-cpu")]
pub use fastembed_cpu::FastEmbedCpu;
pub use http::HttpEmbedder;
pub use noop::Noop;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
