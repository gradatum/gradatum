//! `gradatum-admin vault forget` sub-command.
//!
//! Triggers a semantic forget operation on a given scope.
//!
//! ## Workflow (double-confirmation CLI)
//!
//! 1. **Preview** (`--dry-run`, default): displays candidate and excluded notes.
//! 2. **Confirmation** (`--execute --confirm-ulids <u1,u2,…>`): enqueues a `Job::Forget`
//!    directly into the SQLite queue.
//!
//! Execution is handled by the worker's `handle_forget` function.
//!
//! ## Protected sections
//!
//! `agent-issues` and `council` are automatically excluded from every batch.
//!
//! ## Direct SQLite access
//!
//! - Scope resolution: `<root>/vault/.gradatum/index.db` (WAL)
//! - Job enqueue: `<root>/db/queue.sqlite` (WAL)
//!
//! ## Usage
//!
//! ```text
//! # Dry-run (preview)
//! gradatum-admin vault forget topic --query "project X" --root /var/lib/gradatum
//!
//! # Execute
//! gradatum-admin vault forget topic --query "project X" \
//!     --execute --confirm-ulids "01J...,01J..." --root /var/lib/gradatum
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use gradatum_core::{
    paths::{queue_db_path, vault_index_path},
    section::Section,
    ForgetScope, ForgetSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{apply_sqlite_pragmas, run_migrations, SqliteQueueStore};
use sqlx::SqlitePool;
use ulid::Ulid;

// ── Protected sections ────────────────────────────────────────────────────────
//
// Single source of truth: `Section::PROTECTED_FORGET` in `gradatum-core::section`.
// Consistent with the HTTP handler (`gradatum-server::api_v1::forget`) and the
// worker — ensures that the CLI, API, and worker always exclude the same sections.

fn is_protected(section: &str) -> bool {
    Section::PROTECTED_FORGET
        .iter()
        .any(|s| s.as_str() == section)
}

// ── FTS5 quoting ──────────────────────────────────────────────────────────────
//
// Single source of truth: `gradatum_index::fts5_quote_query`. The local copy
// was removed — one FTS5 quoting algorithm across the whole workspace.
//
// A query such as `lot-c` or `2026-06-10` sent without quoting causes
// `no such column: lot` because the hyphen is interpreted as an FTS5 operator.
use gradatum_index::fts5_quote_query;

// ── Sub-commands ──────────────────────────────────────────────────────────────

/// Scope selector for the `vault forget` command.
#[derive(Debug, Subcommand)]
pub enum ForgetCmd {
    /// Forgets notes matching an FTS query.
    Topic(ForgetTopicArgs),
    /// Forgets notes whose locus starts with a given prefix.
    Locus(ForgetLocusArgs),
    /// Forgets notes produced by a specific agent.
    Agent(ForgetAgentArgs),
}

// ── Common arguments (shared via flatten) ─────────────────────────────────────

/// Arguments shared by all `vault forget` scopes.
#[derive(Debug, Args)]
pub struct ForgetCommonArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Target tenant (`vault_id`), default `"main"`.
    #[arg(long, default_value = "main")]
    pub tenant: String,

    /// Actor triggering the forget (recorded in frontmatters).
    #[arg(long)]
    pub forgotten_by: Option<String>,

    /// Execute the forget operation (dry-run by default when absent).
    #[arg(long)]
    pub execute: bool,

    /// Confirmation note IDs (required with `--execute`).
    ///
    /// Comma-separated list; must match exactly the IDs returned by the preview.
    #[arg(long, value_delimiter = ',')]
    pub confirm_ulids: Vec<String>,
}

// ── Scope-specific arguments ──────────────────────────────────────────────────

/// Arguments for `vault forget topic`.
#[derive(Debug, Args)]
pub struct ForgetTopicArgs {
    /// FTS query string.
    #[arg(long)]
    pub query: String,

    /// Result limit (default 50, max 200).
    #[arg(long)]
    pub limit: Option<usize>,

    #[command(flatten)]
    pub common: ForgetCommonArgs,
}

