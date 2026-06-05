//! # gradatum-admin
//!
//! CLI opérateur Gradatum : init, token, api-key.
//!
//! ## Sous-commandes
//! - `init`    — bootstrap d'un répertoire racine (clés JWT, bearer admin, preset ACL, SQLite)
//! - `token`   — gestion des tokens JWT service (Path 3 bootstrap)
//! - `api-key` — gestion du cycle de vie des API keys (create/list/revoke/rotate)
//!
//! ## Usage
//! ```text
//! gradatum-admin init --preset hierarchical --root /var/lib/gradatum
//! gradatum-admin token issue --root /var/lib/gradatum --sub mcp-stub --scopes vault_read
//! gradatum-admin api-key create --root /var/lib/gradatum --owner mcp-stub
//! gradatum-admin api-key list   --root /var/lib/gradatum
//! gradatum-admin api-key revoke --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin api-key rotate --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum [--tenant main] [--limit 100]
//! gradatum-admin backfill-titles --root /var/lib/gradatum [--tenant main] [--dry-run] [--limit N]
//! gradatum-admin jobs list   --root /var/lib/gradatum [--status pending] [--kind Curate] [--limit 50]
//! gradatum-admin jobs get    --root /var/lib/gradatum <id>
//! gradatum-admin jobs cancel --root /var/lib/gradatum <id>
//! gradatum-admin jobs dlq    --root /var/lib/gradatum [--replay <id>] [--replay-all]
//! ```

use gradatum_admin::{
    api_key_cmd, init, jobs_cmd, token, BackfillArgs, BackfillTitlesArgs, DowngradeFromTrashArgs,
};

use clap::{Parser, Subcommand};

/// CLI opérateur Gradatum.
#[derive(Debug, Parser)]
#[command(version, about = "gradatum-admin — CLI opérateur Gradatum")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

/// Sous-commandes disponibles.
#[derive(Debug, Subcommand)]
enum Cmd {
    /// Bootstrap d'un répertoire racine Gradatum (Phase 2.0a).
    Init(init::InitArgs),
    /// Gestion des tokens JWT service (Path 3 bootstrap).
    Token {
        #[command(subcommand)]
        cmd: token::TokenCmd,
    },
    /// Gestion du cycle de vie des API keys (AUTH-T3).
    #[command(name = "api-key")]
    ApiKey {
        #[command(subcommand)]
        cmd: api_key_cmd::ApiKeyCmd,
    },
    /// Backfill embeddings pour les notes sans embedding (idempotent, LEFT JOIN).
    ///
    /// Scan les notes sans entrée dans `note_embeddings` et enqueue des jobs
    /// `embed_note` pour le worker. Peut être relancé sans risque : les notes
    /// déjà embedded sont ignorées.
    #[command(name = "backfill-embeddings")]
    BackfillEmbeddings {
        /// Répertoire racine Gradatum.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Tenant à traiter (défaut : `"main"`).
        #[arg(long)]
        tenant: Option<String>,
        /// Limite du nombre de notes à enqueuer (illimité si absent).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Backfille les titres manquants pour les notes sans titre (idempotent, WHERE title IS NULL).
    ///
    /// Extrait le H1 Markdown de chaque note et met à jour la colonne `title`.
    /// Utiliser `--dry-run` pour prévisualiser sans modifier la DB.
    #[command(name = "backfill-titles")]
    BackfillTitles {
        /// Répertoire racine Gradatum.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Tenant à traiter (défaut : `"main"`).
        #[arg(long, default_value = "main")]
        tenant: String,
        /// Affiche les actions sans écrire en base.
        #[arg(long)]
        dry_run: bool,
        /// Limite du nombre de notes à traiter (illimité si absent).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Introspection et gestion de la queue de jobs F-16 (list/get/cancel/dlq).
    Jobs {
        #[command(subcommand)]
        cmd: jobs_cmd::JobsCmd,
    },
    /// Migre les notes du `.vault-trash` du legacy vault vers gradatum via `status='downgraded'`.
    ///
    /// Idempotent : skip si la note est déjà `status='downgraded'`.
    /// Mode dry-run disponible. Heuristique match : premiers 200 chars du body (UTF-8 safe).
    #[command(name = "downgrade-from-legacy-vault-trash")]
    DowngradeFromLegacyVaultTrash {
        /// Répertoire racine du legacy vault (contenant `.vault-trash/`).
        #[arg(long, default_value = "/home/maintainer-user/.memory-vault")]
        legacy_vault_path: std::path::PathBuf,
        /// Répertoire racine Gradatum.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Affiche les actions sans écrire en base.
        #[arg(long)]
        dry_run: bool,
        /// Max nombre de notes à downgrader (illimité si absent).
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Init(args) => init::run(args),
        Cmd::Token { cmd } => token::run(cmd),
        Cmd::ApiKey { cmd } => api_key_cmd::run(cmd).await,
        Cmd::Jobs { cmd } => jobs_cmd::run(cmd).await,
        Cmd::BackfillEmbeddings {
            root,
            tenant,
            limit,
        } => {
            let args = BackfillArgs {
                root,
                tenant,
                limit,
            };
            let n = gradatum_admin::backfill(args).await?;
            println!("backfill-embeddings: {n} job(s) enqueued");
            Ok(())
        }
        Cmd::BackfillTitles {
            root,
            tenant,
            dry_run,
            limit,
        } => {
            let args = BackfillTitlesArgs {
                root,
                tenant,
                dry_run,
                limit,
            };
            let report = gradatum_admin::backfill_titles(args).await?;
            if dry_run {
                println!(
                    "backfill-titles [DRY-RUN]: notes_scanned={} titles_extracted={} titles_updated={} titles_no_h1={}",
                    report.notes_scanned,
                    report.titles_extracted,
                    report.titles_updated,
                    report.titles_no_h1,
                );
            } else {
                println!(
                    "backfill-titles: notes_scanned={} titles_extracted={} titles_updated={} titles_no_h1={}",
                    report.notes_scanned,
                    report.titles_extracted,
                    report.titles_updated,
                    report.titles_no_h1,
                );
            }
            Ok(())
        }
        Cmd::DowngradeFromLegacyVaultTrash {
            legacy_vault_path,
            root,
            dry_run,
            limit,
        } => {
            let args = DowngradeFromTrashArgs {
                legacy_vault_path,
                gradatum_root: root,
                dry_run,
                limit,
            };
            let stats = gradatum_admin::downgrade_from_vault_trash(args).await?;
            println!("downgrade-from-legacy-vault-trash complete: {stats:?}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
