//! Tests d'intégration — `Vault::move_locus()` (D1.1, v0.4.8).
//!
//! Couvre la relocalisation physique du `.md` lors d'un changement de locus :
//! - `vault_read` (via `read_note`) retourne le NOUVEAU locus après le move ;
//! - l'ancien `.md` orphelin est supprimé ;
//! - le CoW history est préservé (snapshot de la version pré-move) ;
//! - un re-upsert ultérieur depuis le `.md` ne régresse pas le locus (fix v0.4.6) ;
//! - idempotence : déplacer vers le même locus est un no-op.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::scope::{LocusId, VaultId};
use gradatum_storage::Storage as _;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Move depuis « pas de locus » vers un locus : le `.md` est relocalisé, `read_note`
/// retourne le nouveau locus, et l'ancien chemin est nettoyé.
#[tokio::test]
async fn move_locus_relocates_md_and_read_returns_new_locus() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Note sans locus → écrite à main/<id>.md.
    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "Corps move locus".into())
        .await
        .unwrap();
    let id = note.id;
    let id_str = id.to_string();

    let old_path = format!("main/{id_str}.md");
    assert!(
        vault.storage().exists(&old_path).await.unwrap(),
        "le .md doit exister au chemin racine tenant avant le move"
    );

    // Move vers locus knowledge/rust.
    let target = LocusId::parse("knowledge/rust").unwrap();
    vault.move_locus(id, &target).await.expect("move_locus");

    // read_note doit retrouver la note et exposer le nouveau locus.
    let read = vault.read_note(id).await.expect("read_note après move");
    assert_eq!(
        read.frontmatter.locus.as_ref().map(|l| l.as_str()),
        Some("knowledge/rust"),
        "read_note doit retourner le NOUVEAU locus après move"
    );

    // Le .md doit exister au nouveau chemin, et l'ancien doit être supprimé.
    let new_path = format!("main/knowledge/rust/{id_str}.md");
    assert!(
        vault.storage().exists(&new_path).await.unwrap(),
        "le .md doit exister au nouveau chemin locus après move"
    );
    assert!(
        !vault.storage().exists(&old_path).await.unwrap(),
        "l'ancien .md orphelin doit être supprimé après move"
    );
}

/// Le CoW history préserve la version pré-move : un snapshot `.history/` est créé.
#[tokio::test]
async fn move_locus_preserves_cow_history() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "Corps history move".into())
        .await
        .unwrap();
    let id = note.id;

    // Avant le move : pas d'historique (note jamais modifiée).
    let before = vault.history_versions(id).await.expect("history avant");
    assert!(before.is_empty(), "pas d'historique avant le premier move");

    let target = LocusId::parse("archives").unwrap();
    vault.move_locus(id, &target).await.expect("move_locus");

    // Après le move : un snapshot de la version pré-move existe.
    let after = vault.history_versions(id).await.expect("history après");
    assert_eq!(
        after.len(),
        1,
        "le move doit créer exactement un snapshot CoW de la version pré-move"
    );
}

/// Un re-upsert depuis le `.md` (même contenu) ne régresse pas le locus déplacé
/// (préservation du fix v0.4.6 : `ON CONFLICT` conserve le locus si hash inchangé).
#[tokio::test]
async fn move_locus_then_reupsert_does_not_regress_locus() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault.write_note(fm, "Corps reupsert".into()).await.unwrap();
    let id = note.id;

    let target = LocusId::parse("knowledge").unwrap();
    vault.move_locus(id, &target).await.expect("move_locus");

    // Relire puis ré-écrire la note telle quelle (re-upsert même contenu).
    let read = vault.read_note(id).await.expect("read après move");
    vault
        .write_note_with_id(read.frontmatter.clone(), read.body.markdown.clone(), id)
        .await
        .expect("re-upsert même contenu");

    // Le locus doit toujours être 'knowledge' (pas régressé vers None).
    let read2 = vault.read_note(id).await.expect("read après re-upsert");
    assert_eq!(
        read2.frontmatter.locus.as_ref().map(|l| l.as_str()),
        Some("knowledge"),
        "le re-upsert ne doit pas régresser le locus déplacé"
    );
}

/// Idempotence : déplacer vers le locus courant est un no-op (pas de nouveau snapshot).
#[tokio::test]
async fn move_locus_same_locus_is_noop() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let mut fm = build_minimal_frontmatter();
    fm.locus = Some(LocusId::new("knowledge"));
    let note = vault.write_note(fm, "Corps noop".into()).await.unwrap();
    let id = note.id;

    let history_before = vault.history_versions(id).await.expect("history avant");

    // Move vers le même locus.
    let same = LocusId::parse("knowledge").unwrap();
    vault.move_locus(id, &same).await.expect("move_locus no-op");

    let history_after = vault.history_versions(id).await.expect("history après");
    assert_eq!(
        history_before.len(),
        history_after.len(),
        "move vers le même locus ne doit créer aucun snapshot (no-op)"
    );
}

