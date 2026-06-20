//! Mesure latence reranker — caveat B-rev2-1.
//!
//! Mesure le path NoopReranker (overhead pur du chemin de code) sur 20 candidats.
//! La mesure ONNX réelle (JinaOnnxReranker) nécessite la feature `onnx-reranker`
//! activée + modèle local — exécution manuelle via :
//!
//! ```bash
//! RERANKER_ONNX_PATH=/var/lib/gradatum/models/reranker.onnx \
//!   cargo test -p gradatum-search --features onnx-reranker --release \
//!   reranker_latency_onnx -- --nocapture --ignored
//! ```

use gradatum_search::{NoopReranker, Reranker};

/// Statistiques p50/p95/p99 sur N runs (en microsecondes).
#[derive(Debug, Clone, Copy)]
struct LatencyStats {
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    mean_us: f64,
    min_us: f64,
    max_us: f64,
}

fn percentile(sorted_us: &[f64], p: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() as f64 - 1.0) * p / 100.0).round() as usize;
    sorted_us[idx.min(sorted_us.len() - 1)]
}

fn measure(
    reranker: &dyn Reranker,
    query: &str,
    candidates: &[(String, String)],
    runs: usize,
) -> LatencyStats {
    let mut samples_us: Vec<f64> = Vec::with_capacity(runs);
    // Warmup
    for _ in 0..10 {
        let _ = reranker.rerank(query, candidates);
    }
    // Mesure
    for _ in 0..runs {
        let t0 = std::time::Instant::now();
        let _ = reranker.rerank(query, candidates).expect("rerank");
        let elapsed_us = t0.elapsed().as_secs_f64() * 1_000_000.0;
        samples_us.push(elapsed_us);
    }
    samples_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = samples_us.iter().sum::<f64>() / samples_us.len() as f64;
    LatencyStats {
        p50_us: percentile(&samples_us, 50.0),
        p95_us: percentile(&samples_us, 95.0),
        p99_us: percentile(&samples_us, 99.0),
        mean_us: mean,
        min_us: *samples_us.first().unwrap_or(&0.0),
        max_us: *samples_us.last().unwrap_or(&0.0),
    }
}

#[test]
fn reranker_latency_noop_top20() {
    let reranker = NoopReranker;
    let query = "gradatum search architecture multi-facteur reranker test";
    let candidates: Vec<(String, String)> = (0..20)
        .map(|i| {
            (
                format!("note_{i}"),
                format!(
                    "body content #{i} with relevant keywords gradatum search architecture \
                     multi-facteur reranker test — additional padding to make the body \
                     more realistic in size, around 200-400 chars typical for a vault note. \
                     This avoids zero-cost trivial bodies that don't represent real workload."
                ),
            )
        })
        .collect();

    let stats = measure(&reranker, query, &candidates, 1000);
    println!(
        "[NOOP] N=20 candidates, runs=1000 :\n\
         p50 = {:.2} µs ({:.4} ms)\n\
         p95 = {:.2} µs ({:.4} ms)\n\
         p99 = {:.2} µs ({:.4} ms)\n\
         mean = {:.2} µs / min = {:.2} µs / max = {:.2} µs",
        stats.p50_us,
        stats.p50_us / 1000.0,
        stats.p95_us,
        stats.p95_us / 1000.0,
        stats.p99_us,
        stats.p99_us / 1000.0,
        stats.mean_us,
        stats.min_us,
        stats.max_us
    );

    // NoopReranker = pure compute, doit être < 100 µs sur du LXC raisonnable.
    assert!(
        stats.p99_us < 1000.0,
        "NoopReranker p99 doit être < 1ms, got {:.2} µs",
        stats.p99_us
    );
}

#[cfg(feature = "onnx-reranker")]
#[test]
#[ignore = "requiert RERANKER_ONNX_PATH=/path/to/model.onnx + feature onnx-reranker"]
fn reranker_latency_onnx_top20() {
    use gradatum_search::reranker::JinaOnnxReranker;

    let path = std::env::var("RERANKER_ONNX_PATH").expect("RERANKER_ONNX_PATH requis");
    let reranker = JinaOnnxReranker::from_file(&path).expect("chargement modèle ONNX");

    let query = "gradatum search architecture multi-facteur reranker test";
    let candidates: Vec<(String, String)> = (0..20)
        .map(|i| {
            (
                format!("note_{i}"),
                format!(
                    "body content #{i} with relevant keywords gradatum search architecture \
                     multi-facteur reranker test"
                ),
            )
        })
        .collect();

    // ONNX warmup + mesure réduite (50 runs) : chaque run = 20 inférences cross-encoder.
    let stats = measure(&reranker, query, &candidates, 50);
    println!(
        "[ONNX] N=20 candidates, runs=50 :\n\
         p50 = {:.2} ms\n\
         p95 = {:.2} ms\n\
         p99 = {:.2} ms\n\
         mean = {:.2} ms / min = {:.2} ms / max = {:.2} ms",
        stats.p50_us / 1000.0,
        stats.p95_us / 1000.0,
        stats.p99_us / 1000.0,
        stats.mean_us / 1000.0,
        stats.min_us / 1000.0,
        stats.max_us / 1000.0
    );

    // Cible de latence : p95 < 300ms (sinon : optimisation ou caveat assumé).
    if stats.p95_us / 1000.0 > 300.0 {
        eprintln!(
            "WARN — p95 = {:.0} ms > 300 ms (latence cible dépassée)",
            stats.p95_us / 1000.0
        );
    }
}
