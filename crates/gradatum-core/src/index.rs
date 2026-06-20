//! Gradatum index abstraction trait.
//!
//! ## Design
//!
//! `Index` is a **supertrait facade** composing the three granular traits:
//! - [`DocumentStore`] — CRUD notes
//! - [`IndexStore`] — FTS, overrides, checksums, composite scoring
//! - [`VectorStore`] — embeddings + cosine semantic search
//!
//! The `Index` trait lives in `gradatum-core` (not `gradatum-index`) so that
//! consumer crates (`gradatum-vault`, `gradatum-curator`) import the abstract trait,
//! not the concrete `SqliteIndex` implementation (in `gradatum-index`).
//!
//! Benefit: `gradatum-vault` does not depend on `gradatum-index` → no cycle.
//!
//! ## Backward compatibility
//!
//! `Index as _` remains functional for existing call sites. To access granular methods
//! (e.g. `list_file_checksums`), also import the relevant sub-trait
//! (`IndexStore as _`, `DocumentStore as _`, `VectorStore as _`).
//!
//! ## FileChecksumEntry
//!
//! Entry in the `file_checksums` table — per-file drift detection.
//! Detects files modified outside Gradatum without re-hashing the entire vault.
//! Strategy: (1) fast mtime + size check, (2) 4 KB partial hash, (3) full hash.
//!
//! ## Generic overrides
//!
//! `upsert_override_raw` / `get_override_raw` store any override payload as TOML
//! in the generic `note_overrides` table.
//! The schema is identified by (`override_type`, `schema_version`) and validated via
//! `gradatum-core::schema_registry`.
//!
//! ## Methods remaining concrete on `SqliteIndex` only
//!
//! The following `SqliteIndex` methods are NOT promoted to a trait at this version:
//! - `search_fts_with_snippet`: returns `SearchHitRaw` (a type in `gradatum-index`) —
//!   promotion deferred pending a stable public type.
//! - Admin/lifecycle methods (`downgrade_note`, `patch_note_status`, `list_notes`, etc.):
//!   operational semantics, no trait-based consumer planned at this time.
//! - Seed/bench methods: test/perf utilities, outside the public contract.

use serde::{Deserialize, Serialize};

use crate::document_store::DocumentStore;
use crate::index_store::IndexStore;
use crate::vector_store::VectorStore;

/// Legacy facade — combination of the three storage traits.
///
/// Retained for compatibility with existing call sites.
/// New consumers SHOULD depend on the granular traits
/// (`DocumentStore` / `IndexStore` / `VectorStore`) — more precise and forwards-compatible.
///
/// ## Blanket impl
///
/// Any type implementing the three sub-traits automatically implements `Index`.
/// No manual `impl Index for T` is needed.
pub trait Index: DocumentStore + IndexStore + VectorStore {}

/// Blanket impl: any type implementing the three sub-traits becomes an `Index`.
impl<T: DocumentStore + IndexStore + VectorStore + ?Sized> Index for T {}

// ── TemporalIndex ────────────────────────────────────────────────────────────

/// Source of the temporal anchor in the `temporal_index` table.
///
/// Stored as a `&'static str` in the DB (`anchor_src` column) for readability.
/// Resolution priority (descending): `OccurredAt > EventDate > ValidFrom > Created`.
///
/// Fallback: `Created` is always available (`notes.created` is NOT NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnchorSrc {
    /// `occurred_at` field found in `frontmatter.extra` (ExtraFields YAML).
    #[serde(rename = "occurred_at")]
    OccurredAt,
    /// `event-date` field found in `frontmatter.extra` (ExtraFields YAML).
    #[serde(rename = "event-date")]
    EventDate,
    /// `valid_from` field found in `frontmatter.extra` (ExtraFields YAML).
    #[serde(rename = "valid_from")]
    ValidFrom,
    /// Fallback: `notes.created` timestamp (always available).
    #[serde(rename = "created")]
    Created,
}

impl AnchorSrc {
    /// Stable string representation for DB storage.
    ///
    /// These strings correspond to the CHECK constraint values in migration 0013.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::OccurredAt => "occurred_at",
            Self::EventDate => "event-date",
            Self::ValidFrom => "valid_from",
            Self::Created => "created",
        }
    }
}

