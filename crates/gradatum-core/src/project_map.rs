//! project-map — typed-wikilink schema for traceable work units.
//!
//! A note in section [`Section::ProjectMap`](crate::section::Section::ProjectMap)
//! is a **traceable work unit** (feature/fix/task…) whose lifecycle state is
//! carried by a **typed wikilink schema** instead of duplicated frontmatter fields.
//! This module provides **pure parsing** (`[[role:value]]` → [`ProjectMapLink`])
//! and **cardinality validation** ([`validate_links`]), with no I/O.
//!
//! ## Link convention
//!
//! The "type" of a link is its **reserved prefix** before `:`. Reserved prefixes
//! are: `project` · `status` · `kind` · `version` · `spec` · `plan` · `context` ·
//! `feature` · `release` · `supersedes` · `parent`. Any other prefix (e.g.
//! `decisions:`) is a **content dependency** ([`ProjectMapLink::Dep`]) that reuses
//! the existing `[[section:ULID]]` format without regression.
//!
//! | Role | Form | Variant |
//! |---|---|---|
//! | project | `[[project:gradatum]]` | [`ProjectMapLink::Project`] |
//! | status | `[[status:DONE]]` | [`ProjectMapLink::Status`] |
//! | kind | `[[kind:FIX]]` | [`ProjectMapLink::Kind`] |
//! | version | `[[version:gradatum/0.6.1]]` | [`ProjectMapLink::Version`] |
//! | annex | `[[spec:…]]` `[[plan:…]]` `[[context:…]]` | [`ProjectMapLink::Annex`] |
//! | feature | `[[feature:F-<n>]]` | [`ProjectMapLink::Feature`] |
//! | release | `[[release:planned]]` | [`ProjectMapLink::Release`] |
//! | supersedes | `[[supersedes:F-<n>]]` | [`ProjectMapLink::Supersedes`] |
//! | parent | `[[parent:F-<n>]]` | [`ProjectMapLink::Parent`] |
//! | dependency | `[[decisions:01K…]]` | [`ProjectMapLink::Dep`] |
//!
//! ## Feature cards
//!
//! The presence of at least one `[[feature:F-XX]]` marks the note as a **feature
//! card**: [`validate_links`] then requires exactly one `feature` and one `release`.
//! Notes without a `feature` link (changelog entries) keep the original validation
//! rules unchanged, and are not allowed to carry a `[[release:]]` link at all.
//!
//! ## Case sensitivity
//!
//! `status` and `kind` values are normalised to **SCREAMING_SNAKE** on the wire
//! to prevent case-sensitive bugs (`[[status:done]]` rejected, `[[status:DONE]]`
//! accepted). `release` values are **lowercase** on the wire (mirror of the site
//! enum) : `[[release:planned]]` accepted, `[[release:PLANNED]]` rejected.
//!
//! ## Validation design
//!
//! - The project-map validator ([`validate_links`]) is **dedicated** to this schema
//!   and does not invoke a generic schema-registry subsystem.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Lifecycle status of a project-map work unit.
///
/// Wire values are SCREAMING_SNAKE-cased (e.g. `"IN_PROGRESS"`).
/// `BRAINSTORMING` is the upstream ideation state; `DONE` marks completion;
/// `OBSOLETE` marks abandonment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatusKind {
    /// Upstream ideation, not yet committed to.
    Brainstorming,
    /// Committed to, not started.
    Open,
    /// Under way.
    InProgress,
    /// Blocked by a dependency.
    Blocked,
    /// Completed.
    Done,
    /// Abandoned or superseded.
    Obsolete,
}

impl StatusKind {
    /// Parse the SCREAMING_SNAKE wire value of a `[[status:…]]` wikilink.
    ///
    /// Inverse of [`StatusKind::as_wire`]: it maps a raw wire value back to its
    /// variant, or `None` when the value is not part of the status vocabulary.
    /// Exposed so callers (e.g. the `project-map scope` counters) can classify a
    /// wire status against the authoritative vocabulary rather than re-hardcoding
    /// the list of accepted values.
    ///
    /// Matching is case-sensitive: `"DONE"` is accepted; `"done"` and `"Done"` are rejected.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "BRAINSTORMING" => Some(Self::Brainstorming),
            "OPEN" => Some(Self::Open),
            "IN_PROGRESS" => Some(Self::InProgress),
            "BLOCKED" => Some(Self::Blocked),
            "DONE" => Some(Self::Done),
            "OBSOLETE" => Some(Self::Obsolete),
            _ => None,
        }
    }

    /// Returns the SCREAMING_SNAKE wire representation (inverse of `from_wire`).
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Brainstorming => "BRAINSTORMING",
            Self::Open => "OPEN",
            Self::InProgress => "IN_PROGRESS",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Obsolete => "OBSOLETE",
        }
    }
}

/// Nature of a project-map work unit — drives CHANGELOG categorisation.
///
/// Wire values are SCREAMING_SNAKE-cased. `TASK` is the deliberate catch-all
/// (maintenance, tooling, bounded exploration, uncategorised work): no CHANGELOG
/// section distinguishes those sub-kinds, so splitting below `TASK` would add
/// vocabulary without adding a grouping. Separately — and orthogonally — only
/// `kind:FEATURE` reaches the public website (see [`validate_links`] and the
/// mirror-site filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KindKind {
    /// New capability (CHANGELOG "Added").
    Feature,
    /// Improvement to something that already exists (CHANGELOG "Changed").
    Enhancement,
    /// Bug fix (CHANGELOG "Fixed").
    Fix,
    /// Generic task — the deliberate catch-all (maintenance, tooling, exploration).
    ///
    /// Absorbs the retired `CHORE` / `SPIKE` vocabulary: those wire values have been
    /// removed for good — `KindKind::from_wire` returns `None` for them, and the
    /// `Chore` / `Spike` Rust variants no longer exist. Categorise former
    /// chore/spike work as [`KindKind::Task`].
    Task,
}

impl KindKind {
    /// Parses the SCREAMING_SNAKE wire value of a `[[kind:…]]` wikilink.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "FEATURE" => Some(Self::Feature),
            "ENHANCEMENT" => Some(Self::Enhancement),
            "FIX" => Some(Self::Fix),
            "TASK" => Some(Self::Task),
            _ => None,
        }
    }

    /// Returns the SCREAMING_SNAKE wire representation.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Feature => "FEATURE",
            Self::Enhancement => "ENHANCEMENT",
            Self::Fix => "FIX",
            Self::Task => "TASK",
        }
    }
}

/// Delivery (release) status of a project-map feature card.
///
/// This axis is **orthogonal** to the [`StatusKind`] lifecycle: `StatusKind` describes
/// how far the work has progressed, `ReleaseKind` describes where it sits on the
/// version roadmap. Unlike [`StatusKind`] and [`KindKind`], which are SCREAMING_SNAKE,
/// the wire values of `ReleaseKind` are **lowercase**, mirroring the public website
/// enumeration exactly (`released` / `planned` / `roadmap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReleaseKind {
    /// Considered for the long term, with no target version committed to.
    Roadmap,
    /// Planned for a target version.
    Planned,
    /// Delivered in a published version.
    Released,
    /// Dropped — replaced or cancelled.
    Dropped,
}

impl ReleaseKind {
    /// Parses the **lowercase** wire value of a `[[release:…]]` wikilink.
    ///
    /// Matching is case-sensitive: `"planned"` is accepted, `"PLANNED"` is rejected,
    /// which keeps the values aligned with the public website enumeration.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "roadmap" => Some(Self::Roadmap),
            "planned" => Some(Self::Planned),
            "released" => Some(Self::Released),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }

    /// Returns the **lowercase** wire representation (inverse of `from_wire`).
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Roadmap => "roadmap",
            Self::Planned => "planned",
            Self::Released => "released",
            Self::Dropped => "dropped",
        }
    }
}

/// Role of an annex link — detailed material **referenced, never duplicated**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnnexRole {
    /// Design specification (`[[spec:…]]`).
    Spec,
    /// Implementation plan (`[[plan:…]]`).
    Plan,
    /// Context or discussion (`[[context:…]]`).
    Context,
}

impl AnnexRole {
    /// Reserved prefix associated with this annex role.
    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Plan => "plan",
            Self::Context => "context",
        }
    }
}

/// A typed link of a project-map card, parsed from a `[[role:value]]` wikilink.
///
/// Produced by [`parse_link`]. Cardinality — 1 `Project`, 1 `Status`, 1 `Kind`,
/// at most 1 `Version`, any number of `Annex` and `Dep` — is checked by
/// [`validate_links`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectMapLink {
    /// `[[project:<name>]]` — the owning project (exactly 1 required).
    Project(String),
    /// `[[status:<STATUS>]]` — lifecycle status (exactly 1 required).
    Status(StatusKind),
    /// `[[kind:<KIND>]]` — nature of the work unit (exactly 1 required).
    Kind(KindKind),
    /// `[[version:<project>/<x.y.z>]]` — target version (0 or 1).
    Version {
        /// Namespaced project (the part before the `/`).
        project: String,
        /// Version number (the part after the `/`).
        version: String,
    },
    /// `[[spec|plan|context:<target>]]` — referenced annex (0..N).
    Annex {
        /// Role of the annex.
        role: AnnexRole,
        /// Raw target (a ULID or a path).
        target: String,
    },
    /// `[[<section>:<ULID>]]` — plain content dependency (0..N).
    Dep {
        /// Section of the target note.
        section: String,
        /// ULID of the target note.
        ulid: String,
    },
    /// `[[feature:F-XX]]` — identity of a feature card (exactly 1; also the discriminant).
    ///
    /// The presence of at least one `Feature` switches [`validate_links`] into
    /// feature-card mode, where a `release` link becomes mandatory.
    Feature(String),
    /// `[[release:<r>]]` — delivery status (1 on a feature card, 0 otherwise).
    Release(ReleaseKind),
    /// `[[supersedes:F-YY]]` — a feature this card replaces (0..N).
    ///
    /// Distinct from [`ProjectMapLink::Feature`]: it does **not** count towards the
    /// feature cardinality.
    Supersedes(String),
    /// `[[parent:F-YY]]` — the original feature this card continues (0..N).
    ///
    /// A continuation card points back at the feature it grew out of through this link.
    /// It does **not** count towards the feature cardinality, and several parents are
    /// allowed.
    Parent(String),
}

