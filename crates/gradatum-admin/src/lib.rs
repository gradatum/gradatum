//! Library target backing the `gradatum-admin` operator CLI.
//!
//! The binary is the supported entry point; this library exists so that the
//! sub-command implementations can be exercised directly by integration tests.
//! [`generate_server_toml_template`] and [`merge_user_config`], for instance, are
//! driven from tests to check that `server.toml` merging preserves user edits.
//!
//! Each sub-command lives in its own module, and the handful of types most often
//! needed by callers are re-exported at the crate root.
//!
//! These modules are internal CLI plumbing, exposed only for this crate's own
//! integration tests. They are hidden from the rendered documentation and are
//! **not** a stable public API (this crate is an operator CLI binary, not a
//! reusable library).

#[doc(hidden)]
pub mod admin_client;
#[doc(hidden)]
pub mod admin_cmd;
#[doc(hidden)]
pub mod api_key_cmd;
#[doc(hidden)]
pub mod backfill_embeddings;
#[doc(hidden)]
pub mod backfill_note_links;
#[doc(hidden)]
pub mod backfill_titles;
/// Bulk creation of vault project-map cards from a CHANGELOG file.
#[doc(hidden)]
pub mod changelog_backfill;
/// CHANGELOG parser producing project-map card entries.
#[doc(hidden)]
pub mod changelog_parse;
/// Sub-command `code ingest` — index-only ingestion of a repository via tree-sitter
/// (Rust, Python, Bash, TypeScript/TSX).
#[doc(hidden)]
pub mod code_cmd;
#[doc(hidden)]
pub mod downgrade_from_vault_trash;
/// Bulk creation of vault project-map feature cards from a `features.ts` catalogue.
#[doc(hidden)]
pub mod feature_backfill;
#[doc(hidden)]
pub mod init;
#[doc(hidden)]
pub mod jobs_cmd;
/// Rendering of schema-valid project-map cards from CHANGELOG entries.
#[doc(hidden)]
pub mod project_map_card;
/// JSON export of project-map feature cards (`export-features --json`).
#[doc(hidden)]
pub mod project_map_export;
/// Sub-command `project-map render` — generates `TODO.md` from the wikilink graph.
#[doc(hidden)]
pub mod project_map_render;
/// Read-only summary view of a single project, read straight from the SQLite index.
#[doc(hidden)]
pub mod project_map_scope;
#[doc(hidden)]
pub mod token;
/// Sub-command `vault forget` — semantic forgetting of a batch of notes.
#[doc(hidden)]
pub mod vault_forget_cmd;
/// Sub-command `vault rename` — rename a note and record a redirect.
#[doc(hidden)]
pub mod vault_rename;

// Top-level re-exports for test convenience
#[doc(hidden)]
pub use backfill_embeddings::{BackfillArgs, backfill};
#[doc(hidden)]
pub use backfill_note_links::{BackfillNoteLinksArgs, BackfillNoteLinksReport};
#[doc(hidden)]
pub use backfill_titles::{BackfillTitlesArgs, BackfillTitlesReport, backfill_titles};
#[doc(hidden)]
pub use downgrade_from_vault_trash::{
    DowngradeFromTrashArgs, DowngradeStats, run as downgrade_from_vault_trash,
};
#[doc(hidden)]
pub use init::{generate_server_toml_template, materialize_preset, merge_user_config};
#[doc(hidden)]
pub use vault_rename::{VaultRenameArgs, VaultRenameReport, vault_rename};
