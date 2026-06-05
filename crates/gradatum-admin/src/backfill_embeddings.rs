//! Sub-commande `gradatum-admin backfill-embeddings` — Phase 2.1.1 alpha.8.
//!
//! Scan les notes sans embedding (LEFT JOIN `note_embeddings`) et enqueue
//! des jobs `embed_note` pour le worker. Idempotent : si un embedding existe
//! déjà pour une note, elle est exclue via le LEFT JOIN.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum --tenant main --limit 100
//! ```
//!
//! ## Chemins attendus (convention install standard)
//! - Queue   : `<root>/db/queue.sqlite`
//! - Index   : `<root>/vault/.gradatum/index.db`

use anyhow::{Context, Result};
use gradatum_queue::Queue;
use std::path::PathBuf;

/// Arguments du sub-commande `backfill-embeddings`.
#[derive(Debug, Clone)]
pub struct BackfillArgs {
    /// Répertoire racine Gradatum (ex. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Tenant à traiter (défaut : `"main"`).
    pub tenant: Option<String>,
    /// Limite du nombre de notes à enqueuer (illimité si absent).
    pub limit: Option<usize>,
}

/// Scan les notes sans embedding et enqueue des jobs `embed_note`.
///
/// Retourne le nombre de jobs enqueued.
///
/// # Erreurs
/// - `queue.sqlite` ou `index.db` absents → erreur descriptive.
/// - Erreur SQLite lors du scan ou de l'enqueue.
pub async fn backfill(args: BackfillArgs) -> Result<usize> {
    let queue_path = args.root.join("db/queue.sqlite");
    let index_path = args.root.join("vault/.gradatum/index.db");

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

    // ── Collecte des notes sans embedding (synchrone — rusqlite direct) ──────
    // On ouvre l'index en lecture seule pour le scan : pas besoin des PRAGMA
    // WAL ou des migrations (schéma déjà existant).
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

    // ── Enqueue via SqliteQueue (async) ───────────────────────────────────────
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

        // Progression : log toutes les 100 notes
        if (batch_start + 1) % 100 == 0 {
            eprintln!("backfill: {enqueued}/{total} jobs enqueued...");
        }
    }

    eprintln!("backfill: {enqueued} jobs enqueued (tenant='{tenant}')");
    Ok(enqueued)
}

/// Collecte les `(note_id, body_text)` des notes sans embedding.
///
/// Utilise un LEFT JOIN sur `note_embeddings` — idempotent par construction.
/// Retourne une `Vec` allouée d'un coup pour libérer le `Connection` avant
/// l'ouverture asynchrone de la queue.
fn collect_unembedded_notes(
    index_path: &std::path::Path,
    tenant: &str,
    limit: Option<usize>,
) -> Result<Vec<(String, String)>> {
    let conn = rusqlite::Connection::open(index_path).context("ouverture index.db en lecture")?;

    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    // LEFT JOIN : seules les notes dont il n'existe AUCUNE ligne dans
    // note_embeddings sont sélectionnées (quel que soit l'embedder_id).
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
