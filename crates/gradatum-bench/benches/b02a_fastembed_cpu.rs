//! B2a — FastEmbedCpu single embed (P0 — feature-gated)
//!
//! Feature : `fastembed-cpu` (désactivé par défaut).
//! Cible : < 100ms / single text après warm-up.
//!
//! Gated via `required-features = ["fastembed-cpu"]` dans Cargo.toml du bench.
//! En CI standard (feature off) : ce fichier ne compile pas → automatiquement skippé.
//!
//! Pour exécuter localement :
//! ```bash
//! cargo bench -p gradatum-bench --features fastembed-cpu --bench b02a_fastembed_cpu
//! ```
//! Attention : ~150MB de téléchargement de modèle ONNX au premier run.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

#[cfg(feature = "fastembed-cpu")]
fn bench_fastembed_cpu(c: &mut Criterion) {
    use gradatum_embed::FastEmbedCpu;

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Chargement du modèle — hors boucle de mesure.
    let embedder = rt.block_on(async {
        FastEmbedCpu::new().expect("FastEmbedCpu::new — modèle BGE-small-EN-v1.5 requis")
    });

    let mut group = c.benchmark_group("B2a-fastembed-cpu");
    // Benches CPU coûteux : réduire les itérations.
    group.sample_size(20);

    let text = "This is a benchmark test sentence for embedding performance measurement.";

    group.bench_function("single-embed", |b| {
        b.iter(|| {
            let vec = rt
                .block_on(async { embedder.embed(black_box(text)).await })
                .expect("embed failed");
            black_box(vec);
        });
    });

    group.finish();
}

#[cfg(not(feature = "fastembed-cpu"))]
fn bench_fastembed_cpu(_c: &mut Criterion) {
    // Feature `fastembed-cpu` non activée — bench skippé.
    // Ce cas ne devrait pas se produire car required-features = ["fastembed-cpu"]
    // empêche la compilation de ce bench sans la feature.
}

criterion_group!(benches, bench_fastembed_cpu);
criterion_main!(benches);
