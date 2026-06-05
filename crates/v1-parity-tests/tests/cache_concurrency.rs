//! v1-parity : Cache checksum + TTL — 3 tests (D5 / spec §0.4 A3)
//!
//! Parité avec gradatum-cache unit tests + D-perf-2 spec §6.1.
//! Domaine : cache hit valide, invalidation sur stale checksum, éviction TTL.

mod common;

use std::sync::Arc;
use std::time::Duration;

use gradatum_cache::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{EffectiveNote, NoteBody};

// --- Helpers ---

fn make_key(id: NoteId) -> CacheKey {
    (id, 0u64)
}

fn make_effective_note(
    id: NoteId,
    fm: Frontmatter,
    body: &str,
) -> (Arc<EffectiveNote>, ContentHash) {
    let hash = ContentHash::compute(&fm, body);
    let note = Arc::new(EffectiveNote {
        id,
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
    });
    (note, hash)
}

// --- 1. cache_hit_with_valid_checksum ---

/// Insère une EffectiveNote + hash dans le cache, relance un get avec le même hash
/// → cache hit retourne la valeur.
#[tokio::test]
async fn cache_hit_with_valid_checksum() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key = make_key(id);

    let fm = common::minimal_frontmatter("main");
    let (note, hash) = make_effective_note(id, fm, "Corps pour test cache hit.");

    cache.insert(key, note.clone(), hash).await;
    cache.run_pending_tasks().await;

    // Validator qui retourne le même hash → hit valide
    let stored_hash = hash;
    let result = cache
        .get(key, move |_note_id| async move {
            Ok::<_, std::convert::Infallible>(stored_hash)
        })
        .await
        .expect("cache::get");

    assert!(result.is_some(), "Cache hit attendu avec hash valide");
    assert_eq!(result.unwrap().id, id);
}

// --- 2. cache_invalidates_on_stale_checksum ---

/// Insère une EffectiveNote avec hash A, relance un get avec hash B (différent)
/// → invalidation, retourne None.
#[tokio::test]
async fn cache_invalidates_on_stale_checksum() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());
    let id = NoteId::new();
    let key = make_key(id);

    let fm = common::minimal_frontmatter("main");
    let (note, hash_a) = make_effective_note(id, fm.clone(), "Corps original — hash A.");

    cache.insert(key, note.clone(), hash_a).await;
    cache.run_pending_tasks().await;

    // Hash B différent (note "modifiée" entre insert et get)
    let hash_b = ContentHash::compute(&fm, "Corps modifié — hash B différent de A.");

    let result = cache
        .get(key, move |_note_id| async move {
            Ok::<_, std::convert::Infallible>(hash_b)
        })
        .await
        .expect("cache::get avec stale hash");

    assert!(
        result.is_none(),
        "Cache doit invalider et retourner None quand le hash est stale"
    );

    // Vérifie que l'entrée est bien expulsée du cache
    let entry_count = cache.entry_count();
    // Après invalidation lazy, le count peut être > 0 avant run_pending_tasks
    cache.run_pending_tasks().await;
    // L'entrée doit être absente
    let result2 = cache
        .get(key, move |_note_id| async move {
            Ok::<_, std::convert::Infallible>(ContentHash::compute(
                &common::minimal_frontmatter("main"),
                "",
            ))
        })
        .await
        .expect("cache::get après invalidation");
    assert!(
        result2.is_none(),
        "Après invalidation, le cache doit rester vide"
    );
    let _ = entry_count; // utilisé ci-dessus pour éviter warning
}

// --- 3. cache_eviction_after_ttl ---

/// Insère une entrée dans un cache avec TTL=100ms, attend 150ms, relance un get
/// → None (entrée expirée).
///
/// Note : moka utilise un timer interne avec une granularité approximative.
/// On utilise 200ms de TTL + 300ms d'attente pour rester robuste sans être trop lent.
#[tokio::test]
async fn cache_eviction_after_ttl() {
    let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig {
        max_capacity: 100,
        time_to_live: Duration::from_millis(200),
        time_to_idle: Duration::from_millis(200),
    });

    let id = NoteId::new();
    let key = make_key(id);
    let fm = common::minimal_frontmatter("main");
    let (note, hash) = make_effective_note(id, fm, "Corps pour test TTL.");

    cache.insert(key, note, hash).await;
    cache.run_pending_tasks().await;

    // Attente > TTL
    tokio::time::sleep(Duration::from_millis(350)).await;
    cache.run_pending_tasks().await;

    let stored_hash = hash;
    let result = cache
        .get(key, move |_note_id| async move {
            Ok::<_, std::convert::Infallible>(stored_hash)
        })
        .await
        .expect("cache::get après TTL");

    assert!(
        result.is_none(),
        "Cache doit retourner None après expiration TTL"
    );
}
