//! v1-parity : Persistance vault close+reopen — 2 tests.
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_phase1.rs` (persistence).
//! Domaine : écriture + close + reopen → données intactes + index persisté.

mod common;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

// --- 1. vault_close_reopen_data_intact ---

/// Écrit 5 notes, drop le vault (close implicite), rouvre, lance drift_check
/// → 0 mismatch (données intactes sur disque).
#[tokio::test]
async fn vault_close_reopen_data_intact() {
    let tmp = TempDir::new().unwrap();

    // Phase 1 : écriture
    {
        let vault = Vault::create(tmp.path(), VaultId::new("main"))
            .await
            .expect("vault::create");

        for i in 0..5_u8 {
            let fm = common::minimal_frontmatter("main");
            vault
                .write_note(
                    fm,
                    format!("Note persistée #{i} — test reopen data intact."),
                )
                .await
                .expect("write_note");
        }
        // vault droppé ici — index SQLite et handles fermés
    }

    // Phase 2 : reopen + drift_check
    let vault2 = Vault::open(tmp.path())
        .await
        .expect("vault::open après close");

    let result = vault2
        .drift_check()
        .await
        .expect("drift_check après reopen");

    assert_eq!(
        result.level3_full_hash_mismatch, 0,
        "Aucun mismatch attendu après reopen — données intactes"
    );
    assert!(
        result.missing.is_empty(),
        "Aucun fichier manquant attendu après reopen"
    );
}

// --- 2. index_persists_across_reopens ---

/// Écrit une note, récupère son ContentHash via l'index, drop le vault, rouvre,
/// vérifie que get_content_hash retourne toujours le même hash (index SQLite persisté).
#[tokio::test]
async fn index_persists_across_reopens() {
    let tmp = TempDir::new().unwrap();

    let (note_id, original_hash) = {
        let vault = Vault::create(tmp.path(), VaultId::new("main"))
            .await
            .expect("vault::create");

        let fm = common::minimal_frontmatter("main");
        let note = vault
            .write_note(
                fm,
                "Note dont le hash doit être persisté dans l'index SQLite.".into(),
            )
            .await
            .expect("write_note");

        let hash = note.content_hash.hex();
        (note.id, hash)
        // vault droppé ici
    };

    // Reopen et vérification de l'index
    let vault2 = Vault::open(tmp.path())
        .await
        .expect("vault::open après close");

    let stored_hash = vault2
        .index()
        .get_content_hash(note_id)
        .await
        .expect("get_content_hash")
        .expect("Le ContentHash doit être présent dans l'index après reopen");

    assert_eq!(
        stored_hash.hex(),
        original_hash,
        "Le ContentHash dans l'index doit être identique après reopen"
    );
}
