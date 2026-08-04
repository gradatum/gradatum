//! Tests d'intégration F-100 incrément 1.6 — `Vault::archive_note`.
//!
//! Vérifie que l'archivage DÉPLACE le `.md` (+ `.history/`) sous `.archive/` en miroir
//! de la structure d'origine et inscrit une ligne dans le registre `archive_index`,
//! sans dé-indexer (la cascade index est à la charge de l'appelant serveur).

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::{LocusId, VaultId};
use gradatum_vault::Vault;
use tempfile::TempDir;

const FAR_FUTURE_GC_DUE: i64 = 9_999_999_999_999;

/// Archivage d'une note avec locus : `.md` déplacé vers `.archive/main/<locus>/<id>.md`,
/// absent de l'origine, registre peuplé avec tous les champs.
#[tokio::test]
async fn archive_moves_md_with_locus_and_records_registry() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    let mut fm = build_minimal_frontmatter();
    fm.locus = Some(LocusId::new("projets"));
    vault
        .write_note_with_id(fm, "# Titre archive\n\ncorps".into(), id)
        .await
        .unwrap();

    let origin = dir
        .path()
        .join("main")
        .join("projets")
        .join(format!("{id}.md"));
    assert!(origin.exists(), "le .md doit exister avant archivage");

    let outcome = vault
        .archive_note(id, Some("test-admin".into()), FAR_FUTURE_GC_DUE)
        .await
        .expect("archive_note");

    // Origine vidée, destination miroir peuplée.
    assert!(!origin.exists(), "le .md ne doit plus exister à l'origine");
    let archived = dir
        .path()
        .join(".archive")
        .join("main")
        .join("projets")
        .join(format!("{id}.md"));
    assert!(
        archived.exists(),
        "le .md doit exister sous .archive/ : {}",
        archived.display()
    );
    assert_eq!(
        outcome.archive_path,
        format!(".archive/main/projets/{id}.md")
    );
    assert_eq!(outcome.section, "decisions");
    assert_eq!(outcome.original_locus.as_deref(), Some("projets"));

    // Registre : archive active présente, champs complets.
    let entry = vault
        .index()
        .get_active_archive("main", &id.to_string())
        .await
        .expect("get_active_archive")
        .expect("archive active enregistrée");
    assert_eq!(entry.note_id, id.to_string());
    assert_eq!(entry.section, "decisions");
    assert_eq!(entry.original_locus.as_deref(), Some("projets"));
    assert_eq!(entry.archive_path, format!(".archive/main/projets/{id}.md"));
    assert_eq!(entry.archived_by.as_deref(), Some("test-admin"));
    assert_eq!(entry.gc_due, FAR_FUTURE_GC_DUE);
    assert!(entry.gc_at.is_none() && entry.restored_at.is_none());
}

/// Archivage d'une note SANS locus : `.md` déplacé vers `.archive/main/<id>.md`.
#[tokio::test]
async fn archive_moves_md_without_locus() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    vault
        .write_note_with_id(build_minimal_frontmatter(), "corps racine".into(), id)
        .await
        .unwrap();
    let origin = dir.path().join("main").join(format!("{id}.md"));
    assert!(origin.exists());

    let outcome = vault
        .archive_note(id, None, FAR_FUTURE_GC_DUE)
        .await
        .expect("archive_note");

    assert!(!origin.exists());
    assert!(
        dir.path()
            .join(".archive")
            .join("main")
            .join(format!("{id}.md"))
            .exists(),
        "le .md doit exister sous .archive/main/"
    );
    assert_eq!(outcome.archive_path, format!(".archive/main/{id}.md"));
    assert_eq!(outcome.original_locus, None);
}