/// Schema error for project-map parsing or cardinality validation.
///
/// This error type is **dedicated** to the project-map schema and is distinct
/// from any generic schema-registry validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SchemaError {
    /// A link with no `role:` prefix that is not a `section:ULID` dependency either.
    #[error("link without typed prefix nor section:ULID format: {0:?}")]
    MissingPrefix(String),

    /// A `[[status:…]]` value outside the taxonomy.
    #[error(
        "invalid status {0:?} (expected SCREAMING_SNAKE ∈ BRAINSTORMING/OPEN/IN_PROGRESS/BLOCKED/DONE/OBSOLETE)"
    )]
    InvalidStatus(String),

    /// A `[[kind:…]]` value outside the taxonomy.
    #[error(
        "invalid kind {0:?} (expected SCREAMING_SNAKE ∈ FEATURE/ENHANCEMENT/FIX/TASK — CHORE and SPIKE were removed on 2026-08-19, use TASK)"
    )]
    InvalidKind(String),

    /// A malformed `[[version:…]]` link (expected `project/x.y.z`).
    #[error("malformed version {0:?} (expected project/x.y.z)")]
    MalformedVersion(String),

    /// Empty value after the prefix (e.g. `[[project:]]`).
    #[error("empty value for prefix {0:?}")]
    EmptyValue(String),

    /// A project or version value longer than 64 characters.
    #[error("value {1:?} too long for prefix {0:?} (max 64 chars, got {2})")]
    ValueTooLong(String, String, usize),

    /// A project or version value with characters outside `[a-z0-9._-]`.
    #[error("value {1:?} contains forbidden characters for prefix {0:?} (allowed: a-z 0-9 . _ -)")]
    InvalidChars(String, String),

    /// `project:` missing or repeated (exactly 1 required).
    #[error("exactly 1 project: link required, found {0}")]
    ProjectCardinality(usize),

    /// `status:` missing or repeated (exactly 1 required).
    #[error("exactly 1 status: link required, found {0}")]
    StatusCardinality(usize),

    /// `kind:` missing or repeated (exactly 1 required).
    #[error("exactly 1 kind: link required, found {0}")]
    KindCardinality(usize),

    /// `version:` repeated (at most 1 allowed).
    #[error("at most 1 version: link allowed, found {0}")]
    VersionCardinality(usize),

    /// Inconsistency: the project in `version:<project>/…` differs from `project:<project>`.
    #[error("namespaced version {version_project:?} ≠ project {project:?}")]
    VersionProjectMismatch {
        /// Project declared by `[[project:…]]`.
        project: String,
        /// Project namespaced inside `[[version:…]]`.
        version_project: String,
    },

    /// A `feature:`/`supersedes:` identifier outside the `F-\d{2,3}` format
    /// (e.g. `f-37`, `F-1`).
    #[error("invalid feature identifier {0:?} (expected F-NN or F-NNN, e.g. F-37, F-061)")]
    FeatureIdentInvalid(String),

    /// A `[[release:…]]` value outside the taxonomy.
    #[error("invalid release {0:?} (expected lowercase ∈ roadmap/planned/released/dropped)")]
    InvalidRelease(String),

    /// `feature:` repeated on a feature card (exactly 1 required).
    #[error("exactly 1 feature: link required on a feature card, found {0}")]
    FeatureCardinality(usize),

    /// Wrong number of `release:` links (1 on a feature card, 0 otherwise).
    #[error("incorrect number of release: links (1 if feature card, 0 otherwise), found {0}")]
    ReleaseCardinality(usize),

    /// `version:` link absent or appears more than once on a feature card (exactly 1 required).
    ///
    /// A feature card (carrying `[[feature:F-XX]]`) requires exactly 1 `[[version:]]`.
    /// Use the sentinel `[[version:<project>/backlog]]` when no concrete version is yet
    /// known.
    #[error(
        "feature card: exactly 1 [[version:]] required (or sentinel project/backlog), found {0}"
    )]
    FeatureVersionCardinality(usize),
}

/// Maximum length of a `project:` identifier or of a `version:` component.
///
/// Consistent with `Tag::normalize` (64 characters). Acts as a DoS safety cap.
const MAX_IDENT_LEN: usize = 64;

/// Checks that a project-map identifier respects the `[a-z0-9._-]` charset and the
/// [`MAX_IDENT_LEN`] length cap.
///
/// # Errors
///
/// - [`SchemaError::ValueTooLong`] if `value.len() > MAX_IDENT_LEN`.
/// - [`SchemaError::InvalidChars`] if any character falls outside `[a-z0-9._-]`.
fn validate_ident(prefix: &str, value: &str) -> Result<(), SchemaError> {
    if value.len() > MAX_IDENT_LEN {
        return Err(SchemaError::ValueTooLong(
            prefix.to_string(),
            value.to_string(),
            value.len(),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(SchemaError::InvalidChars(
            prefix.to_string(),
            value.to_string(),
        ));
    }
    Ok(())
}

/// Checks that a `feature:`/`supersedes:`/`parent:` identifier respects the `F-\d{2,3}` format.
///
/// The format is exact — an uppercase `F`, a hyphen, then 2 or 3 ASCII digits — and
/// needs a dedicated parser rather than [`validate_ident`], which would reject the
/// uppercase `F`. Valid: e.g. `F-` followed by `37` or `061`. Invalid: `f-37`, `F-1`, `F-1234`, `feature37`.
///
/// The check is done character by character, so the crate needs no `regex` dependency.
///
/// # Errors
///
/// [`SchemaError::FeatureIdentInvalid`] if `value` does not match the format.
fn validate_feature_ident(value: &str) -> Result<(), SchemaError> {
    let invalid = || SchemaError::FeatureIdentInvalid(value.to_string());
    // `F-` + 2..=3 chiffres → longueur totale 4 ou 5.
    let Some(digits) = value.strip_prefix("F-") else {
        return Err(invalid());
    };
    if !matches!(digits.len(), 2 | 3) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    Ok(())
}

/// Splits a raw `role:value` target at the **first** `:`.
///
/// Returns `(prefix, value)`. When there is no `:` at all, `prefix` is the whole
/// string and `value` is empty — it is up to the caller to decide what that means.
fn split_prefix(target: &str) -> (&str, &str) {
    match target.split_once(':') {
        Some((p, v)) => (p, v),
        None => (target, ""),
    }
}

/// Parse a wikilink target `[[role:value]]` (without the surrounding `[[ ]]`) into a [`ProjectMapLink`].
///
/// `raw` is the **already-extracted** target (e.g. `"status:DONE"`), as returned
/// by `gradatum_curator::wikilinks::extract_wikilinks`.
///
/// ## Disambiguation rule
///
/// - A **reserved prefix** (`project`/`status`/`kind`/`version`/`spec`/`plan`/
///   `context`/`feature`/`release`/`supersedes`/`parent`) → typed structural or
///   annex link.
/// - Any other prefix in `<section>:<ULID>` form → [`ProjectMapLink::Dep`].
///
/// # Errors
///
/// - [`SchemaError::EmptyValue`] if the value after a reserved prefix is empty.
/// - [`SchemaError::InvalidStatus`] / [`SchemaError::InvalidKind`] /
///   [`SchemaError::InvalidRelease`] for values outside their taxonomy.
/// - [`SchemaError::MalformedVersion`] if `version:` is not `project/x.y.z`.
/// - [`SchemaError::FeatureIdentInvalid`] if `feature:`/`supersedes:`/`parent:` is not
///   `F-\d{2,3}`.
/// - [`SchemaError::MissingPrefix`] if the target has no `role:` prefix and is not a
///   valid `section:ULID` either.
#[must_use = "the parse result must be inspected"]
pub fn parse_link(raw: &str) -> Result<ProjectMapLink, SchemaError> {
    let raw = raw.trim();
    let (prefix, value) = split_prefix(raw);

    // Préfixes réservés : valeur obligatoire et non vide.
    let reserved = matches!(
        prefix,
        "project"
            | "status"
            | "kind"
            | "version"
            | "spec"
            | "plan"
            | "context"
            | "feature"
            | "release"
            | "supersedes"
            | "parent"
    );
    if reserved && value.is_empty() {
        return Err(SchemaError::EmptyValue(prefix.to_string()));
    }

    match prefix {
        "project" => {
            validate_ident("project", value)?;
            Ok(ProjectMapLink::Project(value.to_string()))
        }
        "status" => StatusKind::from_wire(value)
            .map(ProjectMapLink::Status)
            .ok_or_else(|| SchemaError::InvalidStatus(value.to_string())),
        "kind" => KindKind::from_wire(value)
            .map(ProjectMapLink::Kind)
            .ok_or_else(|| SchemaError::InvalidKind(value.to_string())),
        "version" => parse_version(value),
        "spec" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Spec,
            target: value.to_string(),
        }),
        "plan" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Plan,
            target: value.to_string(),
        }),
        "context" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Context,
            target: value.to_string(),
        }),
        "feature" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Feature(value.to_string()))
        }
        "release" => ReleaseKind::from_wire(value)
            .map(ProjectMapLink::Release)
            .ok_or_else(|| SchemaError::InvalidRelease(value.to_string())),
        "supersedes" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Supersedes(value.to_string()))
        }
        "parent" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Parent(value.to_string()))
        }
        // Préfixe non réservé : dépendance de contenu section:ULID.
        other => parse_dep(other, value, raw),
    }
}

