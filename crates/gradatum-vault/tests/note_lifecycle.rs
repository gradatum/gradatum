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
    let stored = vault
        .index()
        .get_content_hash("main", note.id)
        .await
        .unwrap();
    assert_eq!(
        stored,
        Some(note.content_hash),
        "le content_hash doit être indexé dans SQLite"
    );
}

/// F-165 — le chemin d'écriture d'une note DOIT alimenter `file_checksums`,
/// seule source d'énumération du scan de dérive (`scan_phase_a`). Sans cet upsert
/// la table reste vide et la détection de drift est inerte (régression v1.0.0).
///
/// Oracle indépendant : on relit les bytes réellement écrits sur disque et on
/// recalcule taille + SHA-256 (préfixe 4 Ko et complet) avec `sha2` directement —
/// sans réutiliser les helpers du code testé (anti test auto-référentiel).
#[tokio::test]
async fn write_note_upserts_file_checksum_in_index() {
    use sha2::Digest as _;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "body drift checksum".into())
        .await
        .unwrap();

    // Chemin relatif on-disk du .md sans locus : <tenant>/<id>.md
    let rel_path = format!("main/{}.md", note.id);

    // Oracle : bytes réellement persistés sur disque.
    let md_bytes = std::fs::read(dir.path().join(&rel_path)).unwrap();
    let expected_size = md_bytes.len() as u64;
    let expected_full: [u8; 32] = sha2::Sha256::digest(&md_bytes).into();
    let prefix_len = md_bytes.len().min(4096);
    let expected_prefix: [u8; 32] = sha2::Sha256::digest(&md_bytes[..prefix_len]).into();

    let entries = vault.index().list_file_checksums().await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.relative_path == rel_path)
        .expect("file_checksums doit contenir une entrée pour la note écrite (F-165)");

    assert_eq!(
        entry.file_kind,
        gradatum_core::index::FileKind::Note,
        "file_kind doit être Note"
    );
    assert_eq!(
        entry.expected_size, expected_size,
        "expected_size doit être la taille du .md sur disque"
    );
    assert_eq!(
        entry.expected_hash, expected_full,
        "expected_hash doit être le SHA-256 complet du .md sur disque"
    );
    assert_eq!(
        entry.expected_hash_prefix_4kb, expected_prefix,
        "expected_hash_prefix_4kb doit être le SHA-256 des 4 premiers Ko"
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

/// Read-before-write : deux writes successifs du même id →
/// la note intermédiaire est lisible avec l'ancien contenu avant le 2e write.
#[tokio::test]
async fn write_inner_reads_existing_before_overwrite() {
    use gradatum_core::identity::NoteId;

    let dir = tempfile::TempDir::new().unwrap();
    let vault =
        gradatum_vault::Vault::create(dir.path(), gradatum_core::scope::VaultId::new("main"))
            .await
            .unwrap();

    let id = NoteId::new();
    let fm1 = build_minimal_frontmatter();
    let note_v1 = vault
        .write_note_with_id(fm1, "v1".into(), id)
        .await
        .unwrap();

    // Lire la note après le premier write — doit retourner v1
    let read = vault.read_note(id).await.unwrap();
    assert_eq!(
        read.body.markdown, "v1",
        "lecture après write_v1 doit retourner le body v1"
    );

    // Le content_hash de la version lue doit être celui de v1
    assert_eq!(
        read.content_hash, note_v1.content_hash,
        "le content_hash lu doit correspondre à v1"
    );

    // 2e write sur le même id
    let fm2 = build_minimal_frontmatter();
    vault
        .write_note_with_id(fm2, "v2".into(), id)
        .await
        .unwrap();

    // Après le 2e write, la note courante est v2
    let read_v2 = vault.read_note(id).await.unwrap();
    assert_eq!(
        read_v2.body.markdown, "v2",
        "lecture après write_v2 doit retourner le body v2"
    );
}

/// TDD item C — `write_note_with_id` doit honorer l'ULID fourni par l'appelant.
///
/// Le note_id retourné par `write_note_with_id` ET l'id de la note lue via
/// `read_note(provided)` doivent tous deux être égaux à `provided`.
/// Garantit que write-time id == stored id (wikilinks valides).
#[tokio::test]
async fn write_note_with_id_honors_provided_id() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let fm = build_minimal_frontmatter();
    let provided = gradatum_core::identity::NoteId::new();
    let note = vault
        .write_note_with_id(fm, "## Corps\ntest".to_string(), provided)
        .await
        .expect("write_note_with_id");
    assert_eq!(note.id, provided, "l'id retourné doit être l'id fourni");
    let read = vault.read_note(provided).await.expect("read_note");
    assert_eq!(read.id, provided, "l'id stocké doit être l'id fourni");
}
