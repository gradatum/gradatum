//! Tests d'intégration F-32B — state machine `NoteStatus`.
//!
//! Couvre :
//! - Toutes les transitions valides du graphe via `update_status` (paramétriques).
//! - Transitions invalides → `VaultError::Core(GradatumError::InvalidStatusTransition)`.
//! - `reason` persiste dans `frontmatter.status_reason` + `status_changed` mis à jour.
//! - Chaque transition crée un snapshot CoW dans `.history/` (preuve déterministe).
//! - Note `downgraded` (hors enum, legacy) : la state machine la rejette avec
//!   `InvalidStatusTransition` sans la corrompre — legacy-coexistence contract.
//! - Idempotence : target == source retourne Ok(()) sans erreur (no-op).

mod common;
use common::build_minimal_frontmatter;

use chrono::Utc;
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::status::NoteStatus;
use gradatum_vault::{Vault, VaultError};
use tempfile::TempDir;

// ── Helper : crée un vault + note avec un statut initial donné ────────────────

/// Crée un vault tmpdir + note en statut `initial_status`, retourne (vault, id).
///
/// Stratégie : on part d'un Draft et on amène la note au statut voulu via les
/// transitions valides — pas de SQL direct pour bypasser la state machine.
async fn vault_with_note_at_status(initial: NoteStatus) -> (Vault, TempDir, NoteId) {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create — invariant test setup");

    // Écrire la note en Draft initial.
    let mut fm = build_minimal_frontmatter();
    fm.status = NoteStatus::Draft;
    let note = vault
        .write_note(fm, "corps test state machine".into())
        .await
        .expect("write_note initial — invariant test setup");

    let id = note.id;

    // Amener la note au statut `initial` via les transitions légales.
    match initial {
        NoteStatus::Draft => { /* déjà là */ }
        NoteStatus::PendingReview => {
            vault
                .update_status(id, NoteStatus::PendingReview, None)
                .await
                .expect("Draft→PendingReview — invariant setup");
        }
        NoteStatus::Staging => {
            vault
                .update_status(id, NoteStatus::PendingReview, None)
                .await
                .expect("Draft→PendingReview — invariant setup");
            vault
                .update_status(id, NoteStatus::Staging, None)
                .await
                .expect("PendingReview→Staging — invariant setup");
        }
        NoteStatus::Live => {
            vault
                .update_status(id, NoteStatus::PendingReview, None)
                .await
                .expect("Draft→PendingReview — invariant setup");
            vault
                .update_status(id, NoteStatus::Live, None)
                .await
                .expect("PendingReview→Live — invariant setup");
        }
        NoteStatus::Deprecated => {
            vault
                .update_status(id, NoteStatus::PendingReview, None)
                .await
                .expect("setup Deprecated step 1");
            vault
                .update_status(id, NoteStatus::Live, None)
                .await
                .expect("setup Deprecated step 2");
            vault
                .update_status(id, NoteStatus::Deprecated, None)
                .await
                .expect("setup Deprecated step 3");
        }
        NoteStatus::Garbage => {
            vault
                .update_status(id, NoteStatus::PendingReview, None)
                .await
                .expect("setup Garbage step 1");
            vault
                .update_status(id, NoteStatus::Garbage, None)
                .await
                .expect("setup Garbage step 2");
        }
    }

    (vault, dir, id)
}

// ── Tests : transitions VALIDES ────────────────────────────────────────────────

/// Draft → PendingReview : transition valide, status persiste dans le frontmatter.
#[tokio::test]
async fn transition_draft_to_pending_review() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Draft).await;

    vault
        .update_status(
            id,
            NoteStatus::PendingReview,
            Some("soumis au curator".into()),
        )
        .await
        .expect("Draft→PendingReview doit réussir");

    // Vérifier que le frontmatter reflète le nouveau status.
    let note = vault
        .read_note(id)
        .await
        .expect("read_note après transition");
    assert_eq!(note.frontmatter.status, NoteStatus::PendingReview);
    assert_eq!(
        note.frontmatter.status_reason.as_deref(),
        Some("soumis au curator")
    );
    assert!(
        note.frontmatter.status_changed.is_some(),
        "status_changed doit être mis à jour"
    );
}

/// Draft → Garbage : transition valide (CLI direct trash).
#[tokio::test]
async fn transition_draft_to_garbage() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Draft).await;

    vault
        .update_status(id, NoteStatus::Garbage, Some("rejeté directement".into()))
        .await
        .expect("Draft→Garbage doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Garbage);
}

