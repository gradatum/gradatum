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
//! `feature` · `release` · `supersedes` · `parent` · `track` · `visibilite` ·
//! `porteuse`. Any other prefix (e.g. `decisions:`) is a **content dependency**
//! ([`ProjectMapLink::Dep`]) that reuses the existing `[[section:ULID]]` format without
//! regression.
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
//! | track | `[[track:gradatum/2.2.0]]` | [`ProjectMapLink::Track`] |
//! | visibilite | `[[visibilite:public]]` | [`ProjectMapLink::Visibility`] |
//! | porteuse | `[[porteuse:gradatum/2.2.0]]` | [`ProjectMapLink::Porteuse`] |
//! | dependency | `[[decisions:01K…]]` | [`ProjectMapLink::Dep`] |
//!
//! ## Structure cards
//!
//! `kind:ROADMAP` (per version) and `kind:BACKLOG` (per project) are **structure cards**:
//! pointed at by work cards through `[[track:<project>/<target>]]`, they point at nothing
//! themselves (no `track`, no `feature`, no `release`). A ROADMAP carries a
//! `[[visibilite:…]]` (interne|public) and an optional `[[porteuse:…]]`. See
//! [`validate_links`] for the exact cardinalities.
//!
//! ## Work cards
//!
//! A work card (`kind` non-structure) allows **at most one** each of `[[feature:F-XX]]`,
//! `[[release:…]]` and `[[version:…]]`. The `[[track:]]` role
//! carries the version/release axis — the server derives `release` from it — so `version`
//! and `release` are optional in the body and their removal must no longer be rejected.
//! The `[[track:]]` role stays **at most one** here (the additive-window cardinality is
//! unchanged — see [`validate_links`]). See [`validate_links`] for the exact cardinalities.
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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

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
    /// Per-version **ROADMAP** structure card.
    ///
    /// A structure card is **pointed at** (via `[[track:…]]`) and points at nothing: it
    /// carries no `[[track:]]` and is never numbered (`[[feature:]]` forbidden). A ROADMAP
    /// additionally carries a `[[visibilite:…]]` (exactly 1) — the interne/public gate that
    /// governs whether its cards reach the public website.
    Roadmap,
    /// Per-project **BACKLOG** structure card.
    ///
    /// The single home a project's cards attach to when no version yet carries them. Like
    /// [`KindKind::Roadmap`] it is a structure card (pointed at, points at nothing, never
    /// numbered), but it carries **no** `[[visibilite:]]`: a backlog is never published.
    Backlog,
}

impl KindKind {
    /// Parses the SCREAMING_SNAKE wire value of a `[[kind:…]]` wikilink.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "FEATURE" => Some(Self::Feature),
            "ENHANCEMENT" => Some(Self::Enhancement),
            "FIX" => Some(Self::Fix),
            "TASK" => Some(Self::Task),
            "ROADMAP" => Some(Self::Roadmap),
            "BACKLOG" => Some(Self::Backlog),
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
            Self::Roadmap => "ROADMAP",
            Self::Backlog => "BACKLOG",
        }
    }

    /// Whether this kind is a **structure card** (a ROADMAP/BACKLOG hub that is pointed at
    /// but points at nothing), as opposed to a **work card** (FEATURE/FIX/TASK/ENHANCEMENT).
    ///
    /// Single source of the structure-vs-work distinction consumed by [`validate_links`]
    /// (structure cards forbid `[[track:]]`/`[[feature:]]`, require an addressable version)
    /// and by the export projection (structure cards never reach the public website).
    #[must_use]
    pub const fn is_structure(&self) -> bool {
        matches!(self, Self::Roadmap | Self::Backlog)
    }
}

/// Visibility of a project-map card — the interne/public gate.
///
/// Carried by a `[[visibilite:…]]` wikilink. Wire values are **lowercase**
/// (`interne` / `public`), mirroring [`ReleaseKind`]. Two card classes carry it, with
/// **opposite** cardinality and reader defaults — see [`validate_links`]:
///
/// - **Structure ROADMAP**: **exactly 1** — mandatory, never absent, never deduced.
///   A ROADMAP without it is *rejected* at validation, so no reader default ever applies to a
///   valid ROADMAP. It gates whether the roadmap's cards reach the public website.
/// - **Work card** (FEATURE/FIX/TASK/ENHANCEMENT): **at most 1** — optional. Absence
///   means **public** (the export includes the card): internality is a *declared act*, never
///   an oversight. A card carrying `[[visibilite:interne]]` is excluded from the public
///   catalogue, **in addition** to the existing `kind`/`dropped` filters. This is the
///   dedicated exclusion axis that replaces the vanished "only FEATURE is exported" side
///   effect (folding ENHANCEMENT onto FEATURE removed that guarantee).
///
/// Deliberately has **no `Default`** (same discipline as [`ReleaseKind`]): an absent value is
/// *interpreted by the reader* (export), never by a `Default` on this type — the default
/// differs by card class (ROADMAP: absence is rejected upstream; work card: absence = public),
/// so encoding a single `Default` here would silently pick the wrong one for one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VisibilityKind {
    /// Internal — never surfaced on the public website.
    Interne,
    /// Public — eligible for the public catalogue / roadmap.
    Public,
}

impl VisibilityKind {
    /// Parses the **lowercase** wire value of a `[[visibilite:…]]` wikilink.
    ///
    /// Matching is case-sensitive: `"public"` is accepted, `"PUBLIC"` is rejected.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "interne" => Some(Self::Interne),
            "public" => Some(Self::Public),
            _ => None,
        }
    }

    /// Returns the **lowercase** wire representation (inverse of `from_wire`).
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Interne => "interne",
            Self::Public => "public",
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

/// Structure card a work card's [`ProjectMapLink::Track`] resolves to, as consumed by
/// `derive_release` (the `track → release` derivation).
///
/// A `[[track:…]]` is only ever valid pointing at a **structure card** — a ROADMAP or a
/// BACKLOG ([`KindKind::is_structure`]). That invariant is enforced by the **resolver**
/// ([`StructureIndex::resolve_track`]), which looks the pointer up in an index built solely from
/// structure cards and errors when it resolves to nothing — **not** by [`validate_links`], which
/// validates one card in isolation and never resolves the target's kind (it only checks the
/// track link's own format and cardinality). This enum encodes exactly the two structure shapes,
/// so once resolution has succeeded the derivation is *total over its input domain*
/// (parse-don't-validate): there is no representable "track points at a FEATURE" state left to
/// guard against at derivation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrackTarget {
    /// A per-version ROADMAP hub.
    Roadmap {
        /// The ROADMAP's own lifecycle status.
        status: StatusKind,
        /// Lifecycle status of the public ROADMAP named by this ROADMAP's `[[porteuse:…]]`
        /// link, when present. **Consulted only when `status` is [`StatusKind::Obsolete`]**
        /// (rule 4, roll-forward); ignored for any other `status`.
        porteuse_status: Option<StatusKind>,
    },
    /// A per-project BACKLOG hub. Its lifecycle status is irrelevant to the derived release:
    /// a backlogged card sits in the "no version committed" bucket whatever the hub's status.
    Backlog,
}

/// Error type of `derive_release`, retained so the derivation stays *typed-fallible* — a
/// malformed structure would yield a typed error, never a panic nor a silently wrong release.
///
/// **Not produced in the current mapping**: every
/// [`StatusKind`] now maps to a release bucket, so `derive_release` never returns `Err`.
/// This type is deliberately kept (rather than removed) because it is referenced *in type
/// position* by the readers' visible-fallback path
/// ([`DerivationFallbackReason::Undetermined`]): removing the sole variant would make that
/// enum uninhabited and cascade "unreachable pattern" breakage across the admin/server
/// readers. Keeping it reachable-in-type also makes the decision reversible. See
/// `derive_roadmap_release`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ReleaseDerivationError {
    /// A tracked ROADMAP (or a porteuse ROADMAP) carried a lifecycle status with no release
    /// mapping. **No longer produced** since the P2-A total mapping (option a, 2026-09-03):
    /// `BRAINSTORMING` and `BLOCKED` now map to `roadmap` / `planned` respectively. Kept as a
    /// reachable-in-type variant for the readers' visible fallback and for reversibility.
    #[error(
        "ROADMAP status {} has no release mapping (expected DONE/OPEN/IN_PROGRESS/OBSOLETE)",
        .0.as_wire()
    )]
    UndeterminedRoadmapStatus(StatusKind),
}

/// The reserved "no real release target" version sentinel.
///
/// A `[[track:<project>/0.0.0]]` names the bootstrap ROADMAP (`gradatum/0.0.0` —
/// *"bootstrap, attribution lost"*): a structural placeholder that commits to **no**
/// shippable version. A work card whose track resolves to it therefore has, by
/// construction, no delivery. Kept as a named constant (ADN 4: no magic literal) so the
/// single sentinel value is defined once.
const SENTINEL_TRACK_VERSION: &str = "0.0.0";

/// `true` when a resolved track version names the [`SENTINEL_TRACK_VERSION`]
/// ("no real release target").
///
/// Only the version component is inspected: an optional `<project>/` prefix is stripped
/// first (the project is already validated against the card's `[[project:]]` link). Same
/// intent as [`map_version_raw`] — drop the `<project>/` namespace — but a **distinct**
/// mechanism: this helper splits on the **last** `/` (`rsplit_once`) while `map_version_raw`
/// splits on the **first** (`split_once`). The two coincide only under the single-slash
/// invariant `<project>/<target>`; they are not interchangeable in general. Pure, total,
/// no allocation.
fn is_sentinel_track_version(track_version: &str) -> bool {
    let version = track_version
        .rsplit_once('/')
        .map_or(track_version, |(_, v)| v);
    version == SENTINEL_TRACK_VERSION
}

/// Derives the delivery status ([`ReleaseKind`]) of a **work card** from its own lifecycle
/// status first, then — only for a non-terminal card — from the structure it tracks.
///
/// Single source of the `track → release` derivation. **Pure** (no I/O, deterministic). It
/// applies the *card-status-first* rule:
///
/// 0. **Track-sentinel guard**: a card whose resolved track version is the
///    [`SENTINEL_TRACK_VERSION`] (`0.0.0` — the bootstrap ROADMAP "bootstrap, attribution
///    lost", which commits to no shippable version) is [`ReleaseKind::Dropped`],
///    evaluated *before* rule 1. A closed/superseded `DONE` card pinned to
///    `gradatum/0.0.0` therefore drops instead of leaking as `released`+`v0.0.0` into
///    `export-features`. A real tracked version is never the sentinel, so rule 1's nominal
///    `DONE → released` path is unaffected.
/// 1. A `DONE` card is [`ReleaseKind::Released`], **whatever** the tracked structure. This
///    alone resolves the known blockers (features delivered under an internal ROADMAP
///    since rolled into a later version). It also upholds the invariant that a `DONE` card
///    never reaches the backlog bucket, because it returns *before* the structure is inspected.
/// 2. An `OBSOLETE` card is [`ReleaseKind::Dropped`].
/// 3. Any other card status (`OPEN` / `IN_PROGRESS` / `BLOCKED` / `BRAINSTORMING`) derives
///    from the tracked structure: BACKLOG → [`ReleaseKind::Roadmap`]; ROADMAP `DONE` →
///    [`ReleaseKind::Released`]; ROADMAP `OPEN` / `IN_PROGRESS` → [`ReleaseKind::Planned`];
///    ROADMAP `OBSOLETE` → [`ReleaseKind::Dropped`] (unless it carries a porteuse — rule 4);
///    ROADMAP `BLOCKED` → [`ReleaseKind::Planned`] and ROADMAP `BRAINSTORMING` →
///    [`ReleaseKind::Roadmap`] (P2-A total mapping, option a — 2026-09-03).
/// 4. Roll-forward: a non-terminal card tracking an `OBSOLETE` ROADMAP that carries a
///    `[[porteuse:…]]` derives from the **carrying** (porteuse) ROADMAP's status instead of
///    dropping — one level, no further recursion.
///
/// # BACKLOG → `ReleaseKind::Roadmap` (documented equivalence, not a fabricated variant)
///
/// The agreed rule text names the backlog-derived state `"backlog"`, but [`ReleaseKind`] has
/// **no `Backlog` variant**: its wire vocabulary mirrors the public-site enum exactly
/// (`roadmap` / `planned` / `released` / `dropped`). In the shipped model the backlog bucket
/// *is* `[[release:roadmap]]` — a backlogged card carries `release: roadmap` +
/// `version: gradatum/backlog` (the "doublon" this migration collapses). This function
/// therefore maps BACKLOG to the existing [`ReleaseKind::Roadmap`] — the documented
/// "considered for the long term, no target version committed" bucket — rather than inventing
/// a variant. Note for the integration phase: the census labels that same bucket
/// `"backlog"`, so the census recabling must treat `roadmap` ≡ `backlog`.
///
/// # Errors
///
/// None in the current mapping: since the P2-A total mapping (option a, 2026-09-03) every
/// [`StatusKind`] resolves to a release bucket, so derivation never fails. The `Result` return
/// and [`ReleaseDerivationError`] are retained for the readers' visible-fallback path and for
/// reversibility — see [`ReleaseDerivationError`] and `derive_roadmap_release`.
#[must_use = "the derived release status must be observed — dropping it discards the derivation"]
pub(crate) fn derive_release(
    card_status: StatusKind,
    track_target: TrackTarget,
    track_version: &str,
) -> Result<ReleaseKind, ReleaseDerivationError> {
    // Rule 0 — a card tracking the "no real release target" sentinel (0.0.0) has no
    // delivery, whatever its own status. Evaluated BEFORE the DONE short-circuit (rule 1):
    // a closed/superseded DONE card pinned to the bootstrap ROADMAP must derive `dropped`,
    // not `released`. Without this guard such cards leaked as `released`+`v0.0.0` into
    // `export-features` (F-184 regression). A real version (e.g. `2.1.0`) is never the
    // sentinel, so the nominal `DONE + real target → released` path (rule 1) is untouched.
    if is_sentinel_track_version(track_version) {
        return Ok(ReleaseKind::Dropped);
    }
    // Rule 1 — a completed work card is released, whatever it tracks. Resolves the 11
    // blockers, and short-circuits before the structure so §1.4 cannot be violated.
    if card_status == StatusKind::Done {
        return Ok(ReleaseKind::Released);
    }
    // Rule 2 — an abandoned work card is dropped.
    if card_status == StatusKind::Obsolete {
        return Ok(ReleaseKind::Dropped);
    }
    // Rules 3 & 4 — a non-terminal card derives from the structure it tracks.
    match track_target {
        TrackTarget::Backlog => Ok(ReleaseKind::Roadmap),
        // Rule 4 — OBSOLETE roadmap carrying a porteuse: roll forward onto the carrier.
        TrackTarget::Roadmap {
            status: StatusKind::Obsolete,
            porteuse_status: Some(carrier),
        } => derive_roadmap_release(carrier),
        // All other roadmaps (incl. OBSOLETE without porteuse) derive from their own status.
        TrackTarget::Roadmap { status, .. } => derive_roadmap_release(status),
    }
}

/// Maps a ROADMAP lifecycle status to a [`ReleaseKind`], shared by the direct ROADMAP case
/// and the porteuse roll-forward.
///
/// The mapping is **total** over [`StatusKind`]: every status resolves to a release bucket,
/// so this helper never returns `Err`.
/// The `Result` return is kept (rather than `-> ReleaseKind`) so the fallible surface — and the
/// visible-fallback path in the readers that still matches
/// [`ReleaseDerivationError::UndeterminedRoadmapStatus`] — stays reachable-in-type, and the
/// decision is reversible by re-introducing an `Err` arm. See [`ReleaseDerivationError`].
///
/// # Errors
///
/// None in the current mapping — the signature keeps `Result` for the reason above. Were a
/// future [`StatusKind`] variant to be added without a delivery meaning, this exhaustive match
/// would fail to compile (`#[non_exhaustive]` does not silence same-crate matches), forcing an
/// explicit decision — that is where the typed error would be produced again.
fn derive_roadmap_release(status: StatusKind) -> Result<ReleaseKind, ReleaseDerivationError> {
    match status {
        StatusKind::Done => Ok(ReleaseKind::Released),
        StatusKind::Open | StatusKind::InProgress => Ok(ReleaseKind::Planned),
        StatusKind::Obsolete => Ok(ReleaseKind::Dropped),
        // P2-A tranché (option a, 2026-09-03) — mapping TOTAL, la dérivation ne peut plus
        // échouer sur ces deux statuts :
        //   BLOCKED → Planned : une version engagée mais stallée reste planifiée.
        StatusKind::Blocked => Ok(ReleaseKind::Planned),
        //   BRAINSTORMING → Roadmap : bucket backlog/exploration, cohérent avec BACKLOG→Roadmap.
        StatusKind::Brainstorming => Ok(ReleaseKind::Roadmap),
    }
}

#[cfg(test)]
mod derive_release_tests {
    use super::*;

    /// The non-terminal card statuses — those that fall through to structure derivation
    /// (rule 3). Used to prove the derivation is independent of *which* non-terminal status
    /// the card holds.
    const NON_TERMINAL: [StatusKind; 4] = [
        StatusKind::Brainstorming,
        StatusKind::Open,
        StatusKind::InProgress,
        StatusKind::Blocked,
    ];

    /// A concrete, shippable tracked version — never the `0.0.0` sentinel. Threaded into
    /// every non-sentinel case so rule 0 is a no-op and the pre-F-184 behaviour is asserted
    /// unchanged.
    const REAL_VERSION: &str = "2.1.0";

    // ── Rule 0 — track-sentinel guard (F-184) ────────────────────────────────────────

    /// F-184 regression: a `DONE` card closed/superseded under the bootstrap ROADMAP
    /// (`[[track:gradatum/0.0.0]]`, resolving to a DONE roadmap with no porteuse) must derive
    /// `dropped`, NOT `released` — the sentinel guard (rule 0) fires before the DONE
    /// short-circuit (rule 1). Reproduces the exact resolved shape of F-09/36/54/65.
    #[test]
    fn done_superseded_card_is_dropped() {
        let got = derive_release(
            StatusKind::Done,
            TrackTarget::Roadmap {
                status: StatusKind::Done,
                porteuse_status: None,
            },
            "0.0.0",
        );
        assert_eq!(got, Ok(ReleaseKind::Dropped));
    }

    /// Control (non-regression): the *same* DONE-card / DONE-roadmap shape tracking a **real**
    /// version derives `released`. Proves rule 0 keys on the version identity alone, not on the
    /// tracked roadmap status (identical here), so the nominal rule-1 path is untouched.
    #[test]
    fn done_card_on_real_version_stays_released() {
        let got = derive_release(
            StatusKind::Done,
            TrackTarget::Roadmap {
                status: StatusKind::Done,
                porteuse_status: None,
            },
            "gradatum/2.1.0",
        );
        assert_eq!(got, Ok(ReleaseKind::Released));
    }

    /// The sentinel guard is independent of the `<project>/` prefix: a bare `0.0.0` and a
    /// namespaced `gradatum/0.0.0` both drop.
    #[test]
    fn sentinel_guard_ignores_project_prefix() {
        for track in ["0.0.0", "gradatum/0.0.0"] {
            assert_eq!(
                derive_release(StatusKind::Done, TrackTarget::Backlog, track),
                Ok(ReleaseKind::Dropped),
                "track {track:?} must resolve to the sentinel and drop"
            );
        }
    }

    /// Rule 0 fires independently of the card status: a **non-terminal** card (here `OPEN`,
    /// which would otherwise fall through to structure derivation, rule 3) tracking the
    /// `0.0.0` sentinel drops just like a `DONE` one. Locks the intended side effect — a
    /// `0.0.0` target commits to no shippable version, so the card is `dropped` whatever its
    /// own lifecycle status — and guards against a future regression that would gate rule 0
    /// behind the DONE short-circuit.
    #[test]
    fn non_terminal_card_tracking_sentinel_is_dropped() {
        let got = derive_release(
            StatusKind::Open,
            TrackTarget::Roadmap {
                status: StatusKind::Open,
                porteuse_status: None,
            },
            "gradatum/0.0.0",
        );
        assert_eq!(got, Ok(ReleaseKind::Dropped));
    }

    // ── Rule 1 — DONE card ⇒ released, whatever it tracks ────────────────────────────

    /// The known-blockers shape reproduced literally: a `DONE` card tracking an `OBSOLETE`
    /// internal ROADMAP with **no** porteuse must derive `released` (not `dropped`).
    #[test]
    fn done_card_under_obsolete_roadmap_without_porteuse_is_released() {
        let got = derive_release(
            StatusKind::Done,
            TrackTarget::Roadmap {
                status: StatusKind::Obsolete,
                porteuse_status: None,
            },
            REAL_VERSION,
        );
        assert_eq!(got, Ok(ReleaseKind::Released));
    }

    /// Rule 1 dominates every structure shape, including BACKLOG and a ROADMAP whose own
    /// status maps to a different bucket — the card status is read first.
    #[test]
    fn done_card_is_released_regardless_of_track_target() {
        let targets = [
            TrackTarget::Backlog,
            TrackTarget::Roadmap {
                status: StatusKind::Open,
                porteuse_status: None,
            },
            TrackTarget::Roadmap {
                status: StatusKind::Done,
                porteuse_status: None,
            },
            TrackTarget::Roadmap {
                status: StatusKind::Obsolete,
                porteuse_status: Some(StatusKind::Obsolete),
            },
            // Structure status mapping to a different bucket — rule 1 still short-circuits.
            TrackTarget::Roadmap {
                status: StatusKind::Brainstorming,
                porteuse_status: None,
            },
        ];
        for target in targets {
            assert_eq!(
                derive_release(StatusKind::Done, target, REAL_VERSION),
                Ok(ReleaseKind::Released),
                "DONE card must be released for {target:?}"
            );
        }
    }

    /// Invariant §1.4 stated explicitly: a `DONE` card tracking a BACKLOG must never fall
    /// into the backlog bucket (`ReleaseKind::Roadmap`) — it is `released`.
    #[test]
    fn done_card_on_backlog_never_derives_the_backlog_bucket() {
        assert_eq!(
            derive_release(StatusKind::Done, TrackTarget::Backlog, REAL_VERSION),
            Ok(ReleaseKind::Released),
        );
    }

    // ── Rule 2 — OBSOLETE card ⇒ dropped, whatever it tracks ─────────────────────────

    #[test]
    fn obsolete_card_is_dropped_regardless_of_track_target() {
        let targets = [
            TrackTarget::Backlog,
            TrackTarget::Roadmap {
                status: StatusKind::Done,
                porteuse_status: None,
            },
            TrackTarget::Roadmap {
                status: StatusKind::Brainstorming,
                porteuse_status: None,
            },
        ];
        for target in targets {
            assert_eq!(
                derive_release(StatusKind::Obsolete, target, REAL_VERSION),
                Ok(ReleaseKind::Dropped),
                "OBSOLETE card must be dropped for {target:?}"
            );
        }
    }

    // ── Rule 3 — non-terminal card ⇒ derive from the tracked structure ───────────────

    #[test]
    fn non_terminal_card_on_backlog_derives_roadmap_bucket() {
        for card in NON_TERMINAL {
            assert_eq!(
                derive_release(card, TrackTarget::Backlog, REAL_VERSION),
                Ok(ReleaseKind::Roadmap),
                "non-terminal card {card:?} tracking BACKLOG must derive the roadmap bucket"
            );
        }
    }

    #[test]
    fn non_terminal_card_on_done_roadmap_is_released() {
        for card in NON_TERMINAL {
            assert_eq!(
                derive_release(
                    card,
                    TrackTarget::Roadmap {
                        status: StatusKind::Done,
                        porteuse_status: None,
                    },
                    REAL_VERSION,
                ),
                Ok(ReleaseKind::Released),
                "non-terminal card {card:?} tracking a DONE roadmap must be released"
            );
        }
    }

    #[test]
    fn non_terminal_card_on_active_roadmap_is_planned() {
        for card in NON_TERMINAL {
            for roadmap in [StatusKind::Open, StatusKind::InProgress] {
                assert_eq!(
                    derive_release(
                        card,
                        TrackTarget::Roadmap {
                            status: roadmap,
                            porteuse_status: None,
                        },
                        REAL_VERSION,
                    ),
                    Ok(ReleaseKind::Planned),
                    "card {card:?} tracking {roadmap:?} roadmap must be planned"
                );
            }
        }
    }

