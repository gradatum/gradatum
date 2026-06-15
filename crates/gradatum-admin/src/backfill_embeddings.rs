//! `gradatum-admin backfill-embeddings` sub-command.
//!
//! Scans notes without an embedding (LEFT JOIN on `note_embeddings`) and enqueues
//! `embed_note` jobs for the worker. Idempotent: notes that already have an
//! embedding are excluded by the LEFT JOIN.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum --tenant main --limit 100
//! ```
//!
//! ## Expected paths (standard install layout)
//! - Queue   : `<root>/db/queue.sqlite`
//! - Index   : `<root>/vault/.gradatum/index.db`

use anyhow::{Context, Result};
use gradatum_core::paths::{queue_db_path, vault_index_path};
use gradatum_queue::Queue;
use std::path::PathBuf;

/// Arguments for the `backfill-embeddings` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Target tenant (default: `"main"`).
    pub tenant: Option<String>,
    /// Maximum number of notes to enqueue; unlimited when absent.
    pub limit: Option<usize>,
}

/// Scans notes without an embedding and enqueues `embed_note` jobs.
///
/// Returns the number of jobs enqueued.
///
/// # Errors
/// - Missing `queue.sqlite` or `index.db` → descriptive error.
/// - SQLite error during scan or enqueue.
pub async fn backfill(args: BackfillArgs) -> Result<usize> {
    // SSOT : chemins via helpers canoniques — jamais root.join(...) manuel.
    let queue_path = queue_db_path(&args.root);
    let index_path = vault_index_path(&args.root);

    if !queue_path.exists() {
        anyhow::bail!(
            "queue.sqlite introuvable : {} — exécuter `gradatum-admin init` d'abord",
            queue_path.display()
        );
    }
    if !index_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            index_path.display()
        );
    }

    let tenant = args.tenant.as_deref().unwrap_or("main").to_string();

    // ── Collect notes without embedding (synchronous — direct rusqlite) ──────
    // Opens the index read-only for the scan: no WAL PRAGMA or migrations
    // needed (schema already exists).
    let candidates = collect_unembedded_notes(&index_path, &tenant, args.limit)
        .context("scan notes sans embedding")?;

    if candidates.is_empty() {
        eprintln!(
            "backfill: 0 jobs enqueued — toutes les notes sont déjà embedded (tenant='{tenant}')"
        );
        return Ok(0);
    }

    let total = candidates.len();
    eprintln!("backfill: {total} notes sans embedding trouvées (tenant='{tenant}') — enqueue...");

    // ── Enqueue via SqliteQueue (async) ──────────────────────────────────────
    let queue = gradatum_queue::SqliteQueue::new(&queue_path)
        .await
        .context("ouverture SqliteQueue")?;

    let mut enqueued = 0usize;
    for (batch_start, (note_id, body_text)) in candidates.into_iter().enumerate() {
        let payload = serde_json::json!({
            "note_id": note_id,
            "body_text": body_text,
        });
        let job = gradatum_queue::NewJob {
            tenant_id: tenant.clone(),
            kind: "embed_note".to_string(),
            payload: serde_json::to_vec(&payload).context("sérialisation payload embed_note")?,
            max_attempts: 3,
        };
        queue.enqueue(job).await.context("enqueue embed_note")?;
        enqueued += 1;

        // Progress: log every 100 notes.
        if (batch_start + 1) % 100 == 0 {
            eprintln!("backfill: {enqueued}/{total} jobs enqueued...");
        }
    }

    eprintln!("backfill: {enqueued} jobs enqueued (tenant='{tenant}')");
    Ok(enqueued)
}

/// Collects `(note_id, body_text)` pairs for notes without an embedding.
///
/// Uses a LEFT JOIN on `note_embeddings` — idempotent by construction.
/// Returns a fully-allocated `Vec` to release the `Connection` before the
/// asynchronous queue is opened.
fn collect_unembedded_notes(
    index_path: &std::path::Path,
    tenant: &str,
    limit: Option<usize>,
) -> Result<Vec<(String, String)>> {
    let conn = rusqlite::Connection::open(index_path).context("ouverture index.db en lecture")?;

    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    // LEFT JOIN: only notes for which no row exists in `note_embeddings`
    // are selected (regardless of `embedder_id`).
    let query = format!(
        "SELECT n.id, n.body_text
         FROM notes n
         LEFT JOIN note_embeddings e ON n.id = e.note_id
         WHERE e.note_id IS NULL
           AND n.vault_id = ?1
         ORDER BY n.id
         {limit_clause}"
    );

    let mut stmt = conn
        .prepare(&query)
        .context("préparation requête backfill")?;
    let rows = stmt
        .query_map(rusqlite::params![tenant], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("exécution requête backfill")?;

    let candidates: Vec<(String, String)> = rows
        .collect::<std::result::Result<_, _>>()
        .context("collecte résultats backfill")?;

    Ok(candidates)
}
