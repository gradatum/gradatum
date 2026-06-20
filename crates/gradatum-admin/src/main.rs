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
    BackfillArgs, BackfillNoteLinksArgs, BackfillTitlesArgs, DowngradeFromTrashArgs,
    VaultRenameArgs, api_key_cmd, code_cmd, init, jobs_cmd, token, vault_forget_cmd, vault_rename,
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
    /// Backfills missing `note_links` for notes with wikilinks but no edges (idempotent).
    ///
    /// Scans notes `status='live'` with at least one `[[` wikilink and no outgoing
    /// edge in `note_links`. Resolves and inserts the missing edges via
    /// `resolve_wikilinks_sync`. Use `--dry-run` to preview without writing.
    #[command(name = "backfill-note-links")]
    BackfillNoteLinks {
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
    /// project-map operations (render the generated `TODO.md` view).
    #[command(name = "project-map")]
    ProjectMap {
        #[command(subcommand)]
        cmd: ProjectMapCmd,
    },
}

/// Sub-commands of `project-map`.
#[derive(Debug, Subcommand)]
enum ProjectMapCmd {
    /// Renders the `TODO.md` view of a project from the wikilink graph (PULL).
    ///
    /// Reads the work-status graph (`note_links`) only — never semantic search.
    /// Prints the generated Markdown to stdout (the operator redirects it to
    /// the target `TODO.md`).
    Render {
        /// Project name (matches `[[project:<name>]]`).
        project: String,
        /// Gradatum root directory.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Vault / tenant to read (default: `"main"`).
        #[arg(long, default_value = "main")]
        vault: String,
    },
    /// Backfills project-map cards from CHANGELOG.md entries (stage2, idempotent).
    ///
    /// Parses `[from..to]` version range from CHANGELOG.md, generates one card per
    /// bullet entry, and posts to `POST /api/v1/vault_write`. Idempotent: entries
    /// with a matching source marker already in the vault are skipped.
    ///
    /// Use `--apply` to write cards to the vault (default: false = dry-run preview). Requires `--api-key`.
    #[command(name = "backfill-changelog")]
    BackfillChangelog {
        /// Path to CHANGELOG.md.
        #[arg(long, default_value = "CHANGELOG.md")]
        changelog: std::path::PathBuf,
        /// Minimum version to include (SemVer, inclusive).
        #[arg(long, default_value = "0.4.0")]
        from: String,
        /// Maximum version to include (SemVer, inclusive).
        #[arg(long, default_value = "0.5.2")]
        to: String,
        /// Write cards to the vault (default: false = dry-run preview). Requires --api-key.
        #[arg(long, default_value_t = false)]
        apply: bool,
        /// Gradatum server base URL.
        #[arg(long, default_value = "http://127.0.0.1:19090")]
        server_url: String,
        /// API key for authentication (empty = dry-run only).
        #[arg(long, default_value = "")]
        api_key: String,
        /// Include meta sections (Tests, Internal, Documentation…) as KindKind::Task cards.
        /// Default false: only standard Keep-a-Changelog sections are included.
        #[arg(long, default_value_t = false)]
        include_meta: bool,
    },
    /// Rapporte l'état courant d'un projet (version, comptages par statut, timeline).
    ///
    /// Lit les cartes project-map depuis l'index de notes (`notes.body_text` + `notes.status`),
    /// filtrées par le wikilink `[[project:<name>]]` — jamais de recherche sémantique (invariant B4).
    /// Les comptages dérivent du schéma de liens forcés (`[[status:…]]`, `[[version:…]]`).
    /// Lecture directe SQLite (rusqlite, sans appel HTTP).
    #[command(name = "scope")]
    Scope {
        /// Project name (matches `[[project:<name>]]`).
        project: String,
        /// Gradatum root directory.
        #[arg(long, default_value = "/var/lib/gradatum")]
        root: std::path::PathBuf,
        /// Vault / tenant to read (default: `"main"`).
        #[arg(long, default_value = "main")]
        vault: String,
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
        Cmd::BackfillNoteLinks {
            root,
            tenant,
            dry_run,
            limit,
        } => {
            let args = BackfillNoteLinksArgs {
                root,
                tenant,
                dry_run,
                limit,
            };
            let report = gradatum_admin::backfill_note_links::run(args).await?;
            if report.dry_run {
                println!(
                    "DRY-RUN: {} notes scanned, {} edges would be written ({} notes touched)",
                    report.notes_scanned, report.edges_written, report.notes_touched
                );
            } else {
                println!(
                    "Backfill complet: {} notes scanned, {} edges written ({} notes touched)",
                    report.notes_scanned, report.edges_written, report.notes_touched
                );
            }
            Ok(())
        }
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
        Cmd::ProjectMap { cmd } => match cmd {
            ProjectMapCmd::Render {
                project,
                root,
                vault,
            } => {
                // PULL : génère la vue à la demande, l'opérateur redirige vers TODO.md.
                let markdown =
                    gradatum_admin::project_map_render::render_project_map(&root, &vault, &project)
                        .await?;
                print!("{markdown}");
                Ok(())
            }
            ProjectMapCmd::BackfillChangelog {
                changelog,
                from,
                to,
                apply,
                server_url,
                api_key,
                include_meta,
            } => {
                use gradatum_admin::changelog_backfill::{
                    BackfillChangelogArgs, HttpVaultClient, VaultWriteClient, run_backfill,
                };

                let args = BackfillChangelogArgs {
                    changelog_path: changelog,
                    from_version: from,
                    to_version: to,
                    apply,
                    server_url: server_url.clone(),
                    api_key: api_key.clone(),
                    include_meta,
                };

                let report = if !apply {
                    // En dry-run : client fictif (marker_exists=false, vault_write jamais appelé).
                    struct DryRunClient;
                    #[async_trait::async_trait]
                    impl VaultWriteClient for DryRunClient {
                        async fn marker_exists(&self, _marker: &str) -> anyhow::Result<bool> {
                            Ok(false)
                        }
                        async fn vault_write(
                            &self,
                            _card: &gradatum_admin::project_map_card::VaultWriteCard,
                        ) -> anyhow::Result<String> {
                            Ok(String::new())
                        }
                    }
                    run_backfill(&args, &DryRunClient).await?
                } else {
                    if api_key.is_empty() {
                        anyhow::bail!("--apply requires a non-empty --api-key");
                    }
                    let client = HttpVaultClient::new(&server_url, &api_key).await?;
                    run_backfill(&args, &client).await?
                };

                if !apply {
                    println!(
                        "backfill-changelog [DRY-RUN]: parsed={} would_create={} skipped_meta={}",
                        report.parsed, report.would_create, report.skipped_meta
                    );
                } else {
                    println!(
                        "backfill-changelog: parsed={} created={} skipped={} skipped_meta={}",
                        report.parsed, report.created, report.skipped, report.skipped_meta
                    );
                }
                Ok(())
            }
            ProjectMapCmd::Scope {
                project,
                root,
                vault,
            } => {
                use gradatum_admin::project_map_scope::project_scope;
                let scope = project_scope(&root, &vault, &project).await?;
                println!("project-map scope: {}", scope.project);
                println!(
                    "  version courante : {}",
                    scope.current_version.as_deref().unwrap_or("—")
                );
                println!("  cartes total     : {}", scope.total_count);
                println!("  OPEN             : {}", scope.open_count);
                println!("  IN_PROGRESS      : {}", scope.in_progress_count);
                println!("  BLOCKED          : {}", scope.blocked_count);
                println!("  DONE             : {}", scope.done_count);
                if !scope.versions.is_empty() {
                    println!("  versions         : {}", scope.versions.join(", "));
                }
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