/// Parse the value of a `[[version:<project>/<x.y.z>]]` wikilink.
///
/// The inner `/` separates the project namespace from the version string,
/// avoiding collision with the `:` prefix delimiter.
fn parse_version(value: &str) -> Result<ProjectMapLink, SchemaError> {
    // Exactement un `/` séparant projet et version, les deux non vides.
    match value.split_once('/') {
        Some((project, version)) if !project.is_empty() && !version.is_empty() => {
            validate_ident("version.project", project)?;
            validate_ident("version.version", version)?;
            Ok(ProjectMapLink::Version {
                project: project.to_string(),
                version: version.to_string(),
            })
        }
        _ => Err(SchemaError::MalformedVersion(value.to_string())),
    }
}

/// Parses a `section:ULID` content dependency (any non-reserved prefix).
///
/// `raw` is only used for the error message when the target has no prefix at all.
fn parse_dep(section: &str, ulid: &str, raw: &str) -> Result<ProjectMapLink, SchemaError> {
    if ulid.is_empty() {
        // Pas de `:` dans la cible (ou rien après) → ni typé, ni section:ULID.
        return Err(SchemaError::MissingPrefix(raw.to_string()));
    }
    Ok(ProjectMapLink::Dep {
        section: section.to_string(),
        ulid: ulid.to_string(),
    })
}

/// Validates the cardinality of a set of project-map links.
///
/// Rule: exactly 1 `Project`, 1 `Status`, 1 `Kind`; at most 1 `Version`;
/// any number of `Annex`, `Dep`, `Supersedes` and `Parent`. If a `Version` is present, its
/// namespaced project must match the `Project` link.
///
/// **Feature cards**: as soon as one `Feature` link is present, exactly 1 `Feature`,
/// 1 `Release`, 1 `Version` and one `kind` are required. Every [`KindKind`] value is
/// accepted; only `kind:FEATURE` is exported to the public website, the other kinds
/// (`FIX`/`TASK`/`ENHANCEMENT`) stay vault-only. Without a `Feature`
/// link (a plain changelog card), no `Release` link is allowed and `Kind` is
/// unconstrained — the original validation rules are unchanged.
///
/// # Errors
///
/// A cardinality or consistency [`SchemaError`], raised at the first deviation found,
/// in this order: project → status → kind → version → project/version mismatch →
/// feature → release.
#[must_use = "the validation result must be inspected before accepting the write"]
pub fn validate_links(links: &[ProjectMapLink]) -> Result<(), SchemaError> {
    let mut projects: Vec<&str> = Vec::new();
    let mut status_count = 0usize;
    let mut kind_count = 0usize;
    let mut versions: Vec<&str> = Vec::new();
    let mut feature_count = 0usize;
    let mut release_count = 0usize;

    for link in links {
        match link {
            ProjectMapLink::Project(p) => projects.push(p),
            ProjectMapLink::Status(_) => status_count += 1,
            ProjectMapLink::Kind(_) => kind_count += 1,
            ProjectMapLink::Version { project, .. } => versions.push(project),
            ProjectMapLink::Feature(_) => feature_count += 1,
            ProjectMapLink::Release(_) => release_count += 1,
            // Supersedes et Parent ne participent à aucun compte de cardinalité.
            ProjectMapLink::Annex { .. }
            | ProjectMapLink::Dep { .. }
            | ProjectMapLink::Supersedes(_)
            | ProjectMapLink::Parent(_) => {}
        }
    }

    if projects.len() != 1 {
        return Err(SchemaError::ProjectCardinality(projects.len()));
    }
    if status_count != 1 {
        return Err(SchemaError::StatusCardinality(status_count));
    }
    if kind_count != 1 {
        return Err(SchemaError::KindCardinality(kind_count));
    }
    if versions.len() > 1 {
        return Err(SchemaError::VersionCardinality(versions.len()));
    }

    // Cohérence version.project == project (si une version est présente).
    let project = projects[0];
    if let Some(version_project) = versions.first()
        && *version_project != project
    {
        return Err(SchemaError::VersionProjectMismatch {
            project: project.to_string(),
            version_project: (*version_project).to_string(),
        });
    }

    // Règle conditionnelle carte-feature (axe orthogonal, non régressif pour les
    // cartes changelog sans feature). Évaluée après les checks existants pour
    // préserver leurs messages d'erreur.
    if feature_count >= 1 {
        // Carte-feature : exactement 1 feature + exactement 1 release +
        // exactement 1 version (sentinel gradatum/backlog admis) + kind ∈ enum.
        // Ordre : feature → release → version (cohérent avec §10e).
        // Note : seul kind:FEATURE est exporté vers le site (export T2 S2) ;
        // les autres kinds sont vault-only (FIX/TASK/ENHANCEMENT).
        if feature_count != 1 {
            return Err(SchemaError::FeatureCardinality(feature_count));
        }
        if release_count != 1 {
            return Err(SchemaError::ReleaseCardinality(release_count));
        }
        // NOMENCLATURE §10e + spec §3.1 : version obligatoire (cardinalité 1).
        // Le sentinel [[version:<projet>/backlog]] satisfait cette contrainte
        // pour les features sans version concrète.
        if versions.len() != 1 {
            return Err(SchemaError::FeatureVersionCardinality(versions.len()));
        }
        // kind_count == 1 est garanti par le check KindCardinality ci-dessus.
        // Slice 1 S1 : tout kind ∈ KindKind est accepté sur une carte-feature.
        // La contrainte kind == FEATURE est intentionnellement retirée.
    } else if release_count != 0 {
        // Carte non-feature : aucun [[release:]] autorisé sans [[feature:]].
        return Err(SchemaError::ReleaseCardinality(release_count));
    }

    Ok(())
}

/// Validates the project-map schema from **already-extracted** wikilink targets.
///
/// Designed for the write path: the server extracts the `[[…]]` targets from the body
/// (via `gradatum_curator::wikilinks::extract_wikilinks`, which lives outside this
/// crate to avoid a dependency cycle) and then delegates validation here.
///
/// # Semantics
///
/// Each target is handed to [`parse_link`]. Targets that **fail** to parse — a bare
/// human title, prose such as `[[see also]]` — are **ignored**: they are not project-map
/// structural links. The targets that do parse are collected and submitted to
/// [`validate_links`] (1 project + 1 status + 1 kind, at most 1 version, and a matching
/// `version.project`).
///
/// A **malformed reserved value** (`[[status:nope]]`, `[[version:x]]`) parses to `Err`
/// and is therefore ignored here — but the three mandatory links still catch it, since
/// an invalid `status:` never yields the required `Status` and the card is rejected on
/// cardinality.
///
/// # Errors
///
/// A cardinality or consistency [`SchemaError`] when the mandatory schema is not
/// satisfied by the typed links present.
#[must_use = "the validation result gates the project-map write"]
pub fn validate_links_from_targets(targets: &[String]) -> Result<(), SchemaError> {
    let links: Vec<ProjectMapLink> = targets.iter().filter_map(|t| parse_link(t).ok()).collect();
    validate_links(&links)
}

/// Feature **identity** role(s) among already-extracted wikilink targets.
///
/// Returns the sorted `F-XX` identifiers carried by `[[feature:…]]` roles — the **identity**
/// of a project-map card. `supersedes:` / `parent:` are deliberately excluded: they
/// *reference* other features, they are not the card's own identity, and counting them would
/// make two distinct cards compare equal.
///
/// A well-formed feature card yields exactly one identifier; a changelog card yields none.
/// Sorting makes the result usable for **order-independent equality** — the basis of the
/// identity-immutability contract enforced on the write path: an external creation may not
/// supply an identity (only the server allocates one), and an external update must preserve
/// the existing identity exactly.
///
/// Mirrors [`validate_links_from_targets`]: the caller extracts the `[[…]]` targets once
/// (via `gradatum_curator::wikilinks::extract_wikilinks`) and hands them here — a single
/// extraction source, no divergence with the schema validator.
#[must_use = "the extracted identity must be compared before accepting the write"]
pub fn feature_identity_from_targets(targets: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = targets
        .iter()
        .filter_map(|t| match parse_link(t).ok()? {
            ProjectMapLink::Feature(id) => Some(id),
            _ => None,
        })
        .collect();
    ids.sort();
    ids
}

