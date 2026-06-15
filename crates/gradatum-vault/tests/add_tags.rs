//! Tests d'intégration A4 unblock — ajout additif de tags via `Vault::add_tags`.
//!
//! Couvre :
//! - UNION case-insensitive avec les tags existants (jamais de remplacement).
//! - Dédup case-insensitive (les doublons d'entrée et les collisions avec l'existant
//!   sont ignorés, la casse existante est préservée).
//! - Idempotence stricte : ré-ajouter les mêmes tags ne crée pas de version `.history/`.
//! - Réindexation FTS : une recherche par nouveau tag retrouve la note.
//! - Note absente → `NoteNotFound`.
//! - Tag mal formé → `Validation`.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_vault::{Vault, VaultError};
use tempfile::TempDir;

/// Crée un vault tmpdir + une note avec les `initial_tags` donnés. Retourne (vault, dir, id).
async fn vault_with_tagged_note(initial_tags: &[&str]) -> (Vault, TempDir, NoteId) {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create — invariant test setup");

    let mut fm = build_minimal_frontmatter();
    for t in initial_tags {
        fm.tags
            .push(gradatum_core::tag::Tag::new(t.to_string()).expect("tag setup valide"));
    }
    let note = vault
        .write_note(fm, "corps test add_tags".into())
        .await
        .expect("write_note initial — invariant test setup");
    let id = note.id;
    (vault, dir, id)
}

/// Lit les tags courants d'une note (triés pour comparaison déterministe).
async fn read_tags_sorted(vault: &Vault, id: NoteId) -> Vec<String> {
    let note = vault.read_note(id).await.expect("read_note");
    let mut tags: Vec<String> = note
        .frontmatter
        .tags
        .iter()
        .map(|t| t.as_str().to_string())
        .collect();
    tags.sort();
    tags
}

#[tokio::test]
async fn add_tags_union_with_existing() {
    let (vault, _dir, id) = vault_with_tagged_note(&["deploy"]).await;

    vault
        .add_tags(id, &["release".to_string(), "migration".to_string()])
        .await
        .expect("add_tags union");

    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(
        tags,
        vec![
            "deploy".to_string(),
            "migration".to_string(),
            "release".to_string()
        ],
        "union additive : existant + nouveaux, jamais de remplacement"
    );
}

#[tokio::test]
async fn add_tags_dedup_case_insensitive_against_existing() {
    // Tag existant 'Deploy' (casse mixte injectée directement n'est PAS possible via Tag::new
    // qui force lowercase ; on simule la collision via un tag déjà lowercase et un input identique).
    let (vault, _dir, id) = vault_with_tagged_note(&["deploy"]).await;

    // Ré-ajouter 'deploy' (déjà présent) + un nouveau → seul le nouveau est ajouté.
    vault
        .add_tags(id, &["deploy".to_string(), "ci-cd".to_string()])
        .await
        .expect("add_tags dedup");

    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(
        tags,
        vec!["ci-cd".to_string(), "deploy".to_string()],
        "le tag déjà présent n'est pas dupliqué"
    );
}

#[tokio::test]
async fn add_tags_dedup_within_input() {
    let (vault, _dir, id) = vault_with_tagged_note(&[]).await;

    // Doublons dans l'input → un seul exemplaire ajouté.
    vault
        .add_tags(
            id,
            &[
                "release".to_string(),
                "release".to_string(),
                "deploy".to_string(),
            ],
        )
        .await
        .expect("add_tags dedup input");

    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(tags, vec!["deploy".to_string(), "release".to_string()]);
}

#[tokio::test]
async fn add_tags_is_idempotent_no_history_churn() {
    let (vault, _dir, id) = vault_with_tagged_note(&["deploy"]).await;

    // Premier ajout réel → crée une version CoW.
    vault
        .add_tags(id, &["release".to_string()])
        .await
        .expect("add_tags 1");
    let versions_after_1 = pipe_history_count(&vault, id).await;

    // Second ajout identique → no-op (aucune version supplémentaire, idempotence stricte).
    vault
        .add_tags(id, &["release".to_string()])
        .await
        .expect("add_tags 2 (idempotent)");
    let versions_after_2 = pipe_history_count(&vault, id).await;

    assert_eq!(
        versions_after_1, versions_after_2,
        "ré-ajout des mêmes tags ne doit PAS créer de version .history/ parasite"
    );

    // L'état final est identique au premier ajout.
    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(tags, vec!["deploy".to_string(), "release".to_string()]);
}

/// Compte les versions `.history/` d'une note (preuve d'idempotence CoW).
///
/// Utilise la méthode inhérente `Vault::history_versions(NoteId)`.
async fn pipe_history_count(vault: &Vault, id: NoteId) -> usize {
    vault
        .history_versions(id)
        .await
        .expect("history_versions")
        .len()
}

