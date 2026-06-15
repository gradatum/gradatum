//! # gradatum-admin
//!
//! Gradatum operator CLI: init, token, api-key, code.
//!
//! ## Sub-commands
//! - `init`        — bootstraps a root directory (JWT keys, admin bearer, ACL preset, SQLite)
//! - `token`       — manages service JWT tokens (bootstrap path)
//! - `api-key`     — manages API key lifecycle (create/list/revoke/rotate)
//! - `code ingest` — ingests source code via tree-sitter into index-only derived notes
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
//! gradatum-admin code ingest <repo_path> --vault code-<projet> --root /var/lib/gradatum
//! ```

use gradatum_admin::{
    api_key_cmd, code_cmd, init, jobs_cmd, token, vault_forget_cmd, vault_rename, BackfillArgs,
    BackfillTitlesArgs, DowngradeFromTrashArgs, VaultRenameArgs,
};
use gradatum_core::paths::vault_index_path;

use clap::{Args, Parser, Subcommand};

/// Parses the `--visibility pub|all` CLI flag into `IngestVisibility`.
fn parse_ingest_visibility(s: &str) -> Result<code_cmd::IngestVisibility, String> {
    match s {
        "pub" => Ok(code_cmd::IngestVisibility::Pub),
        "all" => Ok(code_cmd::IngestVisibility::All),
        other => Err(format!(
            "valeur invalide '{other}' — valeurs acceptées : pub, all"
        )),
    }
}

/// Gradatum operator CLI.
#[derive(Debug, Parser)]
#[command(version, about = "gradatum-admin — CLI opérateur Gradatum")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

