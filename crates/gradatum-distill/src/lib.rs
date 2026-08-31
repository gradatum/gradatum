//! Semantic distillation primitives.
//!
//! Regroups the distillation processing logic formerly scattered across
//! `gradatum-worker` and `gradatum-core`:
//!
//! - [`distill_cluster`] — cosine-similarity clustering of embeddings
//!   (connected components of an adjacency graph).
//! - [`synthesizer`] — cluster-synthesis abstraction, with a deterministic
//!   template MVP ([`TemplateSynthesizer`]).
//! - [`trust`] — distilled trust score: mean of source trusts × confidence,
//!   clamped to `[0, 1]`.
//!
//! The job vocabulary (`DistillMode`, `DistillSource`, `Job::Distill`) stays in
//! `gradatum_core::job` — those are payload contracts, not processing logic.

pub mod distill_cluster;
pub mod synthesizer;
pub mod trust;

pub use distill_cluster::{cluster_by_cosine, cosine_similarity};
pub use synthesizer::{ClusterSynthesis, DistillSynthesizer, SynthesisError, TemplateSynthesizer};
pub use trust::compute_distill_trust;

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
