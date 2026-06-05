//! # gradatum-search
//!
//! Search orchestration: BM25 + semantic + RRF fusion across the index layer.
//!
//! ## T3 P2.0c — Re-exports SqliteIndex + 7 query methods
//!
//! Cette crate expose `SqliteIndex` de `gradatum-index` ainsi que les types
//! associées aux 7 méthodes query ajoutées en T3 P2.0c :
//! `distinct_authors`, `distinct_tags`, `backlinks`, `neighbors`,
//! `trace_lineage`, `title_lookup`, `get_note`.
//!
//! ## Phase 2.x.2 alpha.11 — RRF Fusion
//!
//! `rrf` : module de fusion Reciprocal Rank Fusion (BM25 + semantic -> score unifié).
//! Utilisé par le handler `vault_search` pour combiner les deux signaux.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee.
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Module RRF Fusion — Reciprocal Rank Fusion BM25 + semantic.
pub mod rrf;

/// Module Scoring — fonctions pures pour le ranking multi-facteur (alpha.12 Task 11).
pub mod scoring;

/// Module Reranker — abstraction cross-encoder pour le post-ranking (alpha.12 Task 14).
pub mod reranker;

/// Re-export `rrf_fuse` et `RrfHit` pour usage direct depuis `gradatum_search`.
pub use rrf::{rrf_fuse, RrfHit};

/// Re-export des fonctions de scoring multi-facteur (alpha.12 Task 11).
pub use scoring::{composite_score, pagerank_factor, recency_factor};

/// Re-export du trait `Reranker` et de `NoopReranker` (alpha.12 Task 14).
pub use reranker::{NoopReranker, Reranker};

/// Re-export `SqliteIndex` et types query depuis `gradatum-index`.
///
/// `gradatum-server::AppState.search` consomme ces types directement.
pub use gradatum_index::{AuthorRow, Lineage, NoteRecord, SearchHitRaw, SqliteIndex};

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
