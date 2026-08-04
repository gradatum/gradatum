//! B6 — Cache EffectiveNote cold/hot path (P1)
//!
//! Mesure l'insert + hot get sur `EffectiveNoteCache`.
//! Cible : cold insert < 10ms, hot get < 500µs.
//!
//! Note : `gradatum-vault::get_effective_note()` n'est pas benché directement car
//! il retourne `NoteNotFound` (vault vide sans storage backend configuré).
//! On benche directement le cache — la couche la plus critique en lecture.

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use gradatum_bench::build_note;
use gradatum_cache::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::note::EffectiveNote;
use gradatum_core::scope::VaultId;

/// Convertit une `Note` en `EffectiveNote`.
fn to_effective(note: gradatum_core::note::Note) -> EffectiveNote {
    EffectiveNote {
        id: note.id,
        frontmatter: note.frontmatter,
        body: note.body,
        version: note.version,
        content_hash: note.content_hash,
    }
}

fn bench_effective_note_cache(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let cfg = EffectiveNoteCacheConfig {
        max_capacity: 10_000,
        time_to_live: Duration::from_secs(300),
        time_to_idle: Duration::from_secs(60),
    };

    let note = build_note(512);
    let note_id = note.id;
    let content_hash = note.content_hash;
    let key: CacheKey = (VaultId::new("main"), note_id, 0u64);
    let effective = Arc::new(to_effective(note));

    let mut group = c.benchmark_group("B6-get-effective-note");

    // Cold path : insert + get immédiat (première insertion).
    group.bench_function("cold-insert", |b| {
        b.iter(|| {
            let cache = EffectiveNoteCache::new(cfg.clone());
            rt.block_on(async {
                cache
                    .insert(
                        black_box(key.clone()),
                        Arc::clone(&effective),
                        black_box(content_hash),
                    )
                    .await;
                black_box(cache.entry_count());
            });
        });
    });

    // Hot path : cache déjà peuplé, get avec validator identique.
    let hot_cache = EffectiveNoteCache::new(cfg.clone());
    rt.block_on(async {
        hot_cache
            .insert(key.clone(), Arc::clone(&effective), content_hash)
            .await;
    });

    group.bench_function("hot-get", |b| {
        b.iter(|| {
            rt.block_on(async {
                let result = hot_cache
                    .get::<_, _, ()>(black_box(key.clone()), |_id| {
                        let h = content_hash;
                        async move { Ok(h) }
                    })
                    .await
                    .expect("hot get failed");
                black_box(result);
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_effective_note_cache);
criterion_main!(benches);