/// PendingReview → Live : curator admit.
#[tokio::test]
async fn transition_pending_review_to_live() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::PendingReview).await;

    vault
        .update_status(id, NoteStatus::Live, None)
        .await
        .expect("PendingReview→Live doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

/// PendingReview → Garbage : curator reject.
#[tokio::test]
async fn transition_pending_review_to_garbage() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::PendingReview).await;

    vault
        .update_status(id, NoteStatus::Garbage, Some("contenu insuffisant".into()))
        .await
        .expect("PendingReview→Garbage doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Garbage);
}

/// PendingReview → Staging : humain review optionnel.
#[tokio::test]
async fn transition_pending_review_to_staging() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::PendingReview).await;

    vault
        .update_status(id, NoteStatus::Staging, None)
        .await
        .expect("PendingReview→Staging doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Staging);
}

/// Staging → Live : humain approuve.
#[tokio::test]
async fn transition_staging_to_live() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Staging).await;

    vault
        .update_status(id, NoteStatus::Live, None)
        .await
        .expect("Staging→Live doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

/// Staging → Garbage.
#[tokio::test]
async fn transition_staging_to_garbage() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Staging).await;

    vault
        .update_status(id, NoteStatus::Garbage, None)
        .await
        .expect("Staging→Garbage doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Garbage);
}

/// Live → Deprecated : remplacé par un successeur.
#[tokio::test]
async fn transition_live_to_deprecated() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Live).await;

    vault
        .update_status(
            id,
            NoteStatus::Deprecated,
            Some("remplacé par 01KXXXXX".into()),
        )
        .await
        .expect("Live→Deprecated doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Deprecated);
    assert_eq!(
        note.frontmatter.status_reason.as_deref(),
        Some("remplacé par 01KXXXXX")
    );
}

/// Live → Garbage : delete explicite.
#[tokio::test]
async fn transition_live_to_garbage() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Live).await;

    vault
        .update_status(id, NoteStatus::Garbage, None)
        .await
        .expect("Live→Garbage doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Garbage);
}

/// Deprecated → Live : restore.
#[tokio::test]
async fn transition_deprecated_to_live() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Deprecated).await;

    vault
        .update_status(
            id,
            NoteStatus::Live,
            Some("restauré : décision annulée".into()),
        )
        .await
        .expect("Deprecated→Live doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

/// Garbage → Live : restore avant cleanup async.
#[tokio::test]
async fn transition_garbage_to_live() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Garbage).await;

    vault
        .update_status(id, NoteStatus::Live, Some("restauré par opérateur".into()))
        .await
        .expect("Garbage→Live doit réussir");

    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

// ── Tests : transitions INVALIDES → InvalidStatusTransition ───────────────────

/// Live → Draft : non autorisé par le graphe.
#[tokio::test]
async fn transition_invalid_live_to_draft_returns_error() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Live).await;

    let result = vault.update_status(id, NoteStatus::Draft, None).await;

    match result {
        Err(VaultError::Core(GradatumError::InvalidStatusTransition { from, to })) => {
            assert_eq!(from, NoteStatus::Live);
            assert_eq!(to, NoteStatus::Draft);
        }
        other => panic!(
            "attendu InvalidStatusTransition Live→Draft, obtenu : {:?}",
            other
        ),
    }
}

/// Draft → Staging : non autorisé (Draft ne peut pas aller directement en Staging).
#[tokio::test]
async fn transition_invalid_draft_to_staging_returns_error() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Draft).await;

    let result = vault.update_status(id, NoteStatus::Staging, None).await;

    assert!(
        matches!(
            result,
            Err(VaultError::Core(
                GradatumError::InvalidStatusTransition { .. }
            ))
        ),
        "Draft→Staging doit retourner InvalidStatusTransition"
    );
}

/// Draft → Live : non autorisé (doit passer par PendingReview).
#[tokio::test]
async fn transition_invalid_draft_to_live_returns_error() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Draft).await;

    let result = vault.update_status(id, NoteStatus::Live, None).await;

    assert!(
        matches!(
            result,
            Err(VaultError::Core(
                GradatumError::InvalidStatusTransition { .. }
            ))
        ),
        "Draft→Live doit retourner InvalidStatusTransition"
    );
}

/// Garbage → Draft : non autorisé.
#[tokio::test]
async fn transition_invalid_garbage_to_draft_returns_error() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Garbage).await;

    let result = vault.update_status(id, NoteStatus::Draft, None).await;

    assert!(
        matches!(
            result,
            Err(VaultError::Core(
                GradatumError::InvalidStatusTransition { .. }
            ))
        ),
        "Garbage→Draft doit retourner InvalidStatusTransition"
    );
}

// ── Test : idempotence (target == source) ─────────────────────────────────────

