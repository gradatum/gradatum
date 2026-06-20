//! Distributed overrides — traits, types, and `FrontmatterPatch`.
//!
//! ## Design
//!
//! An override is a metadata modification for a note, scoped to a particular context
//! (`OverrideScope`) and stored in the generic `note_overrides` table.
//!
//! ### Traits
//!
//! - `Overridable`: Associated Types trait (Iterator/Future pattern).
//!   `type Patch` = delta to apply; `type Output` = type resulting from resolution.
//! - `OverridePayload`: storage contract for any override payload (TOML embed + discriminant).
//!
//! ### FrontmatterPatch
//!
//! `NoteMetadataOverride` lives in `gradatum-vault` and implements
//! `Overridable<Patch = FrontmatterPatch, Output = Frontmatter>`.
//! `FrontmatterPatch` lives here because both `gradatum-vault` and `gradatum-curator` construct it.
//!
//! ## Design Decisions
//!
//! - Associated Types for `Overridable` (composition-friendly, idiomatic Rust)
//! - One active override per `(note, scope, type)` + generic table + `OverridePayload` trait
//! - TOML-embedded schema registry

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::author::AuthorRef;
use crate::frontmatter::SchemaVersion;
use crate::identity::NoteId;
use crate::scope::OverrideScope;

/// Common metadata for any override — stored in dedicated columns in `note_overrides`.
///
/// The payload (concrete struct) is serialised as TOML in the `payload_toml` column
/// and identified by (`override_type`, `schema_version`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideMeta {
    /// Note to which the override applies.
    pub note_id: NoteId,

    /// Override scope — determines resolution priority.
    pub scope: OverrideScope,

    /// Resolution priority: a higher-priority override wins on conflict.
    ///
    /// `0` = default priority. Used by `OverrideResolver` to arbitrate conflicts.
    pub priority: u8,

    /// Author of the override (human, agent, or system).
    pub created_by: AuthorRef,

    /// Override creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Optional reason (audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Override resolution trait — composition via Associated Types.
///
/// Pattern identical to `Iterator` (`type Item`) and `Future` (`type Output`):
/// downstream crates can constrain `T: Overridable<Patch = FrontmatterPatch>` without
/// naming the concrete override type.
///
/// ## Reference implementation
///
/// `gradatum-vault::NoteMetadataOverride`:
/// ```text
/// impl Overridable for NoteMetadataOverride {
///     type Patch  = FrontmatterPatch;
///     type Output = Frontmatter;
///     fn resolve(base: &Frontmatter, patch: &FrontmatterPatch) -> Frontmatter { … }
/// }
/// ```
pub trait Overridable {
    /// Type representing the delta to apply on the base value.
    type Patch;
    /// Type of the value resulting from applying the patch.
    type Output;

    /// Applies `patch` to `base` and returns the resolved value.
    ///
    /// Pure and side-effect-free — `base` is not modified.
    fn resolve(base: &Self::Output, patch: &Self::Patch) -> Self::Output;
}

/// Storage contract for an override payload in the `note_overrides` table.
///
/// Every override payload struct implements this trait to register with
/// the schema registry and round-trip through TOML.
///
/// ## Implementation
///
/// ```rust,ignore
/// impl OverridePayload for NoteMetadataOverride {
///     const OVERRIDE_TYPE: &'static str = "metadata";
///     const SCHEMA_VERSION: SchemaVersion = 1;
/// }
/// ```
pub trait OverridePayload: Sized + Serialize + for<'de> Deserialize<'de> {
    /// Stable discriminant used in the `override_type` SQL column.
    ///
    /// Must match the file `schemas/overrides/<OVERRIDE_TYPE>-v<SCHEMA_VERSION>.toml`.
    const OVERRIDE_TYPE: &'static str;

    /// Current schema version — must match the embedded TOML registry.
    const SCHEMA_VERSION: SchemaVersion;

    /// Serialises the payload to pretty TOML for database storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the type contains values not serialisable as TOML.
    fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Deserialises the payload from TOML (database read).
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML is malformed or incompatible with the type.
    fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

/// Metadata patch for `NoteMetadataOverride`.
///
/// All fields are optional — only present fields are applied during resolution.
/// Enables partial patches without re-specifying the entire frontmatter.
///
/// Lives in `gradatum-core` (not `gradatum-vault`) because:
/// - `gradatum-vault` constructs it when writing overrides.
/// - `gradatum-curator` constructs it during categorisation decisions.
/// - Avoids a `curator → vault` dependency (anti-cycle).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrontmatterPatch {
    /// New section to apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<crate::section::Section>,

    /// Tags to add to the current list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_add: Vec<crate::tag::Tag>,

    /// Tags to remove from the current list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_remove: Vec<crate::tag::Tag>,

    /// New status to apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::status::NoteStatus>,

    /// Author override (e.g. to correct an erroneous attribution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_override: Option<AuthorRef>,

    /// Reason for the status change (audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}