/// Available sub-commands.
#[derive(Debug, Subcommand)]
enum Cmd {
    /// Bootstraps a Gradatum root directory.
    Init(init::InitArgs),
    /// Manages service JWT tokens.
    Token {
        #[command(subcommand)]
        cmd: token::TokenCmd,
    },
    /// Manages API key lifecycle (create/list/revoke/rotate).
    #[command(name = "api-key")]
    ApiKey {
        #[command(subcommand)]
        cmd: api_key_cmd::ApiKeyCmd,
    },
    /// Backfills embeddings for notes that have none (idempotent, LEFT JOIN).
    ///
    /// Scans notes with no entry in `note_embeddings` and enqueues `embed_note`
    /// jobs for the worker. Safe to re-run: already-embedded notes are excluded.
    #[command(name = "backfill-embeddings")]
    BackfillEmbeddings {
        /// Gradatum root directory.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Tenant to process (default: `"main"`).
        #[arg(long)]
        tenant: Option<String>,
        /// Maximum number of notes to enqueue (unlimited if absent).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Backfills missing titles for notes where `title IS NULL` (idempotent).
    ///
    /// Extracts the Markdown H1 from each note and updates the `title` column.
    /// Use `--dry-run` to preview without writing to the database.
    #[command(name = "backfill-titles")]
    BackfillTitles {
        /// Gradatum root directory.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Tenant to process (default: `"main"`).
        #[arg(long, default_value = "main")]
        tenant: String,
        /// Preview actions without writing to the database.
        #[arg(long)]
        dry_run: bool,
        /// Maximum number of notes to process (unlimited if absent).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Inspects and manages the job queue (list/get/cancel/dlq).
    Jobs {
        #[command(subcommand)]
        cmd: jobs_cmd::JobsCmd,
    },
    /// Migrates notes from the legacy vault `.vault-trash` to gradatum via `status='downgraded'`.
    ///
    /// Idempotent: skips notes already set to `status='downgraded'`.
    /// Dry-run mode available. Match heuristic: first 200 chars of body (UTF-8 safe).
    #[command(name = "downgrade-from-legacy-vault-trash")]
    DowngradeFromLegacyVaultTrash {
        /// Root directory of the legacy vault (must contain `.vault-trash/`).
        #[arg(long, default_value = "/home/maintainer-user/.memory-vault")]
        legacy_vault_path: std::path::PathBuf,
        /// Gradatum root directory.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Preview actions without writing to the database.
        #[arg(long)]
        dry_run: bool,
        /// Maximum number of notes to downgrade (unlimited if absent).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// vault operations (rename, …).
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
    /// Code-ingest operations.
    ///
    /// Pipeline: tree-sitter (Rust) → index-only derived notes.
    Code {
        #[command(subcommand)]
        cmd: CodeCmd,
    },
}

/// Sub-commands of `vault`.
#[derive(Debug, Subcommand)]
enum VaultCmd {
    /// Renames a note — updates `notes.title` and records a redirect entry.
    ///
    /// Does not modify the Markdown body on disk — only index metadata is updated.
    Rename(VaultRenameCliArgs),
    /// Semantic forgetting of a batch of notes (frontmatter + index).
    ///
    /// Two-step confirmation workflow:
    ///   1. Preview (dry-run by default): displays candidate notes.
    ///   2. Execute: `--execute --confirm-ulids "<u1,u2,…>"` (enqueues `Job::Forget`).
    ///
    /// Sub-scopes: topic | locus | agent
    Forget {
        #[command(subcommand)]
        cmd: vault_forget_cmd::ForgetCmd,
    },
}

/// Sub-commands of `code`.
#[derive(Debug, Subcommand)]
enum CodeCmd {
    /// Ingests a git repository into a logical vault `code-<project>`.
    ///
    /// Pipeline: `git ls-files` (.rs) → tree-sitter parse → `DerivedNote` → SQLite index.
    /// Idempotent: unchanged files (same `content_hash_source`) are skipped.
    /// Full rebuild available via `--rebuild` (drop + re-ingest).
    Ingest(CodeIngestCliArgs),
    /// Updates a code vault in O(diff) from the last ingest.
    ///
    /// `git diff --name-status <last_sha>..HEAD` → re-ingests only Added/Modified files,
    /// deletes Deleted files, and stores the new HEAD. Target latency: < 3 s.
    /// Falls back to a full ingest if no prior ingest exists.
    Update(CodeUpdateCliArgs),
}

/// Arguments for the `code ingest` command.
#[derive(Debug, Args)]
struct CodeIngestCliArgs {
    /// Path to the git repository to ingest.
    pub repo_path: std::path::PathBuf,
    /// Target logical vault (e.g. `code-gradatum`).
    #[arg(long)]
    pub vault: String,
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: std::path::PathBuf,
    /// Forces a full rebuild (drop + total re-ingest).
    #[arg(long)]
    pub rebuild: bool,
    /// Ingestion visibility mode: `pub` (default) or `all` (includes private items).
    ///
    /// `pub`: only public items (`pub`, `pub(crate)`, etc.) are indexed.
    ///        Preserves the visible API surface. Default behaviour.
    /// `all`: all items are indexed, including private items.
    ///        Useful for indexing the internal implementation of a crate.
    #[arg(long, default_value = "pub", value_parser = parse_ingest_visibility)]
    pub visibility: code_cmd::IngestVisibility,
}

/// Arguments for the `code update` command.
#[derive(Debug, Args)]
struct CodeUpdateCliArgs {
    /// Path to the git repository to update.
    pub repo_path: std::path::PathBuf,
    /// Target logical vault (e.g. `code-gradatum`).
    #[arg(long)]
    pub vault: String,
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: std::path::PathBuf,
    /// Optional visibility mode override for this update only.
    ///
    /// Absent (default): the mode is read from the database (persisted at last ingest).
    /// `pub` or `all`: forces this mode for the current update (and persists it).
    #[arg(long, value_parser = parse_ingest_visibility)]
    pub visibility: Option<code_cmd::IngestVisibility>,
}

/// Arguments for `vault rename`.
#[derive(Debug, Args)]
struct VaultRenameCliArgs {
    /// Current title of the note (must exist with `status='live'`).
    pub ancien: String,
    /// New title to apply.
    pub nouveau: String,
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: std::path::PathBuf,
    /// Tenant (`vault_id`) — default `"main"`.
    #[arg(long, default_value = "main")]
    pub tenant: String,
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
        Cmd::Vault { cmd } => match cmd {
            VaultCmd::Rename(cli_args) => {
                let args = VaultRenameArgs {
                    root: cli_args.root,
                    ancien: cli_args.ancien,
                    nouveau: cli_args.nouveau,
                    tenant: cli_args.tenant,
                };
                let report = vault_rename(args).await?;
                println!(
                    "vault rename: note_id={} slug={}",
                    report.note_id, report.slug
                );
                Ok(())
            }
            VaultCmd::Forget { cmd } => vault_forget_cmd::run_forget(cmd).await,
        },
        Cmd::Code { cmd } => match cmd {
            CodeCmd::Ingest(cli_args) => {
                // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
                let index_path = vault_index_path(&cli_args.root);
                let args = code_cmd::CodeIngestArgs {
                    repo_path: cli_args.repo_path,
                    vault_id: cli_args.vault,
                    index_path,
                    rebuild: cli_args.rebuild,
                    visibility: cli_args.visibility,
                };
                let report = code_cmd::run_ingest(args).await?;
                println!(
                    "code ingest: files_total={} ingested={} skipped={} deleted={} notes_inserted={} duration_ms={}",
                    report.files_total,
                    report.files_ingested,
                    report.files_skipped,
                    report.files_deleted,
                    report.notes_inserted,
                    report.duration_ms,
                );
                Ok(())
            }
            CodeCmd::Update(cli_args) => {
                // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
                let index_path = vault_index_path(&cli_args.root);
                let args = code_cmd::CodeUpdateArgs {
                    repo_path: cli_args.repo_path,
                    vault_id: cli_args.vault,
                    index_path,
                    visibility_override: cli_args.visibility,
                };
                let report = code_cmd::run_update(args).await?;
                println!(
                    "code update: changed={} ingested={} deleted={} notes_inserted={} from_sha={} to_sha={} duration_ms={}",
                    report.files_changed,
                    report.files_ingested,
                    report.files_deleted,
                    report.notes_inserted,
                    report.from_sha,
                    report.to_sha,
                    report.duration_ms,
                );
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
