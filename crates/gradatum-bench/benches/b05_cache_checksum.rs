//! B5 — Cache moka + validation checksum (P1)
//!
//! Mesure la performance de `EffectiveNoteCache::get()` en hot path (hit avec
//! hash identique) et cold path (miss).
//! Target spec §3 : > 70% hit rate, p99 < 500µs.
//!
//! Ce bench simule un cache peuplé de 100 entrées puis mesure le get avec
//! validator synchrone qui retourne le même hash → cache hit garanti.

use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use gradatum_bench::build_note;
use gradatum_cache::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::note::EffectiveNote;

/// Convertit une `Note` en `EffectiveNote` (Phase 1 = identique).
fn to_effective(note: gradatum_core::note::Note) -> EffectiveNote {
    EffectiveNote {
        id: note.id,
        frontmatter: note.frontmatter,
        body: note.body,
        version: note.version,
        content_hash: note.content_hash,
    }
}

fn bench_cache_checksum(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let cfg = EffectiveNoteCacheConfig {
        max_capacity: 200,
        time_to_live: Duration::from_secs(60),
        time_to_idle: Duration::from_secs(30),
    };
    let cache = EffectiveNoteCache::new(cfg);

    // Pré-peuple le cache avec 100 notes.
    let mut keys: Vec<(CacheKey, ContentHash)> = Vec::with_capacity(100);

    rt.block_on(async {
        for _ in 0..100 {
            let note = build_note(256);
            let id = note.id;
            let hash = note.content_hash;
            let key: CacheKey = (id, 0u64);
            let effective = Arc::new(to_effective(note));
            cache.insert(key, effective, hash).await;
            keys.push((key, hash));
        }
    });

    let mut group = c.benchmark_group("B5-cache-checksum");

    // Hot path : cache hit avec hash identique.
    group.bench_function("hot-hit-100-entries", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut hits = 0u32;
                for (key, expected_hash) in &keys {
                    let result = cache
                        .get::<_, _, ()>(*key, |_note_id| {
                            let h = *expected_hash;
                            async move { Ok(h) }
                        })
                        .await
                        .expect("cache.get failed");
                    if result.is_some() {
                        hits += 1;
                    }
                }
                black_box(hits);
            });
        });
    });

    // Cold path : cache miss (clé inconnue).
    let unknown_key: CacheKey = (NoteId::new(), 999u64);
    group.bench_function("cold-miss", |b| {
        b.iter(|| {
            rt.block_on(async {
                let result = cache
                    .get::<_, _, ()>(black_box(unknown_key), |_id| async {
                        Ok(ContentHash([0u8; 32]))
                    })
                    .await
                    .expect("cache.get miss failed");
                black_box(result);
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_cache_checksum);
criterion_main!(benches);