    #[test]
    fn non_terminal_card_on_obsolete_roadmap_without_porteuse_is_dropped() {
        for card in NON_TERMINAL {
            assert_eq!(
                derive_release(
                    card,
                    TrackTarget::Roadmap {
                        status: StatusKind::Obsolete,
                        porteuse_status: None,
                    },
                    REAL_VERSION,
                ),
                Ok(ReleaseKind::Dropped),
                "non-terminal card {card:?} tracking an OBSOLETE roadmap without porteuse drops"
            );
        }
    }

    // ── Rule 4 — porteuse roll-forward ───────────────────────────────────────────────

    #[test]
    fn obsolete_roadmap_with_porteuse_rolls_forward_onto_carrier() {
        let cases = [
            (StatusKind::Done, ReleaseKind::Released),
            (StatusKind::Open, ReleaseKind::Planned),
            (StatusKind::InProgress, ReleaseKind::Planned),
            (StatusKind::Obsolete, ReleaseKind::Dropped),
        ];
        // Rule 4 is independent of *which* non-terminal status the card holds (rule 3 domain).
        for card in NON_TERMINAL {
            for (carrier, expected) in cases {
                assert_eq!(
                    derive_release(
                        card,
                        TrackTarget::Roadmap {
                            status: StatusKind::Obsolete,
                            porteuse_status: Some(carrier),
                        },
                        REAL_VERSION,
                    ),
                    Ok(expected),
                    "card {card:?}: roll-forward onto a {carrier:?} carrier must derive {expected:?}"
                );
            }
        }
    }

    /// The porteuse is consulted **only** when the tracked roadmap is OBSOLETE. A porteuse on
    /// a non-obsolete roadmap is ignored; the roadmap's own status wins.
    #[test]
    fn porteuse_is_ignored_when_tracked_roadmap_is_not_obsolete() {
        for card in NON_TERMINAL {
            let got = derive_release(
                card,
                TrackTarget::Roadmap {
                    status: StatusKind::Done,
                    porteuse_status: Some(StatusKind::Obsolete),
                },
                REAL_VERSION,
            );
            assert_eq!(
                got,
                Ok(ReleaseKind::Released),
                "card {card:?}: porteuse must be ignored on a non-obsolete (DONE) roadmap"
            );
        }
    }

    // ── Total mapping — ROADMAP BLOCKED / BRAINSTORMING (P2-A tranché, option a) ──────

    /// P2-A option a (2026-09-03): a non-terminal card directly tracking a `BLOCKED` roadmap
    /// derives `planned` (version committed but stalled), and a `BRAINSTORMING` roadmap
    /// derives the `roadmap` backlog bucket. Neither errors any longer — the derivation is
    /// total. (Replaces the former `…_undeterminable_roadmap_errors` expectation.)
    #[test]
    fn non_terminal_card_on_blocked_or_brainstorming_roadmap_derives_total() {
        let cases = [
            (StatusKind::Blocked, ReleaseKind::Planned),
            (StatusKind::Brainstorming, ReleaseKind::Roadmap),
        ];
        for card in NON_TERMINAL {
            for (roadmap, expected) in cases {
                assert_eq!(
                    derive_release(
                        card,
                        TrackTarget::Roadmap {
                            status: roadmap,
                            porteuse_status: None,
                        },
                        REAL_VERSION,
                    ),
                    Ok(expected),
                    "card {card:?}: a {roadmap:?} roadmap must derive {expected:?}"
                );
            }
        }
    }

    /// The porteuse roll-forward inherits the same total mapping: rolling an OBSOLETE roadmap
    /// forward onto a `BLOCKED` carrier derives `planned`, onto a `BRAINSTORMING` carrier
    /// derives `roadmap` — no error, mirroring the direct case. (Replaces the former
    /// `roll_forward_onto_undeterminable_carrier_errors` expectation.)
    #[test]
    fn roll_forward_onto_blocked_or_brainstorming_carrier_derives_total() {
        let cases = [
            (StatusKind::Blocked, ReleaseKind::Planned),
            (StatusKind::Brainstorming, ReleaseKind::Roadmap),
        ];
        for card in NON_TERMINAL {
            for (carrier, expected) in cases {
                assert_eq!(
                    derive_release(
                        card,
                        TrackTarget::Roadmap {
                            status: StatusKind::Obsolete,
                            porteuse_status: Some(carrier),
                        },
                        REAL_VERSION,
                    ),
                    Ok(expected),
                    "card {card:?}: roll-forward onto a {carrier:?} carrier must derive {expected:?}"
                );
            }
        }
    }
}

/// Error returned by [`StructureIndex::resolve_track`] when a work card's `[[track:]]` pointer
/// cannot be resolved to a structure card.
///
/// Never a panic: a dangling or mis-targeted pointer yields a typed error that the reader
/// **surfaces** (log + fall back to the stored `[[release:]]`), never a silent drop nor a
/// fabricated release. This is the resolution counterpart of [`ReleaseDerivationError`] (which
/// covers a *resolved* structure whose status has no release meaning).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TrackResolutionError {
    /// No structure card carries the tracked identity `project/target` — the pointer is dangling,
    /// or names a version that only a FEATURE card (never indexed) carries.
    #[error(
        "track target {identity:?} resolves to no structure card (dangling, or points at a non-structure)"
    )]
    TargetNotFound {
        /// The unresolved `project/target` identity.
        identity: String,
    },
    /// The tracked `OBSOLETE` ROADMAP names a `[[porteuse:…]]` that itself resolves to no
    /// structure card, so the roll-forward carrier (rule 4 of `derive_release`) cannot be read.
    #[error(
        "OBSOLETE roadmap {roadmap:?} names porteuse {porteuse:?} which resolves to no structure card"
    )]
    PorteuseNotFound {
        /// Identity of the tracked OBSOLETE ROADMAP.
        roadmap: String,
        /// Identity of the missing porteuse carrier.
        porteuse: String,
    },
}

/// Derivation-relevant facts about a single **structure card** (ROADMAP/BACKLOG), indexed by
/// [`StructureIndex`] under the card's addressable identity (`project/target`).
#[derive(Debug, Clone)]
struct StructureFacts {
    /// Always a structure kind (`Roadmap`/`Backlog`) — an invariant of the index build.
    kind: KindKind,
    /// The structure card's own lifecycle status.
    status: StatusKind,
    /// Identity (`project/version`) named by this card's `[[porteuse:…]]`, when present. Only a
    /// ROADMAP can carry one; consulted for the roll-forward (rule 4 of `derive_release`).
    porteuse: Option<String>,
}

/// In-memory index of the **structure cards** (ROADMAP/BACKLOG) of a project-map corpus, keyed by
/// their addressable identity (`project/target`, taken from their `[[version:…]]` link).
///
/// Built once from the raw notes, it turns a work card's `[[track:project/target]]` pointer into a
/// [`TrackTarget`] via [`StructureIndex::resolve_track`]. This is the **single** place that
/// guarantees the invariant "a `[[track:]]` resolves to a *structure* card": the index only ever
/// holds ROADMAP/BACKLOG cards, so a successful resolution is a structure by construction — a
/// pointer that would name a FEATURE card's version simply finds no structure and errors.
/// [`validate_links`] cannot enforce this: it validates one card in isolation and never resolves
/// the target's kind.
#[derive(Debug, Clone, Default)]
pub struct StructureIndex {
    /// Structure cards keyed by `project/target`. Accessed only via `.get()` / `.insert()`, never
    /// iterated, so the map type is not load-bearing for output determinism: the "last one wins"
    /// tie-break on a duplicate identity is deterministic solely because
    /// [`StructureIndex::from_notes`] inserts in the order of the `notes` slice — a `HashMap` would
    /// yield the same tie-break. `BTreeMap` is retained only for its ordered `Debug` output (ADN 2).
    by_identity: BTreeMap<String, StructureFacts>,
}

impl StructureIndex {
    /// Builds the index from the raw `(body_text, title)` notes of the `project-map` section — the
    /// same slice the export projections consume, so no extra I/O.
    ///
    /// Only structure cards (`kind:ROADMAP`/`kind:BACKLOG`) carrying **both** a `[[status:…]]` and
    /// an addressable `[[version:…]]` are indexed; every other note (work cards, or a malformed
    /// structure card missing one of those roles) is skipped. When two structure cards share an
    /// identity — a registry anomaly the write path forbids — the **last** one in iteration order
    /// wins, deterministically.
    #[must_use]
    pub fn from_notes(notes: &[(String, String)]) -> Self {
        Self::from_bodies(notes.iter().map(|(body, _title)| body.as_str()))
    }

    /// Builds the index from the card **bodies** alone (the title is never read).
    ///
    /// Internal constructor shared by [`StructureIndex::from_notes`] (`(body, title)` corpus) and
    /// [`project_map_card_index`] (`(id, body, title)` corpus): both feed only the body, so this
    /// borrows `&str` bodies without cloning them. Same indexing rule as `from_notes`.
    fn from_bodies<'a>(bodies: impl Iterator<Item = &'a str>) -> Self {
        let mut by_identity: BTreeMap<String, StructureFacts> = BTreeMap::new();
        for body in bodies {
            let links: Vec<ProjectMapLink> = extract_structural_targets(body)
                .into_iter()
                .filter_map(|t| parse_link(&t).ok())
                .collect();

            let mut kind: Option<KindKind> = None;
            let mut status: Option<StatusKind> = None;
            let mut identity: Option<String> = None;
            let mut porteuse: Option<String> = None;
            for link in &links {
                match link {
                    ProjectMapLink::Kind(k) => kind = Some(*k),
                    ProjectMapLink::Status(s) => status = Some(*s),
                    ProjectMapLink::Version { project, version } => {
                        identity = Some(format!("{project}/{version}"));
                    }
                    ProjectMapLink::Porteuse { project, version } => {
                        porteuse = Some(format!("{project}/{version}"));
                    }
                    _ => {}
                }
            }

            // N'indexer que les cartes de STRUCTURE adressables et complètes.
            let (Some(kind), Some(status), Some(identity)) = (kind, status, identity) else {
                continue;
            };
            if !kind.is_structure() {
                continue;
            }
            by_identity.insert(
                identity,
                StructureFacts {
                    kind,
                    status,
                    porteuse,
                },
            );
        }
        Self { by_identity }
    }

    /// Resolves a work card's `[[track:project/target]]` pointer to the structure it attaches to.
    ///
    /// Reads the target structure card's kind and status, and — for an `OBSOLETE` ROADMAP that
    /// carries a `[[porteuse:…]]` — resolves the carrier **one level** to fill
    /// [`TrackTarget::Roadmap::porteuse_status`] (the roll-forward input of `derive_release`,
    /// rule 4). The porteuse is looked up **only** when the tracked ROADMAP is `OBSOLETE`,
    /// mirroring exactly what `derive_release` consumes: a porteuse on any other status is never
    /// resolved, so a dangling carrier there can never spuriously error.
    ///
    /// # Errors
    ///
    /// - [`TrackResolutionError::TargetNotFound`] — no structure card carries `project/target`.
    /// - [`TrackResolutionError::PorteuseNotFound`] — the tracked `OBSOLETE` ROADMAP names a
    ///   porteuse that resolves to no structure card.
    #[must_use = "the resolved track target must be fed to derive_release"]
    pub fn resolve_track(
        &self,
        project: &str,
        target: &str,
    ) -> Result<TrackTarget, TrackResolutionError> {
        let identity = format!("{project}/{target}");
        let facts = self.by_identity.get(&identity).ok_or_else(|| {
            TrackResolutionError::TargetNotFound {
                identity: identity.clone(),
            }
        })?;

        match facts.kind {
            KindKind::Backlog => Ok(TrackTarget::Backlog),
            KindKind::Roadmap => {
                // Roll-forward : la porteuse n'est consultée QUE sur une ROADMAP OBSOLETE
                // (exactement le seul cas où derive_release lit porteuse_status).
                let porteuse_status = match (&facts.porteuse, facts.status) {
                    (Some(carrier), StatusKind::Obsolete) => Some(
                        self.by_identity
                            .get(carrier)
                            .map(|c| c.status)
                            .ok_or_else(|| TrackResolutionError::PorteuseNotFound {
                                roadmap: identity.clone(),
                                porteuse: carrier.clone(),
                            })?,
                    ),
                    _ => None,
                };
                Ok(TrackTarget::Roadmap {
                    status: facts.status,
                    porteuse_status,
                })
            }
            // Inatteignable : `from_notes` n'insère que des kinds `is_structure()` (Roadmap|Backlog).
            // Le bras existe pour l'exhaustivité (KindKind #[non_exhaustive]) sans jamais paniquer.
            _ => Err(TrackResolutionError::TargetNotFound { identity }),
        }
    }
}

#[cfg(test)]
mod structure_index_tests {
    use super::*;

    /// A ROADMAP structure card body for `gradatum/<version>` with the given status, and an
    /// optional porteuse identity.
    fn roadmap(version: &str, status: StatusKind, porteuse: Option<&str>) -> (String, String) {
        let mut body = format!(
            "[[project:gradatum]] [[status:{}]] [[kind:ROADMAP]] [[version:gradatum/{version}]] [[visibilite:interne]]",
            status.as_wire()
        );
        if let Some(p) = porteuse {
            body.push_str(&format!(" [[porteuse:{p}]]"));
        }
        (body, format!("ROADMAP {version}"))
    }

    /// A BACKLOG structure card body for `gradatum/backlog`.
    fn backlog(status: StatusKind) -> (String, String) {
        (
            format!(
                "[[project:gradatum]] [[status:{}]] [[kind:BACKLOG]] [[version:gradatum/backlog]]",
                status.as_wire()
            ),
            "BACKLOG".to_string(),
        )
    }

    #[test]
    fn resolves_backlog_target() {
        let notes = [backlog(StatusKind::Open)];
        let idx = StructureIndex::from_notes(&notes);
        assert_eq!(
            idx.resolve_track("gradatum", "backlog"),
            Ok(TrackTarget::Backlog)
        );
    }

    #[test]
    fn resolves_roadmap_target_with_its_status() {
        let notes = [roadmap("2.2.0", StatusKind::InProgress, None)];
        let idx = StructureIndex::from_notes(&notes);
        assert_eq!(
            idx.resolve_track("gradatum", "2.2.0"),
            Ok(TrackTarget::Roadmap {
                status: StatusKind::InProgress,
                porteuse_status: None,
            })
        );
    }

    #[test]
    fn obsolete_roadmap_with_porteuse_fills_porteuse_status() {
        let notes = [
            roadmap("0.7.0", StatusKind::Obsolete, Some("gradatum/0.7.6")),
            roadmap("0.7.6", StatusKind::Done, None),
        ];
        let idx = StructureIndex::from_notes(&notes);
        assert_eq!(
            idx.resolve_track("gradatum", "0.7.0"),
            Ok(TrackTarget::Roadmap {
                status: StatusKind::Obsolete,
                porteuse_status: Some(StatusKind::Done),
            })
        );
    }

    #[test]
    fn porteuse_ignored_when_roadmap_not_obsolete() {
        // A porteuse on a non-OBSOLETE roadmap is never resolved — even a dangling one must not error.
        let notes = [roadmap(
            "2.2.0",
            StatusKind::Open,
            Some("gradatum/nonexistent"),
        )];
        let idx = StructureIndex::from_notes(&notes);
        assert_eq!(
            idx.resolve_track("gradatum", "2.2.0"),
            Ok(TrackTarget::Roadmap {
                status: StatusKind::Open,
                porteuse_status: None,
            })
        );
    }

    #[test]
    fn dangling_track_target_errors() {
        let idx = StructureIndex::from_notes(&[]);
        assert_eq!(
            idx.resolve_track("gradatum", "9.9.9"),
            Err(TrackResolutionError::TargetNotFound {
                identity: "gradatum/9.9.9".to_string(),
            })
        );
    }

    #[test]
    fn obsolete_roadmap_with_dangling_porteuse_errors() {
        let notes = [roadmap(
            "0.7.0",
            StatusKind::Obsolete,
            Some("gradatum/gone"),
        )];
        let idx = StructureIndex::from_notes(&notes);
        assert_eq!(
            idx.resolve_track("gradatum", "0.7.0"),
            Err(TrackResolutionError::PorteuseNotFound {
                roadmap: "gradatum/0.7.0".to_string(),
                porteuse: "gradatum/gone".to_string(),
            })
        );
    }

    #[test]
    fn feature_card_is_not_indexed_as_structure() {
        // A FEATURE card carrying [[version:gradatum/2.2.0]] must NOT be resolvable as a structure.
        let feature = (
            "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]] [[track:gradatum/2.2.0]]"
                .to_string(),
            "F-99".to_string(),
        );
        let idx = StructureIndex::from_notes(&[feature]);
        assert_eq!(
            idx.resolve_track("gradatum", "2.2.0"),
            Err(TrackResolutionError::TargetNotFound {
                identity: "gradatum/2.2.0".to_string(),
            }),
            "a FEATURE card's version must never resolve as a structure target"
        );
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
    /// `[[track:<project>/<target>]]` — attachment of a **work card** to a structure card.
    /// `target` is a version (`2.2.0`) or the `backlog` sentinel; the pair
    /// `<project>/<target>` resolves to the ROADMAP/BACKLOG carrying that `[[version:]]`.
    ///
    /// At most one per work card (additive window), forbidden on a structure card.
    /// The role name is `track` — not `roadmap` — because the target may equally be a
    /// `BACKLOG`, and naming it after one of the two possible kinds would misname it half the
    /// time.
    Track {
        /// Namespaced project (the part before the `/`).
        project: String,
        /// Target identity of the structure card (a version or the `backlog` sentinel).
        target: String,
    },
    /// `[[visibilite:<v>]]` — interne/public gate (ROADMAP and work card).
    ///
    /// Exactly 1 on a ROADMAP; at most 1 on a work card (optional — absent = public);
    /// forbidden on a BACKLOG (a backlog is never published). See [`VisibilityKind`].
    Visibility(VisibilityKind),
    /// `[[porteuse:<project>/<version>]]` — the public version under which a ROADMAP's
    /// content is available. Written once, never recomputed. 0 or 1, only on a
    /// ROADMAP; `∅` is encoded by the **absence** of the role.
    Porteuse {
        /// Namespaced project (the part before the `/`).
        project: String,
        /// Public version number carrying this roadmap (the part after the `/`).
        version: String,
    },
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

    /// More than one `feature:` link on a work card (at most 1 allowed).
    #[error("at most 1 feature: link allowed on a work card, found {0}")]
    FeatureCardinality(usize),

    /// Wrong number of `release:` links (0 on a structure card, at most 1 on a work card).
    #[error(
        "incorrect number of release: links (0 on a structure card, at most 1 on a work card), found {0}"
    )]
    ReleaseCardinality(usize),

    /// **Dormant.** `version:` is no longer required on a work card
    /// (the `[[track:]]` role now carries the version/release axis, server-derived), so this
    /// error is no longer raised. Kept (variant is `pub` on a `#[non_exhaustive]` enum) to
    /// avoid a breaking removal for downstream matchers. An excess `version:` is caught by
    /// [`SchemaError::VersionCardinality`] (at most 1, globally).
    #[error(
        "feature card: exactly 1 [[version:]] required (or sentinel project/backlog), found {0}"
    )]
    FeatureVersionCardinality(usize),

    /// A malformed `[[track:…]]` link (expected `project/<version|backlog>`).
    #[error("malformed track {0:?} (expected project/<version|backlog>)")]
    MalformedTrack(String),

    /// More than one `[[track:]]` on a work card (additive: at most 1 allowed).
    #[error("at most 1 track: link allowed on a work card, found {0}")]
    TrackCardinality(usize),

    /// A `[[track:]]` present on a structure card (ROADMAP/BACKLOG), where it is forbidden.
    #[error("track: link forbidden on a structure card (ROADMAP/BACKLOG), found {0}")]
    TrackOnStructureCard(usize),

    /// A `[[visibilite:…]]` value outside the taxonomy.
    #[error("invalid visibilite {0:?} (expected lowercase ∈ interne/public)")]
    InvalidVisibility(String),

    /// A ROADMAP without exactly one `[[visibilite:]]` link (exactly 1 required).
    #[error("ROADMAP card: exactly 1 visibilite: link required, found {0}")]
    VisibilityCardinality(usize),

    /// A `[[visibilite:]]` present on a BACKLOG, where it is forbidden (0 required).
    ///
    /// A backlog is never published, so it carries no interne/public gate. A
    /// **work card** may carry an (optional) `[[visibilite:]]`; only the BACKLOG structure card
    /// still forbids it — hence the message no longer says "only on a ROADMAP".
    #[error("visibilite: link forbidden on a BACKLOG (a backlog is never published), found {0}")]
    VisibilityForbidden(usize),

    /// More than one `[[visibilite:]]` on a work card (at most 1 allowed).
    ///
    /// A work card declares its internality **at most once** (absent = public). Two conflicting
    /// `[[visibilite:]]` roles have no defined resolution, so the card is rejected rather than
    /// silently picking one — mirrors [`SchemaError::TrackCardinality`] /
    /// [`SchemaError::FeatureCardinality`].
    #[error("at most 1 visibilite: link allowed on a work card, found {0}")]
    VisibilityWorkCardCardinality(usize),

    /// A malformed `[[porteuse:…]]` link (expected `project/x.y.z`).
    #[error("malformed porteuse {0:?} (expected project/x.y.z)")]
    MalformedPorteuse(String),

    /// More than one `[[porteuse:]]` on a ROADMAP (at most 1 allowed).
    #[error("at most 1 porteuse: link allowed on a ROADMAP, found {0}")]
    PorteuseCardinality(usize),

    /// A `[[porteuse:]]` present where it is forbidden (BACKLOG or work card — 0 required).
    #[error("porteuse: link forbidden here (only on a ROADMAP), found {0}")]
    PorteuseForbidden(usize),

    /// A structure card (ROADMAP/BACKLOG) without exactly one addressable `[[version:]]`.
    ///
    /// A structure card is resolved by `[[track:<project>/<version>]]`; without exactly one
    /// version link it is unaddressable, so it is rejected at write time.
    #[error(
        "structure card (ROADMAP/BACKLOG): exactly 1 addressable version: link required, found {0}"
    )]
    StructureVersionCardinality(usize),

    /// A `[[feature:]]` present on a structure card, where numbering is forbidden.
    #[error(
        "feature: link forbidden on a structure card (ROADMAP/BACKLOG are never numbered), found {0}"
    )]
    StructureFeatureForbidden(usize),

    /// A feature-carrying work card with **no** rattachement at all: neither a
    /// `[[version:]]` nor a `[[track:]]` (the derivability floor).
    ///
    /// Since `[[version:]]`/`[[release:]]` became optional (the `[[track:]]`
    /// role carries the version/release axis, server-derived), a card could otherwise carry
    /// a `[[feature:]]` identity while being anchored to nothing — a silent orphan invisible
    /// to every export. A lone `[[release:]]` does **not** anchor it (`release` is itself
    /// derived from `track`), so the floor requires at least one of `{version, track}`.
    /// Changelog cards (no `[[feature:]]` role) are exempt.
    #[error(
        "feature work card must be derivable: needs a [[version:]] or a [[track:]] (found neither)"
    )]
    WorkCardNotDerivable,

    /// A non-reserved wikilink prefix on a **structural** line whose value is neither a ULID nor
    /// an `F-NN` feature id.
    ///
    /// Previously the parser routed any non-reserved prefix to a generic
    /// [`ProjectMapLink::Dep`] as long as the value was non-empty, so a **misspelled role**
    /// (`[[relese:planned]]`, `[[stauts:OPEN]]`) was accepted with neither error nor effect. A
    /// legitimate content dependency always points at a resolvable target — a ULID note, or an
    /// `F-NN` feature node (relation prefixes such as `blocked-by:F-NN`) — so a value that is
    /// neither is an unknown/misspelled role, now rejected loudly. The message **names the
    /// remedy** (criterion 4): check the spelling, or use a valid dependency target.
    #[error(
        "unknown or misspelled role prefix {prefix:?} (value {value:?}): not a reserved role, \
         and not a valid content dependency (a dependency target must be a ULID or an F-NN id). \
         Fix the spelling against the reserved roles, or use a valid dependency target."
    )]
    UnknownRole {
        /// The offending non-reserved prefix.
        prefix: String,
        /// The value written after the prefix.
        value: String,
    },

    /// A `[[release:roadmap]]` written together with a **concrete** (non-`backlog`) `[[version:]]`
    /// on the same card (the dominant measured incoherence).
    ///
    /// `roadmap` means *"considered for the long term, with no target version committed to"*, so a
    /// concrete engaged version alongside it is a direct contradiction with no legitimate encoding.
    /// The check fires **only** when both links are present, so a Phase-7 card (release omitted,
    /// the version/release axis carried by `[[track:]]`) is never affected, and the
    /// `[[version:<project>/backlog]]` sentinel stays coherent with `roadmap`. The message
    /// **names the remedy** (criterion 4).
    #[error(
        "incoherent roles: [[release:roadmap]] means no committed target version, but a concrete \
         [[version:.../{version}]] is present. Use [[release:planned]] for a committed version, or \
         drop the version (or use the [[version:<project>/backlog]] sentinel) for a backlog card."
    )]
    IncoherentReleaseVersion {
        /// The concrete version value written alongside `release:roadmap`.
        version: String,
    },
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
/// - [`SchemaError::MissingPrefix`] if the target has no `role:` prefix at all (a bare title).
/// - [`SchemaError::UnknownRole`] if a **non-reserved** prefix carries a value that is neither a
///   ULID nor an `F-NN` id — a misspelled/unknown role, not a resolvable content dependency.
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
            | "track"
            | "visibilite"
            | "porteuse"
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
        "track" => parse_track(value),
        "visibilite" => VisibilityKind::from_wire(value)
            .map(ProjectMapLink::Visibility)
            .ok_or_else(|| SchemaError::InvalidVisibility(value.to_string())),
        "porteuse" => parse_porteuse(value),
        // Préfixe non réservé : dépendance de contenu section:ULID.
        other => parse_dep(other, value, raw),
    }
}

