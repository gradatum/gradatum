//! B1 — ContentHash JCS (P0)
//!
//! Mesure le coût de `ContentHash::compute()` pour 4 tailles de body.
//! Cible : < 1ms pour un body de 10KB.
//!
//! Critère de passage T14 : le bench compile et produit des mesures.
//! Le verdict final (PASS/FAIL target) est établi en T15 sur main.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use gradatum_bench::build_frontmatter;
use gradatum_core::identity::ContentHash;

fn bench_jcs_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("B1-content-hash-jcs");
    let frontmatter = build_frontmatter();

    let bodies: &[(&str, String)] = &[
        ("100B", "x".repeat(100)),
        ("1KB", "x".repeat(1024)),
        ("10KB", "x".repeat(10 * 1024)),
        ("100KB", "x".repeat(100 * 1024)),
    ];

    for (label, body) in bodies {
        group.bench_with_input(BenchmarkId::new("compute", label), body, |b, body| {
            b.iter(|| {
                let h = ContentHash::compute(black_box(&frontmatter), black_box(body.as_str()));
                black_box(h);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_jcs_hash);
criterion_main!(benches);
