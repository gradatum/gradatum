//! Tests : éviction par TTL et par capacité maximale.

mod common;
use common::dummy_effective_note;

use std::time::Duration;

use gradatum_cache::{EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::identity::{ContentHash, NoteId};

// ────────────────────────────────────────────────────────
// T6 : éviction TTL — entrée expirée retourne None sur get
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn ttl_eviction_returns_none_after_expiry() {
    let cfg = EffectiveNoteCacheConfig {
        max_capacity: 100,
        time_to_live: Duration::from_millis(50),
        time_to_idle: Duration::from_millis(50),
    };
    let cache = EffectiveNoteCache::new(cfg);
    let id = NoteId::new();
    let key = (id, 0u64);
    let hash = ContentHash([0x42; 32]);

    cache.insert(key, dummy_effective_note(id), hash).await;
    // run_pending_tasks nécessaire pour que entry_count reflète l'insert
    // (moka met à jour les compteurs de façon lazy).
    cache.run_pending_tasks().await;
    assert_eq!(
        cache.entry_count(),
        1,
        "entrée doit être présente avant TTL"
    );

    // Attendre l'expiration.
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Forcer le flush des tâches d'éviction en attente.
    cache.run_pending_tasks().await;

    // Sur get, moka renvoie None pour les entrées expirées (traitement interne TTL).
    let got = cache
        .get::<_, _, std::convert::Infallible>(key, |_id| async move { Ok(hash) })
        .await
        .unwrap();

    assert!(
        got.is_none(),
        "après TTL, get doit retourner None (moka traite les expirés comme des miss)"
    );
}

// ────────────────────────────────────────────────────────
// T7 : éviction LRU — entry_count <= max_capacity après overflow
// ────────────────────────────────────────────────────────

#[tokio::test]
async fn max_capacity_eviction_keeps_count_bounded() {
    let cfg = EffectiveNoteCacheConfig {
        max_capacity: 5,
        time_to_live: Duration::from_secs(60),
        time_to_idle: Duration::from_secs(60),
    };
    let cache = EffectiveNoteCache::new(cfg);

    // Insérer 20 entrées — bien au-delà de la capacité max.
    for _ in 0..20 {
        let id = NoteId::new();
        cache
            .insert((id, 0), dummy_effective_note(id), ContentHash([0; 32]))
            .await;
    }

    // Forcer le flush pour que moka applique les évictions LRU en attente.
    cache.run_pending_tasks().await;

    assert!(
        cache.entry_count() <= 5,
        "après overflow, entry_count doit être <= max_capacity (5), obtenu : {}",
        cache.entry_count()
    );
}
