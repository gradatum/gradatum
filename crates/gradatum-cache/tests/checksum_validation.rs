//! Tests : validation de checksum sur cache hit (D-perf-2 / B22).
//!
//! Vérifie le comportement de `EffectiveNoteCache::get` avec un validator async.

mod common;
use common::dummy_effective_note;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use gradatum_cache::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::identity::{ContentHash, NoteId};

// ────────────────────────────────────────────────────────
// T1 : hit avec hash valide → retour de la valeur cachée
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_hit_with_valid_checksum_returns_value() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key: CacheKey = (id, 0u64);
    let value = dummy_effective_note(id);
    let hash = ContentHash([0x11; 32]);

    cache.insert(key, value.clone(), hash).await;

    let got = cache
        .get::<_, _, std::convert::Infallible>(key, |_id| async move { Ok(hash) })
        .await
        .unwrap();

    assert!(
        got.is_some(),
        "hit avec hash valide doit retourner la valeur"
    );
    assert!(
        Arc::ptr_eq(&got.unwrap(), &value),
        "doit retourner exactement le même Arc (pas une copie)"
    );
}

// ────────────────────────────────────────────────────────
// T2 : hit avec hash périmé → invalidation + None
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_hit_with_stale_checksum_invalidates_and_misses() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key: CacheKey = (id, 0u64);
    let value = dummy_effective_note(id);
    let hash_old = ContentHash([0x11; 32]);
    let hash_new = ContentHash([0x22; 32]);

    cache.insert(key, value, hash_old).await;

    let got = cache
        .get::<_, _, std::convert::Infallible>(key, |_id| async move { Ok(hash_new) })
        .await
        .unwrap();

    assert!(
        got.is_none(),
        "entrée stale doit être invalidée et retourner None"
    );

    // Vérifier que l'entrée a bien été retirée du cache.
    // Note : moka est best-effort sur entry_count — run_pending_tasks pour garantir.
    cache.run_pending_tasks().await;
    assert_eq!(
        cache.entry_count(),
        0,
        "l'entrée invalidée doit être absente du cache après run_pending_tasks"
    );
}

// ────────────────────────────────────────────────────────
// T3 : cache miss → validator NON appelé (zero overhead)
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_miss_returns_none_without_validator_call() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let validator_called = Arc::new(AtomicBool::new(false));
    let validator_called_clone = validator_called.clone();

    let result = cache
        .get::<_, _, std::convert::Infallible>((NoteId::new(), 0), move |_id| {
            let v = validator_called_clone.clone();
            async move {
                v.store(true, Ordering::SeqCst);
                Ok(ContentHash([0; 32]))
            }
        })
        .await
        .unwrap();

    assert!(result.is_none(), "cache miss doit retourner None");
    assert!(
        !validator_called.load(Ordering::SeqCst),
        "validator NE DOIT PAS être appelé sur un cache miss"
    );
}

// ────────────────────────────────────────────────────────
// T4 : erreur du validator → propagée telle quelle
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn validator_error_propagates() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key: CacheKey = (id, 0u64);
    // Insérer une entrée pour provoquer un hit.
    cache
        .insert(key, dummy_effective_note(id), ContentHash([0; 32]))
        .await;

    #[derive(Debug, PartialEq)]
    struct DbError;

    let result = cache
        .get::<_, _, DbError>(key, |_id| async move { Err(DbError) })
        .await;

    assert_eq!(
        result.unwrap_err(),
        DbError,
        "l'erreur du validator doit être propagée intacte"
    );

    // L'entrée NE doit PAS être invalidée en cas d'erreur DB (transitoire).
    let still_present = cache
        .get::<_, _, std::convert::Infallible>(key, |_id| async move { Ok(ContentHash([0; 32])) })
        .await
        .unwrap();
    assert!(
        still_present.is_some(),
        "l'entrée doit rester en cache après une erreur DB transitoire du validator"
    );
}

// ────────────────────────────────────────────────────────
// T5 : invalidation explicite
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn explicit_invalidate_works() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key: CacheKey = (id, 0u64);
    cache
        .insert(key, dummy_effective_note(id), ContentHash([0; 32]))
        .await;

    cache.invalidate(&key).await;
    cache.run_pending_tasks().await;

    assert_eq!(
        cache.entry_count(),
        0,
        "invalidation explicite doit vider l'entrée du cache"
    );
}