/// Canonical identifier of a synthetic **reserved target node** in the link graph.
///
/// The typed links `project:` / `status:` / `kind:` / `version:` do **not** point at an
/// existing note — there is no ULID to resolve. They reference a **reserved node** (a
/// hub) of the `note_links` graph. This function normalises the raw wikilink target into
/// the canonical `dst_note_id` of that node, which the worker resolver can insert
/// directly without any lookup.
///
/// `note_links.dst_note_id` is a free-form `TEXT` column with no foreign key, which is
/// what makes a synthetic `dst` possible and navigable through `vault_graph` and
/// `vault_trace`.
///
/// # How annexes differ
///
/// `spec:` / `plan:` / `context:` are reserved prefixes too, but they point at **real
/// notes** (a ULID or a path). They keep going through the normal ULID resolution and
/// are therefore not reserved nodes here — this function returns `None` for them.
///
/// # Returns
///
/// - `Some(dst)` for well-formed `project`/`status`/`kind`/`version`/`feature`/
///   `release`/`supersedes`/`parent` links, where `dst` is the canonical node
///   identifier (status and kind normalised to their wire form by [`parse_link`]).
/// - `None` for anything else — an annex, a `section:ULID` dependency, a bare title, or
///   a malformed reserved value — leaving the caller on its normal path.
#[must_use]
pub fn reserved_node_target(raw: &str) -> Option<String> {
    match parse_link(raw).ok()? {
        ProjectMapLink::Project(p) => Some(format!("project:{p}")),
        ProjectMapLink::Status(s) => Some(format!("status:{}", s.as_wire())),
        ProjectMapLink::Kind(k) => Some(format!("kind:{}", k.as_wire())),
        ProjectMapLink::Version { project, version } => {
            Some(format!("version:{project}/{version}"))
        }
        // Rôles feature : nœuds réservés typés navigables (pas de note ULID).
        // `supersedes:F-YY` et `parent:F-YY` pointent vers le nœud `feature:F-YY`.
        ProjectMapLink::Feature(f) => Some(format!("feature:{f}")),
        ProjectMapLink::Release(r) => Some(format!("release:{}", r.as_wire())),
        ProjectMapLink::Supersedes(f) => Some(format!("feature:{f}")),
        ProjectMapLink::Parent(f) => Some(format!("feature:{f}")),
        // Annexes (spec/plan/context) et dépendances (section:ULID) pointent vers
        // de vraies notes → résolution ULID normale, pas un nœud réservé.
        ProjectMapLink::Annex { .. } | ProjectMapLink::Dep { .. } => None,
    }
}

// ── Export feature entries — logique de projection partagée ──────────────────
//
// Cette section fournit les types et la fonction de projection PURE qui permettent
// à l'admin CLI ET au serveur HTTP de produire la même liste `Vec<FeatureEntry>`
// depuis des notes project-map brutes, sans duplication du mapping.
//
// ## Architecture DRY
//
// - `FeatureEntry` / `ExportOptions` sont définis ici (gradatum-core), accessible
//   depuis l'admin et le serveur sans dépendance croisée.
// - `project_map_feature_entries` prend `&[(body_text, title)]` — entrées pures,
//   sans I/O. La couche de récupération (SQL admin / Index serveur) est séparée.
// - `map_version_raw` et `feature_sort_key` sont réutilisés par les deux
//   consommateurs.

/// Projection of a feature card for the public-website JSON export.
///
/// Produced by [`project_map_feature_entries`] from the raw notes of the
/// `project-map` section. Used by the admin CLI and by the HTTP handler
/// `GET /api/v1/project-map/export-features`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureEntry {
    /// Identifier of the feature card, of the form `F-<n>`.
    pub feature: String,
    /// Lowercase wire delivery status (`"roadmap"` | `"planned"` | `"released"` | `"dropped"`).
    pub release: String,
    /// Target version in website format (a real `"v0.6.4"`), or the literal `"vX.Y.Z"`
    /// for backlog cards, so that they remain visible instead of being filtered out.
    pub version: Option<String>,
    /// H1 title of the card.
    pub title: String,
}

/// Filtering options for [`project_map_feature_entries`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    /// Switches the export into **full-audit** mode.
    ///
    /// - `false` (default) — public-website mirror: `release:dropped` cards are
    ///   excluded, **and** only `kind:FEATURE` cards are kept.
    /// - `true` — every feature card is exported, whatever its release status and
    ///   whatever its kind.
    ///
    /// Despite its name, this flag lifts **both** filters at once; there is no way to
    /// lift one without the other. `version:*/backlog` cards are included in both modes.
    pub include_dropped: bool,
}

/// Sort key ordering `F-XX` identifiers numerically on their `\d{2,3}` part.
///
/// The `F-` prefix is stripped and the digits parsed (`061` → 61). Invalid identifiers
/// map to 0, which keeps the sort stable rather than panicking.
fn feature_sort_key(id: &str) -> u32 {
    id.strip_prefix("F-")
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// Highest feature number **referenced** in a single project-map card body.
///
/// This is the reliable source of truth for feature-number allocation: it reads the
/// **body role** wikilinks (`[[feature:F-XX]]`, `[[supersedes:F-YY]]`, `[[parent:F-ZZ]]`),
/// parsed by [`parse_link`] — **never the note tags**. Tags are known to be incomplete
/// (some cards carry none) and inconsistent (`f-01` vs `f-1` coexist), so a max computed
/// over tags would be unsafe; the body role is validated against `F-\d{2,3}` when the card
/// is parsed, and is present on every feature card by construction.
///
/// All three feature-shaped roles are considered, not just `feature:`. Counting
/// `supersedes:`/`parent:` raises the floor so an allocation can **never** hand back a
/// number that is still referenced by a live card whose target feature card was removed —
/// a strictly safer, monotone floor at negligible cost.
///
/// The `F-` prefix is stripped and the digits parsed (`061` → 61). Returns `None` when the body references no feature number.
///
/// The scan is char-safe and allocation-free beyond the parse; no `regex` dependency.
#[must_use]
pub fn max_feature_number(body: &str) -> Option<u32> {
    extract_wikilink_targets(body)
        .iter()
        .filter_map(|target| match parse_link(target).ok()? {
            ProjectMapLink::Feature(id)
            | ProjectMapLink::Supersedes(id)
            | ProjectMapLink::Parent(id) => {
                id.strip_prefix("F-").and_then(|digits| digits.parse().ok())
            }
            _ => None,
        })
        .max()
}

/// Maps a raw `version:` value to the string displayed on the public website.
///
/// - `"gradatum/0.6.4"` → `Some("v0.6.4")` — a concrete version, prefixed with `v`.
/// - `"gradatum/backlog"` → `Some("vX.Y.Z")` — backlog cards stay visible on the
///   website behind this literal sentinel instead of being filtered out.
///
/// The project namespace (the part before `/`) is ignored here: the schema already
/// checks it against the card's `[[project:]]` link.
pub fn map_version_raw(raw: &str) -> Option<String> {
    let ver = raw.split_once('/').map(|(_, v)| v).unwrap_or(raw);
    if ver == "backlog" {
        Some("vX.Y.Z".to_string())
    } else {
        Some(format!("v{ver}"))
    }
}

/// Pure projection from project-map notes to `Vec<FeatureEntry>`.
///
/// Input: a slice of `(body_text, title)` tuples holding the raw notes of the
/// `project-map` section, already filtered on `status != 'downgraded'/'garbage'` by the
/// storage layer upstream.
///
/// Processing:
/// 1. Parse the wikilinks with [`parse_link`]; unparseable targets are skipped.
/// 2. Keep only the cards carrying a `[[feature:F-XX]]` link.
/// 3. Skip cards without a `[[release:…]]` link — a feature card without one is invalid.
/// 4. When `opts.include_dropped == false`, drop `release:dropped` cards **and** every
///    card whose kind is not `FEATURE`.
/// 5. Map `[[version:…]]` through [`map_version_raw`] (backlog → `"vX.Y.Z"`).
/// 6. Sort by ascending numeric `F-XX` identifier.
///
/// The function is **pure**: no I/O, no `Result`. Individual wikilink parse failures are
/// swallowed defensively, and a card missing a mandatory role is simply skipped.
#[must_use]
pub fn project_map_feature_entries(
    notes: &[(String, String)],
    opts: ExportOptions,
) -> Vec<FeatureEntry> {
    let mut entries: Vec<FeatureEntry> = Vec::new();

    for (body, title) in notes {
        // Parser les wikilinks — ignorer silencieusement les cibles invalides.
        let links: Vec<ProjectMapLink> = extract_wikilink_targets(body)
            .into_iter()
            .filter_map(|t| parse_link(&t).ok())
            .collect();

        let mut feature_id: Option<String> = None;
        let mut release_wire: Option<String> = None;
        let mut version_raw: Option<String> = None;
        let mut kind_wire: Option<&KindKind> = None;

        for link in &links {
            match link {
                ProjectMapLink::Feature(f) => feature_id = Some(f.clone()),
                ProjectMapLink::Release(r) => release_wire = Some(r.as_wire().to_string()),
                ProjectMapLink::Version { project, version } => {
                    version_raw = Some(format!("{project}/{version}"));
                }
                ProjectMapLink::Kind(k) => kind_wire = Some(k),
                _ => {}
            }
        }

        // Seules les cartes-feature (présence de [[feature:F-XX]]) sont projetées.
        let feature_id = match feature_id {
            Some(id) => id,
            None => continue,
        };

        let release = match release_wire {
            Some(r) => r,
            None => continue, // carte-feature sans release — invalide, ignorée défensivement
        };

        let version = version_raw.as_deref().and_then(map_version_raw);

        // Filtrage miroir-site : exclure release:dropped par défaut (Règle A).
        if !opts.include_dropped && release == "dropped" {
            continue;
        }

        // Filtrage miroir-site S2 : seul kind:FEATURE alimente le site (export T2).
        // Les cartes kind:FIX/TASK/ENHANCEMENT sont vault-only.
        // `include_dropped` = mode audit complet : lève le filtre kind pour
        // inclure toutes les cartes-feature quelle que soit leur taxonomie.
        if !opts.include_dropped && !matches!(kind_wire, Some(KindKind::Feature)) {
            continue;
        }

        entries.push(FeatureEntry {
            feature: feature_id,
            release,
            version,
            title: title.clone(),
        });
    }

    // Tri par identifiant F-XX numérique croissant.
    entries.sort_by_key(|e| feature_sort_key(&e.feature));

    entries
}

/// Extracts the raw `[[target]]` wikilink targets from a body.
///
/// A char-safe scan with no `regex` dependency. Returns whatever sits between `[[` and
/// `]]` — the raw target, `role:` prefix included, ready for `parse_link`.
///
/// Internal helper, factored out of [`project_map_feature_entries`].
fn extract_wikilink_targets(body: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            result.push(after_open[..end].to_string());
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
    result
}

/// Typed roles (`kind`, `status`) extracted from a card body, for filterable indexing.
///
/// Each field carries the **canonical wire form** produced by [`KindKind::as_wire`] /
/// [`StatusKind::as_wire`] (SCREAMING_SNAKE), or `None` when the role is absent — which is
/// the case for any note outside `project-map`, and is legitimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProjectMapRoles {
    /// Wire form of `[[kind:…]]` (e.g. `"FIX"`), or `None`.
    pub kind: Option<&'static str>,
    /// Wire form of `[[status:…]]` (e.g. `"OPEN"`), or `None`.
    pub status: Option<&'static str>,
}

