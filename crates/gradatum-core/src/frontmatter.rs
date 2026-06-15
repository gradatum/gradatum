//! Canonical frontmatter for a Gradatum note.
//!
//! ## Design
//!
//! - `Frontmatter`: main struct serialised as YAML in the `.md` header.
//! - `ExtraFields`: unknown fields preserved verbatim for forward compatibility.
//!   Lazily allocated — `None` when no extra fields are present (avoids heap allocation).
//! - `tags: SmallVec<[Tag; 4]>` — inline up to 4 tags without heap allocation.
//!
//! ## Multi-tenancy
//!
//! `vault_id` is **mandatory** — every note must belong to a tenant.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::BTreeMap;

use crate::author::AuthorRef;
use crate::scope::{LocusId, VaultId};
use crate::section::Section;
use crate::status::NoteStatus;
use crate::tag::Tag;

/// Frontmatter schema version.
///
/// Incremented on every breaking format change. Enables forward-compatible migration
/// via `SchemaVersion::CURRENT` + `match schema_version { 1 => …, 2 => … }`.
pub type SchemaVersion = u32;

/// Unknown frontmatter fields preserved verbatim — lazily allocated.
///
/// Allows v1.x legacy frontmatters to round-trip without loss,
/// even when non-canonical custom fields are present.
///
/// ## Performance
///
/// `Option<Box<BTreeMap<…>>>` avoids any heap allocation for notes with no extra fields.
/// The `Box` reduces the size of `Frontmatter` in the `None` case (lazy allocation).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtraFields(pub Option<Box<BTreeMap<String, toml::Value>>>);

impl ExtraFields {
    /// Returns `true` when no extra fields are present.
    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(|m| m.is_empty())
    }

    /// Constructs an empty `ExtraFields` without allocating.
    pub fn empty() -> Self {
        Self(None)
    }

    /// Inserts an extra field, allocating the inner map on first use.
    ///
    /// # JCS constraint
    ///
    /// **`toml::Value::Datetime` is FORBIDDEN** in `ExtraFields` when the note
    /// will be hashed via [`crate::identity::ContentHash::compute`].
    /// The `Datetime` variant produces a non-portable JSON serialisation in
    /// `toml 0.8.x` (internal representation `{"$__toml_private_datetime": …}`),
    /// which breaks the "bit-identical hash across languages" guarantee.
    ///
    /// To store a datetime in `ExtraFields`, use a raw ISO 8601 string
    /// (`toml::Value::String("2026-05-04T10:00:00Z".to_string())`)
    /// instead of `toml::Value::Datetime(…)`.
    ///
    /// A future improvement will replace `toml::Value` with `serde_json::Value`,
    /// eliminating this constraint by construction.
    pub fn insert(&mut self, k: String, v: toml::Value) {
        self.0.get_or_insert_with(Default::default).insert(k, v);
    }

    /// Retrieves the value of an extra field.
    pub fn get(&self, k: &str) -> Option<&toml::Value> {
        self.0.as_ref().and_then(|m| m.get(k))
    }
}

