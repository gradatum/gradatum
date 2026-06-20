//! `gradatum-admin vault rename <old> <new>` sub-command.
//!
//! Renames a note in the vault:
//! 1. Looks up the note by current title (`title = 'Old'`, `status = 'live'`).
//! 2. Updates `notes.title = 'New'`.
//! 3. Records `redirect_table(slug("Old") → note_id, renamed_at = now_ms)`.
//!
//! **Does not modify source notes**: only index metadata is touched
//! (`title` column + `redirect_table`). The Markdown body on disk is unchanged.
//!
//! ## Usage
//! ```text
//! gradatum-admin vault rename "Old Title" "New Title" --root /var/lib/gradatum
//! ```
//!
//! ## Idempotence
//!
//! If the note is already renamed (current title = `new`), the command fails with
//! a descriptive error. If the `old` title does not exist, an explicit error is returned.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use gradatum_index::links::title_to_slug;

/// Arguments for the `vault rename` sub-command.
#[derive(Debug, Clone)]
pub struct VaultRenameArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Current title of the note (must exist with `status='live'`).
    pub ancien: String,
    /// New title to apply.
    pub nouveau: String,
    /// Target tenant (`vault_id`), default `"main"`.
    pub tenant: String,
}

/// Report for a rename operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRenameReport {
    /// ID of the renamed note.
    pub note_id: String,
    /// Slug recorded in `redirect_table`.
    pub slug: String,
}

/// Renames a note directly in SQLite without going through the HTTP server.
///
/// ## Atomicity
///
/// The `notes.title` UPDATE and the `redirect_table` INSERT are performed inside
/// a single transaction — either both succeed or neither does.
///
/// ## Errors
///
/// - `ancien` not found (`status='live'`, `vault_id=tenant`) → explicit error.
/// - Multiple notes share the same title → the first (`ORDER BY created ASC`) is renamed.
/// - SQLite failure → propagated via `anyhow`.
pub async fn vault_rename(args: VaultRenameArgs) -> Result<VaultRenameReport> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = vault_index_path(&args.root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            db_path.display()
        );
    }
    let ancien = args.ancien.clone();
    let nouveau = args.nouveau.clone();
    let tenant = args.tenant.clone();
    tokio::task::spawn_blocking(move || run_rename_sync(&db_path, &tenant, &ancien, &nouveau))
        .await
        .context("spawn_blocking vault_rename")?
}