/// Extracts the `kind`/`status` roles from a project-map card body.
///
/// **Single source of semantics**: the `[[…]]` targets are parsed by [`parse_link`],
/// the same parser as [`validate_links_from_targets`]. No substring matching:
/// prose, an identifier containing `FIX`, or an off-taxonomy value (`status:done`)
/// produce no role. The **first** well-formed `kind`/`status` wins; a valid card
/// carries only one in any case (guaranteed by the cardinality enforced by
/// [`validate_links_from_targets`] at write time).
#[must_use]
pub fn roles_of_body(body: &str) -> ProjectMapRoles {
    let mut roles = ProjectMapRoles::default();
    for target in extract_wikilink_targets(body) {
        match parse_link(&target) {
            Ok(ProjectMapLink::Kind(k)) if roles.kind.is_none() => roles.kind = Some(k.as_wire()),
            Ok(ProjectMapLink::Status(s)) if roles.status.is_none() => {
                roles.status = Some(s.as_wire());
            }
            _ => {}
        }
        if roles.kind.is_some() && roles.status.is_some() {
            break;
        }
    }
    roles
}

#[cfg(test)]
mod feature_entries_tests {
    use super::*;

    fn note(body: &str, title: &str) -> (String, String) {
        (body.to_string(), title.to_string())
    }

