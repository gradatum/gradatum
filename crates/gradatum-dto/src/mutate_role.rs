//! Wire contract for `vault_mutate_role` — targeted mutation of a typed role on one or
//! more project-map cards, **without the client holding the card body**.
//!
//! The sole write path for a project-map card re-emits the whole body: to change one role a
//! client must own the current body and reconstruct it, which can silently overwrite a curator
//! recomposition — and the cards are unremovable. `vault_mutate_role` moves the mutation
//! server-side: the server reads the current body, replaces the value of the targeted typed role
//! in place (every other byte identical), and re-writes under the existing optimistic lock.
//!
//! ## Per-card verdict, never aggregated
//!
//! A batch (up to a mass operation over ~21 cards) is supported, but the response carries **one
//! verdict per card** — a single aggregate success would hide *which* of N cards failed. Each
//! card is atomic and optimistically locked independently: a stale `expected_sha256` on one card
//! fails that card only; the rest proceed.

use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// A single role mutation targeting one project-map card.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MutateRoleItem {
    /// Bare ULID of the card to mutate (never a `section/ULID`, title or slug).
    pub note_id: String,
    /// Typed role to mutate: one of `status` / `kind` / `release` / `version` / `track`.
    ///
    /// `feature` / `project` are refused (card identity, immutable); any other token is refused
    /// as an unknown role type.
    pub role_type: String,
    /// New value of the role (e.g. `DONE` for `status`, `gradatum/2.4.0` for `track`). Validated
    /// server-side by re-parsing `<role_type>:<new_value>`; an off-taxonomy value is refused.
    pub new_value: String,
    /// Optional history line appended to the card's `Historique` section (created if absent).
    /// Omitted or empty → no history entry is added.
    #[serde(default)]
    pub history_line: Option<String>,
    /// Expected SHA-256 (64 lowercase hex) of the card being mutated — the per-card optimistic
    /// lock. A stale value fails **this** card with a `conflict` verdict, never the whole batch.
    pub expected_sha256: String,
}

/// Request body for `vault_mutate_role` — always a list (a unitary mutation is a one-element
/// list), so the per-card-verdict contract holds uniformly.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MutateRoleRequest {
    /// Target tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default. Applies to every card in
    /// the batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// The mutations to apply — 1..N. Each yields its own verdict.
    pub mutations: Vec<MutateRoleItem>,
}

impl MutateRoleRequest {
    /// Builds a request from a list of mutations, with the tenant resolved server-side.
    #[must_use]
    pub fn new(mutations: Vec<MutateRoleItem>) -> Self {
        Self {
            tenant_id: None,
            mutations,
        }
    }
}

/// Outcome of one card's mutation. Serialized as a lowercase string tag.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MutationOutcome {
    /// The mutated card was accepted and enqueued for the async write (poll `job_id`).
    Queued,
    /// The mutation was refused (unknown/immutable role, absent role, invalid value, or the
    /// post-mutation card would violate the project-map schema).
    Refused,
    /// Optimistic-lock conflict: `expected_sha256` did not match the card's current hash.
    Conflict,
    /// The card ULID resolves to no live note.
    NotFound,
}

/// The verdict for a single card in a `vault_mutate_role` batch.
///
/// Exactly one of the outcome-specific fields is populated:
/// - `Queued` → `job_id` + `poll_url`.
/// - `Conflict` → `current_sha256` (the winning hash).
/// - `Refused` / `NotFound` → `error` (human-readable reason).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MutateRoleCardVerdict {
    /// The card this verdict is about (echoes the request `note_id`).
    pub note_id: String,
    /// Per-card outcome.
    pub outcome: MutationOutcome,
    /// Async job handle — present iff `outcome == Queued`. Poll `job_status` to confirm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Relative URL to poll the job — present iff `outcome == Queued`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_url: Option<String>,
    /// Current SHA-256 of the card — present iff `outcome == Conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_sha256: Option<String>,
    /// Human-readable refusal reason — present for `Refused` / `NotFound`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl MutateRoleCardVerdict {
    /// Card accepted and enqueued for the async write.
    #[must_use]
    pub fn queued(note_id: String, job_id: String, poll_url: String) -> Self {
        Self {
            note_id,
            outcome: MutationOutcome::Queued,
            job_id: Some(job_id),
            poll_url: Some(poll_url),
            current_sha256: None,
            error: None,
        }
    }

    /// Mutation refused (bad role, absent role, invalid value, post-mutation schema violation…).
    #[must_use]
    pub fn refused(note_id: String, error: String) -> Self {
        Self {
            note_id,
            outcome: MutationOutcome::Refused,
            job_id: None,
            poll_url: None,
            current_sha256: None,
            error: Some(error),
        }
    }

    /// Optimistic-lock conflict; `current_sha256` is the winning hash when known.
    #[must_use]
    pub fn conflict(note_id: String, current_sha256: Option<String>, error: String) -> Self {
        Self {
            note_id,
            outcome: MutationOutcome::Conflict,
            job_id: None,
            poll_url: None,
            current_sha256,
            error: Some(error),
        }
    }

    /// The card ULID resolves to no live note.
    #[must_use]
    pub fn not_found(note_id: String, error: String) -> Self {
        Self {
            note_id,
            outcome: MutationOutcome::NotFound,
            job_id: None,
            poll_url: None,
            current_sha256: None,
            error: Some(error),
        }
    }
}

/// Response for `vault_mutate_role` — one verdict per card, in request order, never aggregated.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutateRoleResponse {
    /// Per-card verdicts, aligned with the request `mutations` order.
    pub results: Vec<MutateRoleCardVerdict>,
}
