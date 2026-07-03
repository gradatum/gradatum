//! Internal library for `gradatum-admin`.
//!
//! Exposes internal module functions for integration tests.
//! In particular, `generate_server_toml_template` and `merge_user_config`
//! are used to validate structural `server.toml` merging.
//!
//! Public API is intentionally minimal.

pub mod api_key_cmd;
pub mod backfill_embeddings;
pub mod backfill_note_links;
pub mod backfill_titles;
/// Backfill CHANGELOG → vault project-map cards (stage2).
pub mod changelog_backfill;
/// Parser CHANGELOG → entrées project-map (stage2).
pub mod changelog_parse;
/// Sub-command `code ingest` — index-only ingestion via tree-sitter (Rust).
pub mod code_cmd;
pub mod downgrade_from_vault_trash;
/// Backfill features.ts → vault project-map feature cards (T5).
pub mod feature_backfill;
pub mod init;
pub mod jobs_cmd;
/// Rendu cartes project-map valides depuis entrées CHANGELOG (stage2).
pub mod project_map_card;
/// Export JSON des cartes-feature project-map (`export-features --json`).
pub mod project_map_export;
/// Sub-command `project-map render` — generates `TODO.md` from the wikilink graph.
pub mod project_map_render;
/// Vue de synthèse read-only d'un projet (scope) depuis l'index SQLite.
pub mod project_map_scope;
pub mod token;
/// Sub-command `vault forget` — semantic forgetting of a batch of notes.
pub mod vault_forget_cmd;
/// Sub-command `vault rename` — rename a note and record a redirect.
pub mod vault_rename;

// Top-level re-exports for test convenience
pub use backfill_embeddings::{BackfillArgs, backfill};
pub use backfill_note_links::{BackfillNoteLinksArgs, BackfillNoteLinksReport};
pub use backfill_titles::{BackfillTitlesArgs, BackfillTitlesReport, backfill_titles};
pub use downgrade_from_vault_trash::{
    DowngradeFromTrashArgs, DowngradeStats, run as downgrade_from_vault_trash,
};
pub use init::{generate_server_toml_template, materialize_preset, merge_user_config};
pub use vault_rename::{VaultRenameArgs, VaultRenameReport, vault_rename};