    /// Carte backlog → `version == Some("vX.Y.Z")` (Règle A).
    #[test]
    fn backlog_card_exports_with_vxyz_version() {
        let notes = vec![note(
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 1, "carte backlog incluse : {entries:?}");
        assert_eq!(entries[0].version, Some("vX.Y.Z".to_string()));
        assert_eq!(entries[0].feature, "F-50");
    }

    /// Carte backlog → **incluse** par défaut (miroir-site).
    #[test]
    fn backlog_card_included_in_default_mirror() {
        let notes = vec![note(
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(
            !entries.is_empty(),
            "carte backlog doit être incluse par défaut"
        );
    }

    /// Carte `release:dropped` → **exclue** par défaut.
    #[test]
    fn dropped_card_excluded_by_default() {
        let notes = vec![note(
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
             [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(
            entries.is_empty(),
            "carte dropped exclue par défaut : {entries:?}"
        );
    }

    /// Carte `release:dropped` → **incluse** avec `include_dropped`.
    #[test]
    fn dropped_card_included_with_include_dropped() {
        let notes = vec![note(
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
             [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
        )];
        let entries = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        assert_eq!(
            entries.len(),
            1,
            "carte dropped incluse avec include_dropped"
        );
        assert_eq!(entries[0].feature, "F-51");
    }

    /// Version concrète → `"v0.6.3"` (préfixe `v` ajouté).
    #[test]
    fn concrete_version_exports_with_v_prefix() {
        let notes = vec![note(
            "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/0.6.3]]",
            "F-37 released",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 1, "carte released incluse");
        assert_eq!(entries[0].version, Some("v0.6.3".to_string()));
    }

    /// Feature IDs are sorted numerically: `F-37` sorts before `F-50`.
    #[test]
    fn notes_are_sorted_by_feature_id_numerically() {
        let notes = vec![
            note(
                "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/backlog]]",
                "F-50 backlog",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "F-37 released",
            ),
        ];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].feature, "F-37");
        assert_eq!(entries[1].feature, "F-50");
    }

    /// Carte changelog sans `[[feature:]]` → **exclue**.
    #[test]
    fn changelog_card_without_feature_is_excluded() {
        let notes = vec![note(
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/0.5.2]]\n\nFix.",
            "Fix changelog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(entries.is_empty(), "carte changelog exclue : {entries:?}");
    }

    /// Empty input slice yields an empty result.
    #[test]
    fn empty_notes_returns_empty_vec() {
        let entries = project_map_feature_entries(&[], ExportOptions::default());
        assert!(entries.is_empty());
    }

    /// `map_version_raw` : backlog → `"vX.Y.Z"`, version concrète → `"vX.Y.Z"`.
    #[test]
    fn map_version_raw_formats_correctly() {
        assert_eq!(
            map_version_raw("gradatum/0.5.2"),
            Some("v0.5.2".to_string())
        );
        assert_eq!(
            map_version_raw("gradatum/backlog"),
            Some("vX.Y.Z".to_string())
        );
    }

    /// Titre est bien propagé dans l'entrée.
    #[test]
    fn title_is_propagated_to_entry() {
        let notes = vec![note(
            "[[feature:F-10]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/0.5.0]]",
            "Ma Feature Titre",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries[0].title, "Ma Feature Titre");
    }

    // ── S2 : filtre kind:FEATURE sur l'export miroir-site ────────────────────

    /// The site-mirror export excludes cards with `kind != FEATURE` by default.
    ///
    /// Cards with `kind:FIX` (debt, vault-only) are excluded; cards with `kind:FEATURE`
    /// are included. Only `kind:FEATURE` cards are published to the site export.
    #[test]
    fn project_map_feature_entries_excludes_non_feature_kind() {
        let notes = vec![
            note(
                "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] \
                 [[release:roadmap]] [[version:gradatum/backlog]]",
                "Dette F-99",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "Feature F-37",
            ),
        ];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(
            entries.len(),
            1,
            "seul F-37 kind:FEATURE attendu : {entries:?}"
        );
        assert_eq!(entries[0].feature, "F-37");
    }

    /// Mode audit `include_dropped=true` : lève le filtre kind — toutes les
    /// cartes-feature sont incluses quelle que soit leur taxonomie.
    ///
    /// Ce mode sert les audits internes (inventaire complet du vault) ;
    /// il ne doit PAS être utilisé pour générer `features.ts` site.
    #[test]
    fn project_map_feature_entries_include_dropped_lifts_kind_filter() {
        let notes = vec![
            note(
                "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] \
                 [[release:roadmap]] [[version:gradatum/backlog]]",
                "Dette F-99",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "Feature F-37",
            ),
        ];
        let entries = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        // Mode audit = filtre kind levé : F-37 + F-99 attendus.
        assert_eq!(
            entries.len(),
            2,
            "mode audit doit inclure toutes les cartes-feature : {entries:?}"
        );
        assert_eq!(entries[0].feature, "F-37");
        assert_eq!(entries[1].feature, "F-99");
    }
}

#[cfg(test)]
mod max_feature_number_tests {
    use super::*;

    /// The highest `[[feature:F-XX]]` role in the body wins, 2- and 3-digit forms mixed.
    #[test]
    fn returns_max_over_feature_roles() {
        let body = "[[feature:F-37]] [[project:gradatum]] see also [[feature:F-134]]";
        assert_eq!(max_feature_number(body), Some(134));
    }

    /// `supersedes:` and `parent:` raise the floor beyond the `feature:` role.
    #[test]
    fn supersedes_and_parent_raise_the_floor() {
        let body = "[[feature:F-40]] [[supersedes:F-131]] [[parent:F-98]]";
        assert_eq!(
            max_feature_number(body),
            Some(131),
            "supersedes:F-131 must be counted so the number is never reallocated"
        );
    }

    /// A body with no feature-shaped role yields `None`.
    #[test]
    fn returns_none_when_no_feature_role() {
        let body = "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] plain prose";
        assert_eq!(max_feature_number(body), None);
    }

    /// Tags are NOT the source: an `f-999` in prose (not a `[[feature:]]` wikilink)
    /// is ignored — only well-formed body roles count.
    #[test]
    fn ignores_non_wikilink_text_and_malformed_ids() {
        // `f-999` lowercase is not a valid feature ident; `[[feature:F-1]]` is malformed
        // (1 digit) and rejected by parse_link → ignored. Only F-50 counts.
        let body = "f-999 tag noise [[feature:F-1]] [[feature:F-50]]";
        assert_eq!(max_feature_number(body), Some(50));
    }

    /// Empty body → `None`.
    #[test]
    fn empty_body_returns_none() {
        assert_eq!(max_feature_number(""), None);
    }
}

#[cfg(test)]
mod validate_from_targets_tests {
    use super::*;

    fn t(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn valid_triple_with_prose_and_deps_passes() {
        let targets = t(&[
            "project:gradatum",
            "status:IN_PROGRESS",
            "kind:FEATURE",
            "Mon Titre Humain", // prose ignorée
            "decisions:01KVBTMYNK4XXZJAKWMTB4AM9K",
        ]);
        assert_eq!(validate_links_from_targets(&targets), Ok(()));
    }

    #[test]
    fn missing_status_is_rejected() {
        let targets = t(&["project:gradatum", "kind:FIX"]);
        assert_eq!(
            validate_links_from_targets(&targets),
            Err(SchemaError::StatusCardinality(0))
        );
    }

    #[test]
    fn invalid_status_value_fails_via_cardinality() {
        // status:nope parse en Err → ignoré → 0 Status → rejet par cardinalité.
        let targets = t(&["project:gradatum", "status:nope", "kind:FIX"]);
        assert_eq!(
            validate_links_from_targets(&targets),
            Err(SchemaError::StatusCardinality(0))
        );
    }

    #[test]
    fn empty_targets_is_rejected_no_project() {
        assert_eq!(
            validate_links_from_targets(&[]),
            Err(SchemaError::ProjectCardinality(0))
        );
    }
}

#[cfg(test)]
mod feature_identity_tests {
    use super::*;

    fn t(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn feature_card_yields_its_single_identity() {
        let targets = t(&[
            "project:gradatum",
            "status:OPEN",
            "feature:F-42",
            "release:planned",
        ]);
        assert_eq!(
            feature_identity_from_targets(&targets),
            vec!["F-42".to_string()]
        );
    }

    #[test]
    fn changelog_card_yields_no_identity() {
        let targets = t(&[
            "project:gradatum",
            "status:DONE",
            "kind:FIX",
            "version:gradatum/0.5.2",
        ]);
        assert!(feature_identity_from_targets(&targets).is_empty());
    }

    #[test]
    fn supersedes_and_parent_are_not_identity() {
        // supersedes/parent référencent d'AUTRES features — ils ne sont pas l'identité
        // de la carte. Seul `feature:` compte, sinon deux cartes distinctes compareraient égales.
        let targets = t(&["feature:F-40", "supersedes:F-131", "parent:F-98"]);
        assert_eq!(
            feature_identity_from_targets(&targets),
            vec!["F-40".to_string()]
        );
    }

    #[test]
    fn malformed_and_prose_targets_are_ignored() {
        // `feature:F-1` (1 chiffre) est rejeté par parse_link ; la prose n'a pas de préfixe.
        let targets = t(&["feature:F-1", "Mon Titre Humain", "feature:F-07"]);
        assert_eq!(
            feature_identity_from_targets(&targets),
            vec!["F-07".to_string()]
        );
    }

    #[test]
    fn result_is_sorted_for_order_independent_equality() {
        let a = feature_identity_from_targets(&t(&["feature:F-40", "feature:F-12"]));
        let b = feature_identity_from_targets(&t(&["feature:F-12", "feature:F-40"]));
        assert_eq!(
            a, b,
            "l'égalité d'identité ne doit pas dépendre de l'ordre des cibles"
        );
        assert_eq!(a, vec!["F-12".to_string(), "F-40".to_string()]);
    }
}

#[cfg(test)]
mod reserved_node_tests {
    use super::*;

    #[test]
    fn status_maps_to_canonical_reserved_node() {
        assert_eq!(
            reserved_node_target("status:DONE"),
            Some("status:DONE".to_string())
        );
    }

    #[test]
    fn project_and_kind_and_version_map_to_reserved_nodes() {
        assert_eq!(
            reserved_node_target("project:gradatum"),
            Some("project:gradatum".to_string())
        );
        assert_eq!(
            reserved_node_target("kind:FIX"),
            Some("kind:FIX".to_string())
        );
        assert_eq!(
            reserved_node_target("version:gradatum/0.6.1"),
            Some("version:gradatum/0.6.1".to_string())
        );
    }

    #[test]
    fn status_node_is_normalised_to_wire_casing() {
        // Une casse invalide n'est pas un nœud réservé (parse_link rejette).
        assert_eq!(reserved_node_target("status:done"), None);
    }

    #[test]
    fn annexes_are_not_reserved_nodes() {
        // spec/plan/context pointent vers de vraies notes → flux ULID normal.
        assert_eq!(
            reserved_node_target("spec:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
        assert_eq!(reserved_node_target("plan:plans/x.md"), None);
        assert_eq!(
            reserved_node_target("context:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
    }

    #[test]
    fn dependency_section_ulid_is_not_a_reserved_node_unregressed() {
        // Rétrocompat dure : [[decisions:ULID]] reste une dépendance résolue par
        // ULID, jamais un nœud réservé.
        assert_eq!(
            reserved_node_target("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
    }

    #[test]
    fn human_title_is_not_a_reserved_node() {
        assert_eq!(reserved_node_target("Mon Titre Humain"), None);
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn project_prefix_parses_to_project() {
        assert_eq!(
            parse_link("project:gradatum"),
            Ok(ProjectMapLink::Project("gradatum".to_string()))
        );
    }

    #[test]
    fn status_done_parses_to_status_done() {
        assert_eq!(
            parse_link("status:DONE"),
            Ok(ProjectMapLink::Status(StatusKind::Done))
        );
    }

    #[test]
    fn status_lowercase_is_rejected_case_sensitive() {
        // Spec §15 A4 : SCREAMING_SNAKE strict.
        assert_eq!(
            parse_link("status:done"),
            Err(SchemaError::InvalidStatus("done".to_string()))
        );
    }

    #[test]
    fn status_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("status:NOPE"),
            Err(SchemaError::InvalidStatus("NOPE".to_string()))
        );
    }

    #[test]
    fn kind_fix_parses_to_kind_fix() {
        assert_eq!(
            parse_link("kind:FIX"),
            Ok(ProjectMapLink::Kind(KindKind::Fix))
        );
    }

    #[test]
    fn kind_chore_and_spike_are_rejected() {
        // CHORE + SPIKE retirés de la taxonomie (absorbés par TASK). Un corps qui les
        // porte encore n'est plus lisible par son propre validateur — d'où l'ordre de
        // migration : les cartes CHORE/SPIKE doivent être migrées vers TASK AVANT que
        // ce code n'atteigne la production.
        assert_eq!(
            parse_link("kind:CHORE"),
            Err(SchemaError::InvalidKind("CHORE".to_string()))
        );
        assert_eq!(
            parse_link("kind:SPIKE"),
            Err(SchemaError::InvalidKind("SPIKE".to_string()))
        );
    }

    #[test]
    fn kind_from_wire_directly_rejects_chore_and_spike() {
        // Garde anti-réouverture (F-220) : les variantes KindKind::Chore /
        // KindKind::Spike sont retirées pour de bon en 2.1.0. Le vocabulaire réseau
        // "CHORE"/"SPIKE" ne doit jamais être ré-accepté. `from_wire` est le seul
        // point d'entrée wire → variante ; on le teste ici directement (pas seulement
        // via parse_link) pour prouver qu'aucun chemin détourné ne l'accepte.
        assert_eq!(KindKind::from_wire("CHORE"), None);
        assert_eq!(KindKind::from_wire("SPIKE"), None);
    }

    #[test]
    fn kind_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("kind:BUGFIX"),
            Err(SchemaError::InvalidKind("BUGFIX".to_string()))
        );
    }

    #[test]
    fn version_namespaced_parses_to_version() {
        assert_eq!(
            parse_link("version:gradatum/0.6.1"),
            Ok(ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.6.1".to_string(),
            })
        );
    }

    #[test]
    fn version_without_slash_is_malformed() {
        assert_eq!(
            parse_link("version:0.6.1"),
            Err(SchemaError::MalformedVersion("0.6.1".to_string()))
        );
    }

    #[test]
    fn version_empty_project_is_malformed() {
        assert_eq!(
            parse_link("version:/0.6.1"),
            Err(SchemaError::MalformedVersion("/0.6.1".to_string()))
        );
    }

    #[test]
    fn annex_spec_plan_context_parse_to_annex() {
        assert_eq!(
            parse_link("spec:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Spec,
                target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
        assert_eq!(
            parse_link("plan:plans/2026-06-19.md"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Plan,
                target: "plans/2026-06-19.md".to_string(),
            })
        );
        assert_eq!(
            parse_link("context:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Context,
                target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
    }

    #[test]
    fn dep_section_ulid_parses_to_dep_unregressed() {
        // Rétrocompat : le format historique section:ULID reste une dépendance.
        assert_eq!(
            parse_link("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Dep {
                section: "decisions".to_string(),
                ulid: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
    }

    #[test]
    fn bare_value_without_prefix_is_missing_prefix() {
        // Un titre humain nu (pas de `:`) n'est ni typé ni section:ULID.
        assert_eq!(
            parse_link("Mon Titre Humain"),
            Err(SchemaError::MissingPrefix("Mon Titre Humain".to_string()))
        );
    }

    #[test]
    fn reserved_prefix_with_empty_value_is_rejected() {
        assert_eq!(
            parse_link("project:"),
            Err(SchemaError::EmptyValue("project".to_string()))
        );
        assert_eq!(
            parse_link("status:"),
            Err(SchemaError::EmptyValue("status".to_string()))
        );
    }

    #[test]
    fn leading_trailing_whitespace_is_trimmed() {
        assert_eq!(
            parse_link("  project:gradatum  "),
            Ok(ProjectMapLink::Project("gradatum".to_string()))
        );
    }

    // ── V1 : contraindre charset + longueur project / version ────────────────

    /// `[[project:../../etc]]` → InvalidChars (path traversal rejeté).
    #[test]
    fn project_path_traversal_is_rejected_invalid_chars() {
        let err = parse_link("project:../../etc").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "project"),
            "attendu InvalidChars(project, …), reçu {err:?}"
        );
    }

    /// `[[project:<65 chars>]]` → ValueTooLong.
    #[test]
    fn project_name_over_64_chars_is_rejected() {
        let long_name = "a".repeat(65);
        let err = parse_link(&format!("project:{long_name}")).unwrap_err();
        assert!(
            matches!(err, SchemaError::ValueTooLong(ref p, _, 65) if p == "project"),
            "attendu ValueTooLong(project, _, 65), reçu {err:?}"
        );
    }

    /// `[[project:My-Project]]` → InvalidChars (majuscules interdites).
    #[test]
    fn project_uppercase_is_rejected() {
        let err = parse_link("project:My-Project").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "project"),
            "attendu InvalidChars(project, …), reçu {err:?}"
        );
    }

    /// `[[version:My_Project/0.6.1]]` → InvalidChars sur version.project (majuscules).
    #[test]
    fn version_project_uppercase_is_rejected() {
        let err = parse_link("version:My_Project/0.6.1").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "version.project"),
            "attendu InvalidChars(version.project, …), reçu {err:?}"
        );
    }

    /// `[[version:gradatum/1.0.0 evil]]` → InvalidChars sur version.version (espace).
    #[test]
    fn version_version_with_space_is_rejected() {
        let err = parse_link("version:gradatum/1.0.0 evil").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "version.version"),
            "attendu InvalidChars(version.version, …), reçu {err:?}"
        );
    }

    /// `[[project:my-project_v1.2]]` → valide (charset OK : `-`, `_`, `.` acceptés).
    #[test]
    fn project_with_dash_underscore_dot_is_valid() {
        assert_eq!(
            parse_link("project:my-project_v1.2"),
            Ok(ProjectMapLink::Project("my-project_v1.2".to_string()))
        );
    }

    /// `[[version:my-proj/1.2.3-rc.1]]` → valide.
    #[test]
    fn version_with_semver_prerelease_is_valid() {
        assert_eq!(
            parse_link("version:my-proj/1.2.3-rc.1"),
            Ok(ProjectMapLink::Version {
                project: "my-proj".to_string(),
                version: "1.2.3-rc.1".to_string(),
            })
        );
    }

    #[test]
    fn status_kind_wire_roundtrip() {
        for s in [
            StatusKind::Brainstorming,
            StatusKind::Open,
            StatusKind::InProgress,
            StatusKind::Blocked,
            StatusKind::Done,
            StatusKind::Obsolete,
        ] {
            assert_eq!(StatusKind::from_wire(s.as_wire()), Some(s));
        }
        for k in [
            KindKind::Feature,
            KindKind::Enhancement,
            KindKind::Fix,
            KindKind::Task,
        ] {
            assert_eq!(KindKind::from_wire(k.as_wire()), Some(k));
        }
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;

    /// Jeu minimal valide : 1 project + 1 status + 1 kind.
    fn minimal_valid() -> Vec<ProjectMapLink> {
        vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
        ]
    }

    #[test]
    fn minimal_triple_is_valid() {
        assert_eq!(validate_links(&minimal_valid()), Ok(()));
    }

    #[test]
    fn zero_project_is_rejected() {
        let links = vec![
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(0))
        );
    }

    #[test]
    fn two_projects_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Project("other".to_string()));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(2))
        );
    }

    #[test]
    fn zero_kind_is_rejected() {
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
        ];
        assert_eq!(validate_links(&links), Err(SchemaError::KindCardinality(0)));
    }

    #[test]
    fn two_status_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Status(StatusKind::Done));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::StatusCardinality(2))
        );
    }

    #[test]
    fn two_versions_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.1".to_string(),
        });
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.2".to_string(),
        });
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionCardinality(2))
        );
    }

    #[test]
    fn version_project_mismatch_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "example-project".to_string(),
            version: "0.6.1".to_string(),
        });
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionProjectMismatch {
                project: "gradatum".to_string(),
                version_project: "example-project".to_string(),
            })
        );
    }

    #[test]
    fn one_version_matching_project_is_valid() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.1".to_string(),
        });
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn annexes_and_deps_in_any_number_are_valid() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Annex {
            role: AnnexRole::Spec,
            target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
        });
        links.push(ProjectMapLink::Annex {
            role: AnnexRole::Plan,
            target: "plans/x.md".to_string(),
        });
        links.push(ProjectMapLink::Dep {
            section: "decisions".to_string(),
            ulid: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
        });
        links.push(ProjectMapLink::Dep {
            section: "council".to_string(),
            ulid: "01KVBTMYNK4XXZJAKWMTB4AM9C".to_string(),
        });
        assert_eq!(validate_links(&links), Ok(()));
    }
}