/// Parse the value of a `[[track:<project>/<target>]]` wikilink.
///
/// The inner `/` separates the project namespace from the structure-card target (a version
/// string or the `backlog` sentinel), mirroring the `[[version:]]` shape.
fn parse_track(value: &str) -> Result<ProjectMapLink, SchemaError> {
    match value.split_once('/') {
        Some((project, target)) if !project.is_empty() && !target.is_empty() => {
            validate_ident("track.project", project)?;
            validate_ident("track.target", target)?;
            Ok(ProjectMapLink::Track {
                project: project.to_string(),
                target: target.to_string(),
            })
        }
        _ => Err(SchemaError::MalformedTrack(value.to_string())),
    }
}

/// Parse the value of a `[[porteuse:<project>/<version>]]` wikilink.
///
/// Same shape as `[[version:]]` — the public version carrying a ROADMAP's content.
fn parse_porteuse(value: &str) -> Result<ProjectMapLink, SchemaError> {
    match value.split_once('/') {
        Some((project, version)) if !project.is_empty() && !version.is_empty() => {
            validate_ident("porteuse.project", project)?;
            validate_ident("porteuse.version", version)?;
            Ok(ProjectMapLink::Porteuse {
                project: project.to_string(),
                version: version.to_string(),
            })
        }
        _ => Err(SchemaError::MalformedPorteuse(value.to_string())),
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
/// A legitimate content dependency points at a **resolvable target**: a Crockford ULID note,
/// or an `F-NN` feature node (relation prefixes such as `blocked-by:F-NN`). Any other value
/// after a non-reserved prefix is a **misspelled/unknown role** — previously it was silently
/// accepted as a generic dependency with no effect (`[[relese:planned]]`), now rejected with
/// [`SchemaError::UnknownRole`] (defect 2).
///
/// `raw` is only used for the error message when the target has no prefix at all.
///
/// # Errors
///
/// - [`SchemaError::MissingPrefix`] if the target has no `:` (a bare human title).
/// - [`SchemaError::UnknownRole`] if the value is neither a ULID nor an `F-NN` feature id.
fn parse_dep(section: &str, value: &str, raw: &str) -> Result<ProjectMapLink, SchemaError> {
    if value.is_empty() {
        // Pas de `:` dans la cible (ou rien après) → ni typé, ni section:ULID.
        return Err(SchemaError::MissingPrefix(raw.to_string()));
    }
    // Une dépendance de contenu pointe TOUJOURS vers une cible résolvable : un ULID de note, ou
    // un nœud feature `F-NN` (préfixes de relation, ex. `blocked-by:F-184`). Une valeur qui n'est
    // ni l'un ni l'autre est un rôle inconnu/mal orthographié, jadis avalé en silence.
    let is_ulid = Ulid::from_string(value).is_ok();
    let is_feature = validate_feature_ident(value).is_ok();
    if !is_ulid && !is_feature {
        return Err(SchemaError::UnknownRole {
            prefix: section.to_string(),
            value: value.to_string(),
        });
    }
    Ok(ProjectMapLink::Dep {
        section: section.to_string(),
        ulid: value.to_string(),
    })
}

/// Validates the cardinality of a set of project-map links.
///
/// Rule: exactly 1 `Project`, 1 `Status`, 1 `Kind`; at most 1 `Version`;
/// any number of `Annex`, `Dep`, `Supersedes` and `Parent`. If a `Version` is present, its
/// namespaced project must match the `Project` link.
///
/// **Work cards**: a work card (`kind` non-structure) allows **at most one**
/// each of `Feature`, `Release` and `Version`. The `Track` role now carries the
/// version/release axis (the server derives `release` from it), so `Version`/`Release` in
/// the body are **optional** — removing them must no longer be
/// rejected. The sentinel `[[version:<project>/backlog]]` stays tolerated (as an
/// at-most-one `Version`). Every work [`KindKind`] value is accepted; only `kind:FEATURE`
/// is exported to the public website, the other kinds (`FIX`/`TASK`/`ENHANCEMENT`) stay
/// vault-only.
///
/// **Derivability floor**: a feature-carrying work card (a `[[feature:]]`
/// role is present) must keep at least one rattachement among `{Version, Track}`. With
/// `Version`/`Release` now optional, a card could otherwise carry a feature identity while
/// anchored to nothing — a silent orphan invisible to every export. A lone `Release` does
/// not anchor it (`release` is server-derived from `track`). Violations yield
/// [`SchemaError::WorkCardNotDerivable`]. Changelog cards (no `[[feature:]]`) are exempt.
///
/// **Structure cards**: when the `kind` is [`KindKind::Roadmap`] or
/// [`KindKind::Backlog`], the card is a *structure* card (pointed at, points at nothing):
/// no `Feature`, no `Release`, no `Track`, and exactly 1 addressable `Version`. A ROADMAP
/// additionally requires exactly 1 `Visibility` and allows at most 1 `Porteuse`; a BACKLOG
/// allows neither.
///
/// **Track (additive)**: a work card allows **at most one** `Track` (0 is
/// still valid during the additive window — a card without the role passes); a structure
/// card allows **zero**. Existence and `kind` of the track target, and the
/// `track.project == card.project` rule, are enforced at the **write path** (they need the
/// registry, which [`validate_links`] cannot see), never here.
///
/// # Errors
///
/// A cardinality or consistency [`SchemaError`], raised at the first deviation found,
/// in this order: project → status → kind → version → project/version mismatch → track →
/// visibilite → porteuse → structure (feature/release/version) → work (feature/release/derivability).
#[must_use = "the validation result must be inspected before accepting the write"]
pub fn validate_links(links: &[ProjectMapLink]) -> Result<(), SchemaError> {
    let mut projects: Vec<&str> = Vec::new();
    let mut status_count = 0usize;
    let mut kinds: Vec<&KindKind> = Vec::new();
    let mut versions: Vec<&str> = Vec::new();
    // Valeur (partie après `/`) de chaque `[[version:]]`, parallèle à `versions` (qui n'en garde
    // que le projet) — nécessaire à la garde de cohérence F-213 pour distinguer une version
    // concrète de la sentinelle `backlog`.
    let mut version_values: Vec<&str> = Vec::new();
    let mut feature_count = 0usize;
    let mut release_count = 0usize;
    // Le seul `[[release:]]` retenu (la cardinalité ≤ 1 est garantie plus bas) — garde F-213.
    let mut release_kind: Option<ReleaseKind> = None;
    let mut track_count = 0usize;
    let mut visibility_count = 0usize;
    let mut porteuse_count = 0usize;

    for link in links {
        match link {
            ProjectMapLink::Project(p) => projects.push(p),
            ProjectMapLink::Status(_) => status_count += 1,
            ProjectMapLink::Kind(k) => kinds.push(k),
            ProjectMapLink::Version { project, version } => {
                versions.push(project);
                version_values.push(version);
            }
            ProjectMapLink::Feature(_) => feature_count += 1,
            ProjectMapLink::Release(r) => {
                release_count += 1;
                release_kind = Some(*r);
            }
            ProjectMapLink::Track { .. } => track_count += 1,
            ProjectMapLink::Visibility(_) => visibility_count += 1,
            ProjectMapLink::Porteuse { .. } => porteuse_count += 1,
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
    if kinds.len() != 1 {
        return Err(SchemaError::KindCardinality(kinds.len()));
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

    // kinds.len() == 1 garanti ci-dessus : le kind pilote structure vs travail (F-184).
    let kind = kinds[0];
    let is_structure = kind.is_structure();

    // ── Cardinalité track (F-184) — additive Phase 2 ────────────────────────────
    // Carte de travail : au plus 1 track (0 admis pendant la fenêtre additive — une
    // carte sans le rôle passe toujours ; la cardinalité exacte 1 arrivera en Phase 7).
    // Carte de structure : 0 track (un ROADMAP/BACKLOG est pointé, il ne pointe rien).
    if is_structure {
        if track_count != 0 {
            return Err(SchemaError::TrackOnStructureCard(track_count));
        }
    } else if track_count > 1 {
        return Err(SchemaError::TrackCardinality(track_count));
    }

    // ── Cardinalité visibilite (F-184 ROADMAP + F-256 carte de travail) ──────────
    // ROADMAP          : exactement 1 (la porte interne|public — jamais absente, jamais déduite).
    // BACKLOG          : 0 (un backlog n'est jamais publié).
    // Carte de travail : au plus 1 (F-256, optionnel — absent = public, appliqué par le
    //                    lecteur/export, jamais par un Default sur le type).
    match kind {
        KindKind::Roadmap => {
            if visibility_count != 1 {
                return Err(SchemaError::VisibilityCardinality(visibility_count));
            }
        }
        KindKind::Backlog => {
            if visibility_count != 0 {
                return Err(SchemaError::VisibilityForbidden(visibility_count));
            }
        }
        // Cartes de travail (FEATURE/FIX/TASK/ENHANCEMENT) : F-256, ≤ 1.
        KindKind::Feature | KindKind::Enhancement | KindKind::Fix | KindKind::Task => {
            if visibility_count > 1 {
                return Err(SchemaError::VisibilityWorkCardCardinality(visibility_count));
            }
        }
    }

    // ── Cardinalité porteuse (F-184, §1.3) ──────────────────────────────────────
    // ROADMAP : 0 ou 1 (∅ = absence). Partout ailleurs : 0.
    if matches!(kind, KindKind::Roadmap) {
        if porteuse_count > 1 {
            return Err(SchemaError::PorteuseCardinality(porteuse_count));
        }
    } else if porteuse_count != 0 {
        return Err(SchemaError::PorteuseForbidden(porteuse_count));
    }

    if is_structure {
        // ── Carte de structure (ROADMAP/BACKLOG) ────────────────────────────────
        // Jamais numérotée (§1.1) : aucun rôle feature.
        if feature_count != 0 {
            return Err(SchemaError::StructureFeatureForbidden(feature_count));
        }
        // Aucun axe release (pas de livraison propre : c'est un hub).
        if release_count != 0 {
            return Err(SchemaError::ReleaseCardinality(release_count));
        }
        // Adressable par exactement 1 version — c'est l'identité que `[[track:]]`
        // résout (ROADMAP : project/x.y.z ; BACKLOG : project/backlog).
        if versions.len() != 1 {
            return Err(SchemaError::StructureVersionCardinality(versions.len()));
        }
    } else {
        // ── Carte de travail (F-184 Phase 7) ─────────────────────────────────────
        // Le rôle track porte désormais l'axe version/release (le serveur dérive `release`
        // depuis `track`). version et release deviennent FACULTATIFS ici (au plus 1) — le
        // retrait de leur corps (Phase 7) ne doit plus être rejeté. version : cardinalité
        // ≤ 1 déjà garantie globalement (versions.len() > 1 rejeté plus haut). La sentinelle
        // `[[version:<project>/backlog]]` reste tolérée. feature : au plus 1 (seul
        // kind:FEATURE est exporté ; FIX/TASK/ENHANCEMENT restent vault-only). Ordre :
        // feature → release (cohérent §10e).
        if feature_count > 1 {
            return Err(SchemaError::FeatureCardinality(feature_count));
        }
        if release_count > 1 {
            return Err(SchemaError::ReleaseCardinality(release_count));
        }
        // ── Plancher de dérivabilité (F-184 Phase 7, P1 audit) ──────────────────
        // La relaxation version/release-optionnels ne doit pas laisser passer un orphelin :
        // une carte de travail feature-porteuse doit conserver AU MOINS un ancrage parmi
        // {version, track}, sinon elle est invisible à tout export. Un `[[release:]]` seul
        // n'ancre rien (le serveur dérive `release` depuis `track`). Les cartes changelog
        // (feature_count == 0) ne sont pas concernées. Placé APRÈS les bornes at-most-1.
        if feature_count >= 1 && versions.is_empty() && track_count == 0 {
            return Err(SchemaError::WorkCardNotDerivable);
        }

        // ── Cohérence release/version (F-213 garde 1) ────────────────────────────
        // `release:roadmap` = « aucune version cible engagée » ; une version concrète (hors
        // sentinelle `backlog`) écrite à côté la contredit — l'incohérence la plus fréquente
        // mesurée (2026-08-22 : 22 cas de roadmap sur version engagée). Ne se déclenche QUE si
        // les DEUX liens sont présents : une carte Phase 7 (release omis, axe porté par
        // `[[track:]]`) n'est jamais concernée ; `version:<projet>/backlog` reste cohérent avec
        // roadmap. La direction opposée (planned/released + sentinelle backlog) n'est pas gardée
        // ici : la sentinelle `version:backlog` couplée à `planned` est le corps backlog canonique
        // établi (helpers de test `minimal_feature_card`, constante serveur `VALID_BODY`), 0 carte
        // vivante ne la porte (les 9 cas historiques corrigés à la main), et l'axe release est en
        // retrait — l'imposer serait une rupture de contrat sans défaut LIVE correspondant.
        if release_kind == Some(ReleaseKind::Roadmap)
            && let Some(&concrete) = version_values.iter().find(|&&v| v != "backlog")
        {
            return Err(SchemaError::IncoherentReleaseVersion {
                version: concrete.to_string(),
            });
        }
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
        // F-184 : `track:` est un nœud réservé navigable (le hub de la carte de structure
        // ROADMAP/BACKLOG). `visibilite:`/`porteuse:` sont aussi des nœuds réservés typés
        // (mêmes hubs synthétiques que status:/version:), sans note ULID à résoudre.
        ProjectMapLink::Track { project, target } => Some(format!("track:{project}/{target}")),
        ProjectMapLink::Visibility(v) => Some(format!("visibilite:{}", v.as_wire())),
        ProjectMapLink::Porteuse { project, version } => {
            Some(format!("porteuse:{project}/{version}"))
        }
        // Annexes (spec/plan/context) et dépendances (section:ULID) pointent vers
        // de vraies notes → résolution ULID normale, pas un nœud réservé.
        ProjectMapLink::Annex { .. } | ProjectMapLink::Dep { .. } => None,
    }
}

/// Track target (`"<project>/<target>"`) to **inject** on a project-map write, or `None`.
///
/// Server-side injection: a work card's `[[track:]]` pointer must
/// survive the RMW verbs of `gov-todo`, which rewrite the *entire* body copying only the
/// card identity. Without server injection, one mutation during the additive window erases
/// the pointer, the card becomes non-derivable and disappears from every export **without an
/// error**. This function derives the target the server should re-inject, from the
/// `[[version:]]` still present during the window.
///
/// Returns `Some("<project>/<version>")` when `body` is a project-map **work** card that:
/// carries a `kind` that is **not** a structure kind, carries **no** `[[track:]]` already,
/// and carries **exactly one** `[[version:]]`. Returns `None` otherwise — structure cards
/// never carry a track, an existing track is preserved as-is, and a card without a single
/// resolvable version has nothing to derive.
///
/// **Pure** — no I/O. The caller resolves the returned target against the registry and only
/// injects when it resolves to an existing structure card (so a not-yet-created ROADMAP does
/// not brick live writes before it exists).
#[must_use]
pub fn derivable_track_target(body: &str) -> Option<String> {
    let links: Vec<ProjectMapLink> = extract_structural_targets(body)
        .iter()
        .filter_map(|t| parse_link(t).ok())
        .collect();

    let mut kind: Option<&KindKind> = None;
    let mut version: Option<String> = None;
    let mut version_count = 0usize;
    let mut has_track = false;
    for link in &links {
        match link {
            ProjectMapLink::Kind(k) => kind = Some(k),
            ProjectMapLink::Version {
                project,
                version: v,
            } => {
                version = Some(format!("{project}/{v}"));
                version_count += 1;
            }
            ProjectMapLink::Track { .. } => has_track = true,
            _ => {}
        }
    }

    if has_track || version_count != 1 {
        return None;
    }
    match kind {
        Some(k) if !k.is_structure() => version,
        _ => None,
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
///
/// `#[non_exhaustive]`: this is the mirror's **axis surface**, and a card axis that is currently
/// silent may have to be rendered later — the visibility axis was exactly that case. Marking the
/// struct here keeps each such addition additive for a downstream consumer instead of turning
/// every one of them into a SemVer break, and it costs nothing in this crate, which is the only
/// place the struct is ever constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FeatureEntry {
    /// Identifier of the feature card, of the form `F-<n>`.
    pub feature: String,
    /// Lowercase wire delivery status (`"roadmap"` | `"planned"` | `"released"` | `"dropped"`).
    pub release: String,
    /// Target version in website format (a real `"v0.6.4"`), or the literal `"vX.Y.Z"`
    /// for backlog cards, so that they remain visible instead of being filtered out.
    ///
    /// Read from the **same structural source as the title**: the `[[track:]]` pointer for a work
    /// card, its own `[[version:]]` only as a fallback. The two must agree — a card whose title
    /// advertises `— v2.3.0` while this column carries another version is one card giving two
    /// answers, and this is the one a machine reads.
    pub version: Option<String>,
    /// Visibility wire value from `[[visibilite:]]` (`"interne"` / `"public"`), or `None`.
    ///
    /// **Rendered, not filtered**: the exit filter and this axis are two different jobs. The
    /// default mirror excludes an `interne` card outright; this field is what makes the audit
    /// mode readable, where the same card is present and its visibility is the only thing that
    /// distinguishes it from a public neighbour. `None` is an explicit absence — the role is
    /// absent, which reads as public by convention — never a value invented to fill the gap.
    pub visibility: Option<String>,
    /// H1 title of the card.
    pub title: String,
}

/// Filtering options for [`project_map_feature_entries`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    /// Switches the export into **full-audit** mode.
    ///
    /// - `false` (default) — public-website mirror: `release:dropped` cards are
    ///   excluded, only `kind:FEATURE` cards are kept, **and** cards carrying
    ///   `[[visibilite:interne]]` are excluded (dedicated exclusion axis).
    /// - `true` — every feature card is exported, whatever its release status, its kind,
    ///   or its declared visibility (internes included — for diagnosis/audit).
    ///
    /// Despite its name, this flag is the single **full-audit** switch: it lifts **all three**
    /// mirror-site filters at once (`dropped`, non-`FEATURE` kind, `visibilite:interne`); there
    /// is no way to lift one without the others. `version:*/backlog` cards are included in both
    /// modes. Reusing this one flag (rather than adding an `include_internal` field with no
    /// distinct consumer) keeps a single audit surface.
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
    extract_structural_targets(body)
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
/// 4. When `opts.include_dropped == false`, drop `release:dropped` cards, every card whose
///    kind is not `FEATURE`, **and** every card carrying `[[visibilite:interne]]` (a
///    dedicated exclusion axis — applied *in addition* to the kind/dropped filters, never in
///    their place; absence of the role means public).
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
    project_map_feature_entries_scoped(notes, opts, None)
}

/// [`project_map_feature_entries`] with an explicit **project filter**.
///
/// `project_filter`:
/// - `None` — no project constraint (byte-identical to [`project_map_feature_entries`]).
/// - `Some(p)` — keep only cards whose `[[project:]]` equals `p`.
///
/// This is a **defence independent of the link graph** (the `project:system` isolation
/// decision): even if a `project:system` card were wrongly attached to a public
/// ROADMAP of `gradatum` and thus satisfied both export predicates mechanically, the explicit
/// project filter keeps it off the public site. The two-line graph resolution and this filter
/// must both fail for a card to leak, not just one.
///
/// Structure cards (`kind:ROADMAP` / `kind:BACKLOG`) are skipped up front: they carry no
/// `[[feature:]]` and would be skipped anyway, but the explicit guard makes the exclusion a
/// stated invariant (C10) rather than an accident of the feature check.
#[must_use]
pub fn project_map_feature_entries_scoped(
    notes: &[(String, String)],
    opts: ExportOptions,
    project_filter: Option<&str>,
) -> Vec<FeatureEntry> {
    let mut entries: Vec<FeatureEntry> = Vec::new();

    for (body, title) in notes {
        // Parser les wikilinks — ignorer silencieusement les cibles invalides.
        let links: Vec<ProjectMapLink> = extract_structural_targets(body)
            .into_iter()
            .filter_map(|t| parse_link(&t).ok())
            .collect();

        let mut feature_id: Option<String> = None;
        let mut release_wire: Option<String> = None;
        let mut version_raw: Option<String> = None;
        let mut track_raw: Option<String> = None;
        let mut kind_wire: Option<&KindKind> = None;
        let mut project_name: Option<&str> = None;
        let mut visibility: Option<VisibilityKind> = None;

        for link in &links {
            match link {
                ProjectMapLink::Feature(f) => feature_id = Some(f.clone()),
                ProjectMapLink::Release(r) => release_wire = Some(r.as_wire().to_string()),
                ProjectMapLink::Version { project, version } => {
                    version_raw = Some(format!("{project}/{version}"));
                }
                ProjectMapLink::Track { project, target } => {
                    track_raw = Some(format!("{project}/{target}"));
                }
                ProjectMapLink::Kind(k) => kind_wire = Some(k),
                ProjectMapLink::Project(p) => project_name = Some(p),
                ProjectMapLink::Visibility(v) => visibility = Some(*v),
                _ => {}
            }
        }

        // Filtre projet (F-184) : défense indépendante du graphe de liens.
        if let Some(want) = project_filter
            && project_name != Some(want)
        {
            continue;
        }

        // C10 : une carte de structure (ROADMAP/BACKLOG) ne rejoint jamais l'export.
        if matches!(kind_wire, Some(k) if k.is_structure()) {
            continue;
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

        // La version lit la MÊME source que le titre (`title_sources_of_body`), précédence
        // comprise : le `[[track:]]` d'abord, le `[[version:]]` propre à la carte en repli.
        // Une carte peut porter les deux — un `[[version:]]` hérité d'un rattachement de
        // bootstrap survit souvent au track qui vise la version réellement livrée — et lire le
        // rôle stocké seul fait alors diverger la colonne de son propre titre.
        let version = track_raw
            .or(version_raw)
            .as_deref()
            .and_then(map_version_raw);

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

        // Filtrage miroir-site F-256 : une carte marquée `[[visibilite:interne]]` est exclue du
        // catalogue public — EN PLUS des filtres kind/dropped, jamais à leur place. Absence du
        // rôle = public (l'exclusion est un acte déclaré, pas un oubli). Le mode audit
        // `include_dropped` lève aussi ce filtre (voir [`ExportOptions::include_dropped`]).
        if !opts.include_dropped && matches!(visibility, Some(VisibilityKind::Interne)) {
            continue;
        }

        entries.push(FeatureEntry {
            feature: feature_id,
            release,
            version,
            visibility: visibility.map(|v| v.as_wire().to_string()),
            title: title.clone(),
        });
    }

    // Tri par identifiant F-XX numérique croissant.
    entries.sort_by_key(|e| feature_sort_key(&e.feature));

    entries
}

/// Why a feature card's release could not be derived from its tracked structure, and therefore
/// fell back to the stored `[[release:]]`.
///
/// The cases carry different operational meaning, so the caller can choose a log level:
/// [`DerivationFallbackReason::NoTrack`] is **expected** during the additive window (a
/// card may not yet carry a `[[track:]]`, or is missing its `[[status:]]`) and warrants at most a
/// debug line, whereas [`DerivationFallbackReason::Unresolved`] and
/// [`DerivationFallbackReason::Undetermined`] are genuine anomalies to warn on.
/// [`DerivationFallbackReason::UnterminatedCodeFence`] is an anomaly of a different kind — the
/// card's **body** is malformed, not its graph — and is reported in place of the `NoTrack` it
/// would otherwise be mistaken for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DerivationFallbackReason {
    /// No `[[track:]]` pointer (or no `[[status:]]`) on the card — nothing to derive from. Benign
    /// during the additive window.
    NoTrack,
    /// A `[[track:]]` was present but resolved to no structure card.
    Unresolved(TrackResolutionError),
    /// The tracked structure resolved, but its status has no release mapping.
    Undetermined(ReleaseDerivationError),
    /// The card's body opens a code fence that never closes, and the roles below the opening
    /// line were read as code — counted by neither the read projection nor the write validator.
    /// Reported **instead of** the [`DerivationFallbackReason::NoTrack`] such a card would
    /// otherwise raise: the role is present in the body, it is the reading that missed it, and
    /// `NoTrack` would send a reader after a dangling pointer that does not exist.
    ///
    /// The swallowed role stays **uncounted** — the extraction is intentionally left untouched so
    /// that read and write agree. Only the silence is repaired, not the reading.
    UnterminatedCodeFence,
}

/// A single feature card whose `release` could **not** be derived from the tracked structure.
/// Emitted by [`project_map_feature_entries_derived_scoped`] for the caller to **log** — the
/// derivation is never silently dropped nor replaced by a fabricated value.
///
/// Two outcomes are distinguished by [`DerivationDiagnostic::stored`]:
/// - `Some(wire)` — a stored `[[release:]]` was present and used as the **fallback**; the card is
///   still projected with that value.
/// - `None` — no stored `[[release:]]` either (the expected post-retrait shape, once the stored
///   axis is gone): nothing derivable and nothing stored, so the card is **skipped** entirely.
///   This is a defensive path — every work card carries a resolvable `[[track:]]`,
///   so a `None` diagnostic signals a genuine registry anomaly (dangling track pointer).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DerivationDiagnostic {
    /// `F-XX` identity of the card.
    pub feature: String,
    /// The stored `[[release:]]` wire value used as the fallback, or `None` when the card carried
    /// no stored release and was therefore **skipped** (not projected).
    pub stored: Option<String>,
    /// Why the derivation could not be performed.
    pub reason: DerivationFallbackReason,
}

/// Outcome of projecting feature entries with the release **derived** from the tracked structure
/// instead of read from the stored `[[release:]]` (make-before-break).
///
/// Carries the projected [`FeatureEntry`] list — `release` and `version` both derived from the
/// tracked structure — plus one [`DerivationDiagnostic`] per card whose derivation failed (either
/// fell back to a stored release, or was skipped when no stored release existed either).
///
/// Post-retrait the stored `[[release:]]`/`[[version:]]` axes are gone from work
/// cards, so `diagnostics` is expected to be **empty** on a healthy corpus: every card resolves its
/// `[[track:]]`. A non-empty `diagnostics` therefore surfaces a registry anomaly (dangling track).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DerivedExport {
    /// Feature entries with the derived release and version, sorted by ascending numeric `F-XX`.
    pub entries: Vec<FeatureEntry>,
    /// Cards whose release could not be derived — fell back to stored, or were skipped (no stored
    /// either). For the caller to **log**; never a silent drop.
    pub diagnostics: Vec<DerivationDiagnostic>,
}

/// Feature-entry projection with `release` **derived from the tracked structure**.
///
/// Same input, same filtering (project filter, structure exclusion C10, mirror-site `dropped`/kind
/// filters) and same sort as [`project_map_feature_entries_scoped`] — but **both** the card's
/// `release` and its `version` are computed from its `[[track:]]` pointer (the primary source
/// post-retrait), via [`StructureIndex`] + `derive_release`, rather than read from
/// the stored `[[release:]]`/`[[version:]]`:
///
/// - `release` = `derive_release` applied to the card's `[[status:]]` and the resolved track
///   target.
/// - `version` = the tracked structure's identity `project/target` (which *is* that structure's
///   `[[version:]]`) mapped through [`map_version_raw`] — a `[[track:gradatum/2.1.0]]` yields
///   `Some("v2.1.0")`, a `[[track:gradatum/backlog]]` yields `Some("vX.Y.Z")`.
///
/// The mirror-site filters (`dropped` exclusion, `FEATURE`-only, and the
/// `visibilite:interne` exclusion) are applied to the **derived** release, so the projection
/// stays comparable with the stored one during the transition.
///
/// # Behaviour when derivation fails
///
/// A card is **derivable** iff it carries a `[[status:]]` and a `[[track:]]` that resolves to a
/// structure card. When derivation fails (no `[[track:]]`/`[[status:]]`, a dangling pointer, or an
/// undeterminable structure status) the stored `[[release:]]` is used only as an **optional
/// fallback**:
///
/// - stored release present → the entry keeps its stored `release`/`version`, and a
///   [`DerivationDiagnostic`] with `stored: Some(..)` is recorded (the additive-window shape).
/// - stored release absent → the card is **skipped** (nothing derivable, nothing stored) and a
///   [`DerivationDiagnostic`] with `stored: None` is recorded.
///
/// Never a panic, never a silent drop, never an invented value. `project_filter` behaves exactly as
/// in [`project_map_feature_entries_scoped`].
#[must_use]
pub fn project_map_feature_entries_derived_scoped(
    notes: &[(String, String)],
    opts: ExportOptions,
    project_filter: Option<&str>,
) -> DerivedExport {
    let index = StructureIndex::from_notes(notes);
    let mut entries: Vec<FeatureEntry> = Vec::new();
    let mut diagnostics: Vec<DerivationDiagnostic> = Vec::new();

    for (body, title) in notes {
        let scan = scan_structural_targets(body);
        let links: Vec<ProjectMapLink> = scan
            .targets
            .iter()
            .filter_map(|t| parse_link(t).ok())
            .collect();

        let mut feature_id: Option<String> = None;
        let mut stored_release: Option<String> = None;
        let mut version_raw: Option<String> = None;
        let mut kind_wire: Option<&KindKind> = None;
        let mut project_name: Option<&str> = None;
        let mut card_status: Option<StatusKind> = None;
        let mut track: Option<(&str, &str)> = None;
        let mut visibility: Option<VisibilityKind> = None;

        for link in &links {
            match link {
                ProjectMapLink::Feature(f) => feature_id = Some(f.clone()),
                ProjectMapLink::Release(r) => stored_release = Some(r.as_wire().to_string()),
                ProjectMapLink::Version { project, version } => {
                    version_raw = Some(format!("{project}/{version}"));
                }
                ProjectMapLink::Kind(k) => kind_wire = Some(k),
                ProjectMapLink::Project(p) => project_name = Some(p),
                ProjectMapLink::Status(s) => card_status = Some(*s),
                ProjectMapLink::Track { project, target } => track = Some((project, target)),
                ProjectMapLink::Visibility(v) => visibility = Some(*v),
                _ => {}
            }
        }

        // Filtre projet (identique à _scoped) : défense indépendante du graphe de liens.
        if let Some(want) = project_filter
            && project_name != Some(want)
        {
            continue;
        }
        // C10 : une carte de structure (ROADMAP/BACKLOG) ne rejoint jamais l'export.
        if matches!(kind_wire, Some(k) if k.is_structure()) {
            continue;
        }
        // Seules les cartes-feature (présence de [[feature:F-XX]]) sont projetées.
        let feature_id = match feature_id {
            Some(id) => id,
            None => continue,
        };

        // Source de version de repli : le MÊME axe que le titre (`title_sources_of_body`),
        // `[[track:]]` d'abord, `[[version:]]` propre ensuite. Un track présent mais non résolu
        // reste l'axe courant de la carte — replier sur un `[[version:]]` hérité ferait diverger
        // la colonne du titre de la carte.
        let track_raw: Option<String> = track.map(|(tp, tt)| format!("{tp}/{tt}"));
        let version_fallback = track_raw.or(version_raw);

        // ── Dérivation depuis la structure pointée = source PRIMAIRE (post-retrait Phase 7) ──
        // `release` ET `version` viennent du `[[track:]]` ; le stocké n'est plus qu'un repli
        // OPTIONNEL. Résultat = `Ok((release, version_raw))` si dérivable, sinon `Err(reason)`.
        let derivation: Result<(String, Option<String>), DerivationFallbackReason> =
            match (card_status, track) {
                (Some(status), Some((tp, tt))) => match index.resolve_track(tp, tt) {
                    Ok(target) => match derive_release(status, target, tt) {
                        // version dérivée = identité de la structure pointée (= son [[version:]]).
                        Ok(rk) => Ok((rk.as_wire().to_string(), Some(format!("{tp}/{tt}")))),
                        Err(e) => Err(DerivationFallbackReason::Undetermined(e)),
                    },
                    Err(e) => Err(DerivationFallbackReason::Unresolved(e)),
                },
                // Pas de track/status : indérivable (ne doit plus arriver sur un corpus post-retrait).
                _ => Err(DerivationFallbackReason::NoTrack),
            };

        // Jamais de panic, jamais de drop silencieux, jamais de valeur inventée : tout échec de
        // dérivation est rendu VISIBLE via un diagnostic, puis retombe sur le stocké s'il existe,
        // sinon la carte est ignorée (rien à projeter). Le motif est d'abord confronté au corps :
        // une marque de code non fermée explique un `NoTrack` et doit être nommée comme telle.
        let (release, version) = match derivation {
            Ok((rel, vraw)) => (rel, vraw.as_deref().and_then(map_version_raw)),
            Err(reason) => {
                let reason = fence_aware_reason(body, &scan, reason);
                match stored_release {
                    // Repli sur le release/version stockés (fenêtre additive de coexistence).
                    Some(stored) => {
                        diagnostics.push(DerivationDiagnostic {
                            feature: feature_id.clone(),
                            stored: Some(stored.clone()),
                            reason,
                        });
                        (
                            stored,
                            version_fallback.as_deref().and_then(map_version_raw),
                        )
                    }
                    // Ni dérivable ni stocké → rien à projeter : diagnostic + skip.
                    None => {
                        diagnostics.push(DerivationDiagnostic {
                            feature: feature_id.clone(),
                            stored: None,
                            reason,
                        });
                        continue;
                    }
                }
            }
        };

        // Filtrage miroir-site sur la valeur retenue (dérivée ou repli) pour rester comparable.
        if !opts.include_dropped && release == "dropped" {
            continue;
        }
        if !opts.include_dropped && !matches!(kind_wire, Some(KindKind::Feature)) {
            continue;
        }
        // Filtrage F-256 : carte marquée `[[visibilite:interne]]` exclue du catalogue public,
        // EN PLUS des filtres kind/dropped. Absence = public. Levé par le mode audit
        // `include_dropped` (voir [`ExportOptions::include_dropped`]).
        if !opts.include_dropped && matches!(visibility, Some(VisibilityKind::Interne)) {
            continue;
        }

        entries.push(FeatureEntry {
            feature: feature_id,
            release,
            version,
            visibility: visibility.map(|v| v.as_wire().to_string()),
            title: title.clone(),
        });
    }

    entries.sort_by_key(|e| feature_sort_key(&e.feature));

    // Ordre TOTAL et déterministe des diagnostics — ils sont poussés en ordre de note, mais la CLI
    // les émet sur stderr et l'appelant doit les surfacer : l'ordre d'entrée ne doit pas transparaître.
    // `feature` seul n'est PAS une clé totale (des homonymes `F-XX` coexistent dans le corpus), d'où
    // le départage sur tous les axes observables. `feature_sort_key` donne le MÊME ordre numérique
    // que les `entries` ci-dessus (jamais l'ordre lexical `F-10 < F-9`). La représentation `Debug` de
    // `reason` est l'axe le moins coûteux (pas de `derive` d'`Ord`/`Hash` à propager sur
    // `DerivationFallbackReason`) : axe stable POUR CE BINAIRE, non contractuel — la forme `Debug`
    // dérivée n'est pas garantie entre versions de rustc, et `DerivationFallbackReason` est
    // `#[non_exhaustive]` : ne jamais adosser un snapshot des diagnostics à cet axe.
    diagnostics.sort_by_cached_key(|d| {
        (
            feature_sort_key(&d.feature),
            d.feature.clone(),
            d.stored.clone(),
            format!("{:?}", d.reason),
        )
    });

    DerivedExport {
        entries,
        diagnostics,
    }
}

/// Full-axis projection of **one** project-map card — work card *or* structure card — for the
/// sanctioned single-request milestone listing.
///
/// Distinct from [`FeatureEntry`] on purpose. [`FeatureEntry`] is the public-website mirror: it is
/// deliberately `FEATURE`-only and carries just `feature`/`release`/`version`/`title`, and it never
/// receives the note ULID. This listing requires the **opposite** contract — every card (work *and*
/// structure), every identification axis **named** so no two can be confused (criteria 1, 3, 8),
/// and the addressable identifier included (criterion 1). Extending [`FeatureEntry`] would both
/// break that frozen mirror contract and force the ULID through the two stable export projections
/// that do not need it, so this is a separate projection reusing the same derivation machinery
/// ([`StructureIndex`] / `derive_release`).
///
/// A field is `Option` when the axis may legitimately be absent (a structure card is never
/// numbered, carries no `release`; a work card's derivation may fail on a dangling `[[track:]]`).
/// An absent value is rendered as an explicit `null` and **never** dropped silently — the null is
/// the visible signal of the gap (criteria 2, 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMapCardEntry {
    /// Note ULID — the addressable identifier (criterion 1: rendered as a named field, never a
    /// bare path).
    pub id: String,
    /// `F-XX` number from `[[feature:]]`, or `None` for a structure card (never numbered).
    pub feature: Option<String>,
    /// Lifecycle status wire value from `[[status:]]` (`"OPEN"`, `"IN_PROGRESS"`, `"DONE"`, …),
    /// or `None` on a malformed card missing the role.
    pub status: Option<String>,
    /// Kind wire value from `[[kind:]]` (`"FEATURE"`, `"FIX"`, `"TASK"`, `"ROADMAP"`, `"BACKLOG"`,
    /// …), or `None` on a malformed card. This is the very axis whose absence produced the false
    /// count of 2026-09-02 — always exposed here (criterion 8).
    pub kind: Option<String>,
    /// Delivery status. **Work card**: derived from `[[track:]]` + `[[status:]]` via
    /// `derive_release` (the post-Phase-7 primary source; the stored `[[release:]]` is gone).
    /// **Structure card**: `None` (a structure has no delivery axis). `None` on a work card
    /// signals an unresolvable `[[track:]]` (a registry anomaly, rendered visibly).
    pub release: Option<String>,
    /// Website-format version (`"v2.1.0"`, or `"vX.Y.Z"` for the backlog sentinel), or `None`.
    /// **Work card**: the identity of the structure it tracks. **Structure card**: its own
    /// `[[version:]]`.
    pub version: Option<String>,
    /// Visibility wire value from `[[visibilite:]]` (`"interne"` / `"public"`), or `None`
    /// (absent = public by reader convention — see [`VisibilityKind`]).
    pub visibility: Option<String>,
    /// H1 title of the card.
    pub title: String,
    /// `[[supersedes:F-YY]]` dependency roles (0..N).
    pub supersedes: Vec<String>,
    /// `[[parent:F-YY]]` dependency roles (0..N).
    pub parent: Vec<String>,
    /// `[[track:project/target]]` attachment identity (`"gradatum/2.1.0"`), or `None`
    /// (structure cards, or a work card with no track).
    pub track: Option<String>,
}

/// Normalises a version query value so `"2.1.0"` and `"v2.1.0"` both match the stored target.
///
/// Strips a single leading `v` **only** when it precedes a digit, so real targets like `backlog`
/// (or a hypothetical `vN` sentinel) are left untouched. Stored `[[track:]]`/`[[version:]]` targets
/// never carry the `v` prefix (it is a website-display artefact of [`map_version_raw`]).
fn normalize_version_query(v: &str) -> &str {
    match v.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => v,
    }
}

/// Sanctioned single-request projection of the project-map cards, with all identification axes
/// **named** and an optional filter on the **version**.
///
/// Input: `(id, body, title)` triples of the `project-map` section, already filtered on the
/// note-level status (`downgraded`/`garbage`) by the storage layer — so the count of the returned
/// slice equals the count of that same perimeter (criterion 5).
///
/// Unlike the website export ([`project_map_feature_entries_derived_scoped`]) this projection
/// applies **no mirror-site filter**: it never drops a card by kind, by `release:dropped`, nor by
/// `visibilite:interne`. Every card in the perimeter is listed with each axis exposed, so a caller
/// filters on the **named** columns instead of on a hidden predicate (criteria 7, 8). It renders
/// **both** work cards and structure cards (ROADMAP/BACKLOG).
///
/// `version_filter`:
/// - `None` — every card is listed (cards with no version are rendered with `version: null`,
///   never omitted — criterion 2).
/// - `Some(v)` — keep the cards attached to that version: a **work card** whose `[[track:]]`
///   target equals `v`, and the **structure card(s)** whose own `[[version:]]` equals `v` (so a
///   milestone listing includes its own roadmap/backlog card — a listing of a version
///   includes that version's structure cards). `v` accepts `"2.1.0"` or `"v2.1.0"` (see
///   `normalize_version_query`).
///
/// `project_filter` behaves as in [`project_map_feature_entries_scoped`]: `Some(p)` keeps only
/// cards whose `[[project:]]` equals `p`.
///
/// Sorting is deterministic: structure cards first (no number), then work cards by ascending
/// numeric `F-XX`, ties broken by ULID.
///
/// The function is **pure**: no I/O, no panic. A work card whose `[[track:]]` does not resolve is
/// still listed, with `release: None` — the null is the visible anomaly signal, never a silent drop.
#[must_use]
pub fn project_map_card_index(
    notes: &[(String, String, String)],
    version_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Vec<ProjectMapCardEntry> {
    // Build the structure index once from the same corpus (bodies only) — reused for the
    // `track → release` derivation, exactly as the website export does.
    let index = StructureIndex::from_bodies(notes.iter().map(|(_, body, _)| body.as_str()));
    let want_version = version_filter.map(normalize_version_query);

    let mut entries: Vec<ProjectMapCardEntry> = Vec::new();

    for (id, body, title) in notes {
        let links: Vec<ProjectMapLink> = extract_structural_targets(body)
            .into_iter()
            .filter_map(|t| parse_link(&t).ok())
            .collect();

        let mut feature_id: Option<String> = None;
        let mut status: Option<StatusKind> = None;
        let mut kind: Option<KindKind> = None;
        let mut version_identity: Option<(String, String)> = None; // structure card `[[version:]]`
        let mut track: Option<(String, String)> = None; // work card `[[track:]]`
        let mut project_name: Option<&str> = None;
        let mut visibility: Option<VisibilityKind> = None;
        let mut supersedes: Vec<String> = Vec::new();
        let mut parent: Vec<String> = Vec::new();

        for link in &links {
            match link {
                ProjectMapLink::Feature(f) => feature_id = Some(f.clone()),
                ProjectMapLink::Status(s) => status = Some(*s),
                ProjectMapLink::Kind(k) => kind = Some(*k),
                ProjectMapLink::Version { project, version } => {
                    version_identity = Some((project.clone(), version.clone()));
                }
                ProjectMapLink::Track { project, target } => {
                    track = Some((project.clone(), target.clone()));
                }
                ProjectMapLink::Project(p) => project_name = Some(p),
                ProjectMapLink::Visibility(v) => visibility = Some(*v),
                ProjectMapLink::Supersedes(f) => supersedes.push(f.clone()),
                ProjectMapLink::Parent(f) => parent.push(f.clone()),
                _ => {}
            }
        }

        // Filtre projet : défense indépendante du graphe (identique aux projections d'export).
        if let Some(want) = project_filter
            && project_name != Some(want)
        {
            continue;
        }

        let is_structure = matches!(kind, Some(k) if k.is_structure());

        // Cible de version de la carte : identité `[[track:]]` (carte de travail) ou identité
        // `[[version:]]` (carte de structure). Sert au filtre ET à la colonne `version` rendue.
        let version_raw: Option<(String, String)> = if is_structure {
            version_identity.clone()
        } else {
            track.clone()
        };

        // Filtre version : ne garder que les cartes rattachées à la version demandée. Une carte
        // sans version (ni track ni version identity) ne matche jamais un filtre explicite — mais
        // elle est rendue en mode non filtré (criterion 2 : jamais omise en silence).
        if let Some(want) = want_version {
            let card_target = version_raw.as_ref().map(|(_, t)| t.as_str());
            if card_target != Some(want) {
                continue;
            }
        }

        // Colonne `version` en format site (`v2.1.0` / `vX.Y.Z`).
        let version = version_raw
            .as_ref()
            .and_then(|(p, t)| map_version_raw(&format!("{p}/{t}")));

        // Dérivation du `release` — cartes de travail uniquement. Une carte de structure n'a pas
        // d'axe release (`None`). Une carte de travail dont le `[[track:]]` ne résout pas est
        // listée avec `release: None` : le null est le signal visible de l'anomalie de registre,
        // jamais un drop silencieux (criteria 2, 7).
        let release = if is_structure {
            None
        } else {
            match (status, &track) {
                (Some(st), Some((tp, tt))) => index
                    .resolve_track(tp, tt)
                    .ok()
                    .and_then(|target| derive_release(st, target, tt).ok())
                    .map(|rk| rk.as_wire().to_string()),
                _ => None,
            }
        };

        entries.push(ProjectMapCardEntry {
            id: id.clone(),
            feature: feature_id,
            status: status.map(|s| s.as_wire().to_string()),
            kind: kind.map(|k| k.as_wire().to_string()),
            release,
            version,
            visibility: visibility.map(|v| v.as_wire().to_string()),
            title: title.clone(),
            supersedes,
            parent,
            track: track.map(|(p, t)| format!("{p}/{t}")),
        });
    }

    // Tri déterministe : cartes de structure d'abord (pas de numéro → clé 0), puis cartes de
    // travail par F-XX croissant ; égalités départagées par ULID (stable, total).
    entries.sort_by(|a, b| {
        let ka = a.feature.as_deref().map_or(0, feature_sort_key);
        let kb = b.feature.as_deref().map_or(0, feature_sort_key);
        ka.cmp(&kb).then_with(|| a.id.cmp(&b.id))
    });

    entries
}

/// Extracts the wikilink targets of a body's **structural zone** only — the lines composed
/// solely of `[[…]]` tokens and visual separators, outside fenced code blocks.
///
/// This is the **single** target extractor of the registry: every read projection and the
/// write-path validator ([`validate_card_body`]) read roles through it, so a role is counted
/// identically whether a card is being written or projected (read/write parity). A role
/// **cited** in prose or inside a ```` ``` ```` fence — the way a card documents the role syntax —
/// is deliberately excluded, so it never inflates a cardinality count nor a read projection. A
/// **structural line** is a line that, once every `[[…]]` token is removed, contains only
/// whitespace and the separator punctuation actually used between role links
/// (`·`, `•`, `|`, `,`, `—`, `–`, `-`). A prose line keeps letters/digits/backticks in its residue
/// and is therefore skipped; role wikilinks the server injects on their own trailing line
/// (`\n\n[[track:…]]`, `\n\n[[feature:…]]`) are kept, as their residue is empty.
fn extract_structural_targets(body: &str) -> Vec<String> {
    scan_structural_targets(body).targets
}

/// Outcome of a structural scan of a card body.
struct StructuralScan {
    /// Roles on structural lines outside any code fence — the counted set.
    targets: Vec<String>,
    /// Roles on structural lines **inside** a fence that is opened and never closed. Non-empty
    /// only for a body missing its closing mark: everything below the opening line was read as
    /// code, by the read projection and the write validator alike, so these roles count nowhere.
    /// A fence that does close leaves this empty — the roles it held are legitimately cited, not
    /// swallowed.
    swallowed: Vec<String>,
    /// Whether the body ends with a fence still open.
    unterminated: bool,
}

/// Scans a body once, separating the roles that **count** from the roles an unclosed fence
/// swallowed. Single pass, so the two sets can never disagree about where the fence state sits.
fn scan_structural_targets(body: &str) -> StructuralScan {
    let mut targets: Vec<String> = Vec::new();
    let mut fenced: Vec<String> = Vec::new();
    let mut in_fence = false;

    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            // A closing mark makes everything it held legitimately fenced prose.
            if !in_fence {
                fenced.clear();
            }
            continue;
        }
        let line_targets = structural_targets_on_line(line);
        if in_fence {
            fenced.extend(line_targets);
        } else {
            targets.extend(line_targets);
        }
    }

    StructuralScan {
        swallowed: if in_fence { fenced } else { Vec::new() },
        unterminated: in_fence,
        targets,
    }
}