/// Idempotence : appliquer le même statut que l'actuel retourne Ok(()) sans erreur.
///
/// Décision documentée : no-op silencieux (pas 409) quand target == current.
/// Évite les 409 intempestifs lors de rejeux ou retries du curator.
#[tokio::test]
async fn transition_idempotent_same_status_is_noop() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Live).await;

    // Appliquer le même statut deux fois — doit réussir sans erreur.
    vault
        .update_status(id, NoteStatus::Live, None)
        .await
        .expect("Live→Live (idempotence) doit retourner Ok(())");

    // Le statut doit rester Live.
    let note = vault.read_note(id).await.expect("read_note");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

// ── Test : CoW tracé à chaque transition ──────────────────────────────────────

/// Chaque transition valide crée un snapshot CoW dans `.history/`.
///
/// Prouve que `update_status` passe bien par `write_note_with_id` (chemin normal)
/// et non par un SQL direct qui bypasse le CoW.
#[tokio::test]
async fn transition_creates_cow_snapshot() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create");

    // Créer une note initiale en Draft.
    let mut fm = build_minimal_frontmatter();
    fm.status = NoteStatus::Draft;
    let note = vault
        .write_note(fm, "corps pour test CoW lifecycle".into())
        .await
        .expect("write_note");

    let id = note.id;

    // Pas d'historique au départ (première écriture, pas de version précédente).
    // Effectuer une transition valide.
    vault
        .update_status(id, NoteStatus::PendingReview, None)
        .await
        .expect("Draft→PendingReview");

    // Après la transition, il doit exister au moins 1 snapshot dans .history/.
    let versions = vault.history_versions(id).await.expect("history_versions");
    assert!(
        !versions.is_empty(),
        "une transition doit créer un snapshot CoW dans .history/"
    );
}

// ── Test : note introuvable ────────────────────────────────────────────────────

/// `update_status` sur une note inexistante retourne `NoteNotFound`.
#[tokio::test]
async fn transition_nonexistent_note_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create");

    let fake_id = NoteId::new();

    let result = vault
        .update_status(fake_id, NoteStatus::PendingReview, None)
        .await;

    assert!(
        matches!(
            result,
            Err(VaultError::Core(GradatumError::NoteNotFound(_)))
        ),
        "note inexistante doit retourner NoteNotFound, obtenu : {:?}",
        result
    );
}

// ── Test : coexistence legacy downgraded ─────────────────────────────────────

/// Note `downgraded` (legacy, hors enum) : la state machine la rejette avec
/// `InvalidStatusTransition` sans la corrompre.
///
/// Le statut `downgraded` est hors de l'enum `NoteStatus` — il est écrit directement
/// en SQLite par `vault_downgrade` (mécanisme F-39). Le `read_note` parse le
/// frontmatter depuis le .md, qui contient le `NoteStatus` sérialisé en TOML.
///
/// Ce test vérifie que si une note est manuellement amenée à un état invalide
/// (simulant un downgrade legacy), `update_status` ne la corrompt pas davantage.
///
/// Note d'implémentation : comme `downgraded` n'est pas un variant de `NoteStatus`,
/// il est impossible de créer ce cas via l'API Rust normale. Ce test vérifie la
/// protection via une note Draft→Staging (transition invalide) qui est le scénario
/// le plus proche sans SQL direct dans les tests d'intégration.
///
/// La coexistence réelle est testée au niveau E2E via `vault_downgrade_e2e.rs`
/// (endpoint HTTP qui écrit directement en SQLite via `downgrade_note`).
#[tokio::test]
async fn transition_invalid_does_not_corrupt_note() {
    let (vault, _dir, id) = vault_with_note_at_status(NoteStatus::Draft).await;

    // Tentative de transition invalide.
    let _ = vault.update_status(id, NoteStatus::Staging, None).await;

    // La note ne doit pas avoir été modifiée — toujours en Draft.
    let note = vault.read_note(id).await.expect("read_note après échec");
    assert_eq!(
        note.frontmatter.status,
        NoteStatus::Draft,
        "une transition invalide ne doit pas corrompre le statut de la note"
    );
}

// ── Test : status_changed mis à jour ─────────────────────────────────────────

/// `status_changed` est mis à jour à chaque transition réussie.
#[tokio::test]
async fn transition_updates_status_changed_timestamp() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create");

    let mut fm = build_minimal_frontmatter();
    fm.status = NoteStatus::Draft;
    fm.status_changed = None;
    let note = vault
        .write_note(fm, "corps".into())
        .await
        .expect("write_note");
    let id = note.id;

    let before = Utc::now();

    vault
        .update_status(id, NoteStatus::PendingReview, None)
        .await
        .expect("transition");

    let after = Utc::now();

    let updated = vault.read_note(id).await.expect("read_note");
    let changed = updated
        .frontmatter
        .status_changed
        .expect("status_changed doit être défini après une transition");

    assert!(
        changed >= before && changed <= after,
        "status_changed doit être dans la fenêtre [before, after] : {:?} ∉ [{:?}, {:?}]",
        changed,
        before,
        after
    );
}
