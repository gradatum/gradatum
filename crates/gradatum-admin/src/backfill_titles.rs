//! Sub-commande `gradatum-admin backfill-titles` — Phase 2.x.5 alpha.15 Task 21.
//!
//! Itère les notes WHERE title IS NULL dans une DB SQLite gradatum,
//! extrait le H1 Markdown via `extract_h1_title`, et UPDATE la colonne title.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-titles --root /var/lib/gradatum --dry-run
//! gradatum-admin backfill-titles --root /var/lib/gradatum --tenant main
//! ```
//!
//! Idempotent : re-exécuter sur une DB déjà backfillée ne modifie rien
//! (WHERE title IS NULL → 0 résultats).
//!
//! Backup obligatoire avant exécution en production (voir spec §7.5 Phase 5).

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Arguments du sub-commande `backfill-titles`.
#[derive(Debug, Clone)]
pub struct BackfillTitlesArgs {
    /// Répertoire racine Gradatum (ex. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Tenant à traiter (défaut : `"main"`).
    pub tenant: String,
    /// Mode dry-run : calcule et affiche les titres sans les persister.
    pub dry_run: bool,
    /// Limite du nombre de notes à traiter (illimité si absent).
    pub limit: Option<usize>,
}

/// Rapport du backfill de titres.
#[derive(Debug, Default, Clone)]
pub struct BackfillTitlesReport {
    /// Nombre de notes sans titre scannées.
    pub notes_scanned: usize,
    /// Nombre de notes pour lesquelles un H1 a été extrait.
    pub titles_extracted: usize,
    /// Nombre de notes effectivement mises à jour (0 en dry-run).
    pub titles_updated: usize,
    /// Notes sans H1 valide (H1 absent ou H1 vide après strip).
    pub titles_no_h1: usize,
}

/// Backfille les titres manquants via `extract_h1_title`.
///
/// Itère les notes WHERE `title IS NULL` pour le tenant donné, extrait le H1
/// Markdown et met à jour la colonne `title`. Les notes sans H1 valide sont
/// ignorées silencieusement (comptées dans `titles_no_h1`).
///
/// # Erreurs
///
/// Retourne une erreur si la DB est inaccessible ou si un UPDATE échoue.
///
/// # Notes
///
/// - Idempotent : re-run sur DB déjà backfillée → `titles_updated = 0`.
/// - Pattern skip H1 vide OBLIGATOIRE : `Some("")` est sémantiquement distinct
///   de NULL ; écrire `title = ""` empêcherait un second passage (WHERE title
///   IS NULL ne sélectionnerait plus la note).
pub async fn backfill_titles(args: BackfillTitlesArgs) -> Result<BackfillTitlesReport> {
    let db_path = args.root.join("vault/.gradatum/index.db");

    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            db_path.display()
        );
    }

    tokio::task::spawn_blocking(move || {
        run_backfill_sync(&db_path, &args.tenant, args.dry_run, args.limit)
    })
    .await
    .context("spawn_blocking backfill_titles")?
}

/// Exécution synchrone du backfill (appelée depuis spawn_blocking).
fn run_backfill_sync(
    db_path: &std::path::Path,
    tenant: &str,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<BackfillTitlesReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("ouverture index.db pour backfill-titles")?;

    // Activer WAL pour la lecture/écriture concurrente avec gradatum-server.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    let query = format!(
        "SELECT id, body_text FROM notes \
         WHERE (title IS NULL OR title = '') AND vault_id = ?1 \
         ORDER BY created ASC \
         {limit_clause}"
    );

    // Collecte dans un Vec pour libérer le Statement avant les UPDATEs.
    let mut stmt = conn
        .prepare(&query)
        .context("préparation SELECT notes sans titre")?;

    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![tenant], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("exécution SELECT notes sans titre")?
        .collect::<std::result::Result<_, _>>()
        .context("collecte notes sans titre")?;

    drop(stmt);

    let notes_scanned = rows.len();
    let mut report = BackfillTitlesReport {
        notes_scanned,
        ..Default::default()
    };

    if dry_run {
        // En dry-run : calculer les stats sans écrire en DB.
        for (_id, body) in &rows {
            match gradatum_curator::extract_h1_title(body) {
                Some(title) if !title.is_empty() => {
                    report.titles_extracted += 1;
                    // titles_updated reste 0 : dry-run, pas d'écriture.
                }
                Some(_) | None => {
                    report.titles_no_h1 += 1;
                }
            }
        }
        return Ok(report);
    }

    // Mode apply : UPDATE dans une transaction unique.
    let tx = conn
        .unchecked_transaction()
        .context("début transaction backfill-titles")?;

    for (id, body) in &rows {
        // La durée de vie de `extract_h1_title` est liée à `body`.
        // On clone le titre pour l'utiliser dans le param SQL.
        match gradatum_curator::extract_h1_title(body) {
            Some(title) if !title.is_empty() => {
                let title_owned = title.to_string();
                report.titles_extracted += 1;
                tx.execute(
                    "UPDATE notes SET title = ?1 WHERE id = ?2",
                    rusqlite::params![title_owned, id],
                )
                .context("UPDATE title")?;
                report.titles_updated += 1;
            }
            Some(_) | None => {
                // H1 absent ou H1 vide après strip → skip silencieux.
                report.titles_no_h1 += 1;
            }
        }
    }

    tx.commit().context("commit transaction backfill-titles")?;

    Ok(report)
}