#[cfg(test)]
mod feature_card_tests {
    use super::*;

    // ── parse_link unitaires : nouveaux rôles ────────────────────────────────

    #[test]
    fn release_value_parses_to_release_kind() {
        assert_eq!(
            parse_link("release:released"),
            Ok(ProjectMapLink::Release(ReleaseKind::Released))
        );
        assert_eq!(
            parse_link("release:planned"),
            Ok(ProjectMapLink::Release(ReleaseKind::Planned))
        );
        assert_eq!(
            parse_link("release:roadmap"),
            Ok(ProjectMapLink::Release(ReleaseKind::Roadmap))
        );
        assert_eq!(
            parse_link("release:dropped"),
            Ok(ProjectMapLink::Release(ReleaseKind::Dropped))
        );
    }

    #[test]
    fn release_uppercase_is_rejected_lowercase_wire() {
        // ReleaseKind est lowercase (miroir Zod du site), pas SCREAMING_SNAKE.
        assert_eq!(
            parse_link("release:PLANNED"),
            Err(SchemaError::InvalidRelease("PLANNED".to_string()))
        );
    }

    #[test]
    fn release_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("release:wip"),
            Err(SchemaError::InvalidRelease("wip".to_string()))
        );
    }

    #[test]
    fn release_empty_value_is_rejected() {
        assert_eq!(
            parse_link("release:"),
            Err(SchemaError::EmptyValue("release".to_string()))
        );
    }

    #[test]
    fn feature_ident_parses_to_feature() {
        assert_eq!(
            parse_link("feature:F-37"),
            Ok(ProjectMapLink::Feature("F-37".to_string()))
        );
        assert_eq!(
            parse_link("feature:F-061"),
            Ok(ProjectMapLink::Feature("F-061".to_string()))
        );
    }

    #[test]
    fn feature_empty_value_is_rejected() {
        assert_eq!(
            parse_link("feature:"),
            Err(SchemaError::EmptyValue("feature".to_string()))
        );
    }

    #[test]
    fn feature_lowercase_f_is_rejected() {
        assert_eq!(
            parse_link("feature:f-37"),
            Err(SchemaError::FeatureIdentInvalid("f-37".to_string()))
        );
    }

    #[test]
    fn feature_single_digit_is_rejected() {
        assert_eq!(
            parse_link("feature:F-1"),
            Err(SchemaError::FeatureIdentInvalid("F-1".to_string()))
        );
    }

    #[test]
    fn feature_four_digit_is_rejected() {
        assert_eq!(
            parse_link("feature:F-1234"),
            Err(SchemaError::FeatureIdentInvalid("F-1234".to_string()))
        );
    }

    #[test]
    fn feature_non_pattern_is_rejected() {
        assert_eq!(
            parse_link("feature:feature37"),
            Err(SchemaError::FeatureIdentInvalid("feature37".to_string()))
        );
    }

    #[test]
    fn supersedes_ident_parses_to_supersedes() {
        assert_eq!(
            parse_link("supersedes:F-12"),
            Ok(ProjectMapLink::Supersedes("F-12".to_string()))
        );
    }

    #[test]
    fn supersedes_invalid_ident_is_rejected() {
        assert_eq!(
            parse_link("supersedes:f-12"),
            Err(SchemaError::FeatureIdentInvalid("f-12".to_string()))
        );
    }

    #[test]
    fn supersedes_empty_value_is_rejected() {
        assert_eq!(
            parse_link("supersedes:"),
            Err(SchemaError::EmptyValue("supersedes".to_string()))
        );
    }

    #[test]
    fn release_kind_wire_roundtrip() {
        for r in [
            ReleaseKind::Roadmap,
            ReleaseKind::Planned,
            ReleaseKind::Released,
            ReleaseKind::Dropped,
        ] {
            assert_eq!(ReleaseKind::from_wire(r.as_wire()), Some(r));
        }
    }

    // ── validate_links : carte-feature ───────────────────────────────────────

    /// Carte-feature minimale valide : feature + project + status + kind + release + version.
    ///
    /// Le sentinel `[[version:gradatum/backlog]]` satisfait l'obligation §10e pour
    /// les features sans version concrète encore connue.
    fn minimal_feature_card() -> Vec<ProjectMapLink> {
        vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ]
    }

    #[test]
    fn feature_card_complete_is_valid() {
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn feature_card_with_backlog_version_is_valid() {
        // minimal_feature_card() porte déjà version:gradatum/backlog — le test
        // vérifie que le sentinel est bien accepté comme version conforme §10e.
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn feature_card_with_concrete_version_is_valid() {
        // Carte-feature avec version concrète (0.6.4) à la place du sentinel.
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.6.4".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_version_project_mismatch_is_rejected() {
        // La version namespacée doit correspondre au projet déclaré.
        // Carte construite directement (pas de push sur minimal) pour
        // avoir exactement 1 version — celle qui mismatche.
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "example-project".to_string(),
                version: "0.6.4".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionProjectMismatch {
                project: "gradatum".to_string(),
                version_project: "example-project".to_string(),
            })
        );
    }

    #[test]
    fn feature_card_with_one_supersedes_is_valid() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Supersedes("F-12".to_string()));
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_with_two_supersedes_does_not_inflate_feature_count() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Supersedes("F-12".to_string()));
        links.push(ProjectMapLink::Supersedes("F-13".to_string()));
        // supersedes ne compte pas comme feature → toujours valide.
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn two_features_is_rejected_by_feature_cardinality() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Feature("F-38".to_string()));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::FeatureCardinality(2))
        );
    }

    #[test]
    fn feature_card_zero_release_is_rejected() {
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(0))
        );
    }

    #[test]
    fn feature_card_two_releases_is_rejected() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Release(ReleaseKind::Released));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(2))
        );
    }

    #[test]
    fn release_without_feature_is_rejected() {
        // Un [[release:]] sur une carte SANS [[feature:]] est invalide.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(1))
        );
    }

    // ── NON-RÉGRESSION : cartes changelog (sans feature) ─────────────────────

    #[test]
    fn changelog_card_without_feature_or_release_stays_valid() {
        // Carte changelog historique typique : project + status + kind, RIEN d'autre.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn changelog_card_with_version_stays_valid() {
        // Carte changelog avec version : toujours valide, inchangée.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.5.2".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn changelog_card_zero_project_still_rejected_same_error() {
        // Régression : une carte changelog invalide (0 project) garde la MÊME erreur.
        let links = vec![
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(0))
        );
    }

    #[test]
    fn validate_from_targets_routes_feature_card() {
        // Wiring : validate_links_from_targets construit bien une carte-feature.
        // Quintuple §10e : feature + project + status + release + version.
        // Le sentinel version:gradatum/backlog est la forme minimale conforme.
        let targets: Vec<String> = [
            "feature:F-37",
            "project:gradatum",
            "status:IN_PROGRESS",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/backlog",
            "Titre humain ignoré",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(validate_links_from_targets(&targets), Ok(()));
    }

    // ── P2-1 (mis à jour S1) : carte-feature accepte tout kind ∈ KindKind ─────
    //
    // Slice 1 S1 : la contrainte `kind == FEATURE` sur les cartes-feature est
    // levée. Seul `kind:FEATURE` est exporté vers le site (export T2) ; les
    // autres kinds (FIX/TASK/ENHANCEMENT) restent vault-only.

    #[test]
    fn feature_card_with_kind_fix_is_accepted() {
        // Carte-feature avec kind:FIX → Ok (Slice 1 S1, mapping debt→FIX).
        // La version est requise (§10e).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_with_kind_feature_is_valid_kind_check() {
        // Carte-feature avec kind:FEATURE → Ok (inchangé, compatibilité).
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn changelog_card_kind_fix_stays_valid_without_feature() {
        // Non-régression : kind:FIX est légitime sur une carte changelog (sans feature).
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── S1 : carte-feature accepte tout kind ∈ KindKind (pas FEATURE seul) ────

    /// A feature card with `kind:FIX` passes validation (the `kind` link is not constrained).
    ///
    /// The 5 required link roles (feature/project/status/release/version) remain
    /// mandatory; only the `kind == FEATURE` constraint is relaxed.
    #[test]
    fn validate_links_accepts_feature_card_with_non_feature_kind() {
        // kind:FIX — mapping gov-todo debt→FIX (§3.1 spec Slice 1).
        let links_fix = vec![
            ProjectMapLink::Feature("F-83".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links_fix),
            Ok(()),
            "kind:FIX doit être accepté sur une carte-feature"
        );

        // kind:TASK — le catch-all qui absorbe l'ex-CHORE (mapping gov-todo chore→TASK).
        let links_task = vec![
            ProjectMapLink::Feature("F-84".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Task),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links_task),
            Ok(()),
            "kind:TASK doit être accepté sur une carte-feature"
        );
    }

    /// Non-régression : un `kind` absent reste rejeté (`KindCardinality`).
    ///
    /// Le relâchement S1 ne touche que la contrainte `== FEATURE` ;
    /// l'obligation d'avoir exactement 1 `kind` reste inchangée.
    #[test]
    fn validate_links_still_rejects_missing_kind() {
        let links = vec![
            ProjectMapLink::Feature("F-83".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            // Pas de Kind → KindCardinality(0).
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::KindCardinality(0)),
            "kind absent doit toujours être rejeté"
        );
    }

    // ── P1 conformité §10e : [[version:]] OBLIGATOIRE sur carte-feature ──────

    #[test]
    fn feature_card_without_version_is_rejected() {
        // NOMENCLATURE §10e + spec §3.1 : quintuple feature+project+status+release+version.
        // Une carte-feature sans [[version:]] doit être rejetée (le sentinel
        // [[version:gradatum/backlog]] existe précisément pour ce cas).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Kind(KindKind::Feature),
            // Pas de Version — doit provoquer FeatureVersionCardinality(0).
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::FeatureVersionCardinality(0))
        );
    }
}

