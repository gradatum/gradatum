//! # gradatum-search
//!
//! Search orchestration: BM25 + semantic + RRF fusion across the index layer.
//!
//! ## Re-exports: `SqliteIndex` and query methods
//!
//! Re-exports `SqliteIndex` from `gradatum-index` together with the types
//! associated with the query methods:
//! `distinct_authors`, `distinct_tags`, `backlinks`, `neighbors`,
//! `trace_lineage`, `title_lookup`, `get_note`.
//!
//! ## RRF Fusion
//!
//! `rrf`: Reciprocal Rank Fusion module (BM25 + semantic → unified score).
//! Used by the `vault_search` handler to combine the two signals.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org); backward-compatible additions only within `2.x`.
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// RRF Fusion module — Reciprocal Rank Fusion over BM25 and semantic signals.
pub mod rrf;

/// Scoring module — pure functions for multi-factor ranking.
pub mod scoring;

/// Reranker module — cross-encoder abstraction for post-ranking.
pub mod reranker;

/// Re-exports `rrf_fuse` and `RrfHit` for direct use from `gradatum_search`.
pub use rrf::{RrfHit, rrf_fuse};

/// Re-exports multi-factor scoring functions and trust decay utilities.
pub use scoring::{
    DEFAULT_TRUST_HALF_LIVES, GAMMA_TRUST, ResolvedWeights, SalienceParams, ScoringWeightsWire,
    TrustDecayConfig, apply_salience, composite_score, composite_score_weighted,
    composite_score_with_trust, default_half_lives, pagerank_factor, recency_factor,
    resolve_weights, salience_factor, salience_weighted_sum, trust_decay_factor,
};

/// Re-exports the `Reranker` trait and `NoopReranker`.
pub use reranker::{NoopReranker, Reranker};

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
