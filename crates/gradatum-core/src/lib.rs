//! # gradatum-core
//!
//! Shared primitives: traits, canonical types, errors. The L0 crate every other Gradatum crate depends on.
//!
//! ## Stability
//!
//! `1.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `1.x`, breaking changes deferred to the next major. The
//! finer-grained trait-stability tiers described in RELEASE-POLICY.md are **not applied
//! in `1.0.0`**: no trait in this crate carries a `stability::` attribute, so treat the
//! whole public surface as SemVer-strict.
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Contents
//!
//! Shared primitives used across all Gradatum crates: note identity ([`identity`]),
//! canonical frontmatter ([`frontmatter`]), provenance and trust scoring ([`provenance`],
//! [`trust`]), JCS-canonical hashing ([`history`]), job types and the [`QueueStore`] trait
//! ([`job`]), storage traits ([`DocumentStore`], [`IndexStore`], [`VectorStore`]),
//! and error types ([`error`]).
//!
//! ACL evaluation is **not** part of this crate: it lives in `gradatum-acl-policy`
//! (`AclEngine::evaluate`, locus globs with deny-wins).
//!
//! ## Multi-tenancy invariant
//!
//! Note-scoped rows carry `vault_id TEXT NOT NULL` (default vault: `"main"`).
//! Tenant scoping is carried by `tenant_id` on the credential, job and grant
//! tables (`api_keys`, `gradatum_jobs`, `tenant_vault_grants`, `session_trace`,
//! `event_log`, `note_usage`). Operational tables (`metric_sample`,
//! `scheduled_task_health`, `file_checksums`, `proactive_recall_*`) carry
//! neither column and are not tenant-scoped.
//! ACL is evaluated on identity + locus globs, then vault access is attested
//! via `AclCheckedVaultId`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

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
/// Single source of truth for the canonical paths of the Gradatum on-disk layout.
///
/// Every path derived from `storage.root` or from a `vault/` directory MUST go through
/// these helpers.
/// See [`paths::vault_index_path`], [`paths::vault_dir_index_path`] and [`paths::queue_db_path`].
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
    spec_tenant,
};

pub use document_store::DocumentStore;
pub use index_store::{AuditScanRow, AuthorRow, IndexStore, Lineage, ReviewQueueRow, SearchHitRaw};
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
