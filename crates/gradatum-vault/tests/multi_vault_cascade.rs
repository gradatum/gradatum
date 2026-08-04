//! Tests d'intégration C3a (P2-2, clôture gate flag-ON C2) — variantes multi-vault
//! `Vault::delete_note_in` / `Vault::archive_note_in`.
//!
//! Le bug fermé : `delete_note`/`archive_note` résolvent les chemins disque sous
//! `self.tenant_id` (vault racine de l'instance) — les `.md` d'un vault secondaire
//! (`<root>/<vault_id>/…`) leur sont invisibles et survivaient à une purge (résidu
//! orphelin). Les variantes `_in` reçoivent le vault propriétaire explicitement.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_vault::{Vault, VaultError};
use tempfile::TempDir;

const FAR_FUTURE_GC_DUE: i64 = 9_999_999_999_999;

/// Écrit une note appartenant au vault secondaire `research` (le `.md` atterrit
/// sous `<root>/research/<id>.md`) et retourne son ULID.
async fn write_secondary_note(vault: &Vault) -> NoteId {
    let id = NoteId::new();
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = VaultId::new("research");
    vault
        .write_note_with_id(fm, "# Note secondaire\n\ncorps".into(), id)
        .await
        .expect("write_note_with_id vault secondaire");
    id
}

/// Le bug d'origine, documenté : `delete_note` (résolution `self.tenant_id = main`)
/// ne voit PAS une note d'un vault secondaire → `NoteNotFound`, `.md` résiduel.
#[tokio::test]
async fn legacy_delete_note_cannot_see_secondary_vault_md() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = write_secondary_note(&vault).await;

    let md = dir.path().join("research").join(format!("{id}.md"));
    assert!(md.exists(), "le .md secondaire doit exister avant delete");

    let err = vault
        .delete_note(id)
        .await
        .expect_err("delete_note legacy ne doit pas voir le vault secondaire");
    assert!(
        matches!(
            err,
            VaultError::Core(GradatumError::NoteNotFound(nid)) if nid == id
        ),
        "NoteNotFound attendu, obtenu : {err:?}"
    );
    assert!(md.exists(), "le .md résiduel prouve le bug fermé par _in");
}

/// `delete_note_in("research", …)` supprime réellement le `.md` du vault secondaire.
#[tokio::test]
async fn delete_note_in_removes_secondary_vault_md() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = write_secondary_note(&vault).await;

    let md = dir.path().join("research").join(format!("{id}.md"));
    assert!(md.exists(), "le .md secondaire doit exister avant delete");

    vault
        .delete_note_in("research", id)
        .await
        .expect("delete_note_in vault secondaire");
    assert!(!md.exists(), "aucun .md résiduel après delete_note_in");
}

/// `delete_note_in` avec le vault racine == comportement historique `delete_note`.
#[tokio::test]
async fn delete_note_in_root_vault_matches_legacy() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();
    vault
        .write_note_with_id(build_minimal_frontmatter(), "# Racine\n\ncorps".into(), id)
        .await
        .expect("write_note_with_id vault racine");

    let md = dir.path().join("main").join(format!("{id}.md"));
    assert!(md.exists(), "le .md racine doit exister avant delete");

    vault
        .delete_note_in("main", id)
        .await
        .expect("delete_note_in vault racine");
    assert!(!md.exists(), "delete_note_in('main') == delete_note legacy");
}

/// `archive_note_in("research", …)` déplace le `.md` secondaire sous
/// `.archive/research/…` et inscrit la ligne registre avec le bon `vault_id`.
#[tokio::test]
async fn archive_note_in_moves_secondary_vault_md_and_records_registry() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = write_secondary_note(&vault).await;

    let origin = dir.path().join("research").join(format!("{id}.md"));
    assert!(
        origin.exists(),
        "le .md secondaire doit exister avant archive"
    );

    let outcome = vault
        .archive_note_in("research", id, Some("test-admin".into()), FAR_FUTURE_GC_DUE)
        .await
        .expect("archive_note_in vault secondaire");

    assert!(!origin.exists(), "le .md ne doit plus exister à l'origine");
    let archived = dir
        .path()
        .join(".archive")
        .join("research")
        .join(format!("{id}.md"));
    assert!(
        archived.exists(),
        "le .md doit exister sous .archive/research/ : {}",
        archived.display()
    );
    assert_eq!(outcome.archive_path, format!(".archive/research/{id}.md"));

    let entry = vault
        .index()
        .get_active_archive("research", &id.to_string())
        .await
        .expect("get_active_archive")
        .expect("archive active enregistrée");
    assert_eq!(entry.vault_id, "research", "registre multi-vault-aware");
}
