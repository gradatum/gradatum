//! Errors specific to the `gradatum-vault` crate.
//!
//! `VaultError` wraps errors from underlying layers and adds vault-specific
//! variants (lifecycle operations, overrides, drift).
//!
//! No `Box<dyn Error>` in public library code — strong typing via `thiserror`.

use thiserror::Error;

/// Top-level error type for `gradatum-vault`.
///
/// Produced by all public vault operations (create/open, lifecycle,
/// overrides, drift, effective_note cache).
#[derive(Debug, Error)]
pub enum VaultError {
    /// Error originating from `gradatum-core` (note not found, drift, validation, etc.).
    #[error("core: {0}")]
    Core(#[from] gradatum_core::error::GradatumError),

    /// Storage error (OpenDAL, filesystem).
    ///
    #[error("storage: {0}")]
    Storage(String),

    /// Markdown serialisation/parsing error.
    #[error("markdown: {0}")]
    Markdown(String),

    /// Optimistic-lock conflict: the supplied `expected_sha256` does not match
    /// the current hash of the note. Carries the current hash to enable resolution.
    ///
    /// The `[u8; 32]` is the current SHA-256 hash (the value held by the concurrent winner).
    ///
    /// **Never constructed — by design, even though the optimistic lock is now live.**
    /// `write_if_match` (wired into production via
    /// [`crate::Registry::write_if_match_internal`]) signals a mismatch through
    /// [`crate::write::WriteResult::Conflict`], a **success value** — not through this error
    /// variant. No code in the workspace produces `VaultError::Conflict`; the `match` arms
    /// that map it are defensive only. Callers must not use it to detect a conflict: the
    /// conflict is an `Ok(WriteResult::Conflict)`, not an `Err`.
    #[error("optimistic-lock conflict: current hash = {current_sha256}", current_sha256 = gradatum_core::identity::ContentHash(*(.0)).hex())]
    Conflict([u8; 32]),
}

impl From<gradatum_storage::StorageError> for VaultError {
    fn from(e: gradatum_storage::StorageError) -> Self {
        VaultError::Storage(e.to_string())
    }
}

impl From<gradatum_markdown::MarkdownError> for VaultError {
    fn from(e: gradatum_markdown::MarkdownError) -> Self {
        VaultError::Markdown(e.to_string())
    }
}
