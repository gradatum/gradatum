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
    pub current_title: String,
    /// New title to apply.
    pub new_title: String,
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
/// ## Vault scoping
///
/// The `notes.title` UPDATE filters `WHERE id = <note_id> AND vault_id = <tenant>`
/// (defense in depth: `note_id` is already resolved within `tenant` by the lookup,
/// so the predicate removes no legitimate row). Without it, a rename targeting one
/// vault would overwrite the title of a homonymous note (same id) in another vault.
///
/// ## Errors
///
/// - `current_title` not found (`status='live'`, `vault_id=tenant`) → explicit error.
/// - Multiple notes share the same title → the first (`ORDER BY created ASC`) is renamed.
/// - SQLite failure → propagated via `anyhow`.
pub async fn vault_rename(args: VaultRenameArgs) -> Result<VaultRenameReport> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = vault_index_path(&args.root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the worker must have started at least once",
            db_path.display()
        );
    }
    let current_title = args.current_title.clone();
    let new_title = args.new_title.clone();
    let tenant = args.tenant.clone();
    tokio::task::spawn_blocking(move || {
        run_rename_sync(&db_path, &tenant, &current_title, &new_title)
    })
    .await
    .context("spawn_blocking vault_rename")?
}

/// Synchronous rename implementation; called from `spawn_blocking`.
fn run_rename_sync(
    db_path: &std::path::Path,
    tenant: &str,
    current_title: &str,
    new_title: &str,
) -> Result<VaultRenameReport> {
    let conn = rusqlite::Connection::open(db_path).context("opening index.db for vault-rename")?;

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
            rusqlite::params![tenant, current_title],
            |row| row.get(0),
        )
        .with_context(|| {
            format!(
                "note '{current_title}' not found (status='live', vault_id='{tenant}') — \
                 check the exact title or that the note is active"
            )
        })?;

    let slug = title_to_slug(current_title);
    let renamed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 2. Atomic transaction: UPDATE title + INSERT redirect.
    let tx = conn
        .unchecked_transaction()
        .context("starting vault-rename transaction")?;

    // UPDATE notes.title, scopé par vault_id (C4-1e, M3 : défense en profondeur).
    //
    // Sans le prédicat `vault_id`, un rename ciblant un tenant écrasait le titre
    // d'une note homonyme (même id) dans un autre vault — le `tenant` est déjà
    // résolu par le lookup ci-dessus (`:95`), simple réutilisation locale.
    let rows_updated = tx
        .execute(
            "UPDATE notes SET title = ?1 WHERE id = ?2 AND vault_id = ?3",
            rusqlite::params![new_title, note_id, tenant],
        )
        .context("UPDATE notes.title")?;

    if rows_updated == 0 {
        anyhow::bail!("UPDATE title affected no row for note_id={note_id}");
    }

    // INSERT OR REPLACE into redirect_table (idempotent), scopé par vault_id
    // (Groupe B, M4 : PK composite `(vault_id, title_slug)`, migration 0035).
    // Le `tenant` est le vault courant, déjà résolu par le lookup ci-dessus (`:99`).
    tx.execute(
        "INSERT OR REPLACE INTO redirect_table (vault_id, title_slug, ulid, renamed_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![tenant, slug, note_id, renamed_at_ms],
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
        idx.upsert_note_title("main", &nid, title)
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
            current_title: "Ancien Titre Test".to_string(),
            new_title: "Nouveau Titre Test".to_string(),
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

    /// Prépare une DB avec 2 vaults ("main" + "vault-b") partageant le MÊME id de
    /// note (collision volontaire) — harnais isolation cross-vault (C4-1e, M3/A6).
    ///
    /// Vault physique unique (`root/vault/.gradatum/index.db`) ; deux lignes `notes`
    /// y coexistent grâce à la clé composite `(vault_id, id)` (migration 0032).
    /// Titre distinct par vault pour détecter une fuite d'écriture cross-vault.
    ///
    /// Retourne `(TempDir, PathBuf racine, String ULID colliding)`.
    async fn setup_db_with_colliding_notes(
        title_main: &str,
        title_b: &str,
    ) -> (TempDir, PathBuf, String) {
        use gradatum_core::scope::VaultId;

        let tmp = TempDir::new().expect("TempDir vault_rename cross-vault test");
        let vault_path = tmp.path().join("vault");
        let vault = gradatum_vault::Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create vault_rename cross-vault test");

        let idx = vault.index();
        let ulid_str = ulid::Ulid::new().to_string();
        let nid = gradatum_core::identity::NoteId(
            ulid::Ulid::from_string(&ulid_str).expect("ULID parse setup collision"),
        );

        // Note "main".
        idx.seed_note_with_fts(&ulid_str, "decisions", &format!("# {title_main}\ncorps."))
            .await
            .expect("seed_note_with_fts main");
        idx.upsert_note_title("main", &nid, title_main)
            .await
            .expect("upsert_note_title main");

        // Note "vault-b", MÊME id que "main" — collision volontaire.
        idx.seed_note_with_fts_vault(
            &ulid_str,
            "vault-b",
            "decisions",
            None,
            &format!("# {title_b}\ncorps."),
        )
        .await
        .expect("seed_note_with_fts_vault vault-b");
        idx.upsert_note_title("vault-b", &nid, title_b)
            .await
            .expect("upsert_note_title vault-b");

        let db_path = vault_path.join(".gradatum/index.db");
        let root = db_path
            .parent() // .gradatum/
            .unwrap()
            .parent() // vault/
            .unwrap()
            .parent() // tmp/
            .unwrap()
            .to_path_buf();
        (tmp, root, ulid_str)
    }

    /// Test ON (isolation cross-vault) : renommer une note dans `vault-b` ne doit PAS
    /// toucher la note homonyme (même id) de `main`. RED avant le fix de `:122`
    /// (`UPDATE notes SET title = ?1 WHERE id = ?2` sans prédicat `vault_id`) :
    /// l'UPDATE touche les deux lignes puisqu'elles partagent le même `id`.
    #[tokio::test]
    async fn vault_rename_does_not_cross_vault() {
        let (_tmp, root, ulid_str) = setup_db_with_colliding_notes("doc-main", "doc-b").await;

        let args = VaultRenameArgs {
            root,
            current_title: "doc-b".to_string(),
            new_title: "doc-b-renamed".to_string(),
            tenant: "vault-b".to_string(),
        };
        let report = vault_rename(args)
            .await
            .expect("vault_rename vault-b ne doit pas échouer");
        assert_eq!(report.note_id, ulid_str);

        // Relecture directe via le chemin connu construit par le setup
        // (root/vault/.gradatum/index.db — SSOT `vault_index_path`).
        let db_path = vault_index_path(_tmp.path());
        let conn = rusqlite::Connection::open(&db_path).expect("open db post-rename");

        let main_title: String = conn
            .query_row(
                "SELECT title FROM notes WHERE id = ?1 AND vault_id = 'main'",
                [&ulid_str],
                |r| r.get(0),
            )
            .expect("SELECT title main");
        assert_eq!(
            main_title, "doc-main",
            "le titre de la note `main` ne doit PAS être écrasé par un rename ciblant `vault-b`"
        );

        let b_title: String = conn
            .query_row(
                "SELECT title FROM notes WHERE id = ?1 AND vault_id = 'vault-b'",
                [&ulid_str],
                |r| r.get(0),
            )
            .expect("SELECT title vault-b");
        assert_eq!(
            b_title, "doc-b-renamed",
            "le titre de la note `vault-b` doit refléter le rename"
        );
    }

    /// Test OFF (régime mono-vault, byte-identical) : le rename met bien à jour le
    /// titre et crée le redirect — comportement inchangé par le durcissement de `:122`.
    #[tokio::test]
    async fn vault_rename_single_vault_unchanged() {
        let (_tmp, db_path, ulid_str) = setup_db_with_note("Titre Mono Vault").await;

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
            current_title: "Titre Mono Vault".to_string(),
            new_title: "Titre Mono Vault Renommé".to_string(),
            tenant: "main".to_string(),
        };
        let report = vault_rename(args)
            .await
            .expect("vault_rename mono-vault ne doit pas échouer");
        assert_eq!(report.note_id, ulid_str);

        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        let title: String = conn
            .query_row("SELECT title FROM notes WHERE id = ?1", [&ulid_str], |r| {
                r.get(0)
            })
            .expect("SELECT title");
        assert_eq!(title, "Titre Mono Vault Renommé");

        let redirect_ulid: String = conn
            .query_row(
                "SELECT ulid FROM redirect_table WHERE title_slug = ?1",
                [&report.slug],
                |r| r.get(0),
            )
            .expect("SELECT redirect_table");
        assert_eq!(redirect_ulid, ulid_str);
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
            current_title: "Titre Inexistant XYZ".to_string(),
            new_title: "Nouveau".to_string(),
            tenant: "main".to_string(),
        };
        let result = vault_rename(args).await;
        assert!(
            result.is_err(),
            "titre inexistant doit retourner une erreur"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found"),
            "message d'erreur doit mentionner 'not found' : {msg}"
        );
    }
}