/// Synchronous rename implementation; called from `spawn_blocking`.
fn run_rename_sync(
    db_path: &std::path::Path,
    tenant: &str,
    ancien: &str,
    nouveau: &str,
) -> Result<VaultRenameReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("ouverture index.db pour vault-rename")?;

    // WAL for concurrent access alongside gradatum-server.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    // 1. Find the note ID with the old title (live notes only).
    let note_id: String = conn
        .query_row(
            "SELECT id FROM notes
             WHERE vault_id = ?1 AND title = ?2 AND status = 'live'
             ORDER BY created ASC
             LIMIT 1",
            rusqlite::params![tenant, ancien],
            |row| row.get(0),
        )
        .with_context(|| {
            format!(
                "note '{ancien}' introuvable (status='live', vault_id='{tenant}') — \
                 vérifier le titre exact ou que la note est active"
            )
        })?;

    let slug = title_to_slug(ancien);
    let renamed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 2. Atomic transaction: UPDATE title + INSERT redirect.
    let tx = conn
        .unchecked_transaction()
        .context("début transaction vault-rename")?;

    // UPDATE notes.title.
    let rows_updated = tx
        .execute(
            "UPDATE notes SET title = ?1 WHERE id = ?2",
            rusqlite::params![nouveau, note_id],
        )
        .context("UPDATE notes.title")?;

    if rows_updated == 0 {
        anyhow::bail!("UPDATE title n'a affecté aucune ligne pour note_id={note_id}");
    }

    // INSERT OR REPLACE into redirect_table (idempotent).
    tx.execute(
        "INSERT OR REPLACE INTO redirect_table (title_slug, ulid, renamed_at) \
         VALUES (?1, ?2, ?3)",
        rusqlite::params![slug, note_id, renamed_at_ms],
    )
    .context("INSERT redirect_table")?;

    tx.commit().context("commit transaction vault-rename")?;

    Ok(VaultRenameReport { note_id, slug })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Prépare une DB in-memory SQLite migrée avec une note seed pour les tests.
    ///
    /// Retourne `(TempDir, PathBuf vers index.db)`.
    async fn setup_db_with_note(title: &str) -> (TempDir, PathBuf, String) {
        use gradatum_core::scope::VaultId;

        // Vault réel dans TempDir pour que index.db existe sur disque
        let tmp = TempDir::new().expect("TempDir vault_rename test");
        let vault_path = tmp.path().join("vault");
        let vault = gradatum_vault::Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create vault_rename test");

        let idx = vault.index();

        // Seed une note avec le titre initial
        let ulid_str = ulid::Ulid::new().to_string();
        idx.seed_note_with_fts(&ulid_str, "decisions", &format!("# {title}\ncorps."))
            .await
            .expect("seed_note_with_fts");
        // Mettre le titre dans la colonne title
        let nid = gradatum_core::identity::NoteId(
            ulid::Ulid::from_string(&ulid_str).expect("ULID parse setup"),
        );
        idx.upsert_note_title(&nid, title)
            .await
            .expect("upsert_note_title setup");

        let db_path = vault_path.join(".gradatum/index.db");
        (tmp, db_path, ulid_str)
    }

    #[tokio::test]
    async fn vault_rename_updates_title_and_creates_redirect() {
        let (_tmp, db_path, ulid_str) = setup_db_with_note("Ancien Titre Test").await;

        // root = tmp.path() (la fn vault_rename dérive : root/vault/.gradatum/index.db)
        let root = db_path
            .parent() // .gradatum/
            .unwrap()
            .parent() // vault/
            .unwrap()
            .parent() // tmp/
            .unwrap()
            .to_path_buf();
        let args = VaultRenameArgs {
            root,
            ancien: "Ancien Titre Test".to_string(),
            nouveau: "Nouveau Titre Test".to_string(),
            tenant: "main".to_string(),
        };
        let report = vault_rename(args)
            .await
            .expect("vault_rename ne doit pas échouer");

        assert_eq!(
            report.note_id, ulid_str,
            "le ULID retourné doit correspondre"
        );
        assert_eq!(
            report.slug,
            title_to_slug("Ancien Titre Test"),
            "le slug doit être la normalisation de l'ancien titre"
        );

        // Vérifier en DB : title = "Nouveau Titre Test"
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        let new_title: String = conn
            .query_row("SELECT title FROM notes WHERE id = ?1", [&ulid_str], |r| {
                r.get(0)
            })
            .expect("SELECT title");
        assert_eq!(
            new_title, "Nouveau Titre Test",
            "notes.title doit être mis à jour"
        );

        // Vérifier redirect_table : slug("Ancien") → ULID
        let redirect_ulid: String = conn
            .query_row(
                "SELECT ulid FROM redirect_table WHERE title_slug = ?1",
                [&report.slug],
                |r| r.get(0),
            )
            .expect("SELECT redirect_table");
        assert_eq!(
            redirect_ulid, ulid_str,
            "redirect_table doit contenir le ULID"
        );
    }

    #[tokio::test]
    async fn vault_rename_returns_error_when_note_not_found() {
        let (_tmp, db_path, _) = setup_db_with_note("Titre Existant").await;

        let root = db_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let args = VaultRenameArgs {
            root,
            ancien: "Titre Inexistant XYZ".to_string(),
            nouveau: "Nouveau".to_string(),
            tenant: "main".to_string(),
        };
        let result = vault_rename(args).await;
        assert!(
            result.is_err(),
            "titre inexistant doit retourner une erreur"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("introuvable"),
            "message d'erreur doit mentionner 'introuvable' : {msg}"
        );
    }
}
