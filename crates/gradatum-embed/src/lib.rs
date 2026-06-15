//! # gradatum-embed
//!
//! `Embedder` trait, HTTP and CPU backends, and the `FallbackEmbedder` decorator.
//!
//! ## Architecture
//!
//! ```text
//! Embedder (trait)
//! ├── FastEmbedCpu  — local ONNX inference via fastembed (feature = "fastembed-cpu")
//! ├── HttpEmbedder  — HTTP call to an OpenAI-compatible /v1/embeddings endpoint
//! ├── Noop          — zero vectors (tests / disabled embedding)
//! └── FallbackEmbedder<P, F>  — circuit-breaker decorator: primary → fallback
//! ```
//!
//! ## Feature flags
//!
//! - `fastembed-cpu` (disabled by default): enables `FastEmbedCpu`.
//!   Requires a compatible ONNX Runtime installed or downloaded by fastembed.
//!
//! ## Anti-cycle invariant
//!
//! `gradatum-embed` MUST NOT depend on `gradatum-engine`.
//! `gradatum-engine` MAY depend on `gradatum-embed` (local adapters).
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee.
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

// Public re-exports — main crate API.
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
