//! B3 — SQLite WAL INSERT sustained throughput (P0)
//!
//! Mesure le débit d'`upsert_note` en mémoire sur `SqliteIndex`.
//! Cible : > 5000 INSERT/sec.
//!
//! La base est en mémoire → latence I/O nulle, mesure du coût serde + SQL pur.
//! Sur un fichier réel NVMe ZFS, le débit sera inférieur (sync NORMAL WAL).

use criterion::{Criterion, criterion_group, criterion_main};

use gradatum_bench::build_note;
use gradatum_index::SqliteIndex;

fn bench_sqlite_wal_insert(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Ouvre l'index en mémoire une seule fois — pas de setup dans la boucle chaude.
    let idx = rt
        .block_on(async { SqliteIndex::open_in_memory().await })
        .expect("SqliteIndex::open_in_memory");

    // Pré-génère 1000 notes en dehors de la boucle de mesure.
    let notes: Vec<_> = (0..1000).map(|_| build_note(256)).collect();

    let mut group = c.benchmark_group("B3-sqlite-wal-insert");
    group.sample_size(20);

    // Mesure : insertion séquentielle de 1000 notes (pas de dédup dans cette boucle).
    group.bench_function("1000-upserts-in-memory", |b| {
        b.iter(|| {
            rt.block_on(async {
                for note in &notes {
                    idx.upsert_note(note).await.expect("upsert_note failed");
                }
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_sqlite_wal_insert);
criterion_main!(benches);
