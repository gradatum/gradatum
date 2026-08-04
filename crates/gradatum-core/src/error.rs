//! Gradatum error taxonomy.
//!
//! See ARCHITECTURE.md for the error model design.
//!
//! ## Hierarchy
//!
//! - `GradatumError` — top-level error for L1+ crate consumers
//! - `ValidationError` — incoming data validation (Tag, VaultId, etc.)
//! - `DriftError` — divergence detected between Markdown source and SQLite index
//! - `ConfigError` — TOML config loading/parsing (see `config.rs`)
//! - `schema_registry::ValidationError` — override payload validation against schema
//! - `schema_registry::MigrationError` — override payload migration
//!
//! ## Strong typing
//!
//! No `Box<dyn Error>` in public library code.
//! All errors are typed via `thiserror`.
//!
//! ## NFS constraint
//!
//! `GradatumError::VaultOnNfs` — the vault cannot be mounted on NFS.
//! Detection via `nix::sys::statfs::statfs` + `STATFS_TYPE == NFS_SUPER_MAGIC`.
//! Check triggered during `gradatum-vault::VaultConfig::validate()`.

use std::path::PathBuf;
use thiserror::Error;

use crate::frontmatter::SchemaVersion;
use crate::identity::{ContentHash, NoteId};
use crate::scope::VaultId;
use crate::status::NoteStatus;

