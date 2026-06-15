//! Error types for the `gradatum-curator` crate.

use thiserror::Error;

/// Errors that can occur during a curation decision.
///
/// Note: [`crate::workflow::Curator::decide`] never returns an error — all
/// internal errors are absorbed into a `CuratorDecision` with
/// `fallback_applied = true`. This type is reserved for utility functions in
/// the crate that can fail explicitly.
#[derive(Debug, Error)]
pub enum CuratorError {
    /// Error propagated from the Chat backend.
    #[error("chat: {0}")]
    Chat(#[from] gradatum_chat::ChatError),
}
