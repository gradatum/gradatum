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
//! ## Contents
//!
//! Shared primitives used across all Gradatum crates: note identity ([`identity`]),
//! canonical frontmatter ([`frontmatter`]), provenance and trust scoring ([`provenance`],
//! [`trust`]), JCS-canonical hashing ([`history`]), job types and the [`QueueStore`] trait
//! ([`job`]), storage traits ([`DocumentStore`], [`IndexStore`], [`VectorStore`]),
//! ACL evaluation ([`acl`]), and error types ([`error`]).
//!
//! ## Multi-tenancy invariant
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
/// History hash module for the Copy-on-Write (CoW) versioning scheme.
///
/// Contains [`history::HISTORY_EXCLUDED_FIELDS`] and [`history::sha256_for_history`].
pub mod history;
pub mod identity;
pub mod index;
pub mod index_store;
/// Primitive types for the job system.
///
/// Contains [`Job`], [`JobRecord`], [`JobSpec`], [`JobClass`], [`JobMode`],
/// [`JobScope`], [`JobPriority`], [`QueueStore`], [`QueueEvent`], [`DryRunAware`]
/// and all associated types of the job pipeline.
pub mod job;
pub mod metric_sample;
pub mod note;
pub mod overrides;
/// SSOT helpers pour les chemins canoniques du layout Gradatum.
///
/// Toute dérivation de chemin depuis `storage.root` ou `vault_dir` DOIT passer par ces helpers.
/// Voir [`paths::vault_index_path`], [`paths::vault_dir_index_path`] et [`paths::queue_db_path`].
pub mod paths;
pub mod project_map;
pub mod provenance;
pub mod scheduled_health;
pub mod schema_registry;
pub mod scope;
pub mod secrets;
pub mod section;
pub mod soul;
pub mod status;
pub mod tag;
pub mod temporal_query;
pub mod trust;
pub mod vector_store;
pub mod write_check;

pub use job::{
    // Source structs nouveaux variants v59
    ConflictStrategy,
    // Specs variants actifs
    CurateSpec,
    // Distillation sémantique F-22
    DistillMode,
    DistillSource,
    // Traits
    DryRunAware,
    EmbedSpec,
    ExportFormat,
    ExportSource,
    // Forget sémantique F-44
    ForgetScope,
    ForgetSpec,
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
    // Ordre de tri (F-37 studio jobs page)
    JobOrder,
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
    // Purge lifecycle F-32C
    PurgeMode,
    PurgeSpec,
    QueueError,
    // Queue events
    QueueEvent,
    QueueStore,
    // ReIndex
    ReIndexMode,
    RetryBackoff,
    TriggerCondition,
    TriggerSource,
    // Memory validation F-43
    ValidateSpec,
    // VaultScope alias
    VaultScope,
    job_kind_str,
};

pub use document_store::DocumentStore;
pub use index_store::{AuthorRow, IndexStore, Lineage, ReviewQueueRow, SearchHitRaw};
pub use temporal_query::{TimelineCursor, TimelineFilter, TimelineRow, parse_temporal_str_as_ms};
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