/// Does `body` open a ```` ``` ```` fence that no later line closes? Pure, and the sole fence
/// predicate of this module: the extractor and the diagnostic both read the fence state here
/// rather than each keeping its own tally.
fn has_unterminated_code_fence(body: &str) -> bool {
    scan_structural_targets(body).unterminated
}

/// The wikilink targets of a **single line**, or nothing when the line is not structural.
///
/// A **structural line** is a line that, once every `[[…]]` token is removed, contains only
/// whitespace and the separator punctuation actually used between role links.
fn structural_targets_on_line(line: &str) -> Vec<String> {
    /// Separators tolerated between role wikilinks on a structural line. Deliberately excludes
    /// the backtick: a role cited as `` `[[release:planned]]` `` in prose must stay non-structural.
    const SEP: &[char] = &[
        ' ', '\t', '\u{00b7}', '\u{2022}', '|', ',', '\u{2014}', '\u{2013}', '-',
    ];
    let mut line_targets: Vec<String> = Vec::new();
    let mut residue = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        residue.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            line_targets.push(after_open[..end].trim().to_string());
            rest = &after_open[end + 2..];
        } else {
            // `[[` non terminé sur la ligne : le reste est du texte, pas un lien.
            residue.push_str(rest);
            rest = "";
            break;
        }
    }
    residue.push_str(rest);
    if line_targets.is_empty() || !residue.chars().all(|c| SEP.contains(&c)) {
        return Vec::new();
    }
    line_targets
}

