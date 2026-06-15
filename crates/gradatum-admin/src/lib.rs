//! Internal library for `gradatum-admin`.
//!
//! Exposes internal module functions for integration tests.
//! In particular, `generate_server_toml_template` and `merge_user_config`
//! are used to validate structural `server.toml` merging.
//!
//! Public API is intentionally minimal.

pub mod api_key_cmd;
pub mod backfill_embeddings;
pub mod backfill_titles;
/// Sub-command `code ingest` — index-only ingestion via tree-sitter (Rust).
pub mod code_cmd;
pub mod downgrade_from_vault_trash;
pub mod init;
pub mod jobs_cmd;
pub mod token;
/// Sub-command `vault forget` — semantic forgetting of a batch of notes.
pub mod vault_forget_cmd;
/// Sub-command `vault rename` — rename a note and record a redirect.
pub mod vault_rename;

// Top-level re-exports for test convenience
pub use backfill_embeddings::{backfill, BackfillArgs};
pub use backfill_titles::{backfill_titles, BackfillTitlesArgs, BackfillTitlesReport};
pub use downgrade_from_vault_trash::{
    run as downgrade_from_vault_trash, DowngradeFromTrashArgs, DowngradeStats,
};
pub use init::{generate_server_toml_template, materialize_preset, merge_user_config};
pub use vault_rename::{vault_rename, VaultRenameArgs, VaultRenameReport};