/// Arguments for `vault forget locus`.
#[derive(Debug, Args)]
pub struct ForgetLocusArgs {
    /// Locus prefix (e.g. `main/projects/old`).
    #[arg(long)]
    pub locus: String,

    #[command(flatten)]
    pub common: ForgetCommonArgs,
}

/// Arguments for `vault forget agent`.
#[derive(Debug, Args)]
pub struct ForgetAgentArgs {
    /// Source agent identifier.
    #[arg(long)]
    pub agent_id: String,

    /// Vault(s) to target (default: all vaults for `--tenant`).
    #[arg(long, value_delimiter = ',')]
    pub vaults: Vec<String>,

    #[command(flatten)]
    pub common: ForgetCommonArgs,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Runs the `vault forget` sub-command.
pub async fn run_forget(cmd: ForgetCmd) -> Result<()> {
    match cmd {
        ForgetCmd::Topic(args) => {
            let scope = ForgetScope::Topic {
                query: args.query.clone(),
                vault: Some(args.common.tenant.clone()),
                limit: Some(args.limit.unwrap_or(50).min(200)),
            };
            run_forget_scope(scope, args.common).await
        }
        ForgetCmd::Locus(args) => {
            let scope = ForgetScope::Locus {
                vault: args.common.tenant.clone(),
                locus: args.locus.clone(),
            };
            run_forget_scope(scope, args.common).await
        }
        ForgetCmd::Agent(args) => {
            let vaults = if args.vaults.is_empty() {
                vec![args.common.tenant.clone()]
            } else {
                args.vaults.clone()
            };
            let scope = ForgetScope::Agent {
                agent_id: args.agent_id.clone(),
                vaults,
            };
            run_forget_scope(scope, args.common).await
        }
    }
}

// ── Core logic ────────────────────────────────────────────────────────────────

async fn run_forget_scope(scope: ForgetScope, common: ForgetCommonArgs) -> Result<()> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = vault_index_path(&common.root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le server doit avoir démarré au moins une fois",
            db_path.display()
        );
    }

    // Resolve scope → (note_id, section).
    let candidates = resolve_scope_direct(&db_path, &scope, &common.tenant).await?;

    // Partition into eligible / excluded.
    let mut eligible: Vec<String> = Vec::new();
    let mut excluded: Vec<(String, String)> = Vec::new();

    for (ulid, section) in candidates {
        if is_protected(&section) {
            excluded.push((ulid, section));
        } else {
            eligible.push(ulid);
        }
    }

    // Print preview.
    println!(
        "=== vault forget preview ({} éligible(s), {} exclue(s)) ===",
        eligible.len(),
        excluded.len(),
    );
    if eligible.is_empty() {
        println!("Aucune note éligible.");
    } else {
        println!("Notes éligibles :");
        for ulid in &eligible {
            println!("  {ulid}");
        }
    }
    if !excluded.is_empty() {
        println!("Notes exclues (sections protégées) :");
        for (ulid, section) in &excluded {
            println!("  {ulid}  (section: {section})");
        }
    }

    // Dry-run: stop here.
    if !common.execute {
        println!(
            "\n[DRY-RUN] Pour exécuter, relancer avec --execute --confirm-ulids \"{}\"",
            eligible.join(","),
        );
        return Ok(());
    }

    // Verify confirm_ulids.
    let mut expected_sorted = eligible.clone();
    expected_sorted.sort();
    let mut confirmed_sorted = common.confirm_ulids.clone();
    confirmed_sorted.sort();

    if expected_sorted != confirmed_sorted {
        anyhow::bail!(
            "confirm_ulids mismatch : {} attendus, {} fournis — relancer en dry-run pour obtenir la liste exacte",
            expected_sorted.len(),
            confirmed_sorted.len(),
        );
    }

    if eligible.is_empty() {
        println!("Aucune note à oublier — opération annulée.");
        return Ok(());
    }

    // Enqueue Job::Forget into queue.sqlite.
    let pool = open_queue_pool(&common.root).await?;
    let store = SqliteQueueStore::new(pool);

    let record = build_forget_job_record(
        scope,
        common.confirm_ulids.clone(),
        common.forgotten_by.clone(),
    );
    let job_ulid = store
        .enqueue(record)
        .await
        .context("erreur enqueue Job::Forget")?;

    println!("\nJob::Forget enqueued : {job_ulid}\nPoll : gradatum-admin jobs get {job_ulid}");
    Ok(())
}

