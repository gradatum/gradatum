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
    #[error("erreur de validation : {0}")]
    Validation(#[from] ValidationError),

    /// Drift detected between on-disk Markdown and the SQLite index.
    #[error("drift détecté : {0}")]
    Drift(#[from] DriftError),

    /// Storage error (SQLite, OpenDAL, filesystem).
    ///
    /// Storage layers map their specific errors via `GradatumError::Storage`.
    #[error("erreur de stockage : {0}")]
    Storage(String),

    /// Markdown parsing error.
    #[error("erreur parse Markdown : {0}")]
    Markdown(String),

    /// Note not found in the index.
    #[error("note introuvable : {0:?}")]
    NoteNotFound(NoteId),

    /// Invalid status transition — does not respect the lifecycle state machine.
    #[error("transition de statut invalide : {from:?} → {to:?}")]
    InvalidStatusTransition {
        /// Source status (before the transition).
        from: NoteStatus,
        /// Rejected target status.
        to: NoteStatus,
    },

    /// Frontmatter schema version mismatch.
    #[error("version de schéma incorrecte : attendu {expected}, trouvé {found}")]
    SchemaVersionMismatch {
        /// Version expected by the current crate.
        expected: SchemaVersion,
        /// Version found in the frontmatter.
        found: SchemaVersion,
    },

    /// Vault not found in configuration.
    #[error("vault introuvable : {0:?}")]
    VaultNotFound(VaultId),

    /// Vault mounted on NFS — not supported.
    ///
    /// The vault must reside on a local filesystem. Detection uses
    /// `nix::sys::statfs::statfs` and compares against `NFS_SUPER_MAGIC`.
    #[error("vault root sur NFS (NFS_SUPER_MAGIC), non supporté : {path:?}")]
    VaultOnNfs {
        /// Vault root path whose `statfs` returned `NFS_SUPER_MAGIC`.
        path: PathBuf,
    },

    /// Override payload validation error against the schema registry.
    #[error("validation schéma override : {0}")]
    SchemaValidation(#[from] crate::schema_registry::ValidationError),

    /// Override payload migration error.
    #[error("migration schéma override : {0}")]
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
}

/// Incoming data validation error.
///
/// Returned by constructors that validate the format of newtype values
/// (e.g. `Tag::new()`, `VaultId::validate()`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Malformed tag (invalid format or too long).
    ///
    /// Expected format: `^[a-z0-9][a-z0-9-]{0,63}$`
    #[error("tag invalide : {0:?} (format attendu: ^[a-z0-9][a-z0-9-]{{0,63}}$)")]
    InvalidTag(String),

    /// Malformed vault ID.
    #[error("vault_id invalide : {0:?}")]
    InvalidVaultId(String),

    /// Malformed locus ID.
    #[error("locus_id invalide : {0:?}")]
    InvalidLocusId(String),

    /// Invalid section.
    #[error("section invalide : {0:?}")]
    InvalidSection(String),

    /// Invalid status.
    #[error("statut invalide : {0:?}")]
    InvalidStatus(String),

    /// Empty note body.
    #[error("corps de note vide")]
    EmptyBody,

    /// Business constraint violated (semantically invalid input).
    ///
    /// Used for rules beyond format validation (e.g. self-reference
    /// `replaced_by == note_id`). Distinct from the format variants above.
    #[error("input invalide : {0}")]
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
    #[error("fichier md absent sur disque : {note_id:?}")]
    NoteMdMissing {
        /// Identifier of the note whose `.md` file is missing.
        note_id: NoteId,
    },

    /// Orphaned Markdown file (no corresponding note in the index).
    #[error("fichier md orphelin : {0}")]
    OrphanMd(String),
}
