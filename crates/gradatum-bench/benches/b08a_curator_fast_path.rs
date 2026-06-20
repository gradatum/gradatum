//! B8a — Curator fast path heuristique (P1)
//!
//! Mesure `Curator<Noop>::decide()` quand l'heuristique produit une confiance
//! élevée (> 0.7 threshold) — le LLM n'est JAMAIS appelé (invariant offline-first #3).
//!
//! Cible : > 100 notes/sec.
//!
//! Setup : `CuratorConfig.confidence_threshold = Some(0.3)` → toute note avec
//! confiance heuristique > 0.3 prend le fast path.
//! L'heuristique `Heuristic::new()` retourne `confidence = 0.5` par défaut
//! pour les notes sans keywords suspects → fast path garanti.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use gradatum_bench::build_note;
use gradatum_chat::{CuratorContext, Heuristic, Noop};
use gradatum_core::config::CuratorConfig;
use gradatum_curator::Curator;

fn bench_curator_fast_path(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Seuil bas → toute note à confiance > 0.3 prend le fast path sans LLM.
    let cfg = CuratorConfig {
        confidence_threshold: Some(0.3),
        llm_review_enabled: Some(false),
        ..Default::default()
    };

    // Curator<Noop> : llm = None → LLM jamais appelé.
    let curator: Curator<Noop> = Curator::new(Heuristic::new(), None, cfg);
    let ctx = CuratorContext::default();

    // Pré-génère 100 notes sans keywords suspects → confiance heuristique élevée.
    let notes: Vec<_> = (0..100).map(|_| build_note(256)).collect();

    let mut group = c.benchmark_group("B8a-curator-fast-path");

    // Mesure 100 décisions curator en fast path heuristique.
    group.bench_function("100-notes-heuristic-fast-path", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut count = 0u32;
                for note in &notes {
                    let decision = curator.decide(black_box(note), black_box(&ctx)).await;
                    // Fast path : fallback_applied=false attendu.
                    black_box(&decision);
                    count += 1;
                }
                count
            })
        });
    });

    group.finish();
}

criterion_group!(benches, bench_curator_fast_path);
criterion_main!(benches);