/// Canonical frontmatter for a Gradatum note.
///
/// Serialised as YAML in the `---\n…\n---\n` header of the Markdown file.
/// Authoritative source for the `ContentHash` (invariant #1).
///
/// ## Optional fields
///
/// Fields annotated with `skip_serializing_if` are omitted when absent,
/// keeping minimalist frontmatters readable.
///
/// ## Compatibility
///
/// Unknown YAML fields (e.g. from the legacy v1.x predecessor) are caught by
/// `#[serde(flatten)]` in the YAML fixture. `gradatum-markdown` handles the catch
/// at parser level — `Frontmatter` itself exposes `extra: ExtraFields`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    /// Frontmatter schema version. Incremented on breaking changes.
    pub schema_version: SchemaVersion,

    /// Mandatory tenant identifier. UI alias: `vault`.
    pub vault_id: VaultId,

    /// Optional sub-vault ACL scope. `None` = vault root scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locus: Option<LocusId>,

    /// Canonical section of the note.
    pub section: Section,

    /// Lifecycle status.
    pub status: NoteStatus,

    /// Optional reason for the current status (for audit trail).
    ///
    /// Example: `"rejected by curator: low novelty"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,

    /// Timestamp of the last status change.
    ///
    /// Used by decay/cleanup cron queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_changed: Option<DateTime<Utc>>,

    /// Note tags — inlined in a `SmallVec` up to 4 tags without heap allocation.
    ///
    /// Most notes carry fewer than 4 tags — the inline `SmallVec` eliminates heap allocation
    /// for the vast majority of notes.
    #[serde(default, skip_serializing_if = "SmallVec::is_empty")]
    pub tags: SmallVec<[Tag; 4]>,

    /// Note author.
    ///
    /// Optional for compatibility with legacy v1.x notes that have no author field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<AuthorRef>,

    /// Creation timestamp (immutable after the first commit).
    pub created: DateTime<Utc>,

    /// Last-modified timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,

    /// Unknown TOML fields preserved verbatim for forward compatibility.
    ///
    /// Lazily allocated — no heap allocation when absent.
    /// Omitted in serialisation when empty.
    #[serde(default, skip_serializing_if = "ExtraFields::is_empty")]
    pub extra: ExtraFields,

    /// Provenance source of the note (String, JCS-safe).
    ///
    /// Examples: `"human-decision"`, `"agent-log"`, `"qa-event"`, `"web-scraped"`.
    ///
    /// ## ContentHash invariant
    ///
    /// This field is typed as `String` (NOT `f32`) to remain JCS-compatible.
    /// The `trust` score (float) lives **only** in the `index.db notes.trust REAL` column
    /// (authoritative for scoring) and never belongs in this struct.
    /// Reason: `serde_jcs` panics on `f32::NAN` — any float in `Frontmatter` breaks
    /// `ContentHash::compute` (hash uniqueness invariant, see `identity.rs`).
    ///
    /// Optional — omitted in serialisation when absent (preserves backward compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,

    // ── Semantic Forget ──────────────────────────────────────────────────────
    /// Forgotten flag (bool, JCS-safe).
    ///
    /// `true` when the note is marked forgotten. Absent when `false` (compact serialisation —
    /// notes that are not forgotten do not carry this field).
    ///
    /// ## Scoring effect
    ///
    /// `forgotten = true` triggers exponential score decay:
    /// `score × 0.5^elapsed_days` (half-life 1 day), applied BEFORE the downgraded penalty —
    /// the two penalties are never applied together (see `sqlite.rs::search_fts_scored`).
    ///
    /// ## Hash impact
    ///
    /// Forgetting/unforgetting modifies this field → changes the `sha256_for_history` hash →
    /// triggers a CoW snapshot in `.history/`. This is intentional: a forget is a traceable event.
    /// This field is NOT in `HISTORY_EXCLUDED_FIELDS`.
    ///
    /// ## JCS invariant
    ///
    /// Type `bool` (not `f32`) — JCS-safe, does not break `ContentHash::compute`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten: Option<bool>,

    /// Forgotten timestamp (UTC epoch, JCS-safe).
    ///
    /// `None` when the note is not forgotten or has never been forgotten.
    /// Used by decay computation: `elapsed_days = (now_ms − forgotten_at) / 86_400_000.0`.
    ///
    /// ## Index synchronisation
    ///
    /// The `notes.forgotten_at` column (INTEGER epoch ms) is the authoritative source for
    /// scoring. This frontmatter value is synchronised during `mark_forgotten`
    /// (via the vault) — not directly by index methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_at: Option<DateTime<Utc>>,

    /// Identifier of the actor who set the forgotten mark.
    ///
    /// Optional — `None` when not supplied. Enables auditability (who forgot what).
    /// Examples: `"main-agent"`, `"operator-1"`, `"vault-curator"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_by: Option<String>,
}