/// D1.3 (v0.4.8) — Anti-résurrection : déplacer une note `downgraded` (statut posé
/// index-only, frontmatter `.md` resté `live`) NE DOIT PAS la faire repasser `live`.
///
/// Vecteur P1 audit D1 : `move_locus` re-upsert la note depuis le `.md` ; l'upsert
/// appliquait `status = excluded.status` (= `live` depuis le frontmatter stale),
/// ressuscitant silencieusement la note en search et écrasant
/// `status_reason`/`status_changed`/`replaced_by` (état incohérent `live + replaced_by`).
///
/// Ce test échoue AVANT le fix (status repasse `live`), passe APRÈS (status préservé).
#[tokio::test]
async fn move_locus_preserves_index_only_downgraded_status() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Note remplaçante (cible de replaced_by) — doit exister (FK).
    let replacement = vault
        .write_note(build_minimal_frontmatter(), "Note remplaçante".into())
        .await
        .unwrap();
    let replacement_id = replacement.id;

    // Note à déplacer, écrite live via le .md (frontmatter status=live après curate).
    let mut fm = build_minimal_frontmatter();
    fm.status = gradatum_core::status::NoteStatus::Live;
    let note = vault
        .write_note(fm, "Corps note downgradée move".into())
        .await
        .unwrap();
    let id = note.id;
    let id_str = id.to_string();

    // Downgrade INDEX-ONLY : UPDATE notes SET status='downgraded' ... — le .md reste live.
    vault
        .index()
        .downgrade_note(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            &id,
            "remplacée par v2",
            Some(&replacement_id),
        )
        .await
        .expect("downgrade index-only");

    // Capturer l'état index AVANT le move (pour vérifier la préservation exacte).
    let before = vault
        .index()
        .get_index_status_snapshot("main", &id_str)
        .await
        .expect("snapshot avant")
        .expect("note présente avant move");
    assert_eq!(
        before.status, "downgraded",
        "pré-condition : note downgradée"
    );
    assert_eq!(before.status_reason.as_deref(), Some("remplacée par v2"));
    assert_eq!(
        before.replaced_by.as_deref(),
        Some(replacement_id.to_string().as_str())
    );
    assert!(before.status_changed_ms.is_some());

    // Search AVANT : la note downgradée est exclue (status != 'downgraded').
    let q = "downgradée";
    let hits_before = vault
        .index()
        .search_fts_scored(&VaultId::new("main"), q, 10, false)
        .await
        .expect("search avant move");
    assert!(
        !hits_before.iter().any(|(hid, _, _)| *hid == id),
        "pré-condition : la note downgradée ne doit PAS apparaître en search"
    );

    // MOVE LOCUS — vecteur du bug.
    let target = LocusId::parse("archives/2026").unwrap();
    vault.move_locus(id, &target).await.expect("move_locus");

    // ── Le statut index-only doit être PRÉSERVÉ après le move ──
    let after = vault
        .index()
        .get_index_status_snapshot("main", &id_str)
        .await
        .expect("snapshot après")
        .expect("note présente après move");
    assert_eq!(
        after.status, "downgraded",
        "ANTI-RÉSURRECTION : la note doit RESTER downgraded après le move"
    );
    assert_eq!(
        after.status_reason.as_deref(),
        Some("remplacée par v2"),
        "status_reason doit être préservé"
    );
    assert_eq!(
        after.status_changed_ms, before.status_changed_ms,
        "status_changed doit être préservé à l'identique (pas réécrit)"
    );
    assert_eq!(
        after.replaced_by.as_deref(),
        Some(replacement_id.to_string().as_str()),
        "replaced_by doit être préservé"
    );

    // Le nouveau locus doit être appliqué.
    let read = vault.read_note(id).await.expect("read après move");
    assert_eq!(
        read.frontmatter.locus.as_ref().map(|l| l.as_str()),
        Some("archives/2026"),
        "le nouveau locus doit être appliqué"
    );

    // L'ancien .md (racine tenant) doit être supprimé, le nouveau présent.
    let old_path = format!("main/{id_str}.md");
    let new_path = format!("main/archives/2026/{id_str}.md");
    assert!(
        vault.storage().exists(&new_path).await.unwrap(),
        "le .md doit exister au nouveau chemin locus"
    );
    assert!(
        !vault.storage().exists(&old_path).await.unwrap(),
        "l'ancien .md orphelin doit être supprimé"
    );

    // Search APRÈS : la note doit TOUJOURS être exclue (pas ressuscitée).
    let hits_after = vault
        .index()
        .search_fts_scored(&VaultId::new("main"), q, 10, false)
        .await
        .expect("search après move");
    assert!(
        !hits_after.iter().any(|(hid, _, _)| *hid == id),
        "ANTI-RÉSURRECTION : la note downgradée NE DOIT PAS réapparaître en search après le move"
    );
}

/// Move sur une note absente → NoteNotFound.
#[tokio::test]
async fn move_locus_unknown_note_is_not_found() {
    use gradatum_core::error::GradatumError;
    use gradatum_core::identity::NoteId;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let target = LocusId::parse("knowledge").unwrap();
    let err = vault
        .move_locus(NoteId::new(), &target)
        .await
        .expect_err("move sur note absente doit échouer");
    assert!(
        matches!(
            err,
            gradatum_vault::VaultError::Core(GradatumError::NoteNotFound(_))
        ),
        "erreur attendue NoteNotFound, obtenu {err:?}"
    );
}