/// Entry in the `temporal_index` table — temporal anchor for a note.
///
/// ## Design
///
/// Derived table — all data is computable from `notes` + frontmatter.
///
/// ## Logical reference (no FK cascade)
///
/// `note_id` is a logical reference, NOT a FOREIGN KEY.
/// Deletion must be explicit via `DELETE FROM temporal_index WHERE note_id = ?`
/// in `delete_note_from_index` — do not rely on ON DELETE CASCADE.
///
/// ## Temporal anchor (descending priority)
///
/// 1. `occurred_at` in `frontmatter.extra`
/// 2. `event-date` in `frontmatter.extra`
/// 3. `valid_from` in `frontmatter.extra`
/// 4. `notes.created` (universal fallback)
///
/// ## `valid_until_ms`
///
/// Reserved for temporal windowing in a future release. `None` in the current version.
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalEntry {
    /// Note ULID (primary key of `temporal_index`).
    pub note_id: String,

    /// Tenant of the note.
    pub vault_id: String,

    /// Temporal anchor in UTC epoch milliseconds.
    pub anchor_ms: i64,

    /// Source used to compute `anchor_ms`.
    pub anchor_src: AnchorSrc,

    /// CoALA temporal axis of the note (`"Static"` | `"Event"` | `"Versioned"`).
    ///
    /// Derived from `notes.doc_kind` (migration 0008). `"Versioned"` is reserved.
    pub doc_kind: String,

    /// Optional upper bound in UTC epoch ms (reserved for temporal windowing).
    pub valid_until_ms: Option<i64>,
}

/// Per-file drift detection entry.
///
/// Stored in the `file_checksums` table. Detects files modified outside Gradatum
/// by checking (mtime + size) before re-hashing the entire file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChecksumEntry {
    /// Relative path from the vault root (e.g. `"decisions/2026-05-04-my-note.md"`).
    pub relative_path: String,

    /// File type.
    pub file_kind: FileKind,

    /// Expected size in bytes.
    pub expected_size: u64,

    /// SHA-256 hash of the first 4 KB (fast check before full hash).
    ///
    /// Avoids reading a large file entirely to determine it has not changed.
    pub expected_hash_prefix_4kb: [u8; 32],

    /// Full SHA-256 hash of the file.
    pub expected_hash: [u8; 32],

    /// Expected Unix epoch mtime (seconds).
    pub expected_mtime: i64,

    /// Unix epoch timestamp of the last successful verification.
    pub last_verified: i64,
}

/// File category tracked in `file_checksums`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileKind {
    /// Markdown file for a note.
    Note,
    /// TOML file for an override.
    Override,
    /// Vault configuration file.
    Config,
}

/// Full note record returned by [`DocumentStore::get_note`].
///
/// Portable struct defined in `gradatum-core` to allow use via the `DocumentStore` trait
/// without a dependency on `gradatum-index`.
#[derive(Debug, Clone)]
pub struct NoteRecord {
    /// Note ULID.
    pub id: String,
    /// Vault identifier.
    pub vault_id: String,
    /// Thematic section (e.g. `"decisions"`, `"architecture"`).
    pub section: String,
    /// Physical sub-tenant locus (e.g. `"knowledge/rust"`), `None` when absent.
    ///
    /// Populated by `get_note` to allow `read_note` to resolve the on-disk path
    /// `<tenant>/<locus>/<id>.md` after a physical relocation (`move_locus`).
    /// Without this field, `read_note` could only resolve
    /// `<tenant>/<id>.md` and `<tenant>/<section>/<id>.md`.
    pub locus: Option<String>,
    /// Note status (e.g. `"live"`, `"pending-review"`).
    pub status: String,
    /// Full Markdown body.
    pub body_text: String,
    /// Note author (display name or ID, may be absent).
    pub author: Option<String>,
    /// Space-separated tags (from `notes.tags`, migration 0003).
    pub tags_raw: Option<String>,
    /// SHA-256 content hash (32 bytes) — used for hex computation in handlers.
    pub content_hash: Vec<u8>,
    /// Creation timestamp (epoch ms).
    pub created: i64,
    /// Last-updated timestamp (epoch ms, may be absent).
    pub updated: Option<i64>,
    /// Markdown H1 title of the note (extracted at curate time, may be absent).
    ///
    /// Populated by migration 0005 (backfill) and updated on every curate pass.
    /// `None` if the note has no `# …` line in the first position.
    pub title: Option<String>,
}
