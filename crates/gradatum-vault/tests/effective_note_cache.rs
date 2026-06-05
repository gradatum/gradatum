//! Tests d'intégration T11e — `get_effective_note` cache moka checksum validation.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::{OverrideScope, VaultId};
use gradatum_vault::Vault;
use tempfile::TempDir;

#[tokio::test]
async fn effective_note_cache_miss_returns_not_found_phase1_stub() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    let scope = OverrideScope::Vault(VaultId::new("main"));

    // Phase 1 stub : cache miss → get_content_hash retourne None → NoteNotFound
    let result = vault.get_effective_note(id, &scope).await;

    assert!(
        result.is_err(),
        "get_effective_note Phase 1 doit retourner une erreur sur cache miss"
    );
}

#[tokio::test]
async fn effective_note_scope_hash_is_stable() {
    // Test que la même requête produit le même résultat déterministe
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    let scope = OverrideScope::Vault(VaultId::new("main"));

    // Deux appels successifs — Phase 1 les deux retournent NoteNotFound, mais
    // le chemin de code (scope_hash) doit être stable (pas de panique)
    let r1 = vault.get_effective_note(id, &scope).await;
    let r2 = vault.get_effective_note(id, &scope).await;

    assert!(r1.is_err());
    assert!(r2.is_err());
}

#[tokio::test]
async fn effective_note_after_write_returns_note_t4() {
    // Depuis T4 P2.0c, get_effective_note câble le cache miss path réel :
    // fetch index + storage + populate cache. Une note écrite doit être lisible.
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "body effective note".into())
        .await
        .unwrap();
    let note_id = note.id;

    let scope = OverrideScope::Vault(VaultId::new("main"));

    // Cache miss → fetch depuis index + storage → succès
    let result = vault.get_effective_note(note_id, &scope).await;
    assert!(
        result.is_ok(),
        "T4 P2.0c : get_effective_note doit réussir pour une note indexée — cache miss path réel"
    );
    let effective = result.unwrap();
    assert_eq!(
        effective.body.markdown.trim(),
        "body effective note",
        "le corps de la EffectiveNote doit correspondre"
    );
}
