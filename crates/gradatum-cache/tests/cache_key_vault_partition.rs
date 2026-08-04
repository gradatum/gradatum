//! Test : partition de la clé de cache par `vault_id` (fail-safe multi-vault).
//!
//! Garantit qu'un même `NoteId` + `scope_hash` dans deux vaults distincts produit
//! deux entrées de cache **distinctes**. Sans la dimension `vault_id` dans la clé,
//! un `get` sur `vault-b` renverrait à tort la valeur insérée pour `main` (hit
//! erroné cross-vault). Cette classe de fuite est fermée **structurellement**,
//! avant tout partage de cache : à flag OFF les caches sont per-instance, mais la
//! clé `(NoteId, u64)` était ambiguë cross-vault — la partition la lève à la racine.

mod common;
use common::dummy_effective_note;

use gradatum_cache::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::scope::VaultId;

// ────────────────────────────────────────────────────────
// Cas nominal : insertion sous `main`, lecture sous `vault-b` → MISS.
// ────────────────────────────────────────────────────────
#[tokio::test]
async fn get_other_vault_same_note_id_misses() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let hash = ContentHash([0x11; 32]);

    let key_main: CacheKey = (VaultId::new("main"), id, 0u64);
    cache.insert(key_main, dummy_effective_note(id), hash).await;

    // Lecture cross-vault : même NoteId + même scope_hash, vault DIFFÉRENT.
    let key_other: CacheKey = (VaultId::new("vault-b"), id, 0u64);
    let got = cache
        .get::<_, _, std::convert::Infallible>(key_other, |_id| async move { Ok(hash) })
        .await
        .expect("get ne doit pas échouer");

    assert!(
        got.is_none(),
        "clé partitionnée par vault_id : un NoteId inséré dans `main` ne doit JAMAIS \
         être un hit sous `vault-b` (sinon fuite cross-vault)"
    );
}

// ────────────────────────────────────────────────────────
// Contrôle positif : lecture sous le MÊME vault → HIT.
// (la partition ne doit pas étouffer les hits légitimes intra-vault)
// ────────────────────────────────────────────────────────
#[tokio::test]
async fn get_same_vault_same_note_id_hits() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let hash = ContentHash([0x11; 32]);

    let key: CacheKey = (VaultId::new("main"), id, 0u64);
    cache
        .insert(key.clone(), dummy_effective_note(id), hash)
        .await;

    let got = cache
        .get::<_, _, std::convert::Infallible>(key, |_id| async move { Ok(hash) })
        .await
        .expect("get ne doit pas échouer");

    assert!(
        got.is_some(),
        "hit légitime intra-vault doit renvoyer la valeur cachée"
    );
}
