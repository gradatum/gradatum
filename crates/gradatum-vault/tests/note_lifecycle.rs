//! Tests d'intégration T11b — `write_note` : persist .md + upsert index.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

#[tokio::test]
async fn write_note_persists_md_on_disk() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "corps de la note".into())
        .await
        .unwrap();

    // Le fichier .md doit exister sous <root>/main/<id>.md
    let md_path = dir.path().join("main").join(format!("{}.md", note.id));
    assert!(
        md_path.exists(),
        "le fichier .md doit exister sur disque : {}",
        md_path.display()
    );
}

#[tokio::test]
async fn write_note_upserts_content_hash_in_index() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault.write_note(fm, "body test".into()).await.unwrap();

    // L'index SQLite doit avoir le content_hash de la note
    let stored = vault.index().get_content_hash(note.id).await.unwrap();
    assert_eq!(
        stored,
        Some(note.content_hash),
        "le content_hash doit être indexé dans SQLite"
    );
}

#[tokio::test]
async fn write_note_content_hash_integrity() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault.write_note(fm, "test intégrité".into()).await.unwrap();

    // verify_integrity() doit passer (hash recalculé == hash stocké)
    note.verify_integrity()
        .expect("verify_integrity doit passer pour une note fraîchement écrite");
}

#[tokio::test]
async fn write_note_sets_vault_id() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Frontmatter avec vault_id vide — doit être écrasé par le tenant courant
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = gradatum_core::scope::VaultId::new("");

    let note = vault.write_note(fm, "body".into()).await.unwrap();

    assert_eq!(
        note.frontmatter.vault_id.as_str(),
        "main",
        "vault_id doit être forcé au tenant courant si absent"
    );
}

#[tokio::test]
async fn write_note_with_locus_uses_subdirectory() {
    use gradatum_core::scope::LocusId;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let mut fm = build_minimal_frontmatter();
    fm.locus = Some(LocusId::new("my-locus"));

    let note = vault.write_note(fm, "body".into()).await.unwrap();

    // Le fichier doit être sous <root>/main/my-locus/<id>.md
    let md_path = dir
        .path()
        .join("main")
        .join("my-locus")
        .join(format!("{}.md", note.id));
    assert!(
        md_path.exists(),
        "le fichier .md avec locus doit être dans le sous-répertoire locus"
    );
}

#[tokio::test]
async fn read_note_returns_not_found_phase1_stub() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = gradatum_core::identity::NoteId::new();
    let result = vault.read_note(id).await;

    assert!(
        result.is_err(),
        "read_note Phase 1 stub doit retourner une erreur"
    );
}
