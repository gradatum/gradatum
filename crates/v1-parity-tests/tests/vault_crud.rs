//! v1-parity : Vault CRUD — 5 tests (D5 / spec §0.4 A3)
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_phase1.rs`.
//! Domaine : création vault, écriture notes, layout locus, reopen, invariant tenant_id.

mod common;

use gradatum_core::scope::{LocusId, VaultId};
use gradatum_vault::Vault;
use tempfile::TempDir;

// --- 1. create_vault_and_persist_note ---

/// Crée un vault, écrit une note, vérifie que le fichier .md existe sur disque.
#[tokio::test]
async fn create_vault_and_persist_note() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let fm = common::minimal_frontmatter("main");
    let note = vault
        .write_note(
            fm,
            "Corps de la note — test create_vault_and_persist_note.".into(),
        )
        .await
        .expect("write_note");

    // Le fichier .md doit exister à <root>/main/<id>.md
    let md_path = tmp.path().join("main").join(format!("{}.md", note.id));
    assert!(
        md_path.exists(),
        "Le fichier .md doit exister à {md_path:?}"
    );

    // Le ContentHash doit être non-nul
    assert_ne!(
        note.content_hash.hex(),
        "0000000000000000000000000000000000000000000000000000000000000000",
        "ContentHash ne doit pas être zéro"
    );
}

// --- 2. multiple_notes_distinct_ids ---

/// Écrit 3 notes dans le vault, vérifie que les NoteIds sont tous distincts
/// et que les ContentHash sont distincts (corps différents).
#[tokio::test]
async fn multiple_notes_distinct_ids() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let mut ids = Vec::new();
    let mut hashes = Vec::new();

    for i in 0..3_u8 {
        let fm = common::minimal_frontmatter("main");
        let note = vault
            .write_note(
                fm,
                format!("Corps distinct #{i} — données uniques par note."),
            )
            .await
            .expect("write_note");
        ids.push(note.id.to_string());
        hashes.push(note.content_hash.hex());
    }

    // Tous les IDs doivent être uniques
    let unique_ids: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique_ids.len(), 3, "Les 3 NoteIds doivent être distincts");

    // Tous les ContentHash doivent être uniques (bodies différents)
    let unique_hashes: std::collections::HashSet<_> = hashes.iter().collect();
    assert_eq!(
        unique_hashes.len(),
        3,
        "Les 3 ContentHash doivent être distincts"
    );
}

// --- 3. vault_with_locus_creates_subdirectory ---

/// Écrit une note avec locus="decisions" — vérifie que le fichier est dans
/// <root>/main/decisions/<id>.md.
#[tokio::test]
async fn vault_with_locus_creates_subdirectory() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let mut fm = common::minimal_frontmatter("main");
    fm.locus = Some(LocusId::new("decisions"));

    let note = vault
        .write_note(
            fm,
            "Note dans le locus decisions — test locus subdirectory.".into(),
        )
        .await
        .expect("write_note avec locus");

    let md_path = tmp
        .path()
        .join("main")
        .join("decisions")
        .join(format!("{}.md", note.id));

    assert!(
        md_path.exists(),
        "Le fichier .md doit être dans le sous-répertoire locus : {md_path:?}"
    );
}

// --- 4. vault_open_after_create_preserves_state ---

/// Crée un vault + écrit une note, drop le vault, rouvre, vérifie que le fichier
/// .md est toujours présent (persistance disque, pas seulement mémoire).
#[tokio::test]
async fn vault_open_after_create_preserves_state() {
    let tmp = TempDir::new().unwrap();

    let note_id = {
        let vault = Vault::create(tmp.path(), VaultId::new("main"))
            .await
            .expect("vault::create");
        let fm = common::minimal_frontmatter("main");
        let note = vault
            .write_note(fm, "Note persistée avant reopen.".into())
            .await
            .expect("write_note");
        note.id.to_string()
    };
    // Ici le vault est droppé — handles fermés.

    // Rouvre le vault existant
    let _vault2 = Vault::open(tmp.path())
        .await
        .expect("vault::open après create");

    // Le fichier .md doit toujours exister
    let md_path = tmp.path().join("main").join(format!("{note_id}.md"));
    assert!(
        md_path.exists(),
        "Le fichier .md doit être présent après reopen : {md_path:?}"
    );
}

// --- 5. tenant_id_defaults_to_main ---

/// Vérifie que le vault créé avec le tenant "main" expose bien "main" via
/// `vault.tenant_id()` — invariant D10 (tenant_id mandatory).
///
/// Note : Vault::create exige un VaultId explicite — le fallback "main" est assuré
/// par Vault::open (lit config.toml). Ce test valide le chemin create.
#[tokio::test]
async fn tenant_id_defaults_to_main() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    assert_eq!(
        vault.tenant_id().as_str(),
        "main",
        "tenant_id doit être 'main'"
    );

    // Le répertoire tenant doit exister
    assert!(
        tmp.path().join("main").is_dir(),
        "Le répertoire <root>/main/ doit exister"
    );
}
