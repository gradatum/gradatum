//! Bibliothèque interne de `gradatum-admin`.
//!
//! Expose les fonctions des modules internes aux tests d'intégration.
//! En particulier, `init_merge.rs` utilise `generate_server_toml_template`
//! et `merge_user_config` pour valider le merge structurel `server.toml`.
//!
//! L'API publique est intentionnellement minimale.

pub mod api_key_cmd;
pub mod backfill_embeddings;
pub mod backfill_titles;
pub mod downgrade_from_vault_trash;
pub mod init;
pub mod jobs_cmd;
pub mod token;

// Réexportations de premier niveau pour la commodité des tests
pub use backfill_embeddings::{backfill, BackfillArgs};
pub use backfill_titles::{backfill_titles, BackfillTitlesArgs, BackfillTitlesReport};
pub use downgrade_from_vault_trash::{
    run as downgrade_from_vault_trash, DowngradeFromTrashArgs, DowngradeStats,
};
pub use init::{generate_server_toml_template, materialize_preset, merge_user_config};
