//! B8b — Curator mixed : heuristique + fallback LLM (P1)
//!
//! Mesure `Curator<Noop>::decide()` pour un mix de notes :
//! - 70% high confidence → fast path (heuristique seule)
//! - 30% low confidence → LLM path (mais LLM = Noop → PendingReview sans réseau)
//!
//! Cible : > 50 notes/sec.
//!
//! Pour simuler 30% de notes low-confidence, on utilise des corps très courts
//! (< 50 chars) → heuristique retourne confidence=0.50.
//! Avec `confidence_threshold = Some(0.7)`, 0.50 < 0.70 → escalade LLM.
//! `llm_review_enabled = true` + `llm = Some(Arc::new(Noop))` → Noop appelé.

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use gradatum_bench::build_note;
use gradatum_chat::{CuratorContext, Heuristic, Noop};
use gradatum_core::config::CuratorConfig;
use gradatum_curator::Curator;

fn bench_curator_mixed(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Seuil spec (0.7) : les notes à confiance 0.50 escaladent vers Noop LLM.
    let cfg = CuratorConfig {
        confidence_threshold: Some(0.7),
        llm_review_enabled: Some(true),
        ..Default::default()
    };

    // Noop LLM : retourne PendingReview confidence 0.0 immédiatement — pas de réseau.
    let curator: Curator<Noop> = Curator::new(Heuristic::new(), Some(Arc::new(Noop)), cfg);
    let ctx = CuratorContext::default();

    // 70 notes "substantielles" (corps > 256 chars) → heuristique confidence ~0.65-0.80.
    // 30 notes courtes (corps < 50 chars) → heuristique confidence 0.50 < 0.70.
    let notes_high: Vec<_> = (0..70).map(|_| build_note(256)).collect();
    let notes_low: Vec<_> = (0..30).map(|_| build_note(20)).collect();

    let mut group = c.benchmark_group("B8b-curator-mixed");

    group.bench_function("100-notes-mixed-30pct-noop-llm", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut count = 0u32;
                for note in &notes_high {
                    let decision = curator.decide(black_box(note), black_box(&ctx)).await;
                    black_box(&decision);
                    count += 1;
                }
                for note in &notes_low {
                    let decision = curator.decide(black_box(note), black_box(&ctx)).await;
                    black_box(&decision);
                    count += 1;
                }
                count
            })
        });
    });

    group.finish();
}

criterion_group!(benches, bench_curator_mixed);
criterion_main!(benches);