#[tokio::test]
async fn add_tags_reindexes_fts_search_finds_note() {
    let (vault, _dir, id) = vault_with_tagged_note(&[]).await;

    // Ajouter un tag distinctif absent du corps.
    vault
        .add_tags(id, &["zztagunique".to_string()])
        .await
        .expect("add_tags fts");

    // Recherche FTS par le tag → la note doit être retrouvée (FTS réindexé via upsert_note).
    let results = vault
        .index()
        .search_fts_scored(&VaultId::new("main"), "zztagunique", 10, false)
        .await
        .expect("search_fts_scored par tag");

    assert!(
        results.iter().any(|(nid, _, _)| *nid == id),
        "la note doit être retrouvée par recherche FTS sur le nouveau tag — résultats={:?}",
        results
            .iter()
            .map(|(n, _, _)| n.to_string())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn add_tags_note_not_found() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create");

    let ghost = NoteId::new();
    let err = vault
        .add_tags(ghost, &["x".to_string()])
        .await
        .expect_err("note inexistante → erreur");
    assert!(
        matches!(err, VaultError::Core(GradatumError::NoteNotFound(_))),
        "note absente → NoteNotFound, obtenu {err:?}"
    );
}

#[tokio::test]
async fn add_tags_invalid_tag_rejected() {
    let (vault, _dir, id) = vault_with_tagged_note(&[]).await;

    // 'BadTag' (majuscules) est rejeté par Tag::new → Validation.
    let err = vault
        .add_tags(id, &["BadTag".to_string()])
        .await
        .expect_err("tag mal formé → erreur");
    assert!(
        matches!(err, VaultError::Core(GradatumError::Validation(_))),
        "tag mal formé → Validation, obtenu {err:?}"
    );

    // La note n'a pas été modifiée (aucun tag ajouté).
    let tags = read_tags_sorted(&vault, id).await;
    assert!(tags.is_empty(), "aucun tag ajouté sur échec de validation");
}

/// cap total : note à 195 tags + ajout de 10 (total post-union
/// 205 > MAX_NOTE_TAGS=200) → Validation::InvalidInput, et l'état de la note est INCHANGÉ
/// (toujours 195 tags, aucun write CoW parasite).
#[tokio::test]
async fn add_tags_caps_total_at_max() {
    use gradatum_vault::MAX_NOTE_TAGS;

    // Note initiale avec 195 tags valides (format `^[a-z0-9][a-z0-9-]{0,63}$`).
    let initial: Vec<String> = (0..195).map(|i| format!("tag-{i:03}")).collect();
    let initial_refs: Vec<&str> = initial.iter().map(String::as_str).collect();
    let (vault, _dir, id) = vault_with_tagged_note(&initial_refs).await;

    // 10 tags NOUVEAUX (uniques, non présents) → 195 + 10 = 205 > 200.
    let new_tags: Vec<String> = (200..210).map(|i| format!("tag-{i:03}")).collect();
    let err = vault
        .add_tags(id, &new_tags)
        .await
        .expect_err("dépassement du cap total → erreur");
    assert!(
        matches!(
            err,
            VaultError::Core(GradatumError::Validation(
                gradatum_core::error::ValidationError::InvalidInput(_)
            ))
        ),
        "cap dépassé → Validation::InvalidInput, obtenu {err:?}"
    );

    // État INCHANGÉ : toujours exactement 195 tags (aucun ajout partiel, aucune version parasite).
    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(
        tags.len(),
        195,
        "l'état doit rester inchangé sur dépassement du cap (ni ajout partiel ni write)"
    );

    // Sanity : MAX_NOTE_TAGS exposé et cohérent.
    assert_eq!(MAX_NOTE_TAGS, 200, "cap attendu = 200");
}

/// Cap : ajout qui amène EXACTEMENT à MAX_NOTE_TAGS (200) est accepté (borne inclusive).
#[tokio::test]
async fn add_tags_exactly_at_max_is_accepted() {
    // 195 tags + 5 nouveaux = 200 = MAX_NOTE_TAGS → accepté.
    let initial: Vec<String> = (0..195).map(|i| format!("tag-{i:03}")).collect();
    let initial_refs: Vec<&str> = initial.iter().map(String::as_str).collect();
    let (vault, _dir, id) = vault_with_tagged_note(&initial_refs).await;

    let new_tags: Vec<String> = (200..205).map(|i| format!("tag-{i:03}")).collect();
    vault
        .add_tags(id, &new_tags)
        .await
        .expect("total = 200 doit être accepté (borne inclusive)");

    let tags = read_tags_sorted(&vault, id).await;
    assert_eq!(tags.len(), 200, "200 tags attendus (cap inclusif)");
}
