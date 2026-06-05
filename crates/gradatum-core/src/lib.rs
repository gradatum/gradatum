//! # gradatum-core
//!
//! Shared primitives: traits, canonical types, errors. The L0 crate every other Gradatum crate depends on.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. All public traits are tagged
//! [`#[stability::unstable]`] or [`#[stability::experimental]`].
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! Scaffolding stub — implementation in Phase 1. See [`docs/PHASES.md`](https://github.com/gradatum/gradatum/blob/main/docs/PHASES.md).
//!
//! ## Multi-tenancy invariant (Phase 1)
//!
//! Every persisted row carries `tenant_id TEXT NOT NULL`.
//! Default tenant: `"main"`. Aliased to `vault` in user-facing UI/CLI/SDK.
//! Enforced at storage layer; ACL filters by `tenant_id` first.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod acl;
pub mod audit;
pub mod author;
pub mod config;
pub mod document_store;
pub mod error;
pub mod event_sink;
pub mod frontmatter;
pub mod identity;
pub mod index;
pub mod index_store;
/// Types primitifs pour le système de jobs (v0.2.0+ — ARCH-D15).
///
/// Contient [`Job`], [`JobRecord`], [`JobSpec`], [`JobClass`], [`JobMode`],
/// [`JobScope`], [`JobPriority`], [`QueueStore`], [`QueueEvent`], [`DryRunAware`]
/// et tous les types associés définis par v81 §6 L2159-2779.
pub mod job;
pub mod note;
pub mod overrides;
pub mod schema_registry;
pub mod scope;
pub mod secrets;
pub mod section;
pub mod status;
pub mod tag;
pub mod trust;
pub mod vector_store;

pub use job::{
    job_kind_str,
    // Source structs nouveaux variants v59
    ConflictStrategy,
    // Specs variants actifs
    CurateSpec,
    // Traits
    DryRunAware,
    EmbedSpec,
    ExportFormat,
    ExportSource,
    // Apalis payload
    GradatumJob,
    IngestInputSource,
    IngestSource,
    IngestStrategy,
    // Enum principal + helper routing
    Job,
    JobClass,
    JobError,
    // Filter
    JobFilter,
    // Lifecycle
    JobLifecycle,
    // Lineage
    JobLineage,
    JobMode,
    JobOutput,
    JobOutputFile,
    JobPriority,
    JobProgress,
    // Record principal
    JobRecord,
    JobResult,
    // Retry
    JobRetry,
    // Scheduling
    JobScheduling,
    JobScope,
    JobSource,
    // JobSpec + composants
    JobSpec,
    JobStatus,
    JobTrigger,
    // Workspace + Progress + Output
    JobWorkspace,
    MigrateMode,
    MigrateSource,
    NotifyChannel,
    NotifySource,
    QueueError,
    // Queue events
    QueueEvent,
    QueueStore,
    // ReIndex
    ReIndexMode,
    RetryBackoff,
    TriggerCondition,
    TriggerSource,
    // VaultScope alias
    VaultScope,
};

pub use document_store::DocumentStore;
pub use index_store::{AuthorRow, IndexStore, Lineage, SearchHitRaw};
pub use vector_store::VectorStore;

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
