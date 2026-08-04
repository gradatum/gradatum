//! Test d'intégration — métrique Prometheus `gradatum_note_usage_total{kind}` (F-110, Tâche 5).
//!
//! Vérifie que le flush per-note (`flush_note_usage`) fan-out la somme des deltas par
//! kind dans la famille `note_usage_total`, présente et croissante à chaque flush.

use std::sync::Arc;

use prometheus_client::encoding::text::encode;
use tempfile::TempDir;

use gradatum_server::metrics::AppMetrics;
use gradatum_server::note_usage_store::{KIND_READ, KIND_SEARCH_HIT, NoteUsageStore};
use gradatum_server::state::NoteUsageAccumulators;
use gradatum_server::telemetry_flush::flush_note_usage;

fn encode_metrics(m: &AppMetrics) -> String {
    let mut buf = String::new();
    encode(&mut buf, &m.registry).expect("encode métriques");
    buf
}

async fn open_store(dir: &TempDir) -> NoteUsageStore {
    NoteUsageStore::open_or_create(&dir.path().join("note_usage.db"))
        .await
        .expect("NoteUsageStore::open_or_create")
}

#[tokio::test]
async fn note_usage_total_present_and_increments() {
    let metrics = AppMetrics::new();
    let tmp = TempDir::new().expect("TempDir");
    let store = open_store(&tmp).await;
    let acc = Arc::new(NoteUsageAccumulators::default());

    // Avant tout flush : la série n'existe pas encore (prometheus_client lazy).
    assert!(
        !encode_metrics(&metrics).contains("gradatum_note_usage_total"),
        "aucune série avant le premier flush"
    );

    // 1er flush : 2 events `read` (même note) + 1 `search-hit`.
    acc.record("main", "01AAA", KIND_READ, 100);
    acc.record("main", "01AAA", KIND_READ, 200);
    acc.record("main", "01BBB", KIND_SEARCH_HIT, 150);
    flush_note_usage(&acc, &store, &metrics)
        .await
        .expect("flush 1");

    let text1 = encode_metrics(&metrics);
    assert!(
        text1.contains("gradatum_note_usage_total"),
        "famille présente après flush. metrics=\n{text1}"
    );
    assert!(
        text1.contains("kind=\"read\""),
        "série kind=read présente. metrics=\n{text1}"
    );
    assert!(
        text1.contains("gradatum_note_usage_total{kind=\"read\"} 2"),
        "read = 2 events (2 records même note). metrics=\n{text1}"
    );
    assert!(
        text1.contains("gradatum_note_usage_total{kind=\"search-hit\"} 1"),
        "search-hit = 1. metrics=\n{text1}"
    );

    // 2e flush : +3 events `read` → la série croît (2 → 5).
    for _ in 0..3 {
        acc.record("main", "01AAA", KIND_READ, 300);
    }
    flush_note_usage(&acc, &store, &metrics)
        .await
        .expect("flush 2");

    let text2 = encode_metrics(&metrics);
    assert!(
        text2.contains("gradatum_note_usage_total{kind=\"read\"} 5"),
        "read croît de 2 à 5 après le 2e flush. metrics=\n{text2}"
    );
}