#[cfg(test)]
mod parent_tests {
    use super::*;

    // ── parse_link : rôle parent ─────────────────────────────────────────────

    /// `[[parent:F-31]]` parse en `ProjectMapLink::Parent("F-31")`.
    #[test]
    fn parent_valid_ident_parses_to_parent() {
        assert_eq!(
            parse_link("parent:F-31"),
            Ok(ProjectMapLink::Parent("F-31".to_string()))
        );
    }

    /// `[[parent:F-100]]` : 3 chiffres acceptés.
    #[test]
    fn parent_three_digit_ident_is_accepted() {
        assert_eq!(
            parse_link("parent:F-100"),
            Ok(ProjectMapLink::Parent("F-100".to_string()))
        );
    }

    /// `[[parent:f-31]]` : minuscule `f` rejeté via `validate_feature_ident`.
    #[test]
    fn parent_lowercase_f_is_rejected() {
        assert_eq!(
            parse_link("parent:f-31"),
            Err(SchemaError::FeatureIdentInvalid("f-31".to_string()))
        );
    }

    /// `[[parent:foo]]` : format non-`F-\d{2,3}` rejeté.
    #[test]
    fn parent_invalid_format_is_rejected() {
        assert_eq!(
            parse_link("parent:foo"),
            Err(SchemaError::FeatureIdentInvalid("foo".to_string()))
        );
    }

    /// `[[parent:F-1]]` : 1 chiffre insuffisant.
    #[test]
    fn parent_single_digit_is_rejected() {
        assert_eq!(
            parse_link("parent:F-1"),
            Err(SchemaError::FeatureIdentInvalid("F-1".to_string()))
        );
    }

    /// `[[parent:F-1234]]` : 4 chiffres = trop long.
    #[test]
    fn parent_four_digits_is_rejected() {
        assert_eq!(
            parse_link("parent:F-1234"),
            Err(SchemaError::FeatureIdentInvalid("F-1234".to_string()))
        );
    }

    /// `[[parent:]]` : valeur vide rejetée par la garde `reserved && value.is_empty()`.
    #[test]
    fn parent_empty_value_is_rejected() {
        assert_eq!(
            parse_link("parent:"),
            Err(SchemaError::EmptyValue("parent".to_string()))
        );
    }

    // ── validate_links : cardinalité 0..N, ne déclenche pas d'erreur ─────────

    /// Carte-feature avec 1 parent → acceptée (cardinalité 0..N).
    #[test]
    fn feature_card_with_one_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// Deux parents sur la même carte → accepté (0..N, cardinalité non limitée).
    #[test]
    fn feature_card_with_two_parents_does_not_trigger_cardinality_error() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
            ProjectMapLink::Parent("F-44".to_string()),
        ];
        // multi-parent : ne déclenche aucune erreur de cardinalité.
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// `Parent` ne gonfle pas le compteur `feature_count` (ne fait pas basculer
    /// validate_links en mode carte-feature si aucun `Feature` n'est présent).
    #[test]
    fn parent_alone_does_not_trigger_feature_card_mode() {
        // Carte changelog (sans Feature) + 1 Parent → validate_links évalue en
        // mode changelog (0 feature_count) → le Parent est ignoré du comptage.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── carte-feature COMPLÈTE avec parent → OK ──────────────────────────────

    /// Carte-feature complète (feature+project+status+kind:FEATURE+release+version)
    /// plus `[[parent:F-31]]` → validation OK.
    #[test]
    fn complete_feature_card_with_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.7.0".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── NON-RÉGRESSION : cartes sans parent valident identiquement ────────────

    /// Carte-feature SANS parent reste valide (0 parent = ok).
    #[test]
    fn feature_card_without_parent_stays_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// Carte changelog (sans Feature) + Supersedes + Parent → tous ignorés du
    /// comptage, carte valide si triple obligatoire présent.
    #[test]
    fn changelog_card_with_supersedes_and_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Supersedes("F-12".to_string()),
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── reserved_node_target : parent → nœud feature:F-YY ───────────────────

    /// `parent:F-31` dans reserved_node_target → `Some("feature:F-31")`.
    #[test]
    fn parent_maps_to_feature_reserved_node() {
        assert_eq!(
            reserved_node_target("parent:F-31"),
            Some("feature:F-31".to_string())
        );
    }

    /// `parent:f-31` invalide → `None` (parse échoue, reserved_node_target retourne None).
    #[test]
    fn parent_invalid_maps_to_none() {
        assert_eq!(reserved_node_target("parent:f-31"), None);
    }
}

#[cfg(test)]
mod roles_of_body_tests {
    use super::*;

    #[test]
    fn extracts_kind_and_status_from_body() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FIX]]\n\n## Objet\nrien";
        let roles = roles_of_body(body);
        assert_eq!(roles.kind, Some("FIX"));
        assert_eq!(roles.status, Some("OPEN"));
    }

    #[test]
    fn body_without_roles_yields_none() {
        let roles = roles_of_body("## Objet\nune note ordinaire, sans wikilink typé");
        assert_eq!(roles.kind, None);
        assert_eq!(roles.status, None);
    }

    #[test]
    fn prose_mentioning_a_type_is_not_a_role() {
        // Le mot FIX en prose, ou un [[decisions:…]] : parse_link ne les prend pas
        // pour des rôles. C'est exactement ce que le substring ratait.
        let body = "On corrige le bug (FIX) — voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]].";
        let roles = roles_of_body(body);
        assert_eq!(roles.kind, None, "FIX en prose n'est pas un rôle");
        assert_eq!(roles.status, None);
    }

    #[test]
    fn malformed_reserved_value_is_ignored() {
        // [[status:done]] (minuscule) est rejeté par parse_link → aucun status.
        let roles = roles_of_body("[[kind:FIX]] [[status:done]]");
        assert_eq!(roles.kind, Some("FIX"));
        assert_eq!(
            roles.status, None,
            "status:done minuscule rejeté par la taxonomie"
        );
    }
}
