//! Runner de migrations SQL embarquées.
//!
//! Les fichiers SQL sont inclus via `include_str!` au moment de la compilation.
//! Le runner vérifie la table `_schema_migrations` (bootstrappée si absente)
//! et applique uniquement les versions non encore appliquées.
//!
//! ## Attention
//!
//! Le script `0001_phase1.sql` contient lui-même l'INSERT dans `_schema_migrations`.
//! Le runner ne doit donc pas ré-insérer la ligne — il vérifie simplement l'existence
//! de la version avant d'exécuter le batch.

use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use gradatum_core::error::GradatumError;

/// Liste ordonnée des migrations (version, sql).
///
/// L'ordre est la loi — ne jamais réordonner ni supprimer une entrée existante.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_phase1", include_str!("../migrations/0001_phase1.sql")),
    (
        "0002_wikilinks",
        include_str!("../migrations/0002_wikilinks.sql"),
    ),
    (
        "0003_add_tags_to_notes",
        include_str!("../migrations/0003_add_tags_to_notes.sql"),
    ),
    (
        "0004_vault_downgrade",
        include_str!("../migrations/0004_vault_downgrade.sql"),
    ),
    (
        "0005_add_title_column",
        include_str!("../migrations/0005_add_title_column.sql"),
    ),
    (
        "0006_event_log",
        include_str!("../migrations/0006_event_log.sql"),
    ),
    (
        "0007_event_log_agent_id",
        include_str!("../migrations/0007_event_log_agent_id.sql"),
    ),
    (
        "0008_note_cognitive_kind",
        include_str!("../migrations/0008_note_cognitive_kind.sql"),
    ),
    (
        "0009_backfill_title",
        include_str!("../migrations/0009_backfill_title.sql"),
    ),
];

/// Applique les migrations non encore enregistrées dans `_schema_migrations`.
///
/// Bootstrap : crée `_schema_migrations` si la table n'existe pas encore.
/// Idempotent : appellable plusieurs fois sans effet de bord.
pub async fn run(conn: &Arc<Mutex<Connection>>) -> Result<(), GradatumError> {
    let conn = conn.lock().await;

    // Bootstrap : crée la table de tracking si absente.
    // IMPORTANT : le script 0001_phase1.sql crée aussi _schema_migrations et insère
    // la ligne — mais uniquement pour son propre run. Pour les migrations futures, ce
    // bootstrap garantit que la table existe quelle que soit l'état de la DB.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _schema_migrations (
            version TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )
    .map_err(|e| GradatumError::Storage(format!("bootstrap _schema_migrations : {e}")))?;

    for (version, sql) in MIGRATIONS {
        let already_applied: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                [version],
                |row| row.get(0),
            )
            .map_err(|e| {
                GradatumError::Storage(format!("vérification migration {version} : {e}"))
            })?;

        if already_applied {
            continue;
        }

        // Le batch SQL inclut l'INSERT dans _schema_migrations en fin de fichier.
        conn.execute_batch(sql).map_err(|e| {
            GradatumError::Storage(format!("application migration {version} : {e}"))
        })?;
    }

    Ok(())
}