/// Returns `reason` unchanged, except for the one case the body itself explains: a fallback
/// reported as [`DerivationFallbackReason::NoTrack`] on a card whose unclosed fence swallowed
/// roles is reclassified as [`DerivationFallbackReason::UnterminatedCodeFence`].
///
/// Deliberately narrow. A track that was present and resolved-but-failed, or that dangles, is not
/// caused by the fence — reclassifying those would trade a visible anomaly for a wrong one, which
/// is worse than the silence being repaired here.
fn fence_aware_reason(
    body: &str,
    scan: &StructuralScan,
    reason: DerivationFallbackReason,
) -> DerivationFallbackReason {
    if matches!(reason, DerivationFallbackReason::NoTrack)
        && has_unterminated_code_fence(body)
        && !scan.swallowed.is_empty()
    {
        return DerivationFallbackReason::UnterminatedCodeFence;
    }
    reason
}

/// Validates a project-map card from its **raw body** — the write-path entry point.
///
/// Combines the three guards that a target-list-only validator
/// ([`validate_links_from_targets`]) cannot express, because they need the body's line structure:
///
/// 1. **Structural extraction** (`extract_structural_targets`) — a role cited in prose or in a
///    code fence is not counted, so a card may document the role syntax without a spurious
///    cardinality rejection (defect 3).
/// 2. **Loud rejection of malformed / unknown roles** — a structural wikilink that fails to parse
///    (a misspelled role, an off-taxonomy value, an unknown prefix whose value is not a
///    ULID/`F-NN`) is **propagated** instead of silently swallowed (defect 2). A bare no-`:` human
///    title stays tolerated (ignored), preserving the historical behaviour.
/// 3. **Cardinality + role coherence** via [`validate_links`], which carries the
///    release/version coherence guard (defect 1).
///
/// The single **source** of the coherence rule is [`validate_links`] itself (criterion 5): this
/// entry point only chooses *which* targets reach it, it does not re-encode the matrix.
///
/// # Errors
///
/// The first [`SchemaError`] encountered: a malformed/unknown structural role (defect 2), or a
/// cardinality/coherence violation from [`validate_links`] (defects 1 & 3).
#[must_use = "the validation result gates the project-map write"]
pub fn validate_card_body(body: &str) -> Result<(), SchemaError> {
    let mut links: Vec<ProjectMapLink> = Vec::new();
    for target in extract_structural_targets(body) {
        match parse_link(&target) {
            Ok(link) => links.push(link),
            // Un titre humain nu (sans `:`) n'est pas un rôle → ignoré (tolérance historique
            // inchangée). Toute autre erreur de parse sur une ligne structurelle est un rôle
            // malformé ou inconnu, rejeté bruyamment (F-213 défaut 2).
            Err(SchemaError::MissingPrefix(_)) => {}
            Err(e) => return Err(e),
        }
    }
    validate_links(&links)
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
    for target in extract_structural_targets(body) {
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

/// Category prefix marker of a project-map card title.
///
/// Derived from the destination **section** (`project-map` → `[PROJECT-MAP]`). It is a
/// constant here — not a magic value — precisely because [`derive_canonical_title`] is, by
/// construction, only ever applied to cards whose section is `project-map`. The section is the
/// source; the constant is its faithful rendering, not an assumption about the project.
const PROJECT_MAP_CATEGORY: &str = "PROJECT-MAP";

/// The `[<project>]` and version-suffix sources of a project-map title, read from the card body.
///
/// Both are read from the card body's **structural zone** ([`extract_structural_targets`]) — the
/// same surface the schema validator enforces — so a role merely *cited* in prose or in a code
/// fence never feeds the title. `project` is the `[[project:]]` value; `version` is the version
/// number for the `— vX.Y.Z` suffix, or `None` when there is no committed version (backlog or
/// absent).
///
/// **Version source precedence — `track` before `version`**: the version axis of a *work*
/// card lives in its `[[track:<project>/<target>]]` pointer, so it wins when present. A *structure*
/// card (ROADMAP/BACKLOG) carries no `[[track:]]` and falls back to its own `[[version:]]`. A work
/// card that still carries a stale `[[version:]]` alongside a `[[track:]]` is titled from the
/// track — the authoritative, current axis — never from the stale role.
///
/// The `backlog` sentinel (as a `[[track:]]` target or a `[[version:]]` number) yields
/// `version = None`: a backlogged card has no committed version, so it receives **no** suffix.
fn title_sources_of_body(body: &str) -> (Option<String>, Option<String>) {
    let mut project: Option<String> = None;
    let mut from_version: Option<String> = None;
    let mut from_track: Option<String> = None;
    for target in extract_structural_targets(body) {
        match parse_link(&target) {
            Ok(ProjectMapLink::Project(p)) if project.is_none() => project = Some(p),
            Ok(ProjectMapLink::Version { version, .. }) if from_version.is_none() => {
                from_version = Some(version);
            }
            Ok(ProjectMapLink::Track { target, .. }) if from_track.is_none() => {
                from_track = Some(target);
            }
            _ => {}
        }
    }
    // `track` (current version axis, F-184) wins over a possibly-stale `[[version:]]`.
    let version = from_track.or(from_version).filter(|v| v != "backlog");
    (project, version)
}

/// Recognises a title tail that is a **version suffix candidate** — the part after the last dash
/// separator that a canonical title (or a defective one) may carry.
///
/// Matches: a real version (`v2.2.0`, `v0.6.4` — **required** leading `v`, then only digits and
/// dots with at least one digit), the literal unsubstituted template `vX.Y.Z`/`X.Y.Z` (the exact
/// the unsubstituted-template defect), and the `backlog` marker. A subject fragment such
/// as `multi-vault`, `A — B`, or one ending in a **bare number** (`step-3`, `Phase-2`, `A — 3.11`)
/// fails the test (a letter, an embedded space, or a numeric tail without the canonical leading
/// `v`) and is therefore preserved.
///
/// The leading `v` is **mandatory** for the numeric branch (P2-1): the canonical suffix is always
/// `— vX.Y.Z`, so a bare numeric tail can only be part of the human subject. Accepting it would
/// silently truncate legitimate titles such as `Migration Phase-2` or `A — 3.11`. Idempotence is
/// preserved because a conforming title always carries the `v`.
fn is_version_suffix_candidate(tail: &str) -> bool {
    let tail = tail.trim();
    if tail == "backlog" || tail == "vX.Y.Z" || tail == "X.Y.Z" {
        return true;
    }
    // Version branch: require the canonical leading `v` so a bare numeric subject fragment
    // (`3`, `3.11`) is never mistaken for a version suffix and stripped.
    let Some(core) = tail.strip_prefix('v') else {
        return false;
    };
    !core.is_empty()
        && core.chars().all(|c| c.is_ascii_digit() || c == '.')
        && core.chars().any(|c| c.is_ascii_digit())
}

/// Strips leading bracketed prefix groups and a trailing version suffix from a title, leaving the
/// human **subject**.
///
/// - **Prefix**: every leading `[…]` group is removed (the canonical `[PROJECT-MAP][project]`, but
///   also any *wrong* prefix a defective title carries — the derivation overwrites, it never
///   appends a second prefix).
/// - **Suffix**: a trailing ` <dash> <version>` is removed when the tail is an
///   [`is_version_suffix_candidate`]. The em-dash `—` is tried first (canonical), then the en-dash
///   `–` and the ASCII hyphen `-`; the last occurrence of each is used, so a dash *inside* the
///   subject (`multi-vault`, `A — B`) is preserved because its tail is not a version candidate.
fn strip_prefix_and_version_suffix(title: &str) -> &str {
    let mut s = title.trim();
    // Peel leading `[…]` groups.
    while let Some(rest) = s
        .trim_start()
        .strip_prefix('[')
        .and_then(|inner| inner.find(']').map(|end| &inner[end + 1..]))
    {
        s = rest;
    }
    s = s.trim();
    // Peel a trailing version suffix, preferring the canonical em-dash separator.
    for dash in ['\u{2014}', '\u{2013}', '-'] {
        if let Some((head, tail)) = s.rsplit_once(dash)
            && is_version_suffix_candidate(tail)
        {
            return head.trim();
        }
    }
    s
}

/// Derives the **canonical H1 title** of a project-map card from its body roles.
///
/// Canonical form: `[PROJECT-MAP][<project>] <subject> — v<x.y.z>`. This is the *write-path*
/// counterpart of the schema validator: rather than *rejecting* a non-conforming title (which
/// pushes the burden back onto a caller that just proved it cannot carry it, and invites a
/// work-around), the server *derives* the title it should have — invisible and correct every time
/// (the "derive, don't reject" principle).
///
/// Sources (all read from the card `body`, none a hard-coded project):
/// - `[PROJECT-MAP]` — the category, from the destination section (see `PROJECT_MAP_CATEGORY`);
/// - `[<project>]` — the `[[project:…]]` role;
/// - `— v<x.y.z>` — the version, `track`-first then `version` (see `title_sources_of_body`).
///
/// The `<subject>` is preserved from `current_title`, with any existing bracketed prefix and any
/// existing version suffix stripped first (`strip_prefix_and_version_suffix`) — so a *wrong* or
/// *stale* prefix/suffix is **overwritten** while the human subject survives.
///
/// # Idempotence
///
/// Applying the derivation to an already-canonical title returns it **byte-for-byte** unchanged
/// (an acceptance criterion) — the strip-then-rebuild round-trips exactly on a conforming title.
///
/// # backlog
///
/// A version role of `backlog` (or the absence of any version/track role) yields **no** version
/// suffix — deterministic, documented, and never the literal `— vX.Y.Z` template.
///
/// # Defensive no-op
///
/// If the body carries no `[[project:…]]` role — impossible on a schema-valid card, which requires
/// exactly one, but this function makes no such assumption — the derivation cannot build a faithful
/// prefix and returns `current_title` **unchanged** rather than inventing a project.
///
/// The function is **pure**: no I/O, deterministic, total (never panics, never errors).
#[must_use]
pub fn derive_canonical_title(body: &str, current_title: &str) -> String {
    let (project, version) = title_sources_of_body(body);
    let Some(project) = project else {
        // No project role → cannot build a faithful prefix. Leave the title untouched.
        return current_title.to_string();
    };
    let subject = strip_prefix_and_version_suffix(current_title);
    let mut out = format!("[{PROJECT_MAP_CATEGORY}][{project}]");
    if !subject.is_empty() {
        out.push(' ');
        out.push_str(subject);
    }
    if let Some(version) = version {
        out.push_str(" \u{2014} v");
        out.push_str(&version);
    }
    out
}

// ── F-153 — mutation ciblée d'un rôle typé ─────────────────────────────────────
//
// Le seul chemin d'écriture d'une carte project-map réémet le corps ENTIER : muter un
// rôle exige de posséder le corps courant côté client, qui le reconstruit — un paragraphe
// recomposé par le curateur peut être écrasé sans erreur, et la carte est insupprimable.
// F-153 déplace la mutation CÔTÉ SERVEUR : le corps courant est lu, le rôle typé visé est
// remplacé **en place**, tout le reste du corps reste octet-pour-octet identique.

/// The typed roles of a project-map card that may be mutated **in place**.
///
/// Deliberately excludes the two identity roles: `feature` (the `F-XX` number — the card's
/// identity, guaranteed unique) and `project` (the owning project). Both are rejected as
/// [`RoleMutationError::ImmutableIdentity`] — a rename is a new card plus `[[supersedes:]]`,
/// never an in-place mutation. Only the lifecycle (`status`/`kind`/`release`) and version
/// (`version`/`track`) axes are mutable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MutableRole {
    /// `[[status:<STATUS>]]` — lifecycle status.
    Status,
    /// `[[kind:<KIND>]]` — nature of the work unit.
    Kind,
    /// `[[release:<r>]]` — delivery status.
    Release,
    /// `[[version:<project>/<x.y.z>]]` — target version (structure cards).
    Version,
    /// `[[track:<project>/<target>]]` — version axis of a work card.
    Track,
}

impl MutableRole {
    /// The reserved wire prefix of this role (`status`, `kind`, `release`, `version`, `track`).
    #[must_use]
    pub const fn wire_prefix(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Kind => "kind",
            Self::Release => "release",
            Self::Version => "version",
            Self::Track => "track",
        }
    }

    /// Parses a role-type token into a mutable role.
    ///
    /// Enforces the allow-list at the frontier (parse-don't-validate): only the five mutable
    /// typed roles are accepted. `feature`/`project` are refused **explicitly** as the card
    /// identity; any other token is an unknown role type.
    ///
    /// # Errors
    ///
    /// - [`RoleMutationError::ImmutableIdentity`] for `feature` or `project`.
    /// - [`RoleMutationError::UnknownRoleType`] for any other token.
    pub fn from_wire(role_type: &str) -> Result<Self, RoleMutationError> {
        match role_type {
            "status" => Ok(Self::Status),
            "kind" => Ok(Self::Kind),
            "release" => Ok(Self::Release),
            "version" => Ok(Self::Version),
            "track" => Ok(Self::Track),
            "feature" | "project" => {
                Err(RoleMutationError::ImmutableIdentity(role_type.to_string()))
            }
            other => Err(RoleMutationError::UnknownRoleType(other.to_string())),
        }
    }

    /// `true` when a parsed link is the variant this role designates.
    fn matches_variant(self, link: &ProjectMapLink) -> bool {
        matches!(
            (self, link),
            (Self::Status, ProjectMapLink::Status(_))
                | (Self::Kind, ProjectMapLink::Kind(_))
                | (Self::Release, ProjectMapLink::Release(_))
                | (Self::Version, ProjectMapLink::Version { .. })
                | (Self::Track, ProjectMapLink::Track { .. })
        )
    }
}

/// Error raised by [`mutate_typed_role`] or [`MutableRole::from_wire`].
///
/// Every variant is a **refusal** of one card's mutation — never a silent success. In a batch
/// each refusal is a per-card verdict; the batch never collapses to a single aggregate outcome.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RoleMutationError {
    /// The role-type token is not one of the five mutable typed roles.
    #[error("unknown role type {0:?}: only status/kind/release/version/track are mutable")]
    UnknownRoleType(String),
    /// The role is the card identity (`feature`/`project`) and can never be mutated in place.
    #[error(
        "role {0:?} is the card identity and is immutable — a rename is a new card plus \
         [[supersedes:]], never an in-place mutation"
    )]
    ImmutableIdentity(String),
    /// The target role is absent from the card's structural zone. The mutation refuses to
    /// **create** a role — it only replaces an existing value.
    #[error(
        "role [[{prefix}:…]] is absent from the card's structural zone — mutation refuses to \
         create a role, only to replace an existing value"
    )]
    RoleAbsent {
        /// Wire prefix of the absent role.
        prefix: &'static str,
    },
    /// Several structural occurrences of the same typed role — a valid card carries at most one;
    /// the mutation refuses to guess which to change.
    #[error(
        "multiple structural [[{prefix}:…]] roles — ambiguous, refusing to guess which to mutate"
    )]
    Ambiguous {
        /// Wire prefix of the ambiguous role.
        prefix: &'static str,
    },
    /// The submitted new value does not parse into a valid link of the target role's variant.
    #[error("new value {value:?} is not a valid [[{prefix}:…]] role value")]
    InvalidNewValue {
        /// Wire prefix of the role.
        prefix: &'static str,
        /// The rejected value.
        value: String,
    },
    /// The client-provided history line carries a structural-role opener (`[[`), which
    /// [`append_history_line`] would splice verbatim — smuggling a bare role the client is never
    /// allowed to inject through this path. Refused (not escaped) so the body stays unambiguous.
    #[error(
        "history line carries a structural-role marker `[[` — refused: this path never injects a \
         structural role into a card body"
    )]
    HistoryLineInvalid,
}

/// Byte span of a role's **value** inside a card body (the bytes between `prefix:` and `]]`).
#[derive(Debug, Clone, Copy)]
struct RoleValueSpan {
    /// Byte offset where the value starts (just after the first `:` of the token's inner text).
    value_start: usize,
    /// Byte offset where the value ends (the `]]` closing delimiter).
    value_end: usize,
}

/// `true` when `line` (already stripped of its trailing newline / `\r`) opens or closes a
/// fenced code block.
fn is_fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Strips a trailing `\n` then a trailing `\r` from a line produced by `split_inclusive('\n')`,
/// yielding the line content (CRLF- and LF-safe). Byte offsets are unaffected — callers keep the
/// raw line length for offset tracking.
fn strip_eol(line_with_nl: &str) -> &str {
    let s = line_with_nl.strip_suffix('\n').unwrap_or(line_with_nl);
    s.strip_suffix('\r').unwrap_or(s)
}

/// Locates the **single structural** occurrence of `[[prefix:…]]` in `body`, returning the byte
/// span of its value.
///
/// The role is addressed **by type, never by position**: it may sit on the first line or the
/// last, alone or among other role links. A role merely *cited* in prose or inside a fenced code
/// block is ignored — the structural-zone rule is identical to [`extract_structural_targets`]
/// (a line whose residue, once every `[[…]]` token is removed, is only separators/whitespace).
///
/// # Errors
///
/// - [`RoleMutationError::RoleAbsent`] if no structural occurrence exists.
/// - [`RoleMutationError::Ambiguous`] if more than one structural occurrence exists.
fn locate_structural_role(
    body: &str,
    prefix: &'static str,
) -> Result<RoleValueSpan, RoleMutationError> {
    /// Same separator set as [`extract_structural_targets`] (backtick excluded on purpose).
    const SEP: &[char] = &[
        ' ', '\t', '\u{00b7}', '\u{2022}', '|', ',', '\u{2014}', '\u{2013}', '-',
    ];
    let mut found: Option<RoleValueSpan> = None;
    let mut in_fence = false;
    let mut line_start = 0usize;

    for raw_line in body.split_inclusive('\n') {
        let content = strip_eol(raw_line);

        if is_fence_line(content) {
            in_fence = !in_fence;
            line_start += raw_line.len();
            continue;
        }
        if in_fence {
            line_start += raw_line.len();
            continue;
        }

        // Walk the tokens of the line, building the residue and — in parallel — the candidate
        // span for our prefix. The span is only retained once the line is proven structural.
        let mut residue = String::new();
        let mut candidate: Option<RoleValueSpan> = None;
        let mut line_matches = 0usize; // structural-role matches of `prefix` on THIS line
        let mut rest = content;
        let mut cursor = line_start; // absolute byte offset of `rest[0]` in `body`
        let mut has_token = false;
        while let Some(open) = rest.find("[[") {
            residue.push_str(&rest[..open]);
            let token_abs = cursor + open;
            let after_open = &rest[open + 2..];
            if let Some(end) = after_open.find("]]") {
                has_token = true;
                let inner = &after_open[..end];
                // Structural identity is the TRIMMED inner (parity with extract_structural_targets).
                let (p, _) = split_prefix(inner.trim());
                if p == prefix {
                    line_matches += 1;
                    // Value span: bytes after the FIRST `:` of the RAW inner, up to `]]`.
                    if let Some(colon) = inner.find(':') {
                        let inner_abs = token_abs + 2;
                        candidate = Some(RoleValueSpan {
                            value_start: inner_abs + colon + 1,
                            value_end: inner_abs + inner.len(),
                        });
                    }
                }
                let consumed = open + 2 + end + 2;
                cursor += consumed;
                rest = &rest[consumed..];
            } else {
                // Unterminated `[[` — the remainder is text, not a link.
                residue.push_str(rest);
                rest = "";
                break;
            }
        }
        residue.push_str(rest);

        let structural = has_token && residue.chars().all(|c| SEP.contains(&c));
        if structural && let Some(span) = candidate {
            // A valid card carries at most one structural occurrence of a typed role. Two on the
            // SAME line (`line_matches > 1`) or one already seen on a PRIOR line (`found`) are both
            // ambiguous — refuse to guess which to mutate (parity between intra- and inter-line).
            if found.is_some() || line_matches > 1 {
                return Err(RoleMutationError::Ambiguous { prefix });
            }
            found = Some(span);
        }

        line_start += raw_line.len();
    }

    found.ok_or(RoleMutationError::RoleAbsent { prefix })
}

/// Mutates the value of a single typed role in a project-map card body **in place**.
///
/// The role is located **by type, not by position** (see `locate_structural_role`); its value
/// is replaced and **every other byte of `body` is left identical** — brackets, prefix, colon,
/// separators, prose and fenced blocks all untouched. This is the core guarantee: the client
/// never reconstructs the body, so no paragraph can be lost.
///
/// `new_value` is validated by re-parsing `"<prefix>:<new_value>"` through the link parser
/// ([`parse_link`]); a value that does not yield the target role's variant is refused.
///
/// # Errors
///
/// - [`RoleMutationError::InvalidNewValue`] if `new_value` is not a valid value for `role`.
/// - [`RoleMutationError::RoleAbsent`] if the role is absent from the structural zone.
/// - [`RoleMutationError::Ambiguous`] if more than one structural occurrence exists.
#[must_use = "the mutated body must be persisted or the mutation is lost"]
pub fn mutate_typed_role(
    body: &str,
    role: MutableRole,
    new_value: &str,
) -> Result<String, RoleMutationError> {
    let prefix = role.wire_prefix();

    // (1) Validate the new value at the frontier: the candidate link must parse into the
    //     target role's variant. Refuses off-taxonomy values, wrong shapes, empty values.
    let candidate = format!("{prefix}:{new_value}");
    match parse_link(&candidate) {
        Ok(link) if role.matches_variant(&link) => {}
        _ => {
            return Err(RoleMutationError::InvalidNewValue {
                prefix,
                value: new_value.to_string(),
            });
        }
    }

    // (2) Locate the single structural occurrence and splice the value bytes only.
    let span = locate_structural_role(body, prefix)?;
    let mut out =
        String::with_capacity(body.len() - (span.value_end - span.value_start) + new_value.len());
    out.push_str(&body[..span.value_start]);
    out.push_str(new_value);
    out.push_str(&body[span.value_end..]);
    Ok(out)
}

/// `true` when `line` (stripped of newline / `\r`) is a Markdown heading naming the `Historique`
/// section (`## Historique`, `### Historique : …`, case-insensitive on the word).
fn is_historique_heading(line: &str) -> bool {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 {
        return false;
    }
    // '#' is ASCII (1 byte) → the char count equals the byte offset.
    t[hashes..]
        .trim_start()
        .to_lowercase()
        .starts_with("historique")
}

