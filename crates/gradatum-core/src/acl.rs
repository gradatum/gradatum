//! Access-control traits for Gradatum.
//!
//! ## Design
//!
//! ACL traits live in `gradatum-core` so that downstream crates
//! (`gradatum-vault`, `gradatum-curator`) can depend on the interfaces without importing
//! `gradatum-acl-policy` (avoids circular dependencies).
//!
//! `allow_read` is the primary entry point consumed by `gradatum-vault`.
//! Full multi-tenant ACL implementations live in `gradatum-acl-policy`.
//!
//! ## ACLFilter
//!
//! `ACLFilter` generalises the `personal-classified` visibility marker of the legacy vault
//! (v1.6.2) — rather than a hard-coded marker, the filter logic is injected via this trait.

use crate::note::Note;
use crate::scope::BearerId;

/// Access-control policy for a note.
///
/// Implemented by `gradatum-acl-policy` for multi-tenant deployments.
///
/// In single-tenant mode, `gradatum-vault` uses a permissive default implementation
/// (`PermissivePolicy`). Replace with a strict `AclPolicy` for multi-tenant deployments.
pub trait AclPolicy: Send + Sync {
    /// Returns `true` if the bearer is allowed to read the note.
    ///
    /// The default implementation is permissive (single-user).
    /// Multi-tenant implementations check ACL rules in the [`Frontmatter`](crate::frontmatter::Frontmatter).
    fn allow_read(&self, note: &Note, bearer: &BearerId) -> bool;

    /// Returns `true` if the bearer is allowed to write or modify the note.
    fn allow_write(&self, note: &Note, bearer: &BearerId) -> bool;

    /// Returns `true` if the bearer is allowed to delete the note.
    fn allow_delete(&self, note: &Note, bearer: &BearerId) -> bool;
}

/// Visibility filter over a list of notes.
///
/// Generalises the `personal-classified` visibility marker of the legacy vault (v1.6.2).
/// Instead of a hard-coded marker, the filtering logic is injected via this trait.
///
/// Used by `gradatum-vault` in listing endpoints to hide notes not accessible to the current bearer.
pub trait ACLFilter: Send + Sync {
    /// Filters `notes` and returns only the notes accessible to `bearer`.
    ///
    /// Returns references into the input slice — zero copy.
    fn filter<'a>(&self, notes: &'a [Note], bearer: &BearerId) -> Vec<&'a Note>;
}