/// Le `.history/<id>/` est déplacé sous `.archive/main/.history/<id>/`.
#[tokio::test]
async fn archive_moves_history_snapshots() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Deux écritures (body différent) → 1 snapshot .history/.
    vault
        .write_note_with_id(build_minimal_frontmatter(), "corps v1".into(), id)
        .await
        .unwrap();
    vault
        .write_note_with_id(build_minimal_frontmatter(), "corps v2".into(), id)
        .await
        .unwrap();
    assert_eq!(vault.history_versions(id).await.unwrap().len(), 1);

    let hist_origin = dir
        .path()
        .join("main")
        .join(".history")
        .join(id.to_string());
    assert!(
        hist_origin.exists(),
        "le .history/<id>/ doit exister avant archivage"
    );

    vault
        .archive_note(id, None, FAR_FUTURE_GC_DUE)
        .await
        .expect("archive_note");

    // Snapshots déplacés sous .archive/main/.history/<id>/.
    let hist_archived = dir
        .path()
        .join(".archive")
        .join("main")
        .join(".history")
        .join(id.to_string());
    assert!(
        hist_archived.exists() && hist_archived.read_dir().unwrap().count() >= 1,
        "les snapshots .history/ doivent être déplacés sous .archive/ : {}",
        hist_archived.display()
    );
}

/// Le GC de rétention détruit UNIQUEMENT les archives échues et marque `gc_at`
/// (la ligne registre survit) ; les archives fraîches sont épargnées ; idempotent.
#[tokio::test]
async fn run_archive_gc_destroys_only_due_and_marks_registry() {
    use gradatum_index::ArchiveListFilter;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Archive échue (gc_due=1 = très ancien).
    let due = NoteId::new();
    vault
        .write_note_with_id(build_minimal_frontmatter(), "note échue".into(), due)
        .await
        .unwrap();
    let due_out = vault
        .archive_note(due, Some("admin".into()), 1)
        .await
        .unwrap();

    // Archive fraîche (gc_due très loin dans le futur).
    let fresh = NoteId::new();
    vault
        .write_note_with_id(build_minimal_frontmatter(), "note fraîche".into(), fresh)
        .await
        .unwrap();
    let fresh_out = vault
        .archive_note(fresh, Some("admin".into()), FAR_FUTURE_GC_DUE)
        .await
        .unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    let destroyed = vault
        .run_archive_gc(now, 100)
        .await
        .expect("run_archive_gc");
    assert_eq!(destroyed, 1, "seule l'archive échue est détruite");

    // Fichier de l'archive échue supprimé ; celui de la fraîche conservé.
    assert!(
        !dir.path().join(&due_out.archive_path).exists(),
        "le .md de l'archive échue doit être détruit"
    );
    assert!(
        dir.path().join(&fresh_out.archive_path).exists(),
        "le .md de l'archive fraîche doit être conservé"
    );

    // Registre : échue → gc_at marqué (hors actif), fraîche → toujours active.
    assert!(
        vault
            .index()
            .get_active_archive("main", &due.to_string())
            .await
            .unwrap()
            .is_none(),
        "l'archive échue ne doit plus être active"
    );
    assert!(
        vault
            .index()
            .get_active_archive("main", &fresh.to_string())
            .await
            .unwrap()
            .is_some(),
        "l'archive fraîche doit rester active"
    );
    let gc_marked = vault
        .index()
        .list_archive_entries(&ArchiveListFilter {
            include_gc: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        gc_marked
            .iter()
            .any(|e| e.note_id == due.to_string() && e.gc_at.is_some()),
        "l'archive échue doit porter gc_at dans le registre (trace conservée)"
    );

    // Idempotent : 2e passage → 0.
    assert_eq!(
        vault
            .run_archive_gc(now, 100)
            .await
            .expect("run_archive_gc 2"),
        0,
        "2e passage GC → 0"
    );
}

/// Réconciliation défensive : un fichier d'archive déjà absent est quand même
/// marqué `gc_at` (le registre ne reste jamais bloqué sur un fantôme).
#[tokio::test]
async fn run_archive_gc_reconciles_missing_file() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    vault
        .write_note_with_id(build_minimal_frontmatter(), "note".into(), id)
        .await
        .unwrap();
    let out = vault.archive_note(id, None, 1).await.unwrap();

    // Supprimer manuellement le .md d'archive AVANT le GC (fichier fantôme).
    std::fs::remove_file(dir.path().join(&out.archive_path)).unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    let destroyed = vault
        .run_archive_gc(now, 100)
        .await
        .expect("run_archive_gc");
    assert_eq!(
        destroyed, 1,
        "l'entrée fantôme est réconciliée (gc_at marqué)"
    );
    assert!(
        vault
            .index()
            .get_active_archive("main", &id.to_string())
            .await
            .unwrap()
            .is_none(),
        "l'entrée fantôme ne doit plus être active"
    );
}