/// `true` when `line` (stripped of newline / `\r`) opens any Markdown heading.
fn is_any_heading(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// Rejects a client-provided history line that would smuggle a structural role into a card.
///
/// [`append_history_line`] is a pure byte insertion — it never parses its input. A line containing
/// the structural-role opener `[[` would therefore be spliced verbatim, letting a privileged
/// writer inject a bare `[[status:DONE]]` (or any role) through the history path — exactly what
/// the in-place mutation contract forbids the client to do. The downstream `validate_card_body` safety net would still reject a
/// malformed result, but the invariant *"this path never injects a structural role"* must hold
/// **upstream**, before any byte is spliced. Rejection (rather than escaping) keeps the resulting
/// body unambiguous and the contract simple.
///
/// # Errors
///
/// - [`RoleMutationError::HistoryLineInvalid`] if `line` contains the `[[` opener.
pub fn validate_history_line(line: &str) -> Result<(), RoleMutationError> {
    if line.contains("[[") {
        return Err(RoleMutationError::HistoryLineInvalid);
    }
    Ok(())
}

/// Appends a client-provided `line` to the card's `Historique` section, creating the section at
/// the end of the body when absent.
///
/// Pure byte insertion: every existing byte of `body` is preserved; only `line` (plus the
/// minimal newline needed to separate it) is added. When the section exists, the line is inserted
/// at the **end of the section** — just before the next heading, or at end of body. When it does
/// not, a `## Historique` section is appended at the end.
///
/// Fenced code blocks are skipped when detecting headings, so a `#`-leading or `Historique`-named
/// line inside a fence never fools the placement.
#[must_use]
pub fn append_history_line(body: &str, line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']);

    // Pass 1 — find the Historique heading (outside fences).
    let mut heading_at: Option<usize> = None;
    let mut in_fence = false;
    let mut offset = 0usize;
    for raw in body.split_inclusive('\n') {
        let content = raw.strip_suffix('\n').unwrap_or(raw).trim_end_matches('\r');
        if is_fence_line(content) {
            in_fence = !in_fence;
        } else if !in_fence && is_historique_heading(content) {
            heading_at = Some(offset);
            break;
        }
        offset += raw.len();
    }

    let Some(h) = heading_at else {
        // No section → create it at end of body.
        let mut out = String::with_capacity(body.len() + line.len() + 20);
        out.push_str(body.trim_end_matches('\n'));
        out.push_str("\n\n## Historique\n\n");
        out.push_str(line);
        out.push('\n');
        return out;
    };

    // Pass 2 — the section ends at the next heading after `h`, or at end of body.
    let mut sect_end = body.len();
    let mut in_fence = false;
    let mut o = h;
    let mut first = true;
    for raw in body[h..].split_inclusive('\n') {
        let content = raw.strip_suffix('\n').unwrap_or(raw).trim_end_matches('\r');
        if is_fence_line(content) {
            in_fence = !in_fence;
        } else if !first && !in_fence && is_any_heading(content) {
            sect_end = o;
            break;
        }
        first = false;
        o += raw.len();
    }

    // Minimal byte insertion at the section boundary.
    let need_leading_nl = !body[..sect_end].ends_with('\n');
    let mut out = String::with_capacity(body.len() + line.len() + 2);
    out.push_str(&body[..sect_end]);
    if need_leading_nl {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    out.push_str(&body[sect_end..]);
    out
}

#[cfg(test)]
mod mutate_role_tests {
    use super::{
        MutableRole, RoleMutationError, append_history_line, mutate_typed_role,
        validate_history_line,
    };

    /// A representative "large" card: multi-paragraph body, roles on the FIRST line, an
    /// Historique section, and a fenced code block that *cites* the status role syntax.
    const BIG_CARD: &str = "[[feature:F-153]] [[project:gradatum]] [[status:OPEN]] \
[[kind:FEATURE]] [[release:planned]] [[track:gradatum/2.3.0]]

# Mutation de rôle ciblée

Un long paragraphe qui décrit le besoin métier. Il contient volontairement le mot status
et une citation en prose de `[[status:DONE]]` pour vérifier qu'elle n'est jamais mutée.

```text
Exemple documenté : [[status:BLOCKED]] ne doit pas être touché dans un fence.
```

Un second paragraphe, avec des accents éàü et de la ponctuation — pour l'octet-identité.

## Historique

- 2026-09-01 : CREATED OPEN
";

    #[test]
    fn status_mutation_is_byte_identical_except_the_role_value() {
        let out = mutate_typed_role(BIG_CARD, MutableRole::Status, "DONE").expect("mutation OK");
        let expected = BIG_CARD.replacen("[[status:OPEN]]", "[[status:DONE]]", 1);
        assert_eq!(out, expected, "seule la valeur du rôle status change");
        // La citation en prose et le fence sont intacts.
        assert!(out.contains("`[[status:DONE]]`"), "prose inchangée");
        assert!(out.contains("[[status:BLOCKED]]"), "fence inchangé");
    }

    #[test]
    fn version_axis_track_mutation_replaces_only_the_value() {
        let out =
            mutate_typed_role(BIG_CARD, MutableRole::Track, "gradatum/2.4.0").expect("mutation OK");
        let expected = BIG_CARD.replacen("[[track:gradatum/2.3.0]]", "[[track:gradatum/2.4.0]]", 1);
        assert_eq!(out, expected);
    }

    #[test]
    fn role_on_the_last_line_is_mutated_typed_not_positional() {
        // Rôles en FIN de document (position opposée à BIG_CARD).
        let card = "# Titre\n\nDu texte de corps.\n\n## Historique\n\n- note\n\n\
[[feature:F-9]] [[project:gradatum]] [[status:IN_PROGRESS]] [[kind:FIX]] [[release:planned]] [[version:gradatum/backlog]]\n";
        let out = mutate_typed_role(card, MutableRole::Status, "DONE").expect("mutation OK");
        assert_eq!(
            out,
            card.replacen("[[status:IN_PROGRESS]]", "[[status:DONE]]", 1)
        );
    }

    #[test]
    fn absent_role_is_refused_never_created() {
        // Carte sans rôle visibilite.
        let err = mutate_typed_role(BIG_CARD, MutableRole::Version, "gradatum/2.4.0")
            .expect_err("aucun [[version:]] structurel → refus");
        assert_eq!(err, RoleMutationError::RoleAbsent { prefix: "version" });
    }

    #[test]
    fn role_only_cited_in_prose_or_fence_counts_as_absent() {
        let card =
            "# Carte\n\nProse citant `[[release:dropped]]`.\n\n```\n[[release:planned]]\n```\n";
        let err = mutate_typed_role(card, MutableRole::Release, "released")
            .expect_err("aucune occurrence structurelle");
        assert_eq!(err, RoleMutationError::RoleAbsent { prefix: "release" });
    }

    #[test]
    fn invalid_new_value_is_refused() {
        // "done" minuscule n'est pas un StatusKind (wire SCREAMING_SNAKE).
        let err = mutate_typed_role(BIG_CARD, MutableRole::Status, "done")
            .expect_err("valeur hors taxonomie");
        assert_eq!(
            err,
            RoleMutationError::InvalidNewValue {
                prefix: "status",
                value: "done".to_string(),
            }
        );
    }

    #[test]
    fn multiple_structural_occurrences_are_ambiguous() {
        let card = "[[status:OPEN]]\n\n[[status:DONE]]\n";
        let err = mutate_typed_role(card, MutableRole::Status, "BLOCKED")
            .expect_err("deux occurrences structurelles");
        assert_eq!(err, RoleMutationError::Ambiguous { prefix: "status" });
    }

    #[test]
    fn two_same_type_roles_on_one_line_is_ambiguous() {
        // Deux rôles du MÊME type sur une SEULE ligne structurelle : la garde d'ambiguïté doit
        // opérer au niveau TOKEN, pas seulement inter-lignes — sinon le dernier écrase le premier
        // en silence et le 2ᵉ est muté au lieu d'un refus.
        let card = "[[status:OPEN]] [[status:DONE]]\n";
        let err = mutate_typed_role(card, MutableRole::Status, "BLOCKED")
            .expect_err("deux status sur une ligne → ambigu");
        assert_eq!(err, RoleMutationError::Ambiguous { prefix: "status" });
    }

    #[test]
    fn history_line_with_structural_marker_is_refused() {
        // Une history_line ne peut jamais injecter un rôle structurel bare : `[[` est refusé en
        // amont d'append_history_line (invariant F-153 « jamais injecter un rôle structurel »),
        // avant tout splice — la carte reste donc intacte.
        let err = validate_history_line("- 2026-09-06 : bascule [[status:DONE]] en douce")
            .expect_err("marqueur structurel dans la ligne d'historique");
        assert_eq!(err, RoleMutationError::HistoryLineInvalid);
        // Une ligne d'historique légitime passe sans erreur.
        validate_history_line("- 2026-09-06 : status OPEN→DONE (F-153)").expect("ligne saine OK");
    }

    #[test]
    fn from_wire_refuses_identity_and_unknown_roles() {
        assert_eq!(
            MutableRole::from_wire("feature"),
            Err(RoleMutationError::ImmutableIdentity("feature".to_string()))
        );
        assert_eq!(
            MutableRole::from_wire("project"),
            Err(RoleMutationError::ImmutableIdentity("project".to_string()))
        );
        assert_eq!(
            MutableRole::from_wire("supersedes"),
            Err(RoleMutationError::UnknownRoleType("supersedes".to_string()))
        );
        assert_eq!(MutableRole::from_wire("status"), Ok(MutableRole::Status));
        assert_eq!(MutableRole::from_wire("track"), Ok(MutableRole::Track));
    }

    #[test]
    fn append_history_inserts_at_end_of_existing_section() {
        let out = append_history_line(BIG_CARD, "- 2026-09-06 : status OPEN→DONE (F-153)");
        // Toutes les lignes d'origine sont préservées.
        assert!(out.contains("- 2026-09-01 : CREATED OPEN"));
        assert!(out.contains("- 2026-09-06 : status OPEN→DONE (F-153)"));
        // La nouvelle ligne suit l'ancienne (section = dernière du corps).
        let old = out.find("CREATED OPEN").unwrap();
        let new = out.find("status OPEN→DONE").unwrap();
        assert!(new > old, "append en fin de section");
        // Aucun octet du préambule (rôles + prose) n'a bougé.
        assert!(out.starts_with("[[feature:F-153]] [[project:gradatum]] [[status:OPEN]]"));
    }

    #[test]
    fn append_history_creates_section_when_absent() {
        let card = "# Carte\n\n[[status:OPEN]]\n";
        let out = append_history_line(card, "- 2026-09-06 : créée");
        assert!(out.starts_with("# Carte\n\n[[status:OPEN]]\n"));
        assert!(out.contains("## Historique"));
        assert!(out.trim_end().ends_with("- 2026-09-06 : créée"));
    }

    #[test]
    fn append_history_inserts_before_a_following_section() {
        // Historique n'est PAS la dernière section : l'insertion se fait avant la suivante.
        let card = "## Historique\n\n- a\n\n## Notes\n\ndu texte\n";
        let out = append_history_line(card, "- b");
        let idx_b = out.find("- b").unwrap();
        let idx_notes = out.find("## Notes").unwrap();
        assert!(idx_b < idx_notes, "insertion avant la section suivante");
        assert!(
            out.contains("## Notes\n\ndu texte\n"),
            "section suivante intacte"
        );
    }
}

#[cfg(test)]
mod derive_title_tests {
    use super::derive_canonical_title;

    /// Une carte sans préfixe est relue avec le titre canonique (préfixe + suffixe ajoutés).
    #[test]
    fn adds_prefix_and_suffix_when_absent() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        let got = derive_canonical_title(body, "Dériver le titre canonique");
        assert_eq!(
            got,
            "[PROJECT-MAP][gradatum] Dériver le titre canonique \u{2014} v2.2.0"
        );
    }

    /// Idempotence : un titre déjà conforme est INCHANGÉ À L'OCTET.
    #[test]
    fn already_canonical_title_is_byte_identical() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        let canonical = "[PROJECT-MAP][gradatum] Dériver le titre canonique \u{2014} v2.2.0";
        assert_eq!(derive_canonical_title(body, canonical), canonical);
        // Double application = même résultat (stabilité).
        let once = derive_canonical_title(body, canonical);
        assert_eq!(derive_canonical_title(body, &once), once);
    }

    /// Le suffixe SUIT le rôle de version (cas dominant F-247) : re-dériver depuis un track
    /// nouveau remplace l'ancien suffixe.
    #[test]
    fn version_suffix_follows_track_change() {
        let body_new = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                        [[track:gradatum/2.3.0]] [[feature:F-99]]";
        let stale = "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0";
        assert_eq!(
            derive_canonical_title(body_new, stale),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.3.0"
        );
    }

    /// `track` prime sur un `[[version:]]` résiduel (axe version courant = track, F-184).
    #[test]
    fn track_wins_over_stale_version_role() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[version:gradatum/2.1.0]] [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "Sujet"),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0"
        );
    }

    /// Carte de structure (pas de track) : le suffixe vient de `[[version:]]`.
    #[test]
    fn structure_card_uses_version_role() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] \
                    [[version:gradatum/2.2.0]] [[visibilite:public]]";
        assert_eq!(
            derive_canonical_title(body, "Roadmap 2.2.0"),
            "[PROJECT-MAP][gradatum] Roadmap 2.2.0 \u{2014} v2.2.0"
        );
    }

    /// backlog (via track) → AUCUN suffixe, jamais le gabarit `— vX.Y.Z`.
    #[test]
    fn backlog_track_yields_no_suffix() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/backlog]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "Sujet backlog"),
            "[PROJECT-MAP][gradatum] Sujet backlog"
        );
    }

    /// backlog (via version) → AUCUN suffixe.
    #[test]
    fn backlog_version_yields_no_suffix() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[version:gradatum/backlog]] [[release:planned]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "Sujet"),
            "[PROJECT-MAP][gradatum] Sujet"
        );
    }

    /// Projet autre que le défaut : le préfixe SUIT le rôle, jamais une constante.
    #[test]
    fn prefix_follows_project_role_not_a_constant() {
        let body = "[[project:acme]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:acme/1.0.0]] [[feature:F-1]]";
        assert_eq!(
            derive_canonical_title(body, "Sujet"),
            "[PROJECT-MAP][acme] Sujet \u{2014} v1.0.0"
        );
    }

    /// Titre portant DÉJÀ un autre préfixe → corrigé (la dérivation écrase).
    #[test]
    fn wrong_prefix_is_overwritten() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "[DEBUG][autre] Sujet \u{2014} v9.9.9"),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0"
        );
    }

    /// Le gabarit non substitué `— vX.Y.Z` est retiré puis remplacé par la vraie version.
    #[test]
    fn unsubstituted_template_suffix_is_replaced() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "[PROJECT-MAP][gradatum] Sujet \u{2014} vX.Y.Z"),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0"
        );
    }

    /// Un tiret DANS le sujet (`multi-vault`) est préservé — ce n'est pas un suffixe de version.
    #[test]
    fn hyphen_inside_subject_is_preserved() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "Support multi-vault \u{2014} v2.2.0"),
            "[PROJECT-MAP][gradatum] Support multi-vault \u{2014} v2.2.0"
        );
    }

    /// Un em-dash DANS le sujet est préservé (rsplit prend la dernière occurrence).
    #[test]
    fn em_dash_inside_subject_is_preserved() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        let canonical = "[PROJECT-MAP][gradatum] A \u{2014} B \u{2014} v2.2.0";
        assert_eq!(derive_canonical_title(body, canonical), canonical);
    }

    /// Rôle en PROSE (dans une fence) : ignoré (extraction structurelle, cohérent F-213).
    #[test]
    fn role_cited_in_fence_is_ignored() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]\n\n```\n[[project:autre]]\n```";
        assert_eq!(
            derive_canonical_title(body, "Sujet"),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0"
        );
    }

    /// Aucun rôle project → no-op défensif (titre inchangé).
    #[test]
    fn missing_project_role_is_a_noop() {
        let body = "just some prose with no roles";
        assert_eq!(
            derive_canonical_title(body, "Titre quelconque \u{2014} v1.0.0"),
            "Titre quelconque \u{2014} v1.0.0"
        );
    }

    // ── P2-1 : un token numérique NU en fin de sujet n'est PAS un suffixe de version ──
    // (le suffixe canonique porte toujours le `v` de tête ; un nombre nu appartient au sujet).

    /// Sujet finissant par `-<nombre>` (tiret ASCII) : le dernier token est PRÉSERVÉ, jamais retiré.
    #[test]
    fn ascii_hyphen_bare_number_tail_is_preserved() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "step-2"),
            "[PROJECT-MAP][gradatum] step-2 \u{2014} v2.2.0"
        );
    }

    /// Sujet finissant par `Phase-2` : le `-2` fait partie du sujet, pas un suffixe de version.
    #[test]
    fn phase_dash_number_subject_is_preserved() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "Migration Phase-2"),
            "[PROJECT-MAP][gradatum] Migration Phase-2 \u{2014} v2.2.0"
        );
    }

    /// Sujet `A — 3.11` (em-dash + nombre nu SANS `v`) : le nombre nu est PRÉSERVÉ dans le sujet.
    #[test]
    fn em_dash_bare_number_tail_is_preserved() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "A \u{2014} 3.11"),
            "[PROJECT-MAP][gradatum] A \u{2014} 3.11 \u{2014} v2.2.0"
        );
    }

    /// La VRAIE version `— v2.2.0` (avec `v`) reste correctement traitée : suffixe reconnu, remplacé.
    #[test]
    fn real_v_prefixed_version_suffix_still_replaced() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.3.0]] [[feature:F-99]]";
        assert_eq!(
            derive_canonical_title(body, "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.2.0"),
            "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.3.0"
        );
    }

    /// Idempotence byte-exacte sur un titre dont le sujet finit par un nombre nu (`A — 3.11`).
    #[test]
    fn idempotent_on_subject_ending_in_bare_number() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[track:gradatum/2.2.0]] [[feature:F-99]]";
        let canonical = "[PROJECT-MAP][gradatum] A \u{2014} 3.11 \u{2014} v2.2.0";
        assert_eq!(derive_canonical_title(body, canonical), canonical);
        let once = derive_canonical_title(body, canonical);
        assert_eq!(derive_canonical_title(body, &once), once);
    }
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

    /// The `version` column reads the **same structural source as the title**, precedence
    /// included: the `[[track:]]` pointer first, the card's own `[[version:]]` only as a
    /// fallback.
    ///
    /// A work card can carry both — the stored `[[version:]]` surviving from a bootstrap
    /// attachment while the track points at the version actually being delivered. The title
    /// derivation already prefers the track (`derive_canonical_title` → `title_sources_of_body`),
    /// so a card reading its version from the stored role alone renders a title saying `— v2.3.0`
    /// beside a version column saying `v0.0.0`: one card, two answers, and the wrong one is the
    /// machine-readable axis the website consumes.
    #[test]
    fn version_reads_the_same_source_as_the_title_track_first() {
        let body = "[[feature:F-42]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[release:planned]] [[version:gradatum/0.0.0]] [[track:gradatum/2.3.0]]";
        // Control: the title derivation, on the same body, names the tracked version.
        assert_eq!(
            title_sources_of_body(body).1.as_deref(),
            Some("2.3.0"),
            "le titre lit la version du track"
        );

        let notes = vec![note(body, "[PROJECT-MAP][gradatum] Sujet \u{2014} v2.3.0")];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(
            entries[0].version,
            Some("v2.3.0".to_string()),
            "la même source que le titre : jamais la version stockée périmée"
        );
    }

    /// The visibility axis is **rendered**, not filtered away: the wire value of the role is
    /// carried as-is, and an absent role stays an explicit `null` (absence = public by reader
    /// convention). Rendering it is what lets a reader tell a card that *is* public from a card
    /// that merely failed to say — the two were indistinguishable while the axis was silent.
    #[test]
    fn visibility_is_rendered_as_is_and_null_when_absent() {
        let notes = vec![
            note(
                "[[feature:F-43]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.3.0]] [[visibilite:interne]]",
                "F-43 interne",
            ),
            note(
                "[[feature:F-44]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.3.0]] [[visibilite:public]]",
                "F-44 publique",
            ),
            note(
                "[[feature:F-45]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.3.0]]",
                "F-45 sans rôle",
            ),
        ];
        // Audit mode: the default mirror excludes the `interne` card, so reading all three
        // requires lifting the F-256 filter — the rendering is asserted on the same corpus.
        let entries = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        assert_eq!(entries.len(), 3, "{entries:?}");

        let by_feature = |f: &str| {
            entries
                .iter()
                .find(|e| e.feature == f)
                .unwrap_or_else(|| panic!("{f} absente : {entries:?}"))
        };
        assert_eq!(
            by_feature("F-43").visibility,
            Some("interne".to_string()),
            "valeur du rôle rendue telle quelle"
        );
        assert_eq!(by_feature("F-44").visibility, Some("public".to_string()));
        assert_eq!(
            by_feature("F-45").visibility,
            None,
            "absence de rôle = null explicite, jamais une valeur inventée"
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

    /// F-256 — carte `[[visibilite:interne]]` → **exclue** du catalogue par défaut, en plus
    /// des filtres kind/dropped.
    #[test]
    fn interne_card_excluded_by_default() {
        let notes = vec![note(
            "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]] [[visibilite:interne]]",
            "F-60 interne",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(
            entries.is_empty(),
            "carte visibilite:interne exclue par défaut : {entries:?}"
        );
    }

    /// F-256 — carte `[[visibilite:public]]` → **incluse** (déclaration explicite de publiabilité).
    #[test]
    fn public_card_included_by_default() {
        let notes = vec![note(
            "[[feature:F-61]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]] [[visibilite:public]]",
            "F-61 public",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 1, "carte visibilite:public incluse");
        assert_eq!(entries[0].feature, "F-61");
    }

    /// F-256 — absence de `[[visibilite:]]` → **incluse** (défaut = public, l'exclusion est un acte).
    #[test]
    fn card_without_visibilite_is_public_by_default() {
        let notes = vec![note(
            "[[feature:F-62]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]]",
            "F-62 sans visibilite",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(
            entries.len(),
            1,
            "l'absence de visibilite vaut public (défaut) : {entries:?}"
        );
        assert_eq!(entries[0].feature, "F-62");
    }

    /// F-256 — le mode audit `include_dropped=true` lève AUSSI le filtre interne.
    #[test]
    fn interne_card_included_with_include_dropped() {
        let notes = vec![note(
            "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]] [[visibilite:interne]]",
            "F-60 interne",
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
            "carte interne incluse en mode audit include_dropped"
        );
        assert_eq!(entries[0].feature, "F-60");
        // Non-régression du choix de visibilité : le mode audit **rend** l'axe au lieu de le
        // taire. C'est la seule voie par laquelle un lecteur peut distinguer une carte interne
        // d'une carte publique — sans le champ, les deux sorties seraient identiques.
        assert_eq!(
            entries[0].visibility,
            Some("interne".to_string()),
            "le mode audit rend la visibilité au lieu de la taire"
        );
    }

    /// F-256 — le filtre interne s'applique EN PLUS, pas à la place : une carte interne ET
    /// publiable-par-kind reste exclue ; le décompte d'une carte publique voisine est intact.
    #[test]
    fn interne_filter_is_additive_to_kind_and_dropped() {
        let notes = vec![
            note(
                "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.2.0]] [[visibilite:interne]]",
                "F-60 interne",
            ),
            note(
                "[[feature:F-61]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.2.0]]",
                "F-61 public",
            ),
        ];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(
            entries
                .iter()
                .map(|e| e.feature.as_str())
                .collect::<Vec<_>>(),
            vec!["F-61"],
            "seule la carte non-interne survit : {entries:?}"
        );
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

    // ── Export DÉRIVÉ (make-before-break, F-184 Phase 6) ──────────────────────────────

    /// The known-blockers shape: a `DONE` feature card tracking an `OBSOLETE` internal
    /// ROADMAP without porteuse must **derive** `released` (rule 1) — no divergence with the
    /// stored `released`, and no diagnostic.
    #[test]
    fn derived_done_card_under_obsolete_roadmap_is_released() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:OBSOLETE]] [[kind:ROADMAP]] \
                 [[version:gradatum/0.7.0]] [[visibilite:interne]]",
                "ROADMAP 0.7.0",
            ),
            note(
                "[[feature:F-40]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.7.0]] [[track:gradatum/0.7.0]]",
                "F-40 livrée",
            ),
        ];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].feature, "F-40");
        assert_eq!(out.entries[0].release, "released");
        assert!(
            out.diagnostics.is_empty(),
            "aucune dérivation en échec attendue : {:?}",
            out.diagnostics
        );
    }

    /// F-256 — la voie dérivée exclut aussi une carte de travail `[[visibilite:interne]]` du
    /// miroir par défaut, et le mode audit `include_dropped` la réintègre.
    #[test]
    fn derived_interne_work_card_excluded_by_default_and_included_in_audit() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] \
                 [[version:gradatum/2.1.0]] [[visibilite:public]]",
                "ROADMAP 2.1.0",
            ),
            note(
                "[[feature:F-40]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[track:gradatum/2.1.0]] [[visibilite:interne]]",
                "F-40 interne",
            ),
        ];
        // Défaut : la carte interne est exclue (le miroir ne la voit pas).
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert!(
            out.entries.is_empty(),
            "carte de travail interne exclue du miroir dérivé : {:?}",
            out.entries
        );
        // Mode audit : la carte interne réapparaît, release dérivée = released (carte DONE).
        let audit = project_map_feature_entries_derived_scoped(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
            None,
        );
        assert_eq!(audit.entries.len(), 1, "carte interne réintégrée en audit");
        assert_eq!(audit.entries[0].feature, "F-40");
        assert_eq!(audit.entries[0].release, "released");
    }

    /// A non-terminal card tracking a BACKLOG derives the `roadmap` bucket (backlog ≡ roadmap:
    /// `ReleaseKind` has no `Backlog` variant), and is kept in the mirror export.
    #[test]
    fn derived_backlog_card_maps_to_roadmap_bucket() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:OPEN]] [[kind:BACKLOG]] [[version:gradatum/backlog]]",
                "BACKLOG",
            ),
            note(
                "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:roadmap]] [[version:gradatum/backlog]] [[track:gradatum/backlog]]",
                "F-50 backlog",
            ),
        ];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].release, "roadmap");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    /// A card with no `[[track:]]` (additive window) falls back to its stored release and records a
    /// `NoTrack` diagnostic — visible, never silent.
    #[test]
    fn derived_card_without_track_falls_back_to_stored_with_diagnostic() {
        let notes = vec![note(
            "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]]",
            "F-60 sans track",
        )];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].release, "planned", "repli sur le stocké");
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].feature, "F-60");
        assert_eq!(out.diagnostics[0].stored, Some("planned".to_string()));
        assert_eq!(out.diagnostics[0].reason, DerivationFallbackReason::NoTrack);
    }

    /// A card tracking a dangling structure falls back to stored and records an `Unresolved`
    /// diagnostic carrying the typed [`TrackResolutionError`].
    #[test]
    fn derived_card_with_dangling_track_records_unresolved() {
        let notes = vec![note(
            "[[feature:F-61]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.2.0]] [[track:gradatum/9.9.9]]",
            "F-61 track cassé",
        )];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].release, "planned", "repli sur le stocké");
        assert_eq!(out.diagnostics.len(), 1);
        assert!(matches!(
            out.diagnostics[0].reason,
            DerivationFallbackReason::Unresolved(TrackResolutionError::TargetNotFound { .. })
        ));
    }

    /// A body whose code fence is opened and never closed swallows every role below the opening
    /// line: the extractor stays inside the fence to end-of-body, so those roles are counted by
    /// **neither** the read projection nor the write validator. The card then looks like a card
    /// that never carried the role — most often it reports `NoTrack`, which sends a reader after a
    /// dangling track pointer that does not exist. The diagnostic must name the real cause.
    #[test]
    fn unterminated_code_fence_is_surfaced_as_a_diagnostic() {
        let body = "[[feature:F-70]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[release:planned]] [[version:gradatum/2.2.0]]\n\
                    ```\n[[track:gradatum/2.4.0]]";
        let notes = vec![note(body, "F-70 marque non fermée")];

        // Non-comptage, asserted on the extractor itself: the role below the opening mark is
        // absent from the target list — read/write parity is what makes this invisible.
        let targets = extract_structural_targets(body);
        assert!(
            !targets.iter().any(|t| t.starts_with("track:")),
            "le rôle sous la marque ouverte ne doit pas être compté : {targets:?}"
        );

        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);

        assert_eq!(
            out.entries.len(),
            1,
            "la carte reste projetée via le stocké"
        );
        assert_eq!(
            out.entries[0].version,
            Some("v2.2.0".to_string()),
            "la version reste celle du stocké : le rôle avalé n'a rien piloté"
        );
        assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
        assert_eq!(out.diagnostics[0].feature, "F-70");
        assert_eq!(
            out.diagnostics[0].reason,
            DerivationFallbackReason::UnterminatedCodeFence,
            "le motif doit nommer la marque non fermée, pas un track absent"
        );
    }

    /// Non-regression, the other side of the same guard: a fence that **is** closed is the
    /// documented way to cite a role without counting it, so it must keep the plain reason and
    /// raise no unterminated-fence diagnostic. Over-triggering here would make the new reason
    /// unreadable on the corpus that uses fences correctly.
    #[test]
    fn closed_code_fence_keeps_the_plain_reason() {
        let body = "[[feature:F-71]] [[project:gradatum]] [[kind:FEATURE]]\n\
                    ```\n[[track:gradatum/2.4.0]]\n```";
        let notes = vec![note(body, "F-71 marque fermée")];

        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);

        assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
        assert_eq!(
            out.diagnostics[0].reason,
            DerivationFallbackReason::NoTrack,
            "une marque correctement fermée n'est pas une marque non fermée"
        );
        assert!(
            out.entries.is_empty(),
            "sans track ni stocké, aucune valeur inventée : {entries:?}",
            entries = out.entries
        );
    }

    /// The mirror-site `dropped` filter is applied to the **derived** value: a card that derives
    /// `dropped` (OBSOLETE card) is excluded by default even if its stored release was not.
    #[test]
    fn derived_dropped_is_filtered_on_the_derived_value() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] \
                 [[version:gradatum/2.2.0]] [[visibilite:public]]",
                "ROADMAP 2.2.0",
            ),
            note(
                // Stored `planned`, but the card is OBSOLETE → derives `dropped` (rule 2).
                "[[feature:F-70]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/2.2.0]] [[track:gradatum/2.2.0]]",
                "F-70 abandonnée",
            ),
        ];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert!(
            out.entries.is_empty(),
            "la carte dérivant `dropped` est exclue du miroir : {:?}",
            out.entries
        );
    }

    // ── Post-retrait (F-184 Phase 7) : dérivation SANS release/version stockés ─────────

    /// Régression F-184 (le cœur) : une carte-feature portant **uniquement** un `[[track:]]`
    /// (ni `[[release:]]` ni `[[version:]]`) — la forme post-retrait — doit projeter 1 entrée dont
    /// `release` ET `version` sont dérivés de la structure pointée. AVANT le correctif, l'absence de
    /// `[[release:]]` faisait `continue` (0 entrée) ; APRÈS, la dérivation reprojette la carte.
    #[test]
    fn derived_card_without_stored_release_or_version_projects_from_track() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] \
                 [[version:gradatum/2.1.0]] [[visibilite:public]]",
                "ROADMAP 2.1.0",
            ),
            // Forme post-retrait : pas de [[release:]] ni [[version:]], seulement [[track:]].
            note(
                "[[feature:F-80]] [[project:gradatum]] [[status:IN_PROGRESS]] [[kind:FEATURE]] \
                 [[track:gradatum/2.1.0]]",
                "F-80 sur 2.1.0",
            ),
        ];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(
            out.entries.len(),
            1,
            "la carte doit être reprojetée : {out:?}"
        );
        assert_eq!(out.entries[0].feature, "F-80");
        // release dérivé : carte non-terminale traçant une ROADMAP DONE → released (règle 3).
        assert_eq!(out.entries[0].release, "released");
        // version dérivée de l'identité de la structure pointée (= son [[version:]]).
        assert_eq!(out.entries[0].version, Some("v2.1.0".to_string()));
        assert!(
            out.diagnostics.is_empty(),
            "dérivation réussie sans stocké : aucun diagnostic attendu : {:?}",
            out.diagnostics
        );
    }

    /// Post-retrait, la version backlog se dérive aussi du `[[track:]]` : une carte traçant le
    /// BACKLOG (sans `[[version:]]` stocké) exporte le sentinel `"vX.Y.Z"`.
    #[test]
    fn derived_backlog_version_from_track_without_stored() {
        let notes = vec![
            note(
                "[[project:gradatum]] [[status:OPEN]] [[kind:BACKLOG]] [[version:gradatum/backlog]]",
                "BACKLOG",
            ),
            note(
                "[[feature:F-81]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[track:gradatum/backlog]]",
                "F-81 backlog",
            ),
        ];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(
            out.entries[0].release, "roadmap",
            "backlog ≡ roadmap bucket"
        );
        assert_eq!(out.entries[0].version, Some("vX.Y.Z".to_string()));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    /// Ni dérivable (aucun `[[track:]]`) ni stocké (aucun `[[release:]]`) → carte **ignorée** avec un
    /// diagnostic `stored: None` visible. Chemin défensif : ne doit pas arriver sur un corpus sain
    /// post-retrait (toutes les cartes portent un `[[track:]]` résoluble).
    #[test]
    fn derived_card_without_track_and_without_stored_is_skipped_with_diagnostic() {
        let notes = vec![note(
            "[[feature:F-82]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]]",
            "F-82 orpheline",
        )];
        let out =
            project_map_feature_entries_derived_scoped(&notes, ExportOptions::default(), None);
        assert!(
            out.entries.is_empty(),
            "carte ni dérivable ni stockée : ignorée : {:?}",
            out.entries
        );
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].feature, "F-82");
        assert_eq!(
            out.diagnostics[0].stored, None,
            "aucun release stocké → stored:None (skip, pas repli)"
        );
        assert_eq!(out.diagnostics[0].reason, DerivationFallbackReason::NoTrack);
    }
}

