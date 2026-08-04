//! Tests d'intégration — sub-commande `backfill-embeddings`.
//!
//! Stratégie :
//! - Créer une arborescence TempDir mimant le layout standard.
//! - Ouvrir `SqliteIndex::open(index_path)` pour appliquer les migrations schéma.
//! - Insérer des notes directement via rusqlite (INSERT minimal).
//! - Appeler `backfill()` et vérifier le nombre de jobs enqueued.

use gradatum_admin::BackfillArgs;
use gradatum_index::SqliteIndex;
use tempfile::TempDir;

/// Crée l'arborescence TempDir minimale mimant `/var/lib/gradatum` :
///   - `db/queue.sqlite`  (créé par SqliteQueue::new)
///   - `vault/.gradatum/index.db`  (créé par SqliteIndex::open)
///
/// Retourne `(root, TempDir)` — TempDir doit rester alive le temps du test.
async fn setup_root() -> (std::path::PathBuf, TempDir) {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().to_path_buf();

    // Créer les répertoires
    std::fs::create_dir_all(root.join("db")).expect("mkdir db");
    std::fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");

    // Initialiser le schéma de l'index via SqliteIndex (applique migrations)
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open pour init schéma");
    // Le SqliteIndex est droppé ici → verrou WAL libéré

    // Initialiser la queue via SqliteQueue
    let queue_path = root.join("db/queue.sqlite");
    gradatum_queue::SqliteQueue::new(&queue_path)
        .await
        .expect("SqliteQueue::new pour init schéma");
    // Le SqliteQueue est droppé ici

    (root, tmp)
}

/// Insère une note minimale dans l'index via rusqlite direct.
///
/// `note_id` doit être un ULID-like string unique.
/// Champs obligatoires NOT NULL : `id`, `vault_id`, `section`, `status`,
/// `schema_version`, `created`, `content_hash`, `version`, `body_text`.
fn insert_note(conn: &rusqlite::Connection, note_id: &str, vault_id: &str, body_text: &str) {
    // content_hash : 32 octets zéro (valeur de test, pas un vrai SHA-256)
    let hash = vec![0u8; 32];
    conn.execute(
        "INSERT INTO notes (
            id, vault_id, locus, section, status, schema_version,
            author_kind, author_id, author_display_name,
            created, updated, status_changed, status_reason,
            content_hash, version, body_text, integrity_signature, extra_json, tags
        ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, NULL, NULL, NULL, ?6, NULL, NULL, NULL, ?7, ?8, ?9, NULL, NULL, NULL)",
        rusqlite::params![
            note_id,
            vault_id,
            "reference",  // section
            "indexed",    // status
            1i64,         // schema_version
            1_700_000_000_000i64, // created (epoch ms)
            hash,
            1i64,         // version
            body_text,
        ],
    )
    .expect("INSERT note test");
}

/// Insère un embedding pour une note (marque la note comme déjà embedded).
fn insert_embedding(conn: &rusqlite::Connection, note_id: &str, embedder_id: &str) {
    // vector : 4 octets (f32 = 0.0), dim = 1
    let vector = 0f32.to_le_bytes().to_vec();
    // C4-1e (migration 0033) : `vault_id` NOT NULL — résolu depuis la note parente
    // (insérée au préalable via `insert_note`), cohérent avec le contrat mono-vault du test.
    conn.execute(
        "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, (SELECT vault_id FROM notes WHERE id = ?1))",
        rusqlite::params![
            note_id,
            embedder_id,
            vector,
            1i64,
            1_700_000_000_000i64,
        ],
    )
    .expect("INSERT note_embeddings test");
}

// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn backfill_zero_notes_returns_zero() {
    let (root, _tmp) = setup_root().await;

    let args = BackfillArgs {
        root,
        tenant: Some("main".to_string()),
        limit: None,
    };

    let count = gradatum_admin::backfill(args).await.expect("backfill");
    assert_eq!(count, 0, "aucune note → 0 jobs enqueued");
}

#[tokio::test]
async fn backfill_idempotent_re_run_skips_embedded() {
    let (root, _tmp) = setup_root().await;

    let index_path = root.join("vault/.gradatum/index.db");

    // Insérer 3 notes dans l'index
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        insert_note(&conn, "01AAAA", "main", "Corps de la note 1");
        insert_note(&conn, "01BBBB", "main", "Corps de la note 2");
        insert_note(&conn, "01CCCC", "main", "Corps de la note 3");

        // Marquer les notes 1 et 2 comme déjà embedded
        insert_embedding(&conn, "01AAAA", "bge-small");
        insert_embedding(&conn, "01BBBB", "bge-small");
        // Note 3 (01CCCC) n'a pas d'embedding
    }

    // Run 1 : seule la note 3 doit être enqueued
    let args1 = BackfillArgs {
        root: root.clone(),
        tenant: Some("main".to_string()),
        limit: None,
    };
    let count1 = gradatum_admin::backfill(args1)
        .await
        .expect("backfill run 1");
    assert_eq!(count1, 1, "run 1 : 1 note sans embedding → 1 job enqueued");

    // Marquer la note 3 comme embedded (simuler traitement worker)
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        insert_embedding(&conn, "01CCCC", "bge-small");
    }

    // Run 2 : toutes les notes sont embedded → 0 jobs
    let args2 = BackfillArgs {
        root: root.clone(),
        tenant: Some("main".to_string()),
        limit: None,
    };
    let count2 = gradatum_admin::backfill(args2)
        .await
        .expect("backfill run 2");
    assert_eq!(
        count2, 0,
        "run 2 idempotent : 0 notes sans embedding → 0 jobs"
    );
}