// ── Direct SQLite scope resolution ───────────────────────────────────────────

/// Resolves the scope directly in SQLite without going through the HTTP server.
///
/// Returns `Vec<(note_id, section)>`.
async fn resolve_scope_direct(
    db_path: &PathBuf,
    scope: &ForgetScope,
    tenant: &str,
) -> Result<Vec<(String, String)>> {
    let db_path = db_path.to_owned();
    let scope = scope.clone();
    let tenant = tenant.to_string();
    tokio::task::spawn_blocking(move || resolve_scope_sync(&db_path, &scope, &tenant))
        .await
        .context("spawn_blocking resolve_scope")?
}

fn resolve_scope_sync(
    db_path: &PathBuf,
    scope: &ForgetScope,
    tenant: &str,
) -> Result<Vec<(String, String)>> {
    let conn = rusqlite::Connection::open(db_path)
        .context("ouverture index.db pour vault-forget preview")?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA query_only=1;")
        .context("PRAGMA read-only")?;

    match scope {
        ForgetScope::Topic {
            query,
            vault,
            limit,
        } => {
            let effective_vault = vault.as_deref().unwrap_or(tenant);
            let max_limit = limit.unwrap_or(50).min(200) as i64;

            if query.len() > 512 {
                anyhow::bail!(
                    "query FTS trop longue ({} > 512 chars) — réduire la recherche",
                    query.len()
                );
            }
            if query.trim().is_empty() {
                return Ok(vec![]);
            }

            // FTS5 quoting: each token is wrapped in "..." to neutralise FTS5
            // operators (hyphens, ISO dates, AND/OR/NOT, `*`, `^`, parentheses).
            let quoted_query = fts5_quote_query(query);

            // FTS5 query against note content.
            let mut stmt = conn
                .prepare(
                    "SELECT n.id, n.section
                 FROM notes n
                 JOIN notes_fts f ON f.rowid = n.rowid
                 WHERE f.notes_fts MATCH ?1
                   AND n.vault_id = ?2
                   AND n.status = 'live'
                 ORDER BY rank
                 LIMIT ?3",
                )
                .context("prepare FTS forget topic")?;

            let rows = stmt
                .query_map(
                    rusqlite::params![quoted_query, effective_vault, max_limit],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .context("query FTS forget topic")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect FTS forget topic")?;
            Ok(rows)
        }
        ForgetScope::Locus { vault, locus } => {
            let prefix = format!("{locus}%");
            let mut stmt = conn
                .prepare(
                    "SELECT id, section
                 FROM notes
                 WHERE vault_id = ?1
                   AND locus LIKE ?2
                   AND status = 'live'
                 ORDER BY locus",
                )
                .context("prepare locus forget")?;

            let rows = stmt
                .query_map(rusqlite::params![vault, prefix], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("query locus forget")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect locus forget")?;
            Ok(rows)
        }
        ForgetScope::Agent { agent_id, vaults } => {
            // Matches on `notes.owner = agent_id`.
            let placeholders = vaults
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");

            let sql = if vaults.is_empty() {
                "SELECT id, section FROM notes WHERE owner = ?1 AND status = 'live' ORDER BY created".to_string()
            } else {
                format!(
                    "SELECT id, section FROM notes WHERE owner = ?1 AND vault_id IN ({placeholders}) AND status = 'live' ORDER BY created"
                )
            };

            let mut stmt = conn.prepare(&sql).context("prepare agent forget")?;
            let rows = if vaults.is_empty() {
                stmt.query_map(rusqlite::params![agent_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("query agent forget")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect agent forget")?
            } else {
                // Build dynamic params.
                use rusqlite::types::ToSql;
                let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(vaults.len() + 1);
                params.push(Box::new(agent_id.to_string()));
                for v in vaults {
                    params.push(Box::new(v.clone()));
                }
                let refs: Vec<&dyn ToSql> = params.iter().map(|b| b.as_ref()).collect();
                stmt.query_map(refs.as_slice(), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("query agent forget vaults")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect agent forget vaults")?
            };
            Ok(rows)
        }
        // ForgetScope is #[non_exhaustive] — guard for future variants.
        _ => anyhow::bail!("scope ForgetScope non supporté par cette version de gradatum-admin"),
    }
}

// ── Enqueue ───────────────────────────────────────────────────────────────────

/// Builds a `JobRecord` for a `Job::Forget` job.
fn build_forget_job_record(
    scope: ForgetScope,
    confirm_ulids: Vec<String>,
    forgotten_by: Option<String>,
) -> JobRecord {
    let now = Utc::now();
    let spec = ForgetSpec {
        scope,
        dry_run: false,
        forgotten_by,
        confirm_ulids,
    };
    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Forget(spec),
            class: JobClass::Human,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

// ── Queue pool ────────────────────────────────────────────────────────────────

async fn open_queue_pool(root: &std::path::Path) -> Result<SqlitePool> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = queue_db_path(root);
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = SqlitePool::connect(&url).await.with_context(|| {
        format!(
            "impossible d'ouvrir la queue SQLite : {}",
            db_path.display()
        )
    })?;
    apply_sqlite_pragmas(&pool)
        .await
        .context("erreur pragmas WAL queue")?;
    run_migrations(&pool)
        .await
        .context("erreur migrations queue")?;
    Ok(pool)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_sections_are_excluded() {
        assert!(is_protected("agent-issues"));
        assert!(is_protected("council"));
        assert!(!is_protected("decisions"));
        assert!(!is_protected("retrospectives"));
    }

    #[test]
    fn build_forget_job_record_dry_run_false() {
        let record = build_forget_job_record(
            ForgetScope::Topic {
                query: "test".to_string(),
                vault: None,
                limit: Some(10),
            },
            vec!["01J000000000000000000000A".to_string()],
            Some("admin".to_string()),
        );
        // Le job doit être Pending et en mode réel.
        assert!(matches!(record.lifecycle.status, JobStatus::Pending));
        if let Job::Forget(spec) = &record.spec.kind {
            assert!(!spec.dry_run, "dry_run doit être false en mode réel");
            assert_eq!(spec.confirm_ulids.len(), 1);
        } else {
            panic!("attendu Job::Forget");
        }
    }

    #[test]
    fn confirm_ulids_mismatch_detection() {
        let mut expected = vec!["A".to_string(), "B".to_string()];
        let mut confirmed = vec!["B".to_string()];
        expected.sort();
        confirmed.sort();
        assert_ne!(expected, confirmed);
    }

    /// fts5_quote_query — token unique avec tiret ne doit pas retourner 500 (P1-B).
    #[test]
    fn fts5_quote_query_hyphen_single_token() {
        let q = fts5_quote_query("lot-c");
        assert_eq!(
            q, r#""lot-c""#,
            "tiret dans un token doit être entre guillemets"
        );
    }

    /// fts5_quote_query — date ISO 8601 complète.
    #[test]
    fn fts5_quote_query_iso_date() {
        let q = fts5_quote_query("2026-06-10");
        assert_eq!(q, r#""2026-06-10""#);
    }

    /// fts5_quote_query — plusieurs tokens → chacun entre guillemets.
    #[test]
    fn fts5_quote_query_multiple_tokens() {
        let q = fts5_quote_query("foo bar");
        assert_eq!(q, r#""foo" "bar""#);
    }

    /// fts5_quote_query — query vide retourne chaîne vide.
    #[test]
    fn fts5_quote_query_empty_returns_empty() {
        let q = fts5_quote_query("");
        assert_eq!(q, "");
        let q_ws = fts5_quote_query("   ");
        assert_eq!(q_ws, "");
    }
}