#[cfg(test)]
mod card_index_tests {
    //! F-211 (supersedes F-253) — projection de listage à axes nommés + filtre par version.
    use super::*;

    /// Construit un triplet `(id, body, title)` — le corpus consommé par [`project_map_card_index`].
    fn card(id: &str, body: &str, title: &str) -> (String, String, String) {
        (id.to_string(), body.to_string(), title.to_string())
    }

    /// La ROADMAP de la version 2.1.0 (carte de structure), DONE.
    fn roadmap_210(id: &str) -> (String, String, String) {
        card(
            id,
            "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] \
             [[version:gradatum/2.1.0]] [[visibilite:public]]",
            "ROADMAP 2.1.0",
        )
    }

    /// Corpus de référence : ROADMAP 2.1.0 + 2 cartes de travail sur 2.1.0 + 1 carte sur 2.2.0.
    fn corpus() -> Vec<(String, String, String)> {
        vec![
            roadmap_210("01ROADMAP210AAAAAAAAAAAAAA"),
            card(
                "01WORK00000000000000000267",
                "[[feature:F-267]] [[project:gradatum]] [[status:DONE]] [[kind:FIX]] \
                 [[track:gradatum/2.1.0]] [[supersedes:F-149]]",
                "F-267 correctif FTS5",
            ),
            card(
                "01WORK00000000000000000244",
                "[[feature:F-244]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[track:gradatum/2.1.0]]",
                "F-244 feature 2.1.0",
            ),
            card(
                "01WORK00000000000000000211",
                "[[feature:F-211]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[track:gradatum/2.2.0]] [[visibilite:interne]]",
                "F-211 listage par version",
            ),
        ]
    }

    /// Critère 2/5/7 — filtrer une version rend EXACTEMENT ses cartes : les 2 cartes de travail
    /// rattachées + la carte de structure de la version (jamais plus, jamais moins).
    #[test]
    fn version_filter_returns_exactly_its_cards_incl_structure() {
        let notes = corpus();
        let out = project_map_card_index(&notes, Some("2.1.0"), Some("gradatum"));
        let ids: Vec<&str> = out
            .iter()
            .map(|e| e.feature.as_deref().unwrap_or("STRUCT"))
            .collect();
        assert_eq!(
            ids,
            vec!["STRUCT", "F-244", "F-267"],
            "2.1.0 = ROADMAP (structure, en tête) + F-244 + F-267, triés : {out:?}"
        );
    }

    /// Critère 1/3/8 — chaque axe est une VALEUR nommée, pas un chemin ; le kind (dont l'absence a
    /// produit le faux décompte du 2026-09-02) et le release dérivé sont exposés.
    #[test]
    fn axes_are_named_values_not_paths() {
        let notes = corpus();
        let out = project_map_card_index(&notes, Some("2.1.0"), Some("gradatum"));
        let fix = out
            .iter()
            .find(|e| e.feature.as_deref() == Some("F-267"))
            .expect("F-267 présent dans le listage 2.1.0");
        assert_eq!(fix.status.as_deref(), Some("DONE"));
        assert_eq!(fix.kind.as_deref(), Some("FIX"));
        // DONE ⇒ release dérivé = released (derive_release règle 1), jamais lu d'un stocké.
        assert_eq!(fix.release.as_deref(), Some("released"));
        assert_eq!(fix.version.as_deref(), Some("v2.1.0"));
        assert_eq!(fix.track.as_deref(), Some("gradatum/2.1.0"));
        assert_eq!(fix.supersedes, vec!["F-149".to_string()]);
        assert_eq!(fix.id, "01WORK00000000000000000267");
    }

    /// La carte de structure rend `release: None` (pas d'axe release) et sa propre version.
    #[test]
    fn structure_card_has_no_release_and_its_own_version() {
        let notes = corpus();
        let out = project_map_card_index(&notes, Some("2.1.0"), Some("gradatum"));
        let structure = out
            .iter()
            .find(|e| e.feature.is_none())
            .expect("la ROADMAP de structure est listée");
        assert_eq!(structure.kind.as_deref(), Some("ROADMAP"));
        assert_eq!(
            structure.release, None,
            "une structure n'a pas d'axe release"
        );
        assert_eq!(structure.version.as_deref(), Some("v2.1.0"));
    }

    /// Critère 2 — une version inexistante rend une liste VIDE (jamais une erreur, jamais tout).
    #[test]
    fn unknown_version_returns_empty() {
        let notes = corpus();
        let out = project_map_card_index(&notes, Some("9.9.9"), Some("gradatum"));
        assert!(out.is_empty(), "version inexistante → vide : {out:?}");
    }

    /// `v2.1.0` et `2.1.0` sont équivalents (normalisation du préfixe website).
    #[test]
    fn version_query_accepts_v_prefix() {
        let notes = corpus();
        let with_v = project_map_card_index(&notes, Some("v2.1.0"), Some("gradatum"));
        let without_v = project_map_card_index(&notes, Some("2.1.0"), Some("gradatum"));
        assert_eq!(with_v, without_v, "v2.1.0 ≡ 2.1.0");
        assert_eq!(with_v.len(), 3);
    }

    /// Sans filtre, TOUTES les cartes sont rendues, y compris `visibilite:interne` et les cartes
    /// non-FEATURE — le listage n'applique AUCUN filtre miroir-site (criteria 7, 8).
    #[test]
    fn no_filter_lists_all_incl_internal_and_non_feature() {
        let notes = corpus();
        let out = project_map_card_index(&notes, None, Some("gradatum"));
        assert_eq!(
            out.len(),
            4,
            "les 4 cartes du corpus sont listées : {out:?}"
        );
        let interne = out
            .iter()
            .find(|e| e.feature.as_deref() == Some("F-211"))
            .expect("F-211 (visibilite:interne) présente");
        assert_eq!(
            interne.visibility.as_deref(),
            Some("interne"),
            "la carte interne est listée avec son axe visibilite exposé, jamais filtrée en silence"
        );
    }

    /// Critère 2 — une carte de travail dont le `[[track:]]` ne résout pas est LISTÉE avec
    /// `release: None` (le null est le signal visible de l'anomalie, jamais un drop silencieux).
    #[test]
    fn work_card_with_dangling_track_is_listed_with_null_release() {
        let notes = vec![card(
            "01WORK00000000000000000300",
            "[[feature:F-300]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[track:gradatum/does-not-exist]]",
            "F-300 track pendant",
        )];
        let out = project_map_card_index(&notes, None, Some("gradatum"));
        assert_eq!(out.len(), 1, "la carte est listée malgré le track pendant");
        assert_eq!(
            out[0].release, None,
            "track irrésolu ⇒ release:null visible, jamais omission silencieuse"
        );
        assert_eq!(out[0].version.as_deref(), Some("vdoes-not-exist"));
    }

    /// Le filtre projet exclut les cartes d'un autre projet même si elles rattachent la version.
    #[test]
    fn project_filter_excludes_foreign_project() {
        let notes = vec![
            card(
                "01WORK00000000000000000401",
                "[[feature:F-401]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[track:gradatum/2.1.0]]",
                "gradatum 2.1.0",
            ),
            card(
                "01WORK00000000000000000402",
                "[[feature:F-402]] [[project:system]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[track:system/2.1.0]]",
                "system 2.1.0",
            ),
        ];
        let out = project_map_card_index(&notes, Some("2.1.0"), Some("gradatum"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].feature.as_deref(), Some("F-401"));
    }
}

#[cfg(test)]
mod code_prose_exclusion_f296_tests {
    //! F-296 reliquat 2 — the read projections read the card's **structural zone** only, exactly
    //! like the write-path validator. A role merely *cited* in prose or inside a code fence must
    //! never feed a projection (defect ANO-DEFER-SILENT, A6 sweep 2026-09-14).
    use super::*;

    fn card(id: &str, body: &str, title: &str) -> (String, String, String) {
        (id.to_string(), body.to_string(), title.to_string())
    }

    /// A structure card whose structural zone carries `[[version:gradatum/2.4.0]]` and whose prose
    /// history line *cites* `` `[[version:gradatum/9.9.9]]` `` between code marks: the projection
    /// keeps 2.4.0 only. `[[version:]]` is last-wins in the card loop, so a whole-body scan would
    /// let the prose 9.9.9 override the structural version — the exact defect.
    #[test]
    fn history_line_citing_a_role_in_code_is_not_counted() {
        let notes = vec![card(
            "01STRUCT0000000000000024AA",
            "[[project:gradatum]] [[kind:ROADMAP]] [[version:gradatum/2.4.0]]\n\
             \n\
             History: this roadmap documents the version syntax `[[version:gradatum/9.9.9]]` \
             and the status syntax `[[status:DONE]]` as inline prose examples.",
            "ROADMAP 2.4.0",
        )];

        let out = project_map_card_index(&notes, None, Some("gradatum"));
        assert_eq!(
            out.len(),
            1,
            "one card in, one entry out, never two: {out:?}"
        );
        assert_eq!(
            out[0].version.as_deref(),
            Some("v2.4.0"),
            "the prose-cited 9.9.9 must not override the structural version"
        );

        // The 9.9.9 cited in prose must not attach the card to that version filter.
        let filtered_999 = project_map_card_index(&notes, Some("9.9.9"), Some("gradatum"));
        assert!(
            filtered_999.is_empty(),
            "a version cited in prose must not make the card match that version: {filtered_999:?}"
        );
    }

    /// The same body: `roles_of_body` reports the structural-zone kind, never the `[[status:DONE]]`
    /// cited in prose (the structural zone carries no status).
    #[test]
    fn roles_of_body_reads_the_structural_zone_only() {
        let body = "[[project:gradatum]] [[kind:ROADMAP]] [[version:gradatum/2.4.0]]\n\
             \n\
             History: this roadmap documents the version syntax `[[version:gradatum/9.9.9]]` \
             and the status syntax `[[status:DONE]]` as inline prose examples.";

        let roles = roles_of_body(body);
        assert_eq!(roles.kind, Some("ROADMAP"), "structural kind is kept");
        assert_eq!(
            roles.status, None,
            "a status cited in prose is not a role of the card"
        );
    }

    /// Parity guard: for a body **without** any prose/code citation, the seven migrated read sites
    /// return exactly what they returned under the whole-body scan (literal values). Structural
    /// extraction is a strict subset — it only ever removes prose/code citations.
    #[test]
    fn clean_body_projections_are_unchanged() {
        let structure_body = "[[project:gradatum]] [[kind:ROADMAP]] [[status:DONE]] \
             [[version:gradatum/2.4.0]] [[visibilite:public]]";
        let work_body = "[[feature:F-42]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/2.4.0]] [[track:gradatum/2.4.0]] \
             [[supersedes:F-7]]";

        // Site 1 — StructureIndex::from_bodies (via from_notes) indexes the structure card.
        let bt: Vec<(String, String)> = vec![
            (structure_body.to_string(), "ROADMAP 2.4.0".to_string()),
            (work_body.to_string(), "F-42 fix".to_string()),
        ];
        let index = StructureIndex::from_notes(&bt);
        assert!(
            index.resolve_track("gradatum", "2.4.0").is_ok(),
            "structure card 2.4.0 is indexed"
        );

        // Site 2 — derivable_track_target on a single-version, track-less work card.
        let derivable_body = "[[feature:F-42]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] \
             [[version:gradatum/2.4.0]]";
        assert_eq!(
            derivable_track_target(derivable_body),
            Some("gradatum/2.4.0".to_string())
        );

        // Site 3 — max_feature_number over feature/supersedes/parent on a structural line.
        assert_eq!(max_feature_number(work_body), Some(42));

        // Site 4 — project_map_feature_entries_scoped.
        let scoped =
            project_map_feature_entries_scoped(&bt, ExportOptions::default(), Some("gradatum"));
        assert_eq!(scoped.len(), 1, "only the work card projects: {scoped:?}");
        assert_eq!(scoped[0].feature, "F-42");
        assert_eq!(scoped[0].release, "released");
        assert_eq!(scoped[0].version.as_deref(), Some("v2.4.0"));

        // Site 5 — project_map_feature_entries_derived_scoped (no diagnostic on a clean corpus).
        let derived = project_map_feature_entries_derived_scoped(
            &bt,
            ExportOptions::default(),
            Some("gradatum"),
        );
        assert_eq!(derived.entries.len(), 1);
        assert_eq!(derived.entries[0].release, "released");
        assert!(
            derived.diagnostics.is_empty(),
            "clean corpus derives cleanly: {:?}",
            derived.diagnostics
        );

        // Site 6 — project_map_card_index.
        let ibt = vec![
            (
                "01STRUCTCLEAN0000000024AA".to_string(),
                structure_body.to_string(),
                "ROADMAP 2.4.0".to_string(),
            ),
            (
                "01WORKCLEAN000000000042AA".to_string(),
                work_body.to_string(),
                "F-42 fix".to_string(),
            ),
        ];
        let listed = project_map_card_index(&ibt, None, Some("gradatum"));
        assert_eq!(listed.len(), 2, "structure + work: {listed:?}");
        let fix = listed
            .iter()
            .find(|e| e.feature.as_deref() == Some("F-42"))
            .expect("F-42 listed");
        assert_eq!(fix.kind.as_deref(), Some("FEATURE"));
        assert_eq!(fix.status.as_deref(), Some("DONE"));
        assert_eq!(fix.release.as_deref(), Some("released"));
        assert_eq!(fix.version.as_deref(), Some("v2.4.0"));

        // Site 7 — roles_of_body.
        let roles = roles_of_body(work_body);
        assert_eq!(roles.kind, Some("FEATURE"));
        assert_eq!(roles.status, Some("DONE"));
    }

    /// Step 3b — the derivation diagnostics are emitted in a **deterministic** total order,
    /// independent of card (input) order. Two cards with **distinct** `F-XX`, both failing to
    /// derive (no `[[track:]]`), sort **numerically**: `F-90` before `F-100`. Lexicographically
    /// `"F-100" < "F-90"`, so a plain `String` sort would reverse them — and contradict the
    /// numerically-sorted `entries`. (`F-90`/`F-100` are both well-formed 2-/3-digit idents;
    /// a 1-digit `F-9` is rejected by `parse_link`.)
    #[test]
    fn diagnostics_are_sorted_deterministically_across_distinct_features() {
        let card_90 = (
            "[[feature:F-90]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.4.0]]"
                .to_string(),
            "F-90".to_string(),
        );
        let card_100 = (
            "[[feature:F-100]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.4.0]]"
                .to_string(),
            "F-100".to_string(),
        );

        let forward = project_map_feature_entries_derived_scoped(
            &[card_90.clone(), card_100.clone()],
            ExportOptions::default(),
            None,
        );
        let reverse = project_map_feature_entries_derived_scoped(
            &[card_100, card_90],
            ExportOptions::default(),
            None,
        );

        let features: Vec<&str> = forward
            .diagnostics
            .iter()
            .map(|d| d.feature.as_str())
            .collect();
        assert_eq!(
            features,
            vec!["F-90", "F-100"],
            "F-90 sorts numerically before F-100"
        );
        assert_eq!(
            forward.diagnostics, reverse.diagnostics,
            "diagnostic order is independent of input order"
        );
    }