/// Top-level error for `gradatum-core`.
///
/// Produced by L0 layers and returned to L1+ consumers.
/// Each variant maps to a specific architectural layer.
#[derive(Debug, Error)]
pub enum GradatumError {
    /// Incoming data validation error.
    #[error("validation error: {0}")]
    Validation(#[from] ValidationError),

    /// Drift detected between on-disk Markdown and the SQLite index.
    #[error("drift detected: {0}")]
    Drift(#[from] DriftError),

    /// Storage error (SQLite, OpenDAL, filesystem).
    ///
    /// Storage layers map their specific errors via `GradatumError::Storage`.
    #[error("storage error: {0}")]
    Storage(String),

    /// Markdown parsing error.
    #[error("markdown parse error: {0}")]
    Markdown(String),

    /// Note not found in the index.
    #[error("note not found: {0:?}")]
    NoteNotFound(NoteId),

    /// Invalid status transition — does not respect the lifecycle state machine.
    #[error("invalid status transition: {from:?} → {to:?}")]
    InvalidStatusTransition {
        /// Source status (before the transition).
        from: NoteStatus,
        /// Rejected target status.
        to: NoteStatus,
    },

    /// Frontmatter schema version mismatch.
    #[error("incorrect schema version: expected {expected}, found {found}")]
    SchemaVersionMismatch {
        /// Version expected by the current crate.
        expected: SchemaVersion,
        /// Version found in the frontmatter.
        found: SchemaVersion,
    },

    /// Vault not found in configuration.
    #[error("vault not found: {0:?}")]
    VaultNotFound(VaultId),

    /// Vault mounted on NFS — not supported.
    ///
    /// The vault must reside on a local filesystem. Detection uses
    /// `nix::sys::statfs::statfs` and compares against `NFS_SUPER_MAGIC`.
    #[error("vault root on NFS (NFS_SUPER_MAGIC), not supported: {path:?}")]
    VaultOnNfs {
        /// Vault root path whose `statfs` returned `NFS_SUPER_MAGIC`.
        path: PathBuf,
    },

    /// Override payload validation error against the schema registry.
    #[error("override schema validation: {0}")]
    SchemaValidation(#[from] crate::schema_registry::ValidationError),

    /// Override payload migration error.
    #[error("override schema migration: {0}")]
    SchemaMigration(#[from] crate::schema_registry::MigrationError),

    /// I/O error (file read/write, permissions, etc.).
    #[error("io : {0}")]
    Io(#[from] std::io::Error),

    /// TOML parsing error.
    #[error("toml parse : {0}")]
    TomlParse(#[from] toml::de::Error),

    /// TOML serialisation error.
    #[error("toml serialize : {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// Configuration error (TOML loading, field validation).
    #[error("config : {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Inference error (embedding, reranker, LLM).
    ///
    /// Dedicated variant for embedder/reranker failures.
    /// Allows handlers (e.g. `vault_search`) to distinguish an inference outage
    /// from a storage error and degrade gracefully (BM25 fallback instead of 500).
    ///
    /// `From<EmbedError>` is implemented in `gradatum-embed::error`
    /// to respect orphan rules.
    ///
    /// Recommended HTTP mapping: 503 Service Unavailable — though production handlers
    /// may prefer a graceful fallback (200 + BM25 only) with a warning log.
    #[error("inference : {0}")]
    Inference(String),

    // ── Semantic HTTP variants (for the server layer / MCP logic) ────────────
    /// Unauthenticated request — missing or invalid token.
    ///
    /// HTTP mapping: 401 Unauthorized.
    #[error("not authenticated")]
    Unauthorized,

    /// Access denied — an ACL denial or a cross-tenant violation.
    ///
    /// HTTP mapping: 403 Forbidden.
    #[error("access denied: {0}")]
    Forbidden(String),

    /// Invalid input data — handler-side validation failed.
    ///
    /// HTTP mapping: 400 Bad Request.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Write conflict — optimistic lock or unique constraint.
    ///
    /// HTTP mapping: 409 Conflict.
    #[error("conflict: {0}")]
    Conflict(String),
}

/// Incoming data validation error.
///
/// Returned by constructors that validate the format of newtype values
/// (e.g. [`crate::tag::Tag::new`], [`crate::scope::VaultId::parse`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Malformed tag (invalid format or too long).
    ///
    /// Expected format: `^[a-z0-9][a-z0-9-]{0,63}$`
    #[error("invalid tag: {0:?} (expected format: ^[a-z0-9][a-z0-9-]{{0,63}}$)")]
    InvalidTag(String),

    /// Malformed vault ID.
    #[error("invalid vault_id: {0:?}")]
    InvalidVaultId(String),

    /// Malformed locus ID.
    #[error("invalid locus_id: {0:?}")]
    InvalidLocusId(String),

    /// Malformed agent ID (credential-borne identity).
    #[error("invalid agent_id: {0:?}")]
    InvalidAgentId(String),

    /// Invalid section.
    #[error("invalid section: {0:?}")]
    InvalidSection(String),

    /// Invalid status.
    #[error("invalid status: {0:?}")]
    InvalidStatus(String),

    /// Empty note body.
    #[error("empty note body")]
    EmptyBody,

    /// Business constraint violated (semantically invalid input).
    ///
    /// Used for rules beyond format validation (e.g. self-reference
    /// `replaced_by == note_id`). Distinct from the format variants above.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Drift detection error between Markdown and the SQLite index.
///
/// Produced by `Note::verify_integrity()` and the drift detector (`gradatum-vault`).
/// Triggers a re-parse + re-index + re-embed of the affected file.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum DriftError {
    /// Recomputed `ContentHash` differs from the hash stored in SQLite.
    ///
    /// Likely cause: the Markdown file was edited outside of Gradatum.
    /// Action: re-parse + re-index by the worker.
    #[error("content hash mismatch: stored={stored}, computed={computed}")]
    ContentHashMismatch {
        /// Hash stored in SQLite at the last known write.
        stored: ContentHash,
        /// Hash recomputed from the current Markdown file.
        computed: ContentHash,
    },

    /// Markdown file absent on disk for an indexed note.
    #[error("markdown file missing on disk: {note_id:?}")]
    NoteMdMissing {
        /// Identifier of the note whose `.md` file is missing.
        note_id: NoteId,
    },

    /// Orphaned Markdown file (no corresponding note in the index).
    #[error("orphaned markdown file: {0}")]
    OrphanMd(String),
}