    /// Step 3b (collision) — two cards carrying the **same** `F-XX`, both failing to derive, with
    /// **different** `stored` releases: `feature` is not a total key, so the tie must break on the
    /// remaining axes for the order to be deterministic across input order.
    #[test]
    fn diagnostics_are_sorted_deterministically_on_feature_collision() {
        let planned = (
            "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/2.4.0]]"
                .to_string(),
            "F-60 a".to_string(),
        );
        let released = (
            "[[feature:F-60]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/2.4.0]]"
                .to_string(),
            "F-60 b".to_string(),
        );

        let forward = project_map_feature_entries_derived_scoped(
            &[planned.clone(), released.clone()],
            ExportOptions::default(),
            None,
        );
        let reverse = project_map_feature_entries_derived_scoped(
            &[released, planned],
            ExportOptions::default(),
            None,
        );

        assert_eq!(forward.diagnostics.len(), 2, "two colliding diagnostics");
        let stored: Vec<Option<&str>> = forward
            .diagnostics
            .iter()
            .map(|d| d.stored.as_deref())
            .collect();
        assert_eq!(
            stored,
            vec![Some("planned"), Some("released")],
            "same F-60, tie broken on stored: planned before released"
        );
        assert_eq!(
            forward.diagnostics, reverse.diagnostics,
            "collision order is independent of input order"
        );
    }

    /// The fence predicate, exercised on its own: it is what decides whether a `NoTrack` fallback
    /// is reclassified, so it must be right in both directions. An unclosed fence is the defect;
    /// a closed one is the documented way to cite a role.
    #[test]
    fn unterminated_code_fence_predicate_discriminates() {
        assert!(
            !has_unterminated_code_fence("[[feature:F-50]] [[track:gradatum/2.4.0]]"),
            "aucune marque ouverte"
        );
        assert!(
            !has_unterminated_code_fence("[[feature:F-50]]\n```\n[[track:gradatum/9.9.9]]\n```"),
            "marque ouverte puis fermée"
        );
        assert!(
            has_unterminated_code_fence("[[feature:F-50]]\n```\n[[track:gradatum/2.4.0]]"),
            "marque ouverte jamais fermée"
        );
        assert!(
            !has_unterminated_code_fence("[[feature:F-50]]\n```\n```\n```\n```"),
            "deux paires fermées : l'état revient à zéro"
        );
    }

    /// The scan separates the two sets. A closed fence holds roles that are legitimately not
    /// counted and must **not** be reported as swallowed; an unclosed one holds roles that are
    /// counted nowhere, and those are the ones the diagnostic must name.
    #[test]
    fn scan_separates_counted_roles_from_swallowed_ones() {
        let closed = scan_structural_targets(
            "[[feature:F-50]] [[project:gradatum]]\n```\n[[track:gradatum/9.9.9]]\n```",
        );
        assert_eq!(
            closed.targets.len(),
            2,
            "les deux rôles structurels comptés"
        );
        assert!(
            closed.swallowed.is_empty(),
            "une marque fermée n'avale rien : {:?}",
            closed.swallowed
        );

        let unclosed = scan_structural_targets(
            "[[feature:F-50]] [[project:gradatum]]\n```\n[[track:gradatum/2.4.0]]",
        );
        assert_eq!(unclosed.targets.len(), 2, "mêmes rôles comptés");
        assert_eq!(
            unclosed.swallowed,
            vec!["track:gradatum/2.4.0".to_string()],
            "le rôle sous la marque ouverte est nommé, pas seulement absent"
        );
    }
}

#[cfg(test)]
mod max_feature_number_tests {
    use super::*;

    /// The highest `[[feature:F-XX]]` role in the body wins, 2- and 3-digit forms mixed. The roles
    /// sit on the card's structural line (F-296: a role cited off the structural zone is not read).
    #[test]
    fn returns_max_over_feature_roles() {
        let body = "[[feature:F-37]] [[project:gradatum]] [[feature:F-134]]";
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
        // (1 digit) and rejected by parse_link → ignored. Only F-50 counts. The prose noise
        // sits on its own line so the structural line carries only the two wikilinks (F-296).
        let body = "f-999 tag noise\n[[feature:F-1]] [[feature:F-50]]";
        assert_eq!(max_feature_number(body), Some(50));
    }

    /// Empty body → `None`.
    #[test]
    fn empty_body_returns_none() {
        assert_eq!(max_feature_number(""), None);
    }

    /// The allocation floor, pinned by two **separate** assertions.
    ///
    /// The seed card carries `[[feature:F-1]]` — a single-digit ident the parser rejects, so it
    /// must not contribute a floor of `1`. The two facts are asserted apart on purpose: asserting
    /// only `Some(50)` would also hold on a body where the seed leaked but was out-maxed by a
    /// larger role, and asserting only "not 1" would hold on an empty result. Together they pin
    /// both the exclusion and the value actually returned.
    #[test]
    fn max_feature_number_excludes_seed_and_returns_true_max() {
        let body = "[[feature:F-1]] [[feature:F-50]]";

        let got = max_feature_number(body);

        assert_ne!(
            got,
            Some(1),
            "la graine F-1 (1 chiffre) est rejetée : elle ne fixe pas le plancher"
        );
        assert_eq!(got, Some(50), "le vrai maximum de la carte");
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
    fn feature_card_zero_release_is_now_valid() {
        // F-184 Phase 7 : release devient optionnel (dérivé du track côté serveur).
        // Une carte-feature sans [[release:]] est désormais valide (relaxation minimale).
        // Elle conserve une [[version:]] : le plancher de dérivabilité exige un ancrage
        // {version, track} — c'est la version qui isole ici la relaxation « release absent ».
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
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
    fn release_without_feature_is_now_allowed() {
        // F-184 Phase 7 : la règle carte-de-travail est uniforme (feature/release/version
        // au plus 1). Un [[release:]] sur une carte de travail sans [[feature:]] n'est plus
        // interdit (relaxation minimale ; l'interdiction stricte serait une décision séparée).
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
        ];
        assert_eq!(validate_links(&links), Ok(()));
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

    // ── F-184 Phase 7 : [[version:]] devient OPTIONNEL sur carte de travail ──────

    #[test]
    fn feature_card_without_version_is_now_valid() {
        // F-184 Phase 7 : version devient optionnelle DÈS QU'un [[track:]] ancre la carte
        // (release dérivé côté serveur). Une carte-feature sans [[version:]] mais avec track
        // est valide — c'est ce qui débloque le retrait Phase 7. La track satisfait le
        // plancher de dérivabilité (sans elle, la carte serait un orphelin rejeté).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Track {
                project: "gradatum".to_string(),
                target: "backlog".to_string(),
            },
            // Pas de Version — toléré depuis Phase 7 (au plus 1), ancrage porté par le track.
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn work_card_with_track_no_version_no_release_is_valid() {
        // Cœur du déblocage Phase 7 : l'ÉTAT CIBLE après retrait (étape 3) est une carte de
        // travail dont version ET release ont été retirés du corps, MAIS qui porte un
        // [[track:]] — c'est lui qui ancre la carte (le serveur en dérive version/release).
        // (Ancien nom trompeur `work_card_stripped_of_version_and_release_is_valid` : il
        // canonisait un orphelin sans track, ce que le plancher de dérivabilité rejette.)
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Track {
                project: "gradatum".to_string(),
                target: "2.2.0".to_string(),
            },
            // Ni version ni release — l'état cible après retrait Phase 7, ancré par le track.
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_with_version_and_track_is_valid() {
        // Le plancher accepte « feature + version » quel que soit le track : ici les deux
        // ancrages coexistent (état de transition avant retrait de la version).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "2.2.0".to_string(),
            },
            ProjectMapLink::Track {
                project: "gradatum".to_string(),
                target: "2.2.0".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_without_version_or_track_is_rejected() {
        // Plancher de dérivabilité (P1 audit) : une carte feature-porteuse sans AUCUN
        // ancrage {version, track} est un orphelin, invisible à tout export → rejet.
        // Un [[release:]] seul n'ancre rien (il est dérivé du track côté serveur), donc on
        // en met un ici pour prouver qu'il ne suffit pas à satisfaire le plancher.
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            // Ni version ni track — orphelin.
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::WorkCardNotDerivable)
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

#[cfg(test)]
mod track_phase2_tests {
    //! Le rôle `track`, cartes de structure, visibilite/porteuse, filtre export.
    //!
    //! Preuves PURES (sans registre) : cardinalité, acceptations/refus de schéma, nœud
    //! réservé, dérivation d'injection, filtre projet d'export. Les preuves adossées au
    //! registre (existence/kind de la cible, RESTRICT enfants) vivent dans le test
    //! d'intégration `crates/gradatum-server/tests/project_map_track_phase2_e2e.rs`.

    use super::*;

    /// Construit un `Vec<String>` de cibles à partir de `&str` (déjà sans `[[ ]]`).
    fn targets(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // ── Preuve #2 — une carte sans le rôle track passe toujours ─────────────────
    #[test]
    fn work_card_without_track_passes() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/backlog",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "une carte de travail sans [[track:]] doit passer (fenêtre additive)"
        );
    }

    // ── Preuve (part #1) — une carte de travail portant UN track passe le schéma ─
    #[test]
    fn work_card_with_one_track_passes_schema() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "track:gradatum/2.2.0",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "un track unique sur une carte de travail est accepté au schéma (existence \
             vérifiée au write-path)"
        );
    }

    // ── Preuve #3 — deux [[track:]] refusés ─────────────────────────────────────
    #[test]
    fn two_tracks_rejected() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "track:gradatum/2.2.0",
            "track:gradatum/2.1.0",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::TrackCardinality(2)),
            "deux [[track:]] sur une carte de travail doivent être refusés"
        );
    }

    // ── Preuve #7 — une ROADMAP sans visibilite est refusée ─────────────────────
    #[test]
    fn roadmap_without_visibilite_rejected() {
        let t = targets(&[
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "version:gradatum/2.2.0",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::VisibilityCardinality(0)),
            "une ROADMAP sans [[visibilite:]] doit être refusée (jamais public par défaut)"
        );
    }

    // ── Preuve #9 — une carte de structure valide s'écrit ───────────────────────
    #[test]
    fn valid_roadmap_structure_card_accepted() {
        let t = targets(&[
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "version:gradatum/2.2.0",
            "visibilite:public",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "une ROADMAP (project+status+kind+version+visibilite) doit être acceptée"
        );
    }

    #[test]
    fn valid_backlog_structure_card_accepted() {
        // Un BACKLOG ne porte PAS de visibilite (jamais publié), mais reste adressable
        // par une version sentinelle.
        let t = targets(&[
            "project:gradatum",
            "status:OPEN",
            "kind:BACKLOG",
            "version:gradatum/backlog",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "un BACKLOG (project+status+kind+version, sans visibilite) doit être accepté"
        );
    }

    #[test]
    fn backlog_with_visibilite_rejected() {
        let t = targets(&[
            "project:gradatum",
            "status:OPEN",
            "kind:BACKLOG",
            "version:gradatum/backlog",
            "visibilite:interne",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::VisibilityForbidden(1)),
            "un BACKLOG ne porte jamais de visibilite"
        );
    }

    #[test]
    fn structure_card_without_version_rejected() {
        let t = targets(&[
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "visibilite:public",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::StructureVersionCardinality(0)),
            "une carte de structure non adressable (0 version) est refusée"
        );
    }

    #[test]
    fn structure_card_with_feature_rejected() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "version:gradatum/2.2.0",
            "visibilite:public",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::StructureFeatureForbidden(1)),
            "une carte de structure n'est jamais numérotée"
        );
    }

    #[test]
    fn track_on_structure_card_rejected() {
        let t = targets(&[
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "version:gradatum/2.2.0",
            "visibilite:public",
            "track:gradatum/2.1.0",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::TrackOnStructureCard(1)),
            "une carte de structure ne porte pas de track (pointée, ne pointe rien)"
        );
    }

    /// F-256 — une carte de travail PEUT porter `[[visibilite:interne]]` (axe d'exclusion dédié).
    #[test]
    fn visibilite_interne_on_work_card_accepted() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "visibilite:interne",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "F-256 : une carte de travail peut déclarer son internalité via visibilite:interne"
        );
    }

    /// F-256 — `[[visibilite:public]]` est aussi accepté sur une carte de travail (déclaration
    /// explicite de publiabilité, redondante avec le défaut mais valide).
    #[test]
    fn visibilite_public_on_work_card_accepted() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "visibilite:public",
        ]);
        assert!(
            validate_links_from_targets(&t).is_ok(),
            "F-256 : visibilite:public est un no-op déclaré, accepté sur une carte de travail"
        );
    }

    /// F-256 — au plus 1 : deux `[[visibilite:]]` sur une carte de travail sont refusés.
    #[test]
    fn two_visibilite_on_work_card_rejected() {
        let t = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "visibilite:interne",
            "visibilite:public",
        ]);
        assert_eq!(
            validate_links_from_targets(&t),
            Err(SchemaError::VisibilityWorkCardCardinality(2)),
            "F-256 : une carte de travail déclare son internalité au plus une fois"
        );
    }

    #[test]
    fn porteuse_on_roadmap_accepted_but_forbidden_on_work_card() {
        let roadmap = targets(&[
            "project:gradatum",
            "status:DONE",
            "kind:ROADMAP",
            "version:gradatum/2.0.0",
            "visibilite:interne",
            "porteuse:gradatum/2.1.0",
        ]);
        assert!(
            validate_links_from_targets(&roadmap).is_ok(),
            "une ROADMAP peut porter une porteuse"
        );
        let work = targets(&[
            "feature:F-50",
            "project:gradatum",
            "status:OPEN",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/2.2.0",
            "porteuse:gradatum/2.1.0",
        ]);
        assert_eq!(
            validate_links_from_targets(&work),
            Err(SchemaError::PorteuseForbidden(1)),
            "porteuse est un rôle de ROADMAP, refusé sur une carte de travail"
        );
    }

    // ── Parse : visibilite lowercase strict ─────────────────────────────────────
    #[test]
    fn visibilite_wire_is_lowercase_strict() {
        assert_eq!(
            parse_link("visibilite:public"),
            Ok(ProjectMapLink::Visibility(VisibilityKind::Public))
        );
        assert_eq!(
            parse_link("visibilite:PUBLIC"),
            Err(SchemaError::InvalidVisibility("PUBLIC".to_string())),
            "visibilite est lowercase strict (miroir ReleaseKind)"
        );
    }

    #[test]
    fn track_malformed_rejected() {
        assert_eq!(
            parse_link("track:gradatum"),
            Err(SchemaError::MalformedTrack("gradatum".to_string())),
            "un track sans / est malformé"
        );
    }

    // ── Preuve #10 — le nœud track est un nœud réservé navigable ────────────────
    #[test]
    fn track_is_a_reserved_navigable_node() {
        assert_eq!(
            reserved_node_target("track:gradatum/2.2.0"),
            Some("track:gradatum/2.2.0".to_string()),
            "track: doit être un nœud réservé (apparaît dans vault_graph)"
        );
        assert_eq!(
            reserved_node_target("visibilite:public"),
            Some("visibilite:public".to_string())
        );
        assert_eq!(
            reserved_node_target("porteuse:gradatum/2.2.0"),
            Some("porteuse:gradatum/2.2.0".to_string())
        );
    }

    // ── Déliv. 8 — dérivation du track injectable ───────────────────────────────
    #[test]
    fn derivable_track_from_work_card_version() {
        let body = "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[release:planned]] [[version:gradatum/2.2.0]]";
        assert_eq!(
            derivable_track_target(body),
            Some("gradatum/2.2.0".to_string()),
            "une carte de travail avec version et sans track dérive son track"
        );
    }

    #[test]
    fn derivable_track_none_when_track_present() {
        let body = "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                    [[release:planned]] [[version:gradatum/2.2.0]] [[track:gradatum/2.2.0]]";
        assert_eq!(
            derivable_track_target(body),
            None,
            "un track déjà présent est préservé, pas re-dérivé"
        );
    }

    #[test]
    fn derivable_track_none_for_structure_card() {
        let body = "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] \
                    [[version:gradatum/2.2.0]] [[visibilite:public]]";
        assert_eq!(
            derivable_track_target(body),
            None,
            "une carte de structure ne reçoit jamais de track injecté"
        );
    }

    // ── Preuve NÉGATIVE d'export — project:system ∧ track publique jamais exporté ─
    #[test]
    fn system_feature_card_never_in_project_scoped_export() {
        // Une carte kind:FEATURE ∧ project:system ∧ track vers une roadmap publique :
        // structurellement invisible dès qu'un filtre projet gradatum est appliqué.
        let notes = vec![
            (
                "[[feature:F-99]] [[project:system]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:system/2.2.0]] [[track:gradatum/2.2.0]]"
                    .to_string(),
                "Carte system infiltrée".to_string(),
            ),
            (
                "[[feature:F-50]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/2.2.0]]"
                    .to_string(),
                "Vraie carte gradatum".to_string(),
            ),
        ];
        let entries =
            project_map_feature_entries_scoped(&notes, ExportOptions::default(), Some("gradatum"));
        assert!(
            entries.iter().all(|e| e.feature != "F-99"),
            "la carte project:system ne doit JAMAIS apparaître dans l'export scopé gradatum : {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.feature == "F-50"),
            "la vraie carte gradatum doit rester présente"
        );
    }

    #[test]
    fn structure_card_never_exported() {
        // C10 : une ROADMAP ne rejoint jamais l'export (même en mode audit include_dropped).
        let notes = vec![(
            "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] \
             [[version:gradatum/2.2.0]] [[visibilite:public]]"
                .to_string(),
            "ROADMAP 2.2.0".to_string(),
        )];
        let audit = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        assert!(
            audit.is_empty(),
            "une carte de structure ne rejoint jamais l'export, même en mode audit : {audit:?}"
        );
    }
}

/// The three write-path role-coherence guards:
/// 1. release-version coherence (guard in [`validate_links`]),
/// 2. rejection of an unknown/misspelled role (guard in [`parse_dep`]),
/// 3. structural-zone vs prose/code extraction (guard in [`extract_structural_targets`] /
///    [`validate_card_body`]).
#[cfg(test)]
mod f213_role_guard_tests {
    use super::*;

    // ── Garde 2 — rôle inconnu / mal orthographié rejeté, dépendances légitimes préservées ──

    #[test]
    fn misspelled_role_is_rejected_not_swallowed() {
        // `relese` (typo de `release`) : valeur `planned`, ni ULID ni F-NN → UnknownRole.
        assert_eq!(
            parse_link("relese:planned"),
            Err(SchemaError::UnknownRole {
                prefix: "relese".to_string(),
                value: "planned".to_string(),
            })
        );
        // `stauts` (typo de `status`).
        assert_eq!(
            parse_link("stauts:OPEN"),
            Err(SchemaError::UnknownRole {
                prefix: "stauts".to_string(),
                value: "OPEN".to_string(),
            })
        );
    }

    #[test]
    fn legit_content_dep_with_ulid_still_parses() {
        // Dépendance de contenu par ULID → toujours acceptée (rétrocompat).
        assert_eq!(
            parse_link("decisions:01M0T40SBQ8BMBDSJY0X4Z14MN"),
            Ok(ProjectMapLink::Dep {
                section: "decisions".to_string(),
                ulid: "01M0T40SBQ8BMBDSJY0X4Z14MN".to_string(),
            })
        );
    }

    #[test]
    fn legit_relation_dep_by_feature_id_still_parses() {
        // `blocked-by:F-184` : relation de contenu par identité feature, présente sur 7 cartes
        // vivantes — doit rester acceptée (F-NN valide comme cible de dépendance).
        assert_eq!(
            parse_link("blocked-by:F-184"),
            Ok(ProjectMapLink::Dep {
                section: "blocked-by".to_string(),
                ulid: "F-184".to_string(),
            })
        );
    }

    #[test]
    fn malformed_truncated_ulid_dep_is_rejected() {
        // Un ULID tronqué (10 chars) n'est pas résolvable → UnknownRole (défaut mesuré : 1 carte).
        assert_eq!(
            parse_link("council:01KXGQ3ZJJ"),
            Err(SchemaError::UnknownRole {
                prefix: "council".to_string(),
                value: "01KXGQ3ZJJ".to_string(),
            })
        );
    }

    // ── Garde 1 — cohérence release↔version ─────────────────────────────────────

    fn card(links: &[ProjectMapLink]) -> Vec<ProjectMapLink> {
        links.to_vec()
    }

    #[test]
    fn roadmap_with_concrete_version_is_rejected() {
        let links = card(&[
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Feature("F-99".to_string()),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "2.2.0".to_string(),
            },
        ]);
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::IncoherentReleaseVersion {
                version: "2.2.0".to_string(),
            })
        );
    }

    #[test]
    fn roadmap_with_backlog_sentinel_is_valid() {
        let links = card(&[
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Feature("F-99".to_string()),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ]);
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn roadmap_without_version_is_valid() {
        // Version omise (axe porté par track en Phase 7) → aucune contradiction possible.
        let links = card(&[
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Feature("F-99".to_string()),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Track {
                project: "gradatum".to_string(),
                target: "backlog".to_string(),
            },
        ]);
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn planned_with_concrete_version_is_valid() {
        let links = card(&[
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Feature("F-99".to_string()),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "2.2.0".to_string(),
            },
        ]);
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── Garde 3 + point d'entrée body — extraction zone-structurelle ─────────────

    const VALID_HEAD: &str = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[feature:F-99]] [[track:gradatum/2.2.0]]";

    #[test]
    fn valid_card_body_passes() {
        assert_eq!(validate_card_body(VALID_HEAD), Ok(()));
    }

    #[test]
    fn role_line_with_middot_separators_is_structural() {
        // Reproduit la forme d'une carte vivante (01KZQ1RMP6…) : séparateurs `·` entre rôles.
        let body = "[[project:gradatum]] · [[status:OPEN]] · [[kind:FEATURE]] · · [[track:gradatum/2.3.0]]\n\n## Prose";
        assert_eq!(
            validate_card_body(body),
            Ok(()),
            "les rôles séparés par `·` doivent être vus comme structurels"
        );
    }

    #[test]
    fn role_cited_in_code_fence_is_not_counted() {
        // Une carte documente la syntaxe des rôles dans un bloc de code : ne doit pas gonfler
        // la cardinalité (défaut 3). Sans la garde : `at most 1 version link allowed, found 2`.
        let body = format!(
            "{VALID_HEAD}\n\n```\n[[version:gradatum/1.0.0]] [[version:gradatum/2.0.0]]\n```"
        );
        assert_eq!(validate_card_body(&body), Ok(()));
    }

    #[test]
    fn role_cited_in_backtick_prose_is_not_counted() {
        // Rôle cité entre marques de code inline dans une phrase → non compté.
        let body = format!("{VALID_HEAD}\n\nUne carte `[[kind:FIX]]` illustre le rôle en prose.");
        assert_eq!(validate_card_body(&body), Ok(()));
    }

    #[test]
    fn duplicate_kind_in_prose_does_not_break_cardinality() {
        // Deux `[[kind:…]]` cités en prose ne comptent pas ; seul le rôle structurel compte.
        let body = format!(
            "{VALID_HEAD}\n\nLe rôle [[kind:ROADMAP]] et [[kind:BACKLOG]] sont des structures."
        );
        // La prose ici n'est PAS une ligne structurelle (elle contient des mots) → ignorée.
        assert_eq!(validate_card_body(&body), Ok(()));
    }

    #[test]
    fn unknown_role_on_structural_line_is_rejected_by_body_validator() {
        let body = format!("{VALID_HEAD} [[relese:planned]]");
        assert_eq!(
            validate_card_body(&body),
            Err(SchemaError::UnknownRole {
                prefix: "relese".to_string(),
                value: "planned".to_string(),
            })
        );
    }

    #[test]
    fn incoherent_release_version_rejected_via_body() {
        let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[feature:F-99]] \
                    [[release:roadmap]] [[version:gradatum/2.2.0]]";
        assert_eq!(
            validate_card_body(body),
            Err(SchemaError::IncoherentReleaseVersion {
                version: "2.2.0".to_string(),
            })
        );
    }

    #[test]
    fn legit_dep_on_its_own_bullet_line_is_accepted() {
        // Une dépendance ULID seule sur une ligne à puce (résidu = "- ") reste structurelle et
        // valide — elle ne participe à aucune cardinalité.
        let body = format!("{VALID_HEAD}\n\n- [[decisions:01M0T40SBQ8BMBDSJY0X4Z14MN]]");
        assert_eq!(validate_card_body(&body), Ok(()));
    }

    #[test]
    fn bare_human_title_on_structural_line_is_ignored() {
        // Un titre humain nu (sans `:`) reste toléré (ignoré), pas rejeté.
        let body = format!("{VALID_HEAD}\n\n[[Un Titre Humain]]");
        assert_eq!(validate_card_body(&body), Ok(()));
    }
}
