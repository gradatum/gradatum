//! Tests TDD — table `metric_sample` (v0.7.5 Slice 2a F-85).
//! Couvre : insert batch, query range inclusif, downsample bucket AVG, purge 14j, distinct series.

use gradatum_index::SqliteIndex;

#[tokio::test]
async fn insert_then_query_raw_points_inclusive_bounds() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    // Trois ticks à 0, 60_000, 120_000 ms pour une même série.
    idx.insert_metric_samples(0, &[("a".to_string(), 1.0)])
        .await
        .expect("t0");
    idx.insert_metric_samples(60_000, &[("a".to_string(), 2.0)])
        .await
        .expect("t1");
    idx.insert_metric_samples(120_000, &[("a".to_string(), 3.0)])
        .await
        .expect("t2");

    // bucket_ms = 60_000 → pas de downsample ; bornes inclusives [0, 120_000].
    let pts = idx
        .query_metric_timeseries(&["a".to_string()], 0, 120_000, 60_000)
        .await
        .expect("query");
    assert_eq!(pts.len(), 3, "3 points bruts");
    assert_eq!(pts[0].ts_ms, 0);
    assert_eq!(pts[2].ts_ms, 120_000, "borne haute incluse");
    assert!((pts[1].value - 2.0).abs() < 1e-9);
}

#[tokio::test]
async fn query_downsamples_by_bucket_average() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    // 4 ticks ; bucket de 120_000 ms regroupe (0,60_000) et (120_000,180_000).
    idx.insert_metric_samples(0, &[("a".to_string(), 10.0)])
        .await
        .unwrap();
    idx.insert_metric_samples(60_000, &[("a".to_string(), 20.0)])
        .await
        .unwrap();
    idx.insert_metric_samples(120_000, &[("a".to_string(), 30.0)])
        .await
        .unwrap();
    idx.insert_metric_samples(180_000, &[("a".to_string(), 50.0)])
        .await
        .unwrap();

    let pts = idx
        .query_metric_timeseries(&["a".to_string()], 0, 180_000, 120_000)
        .await
        .expect("query");
    assert_eq!(pts.len(), 2, "2 buckets");
    assert!(
        (pts[0].value - 15.0).abs() < 1e-9,
        "moyenne bucket 1 = (10+20)/2"
    );
    assert!(
        (pts[1].value - 40.0).abs() < 1e-9,
        "moyenne bucket 2 = (30+50)/2"
    );
    assert_eq!(pts[0].ts_ms, 0, "ts du bucket = MIN(ts_ms) du bucket");
    assert_eq!(pts[1].ts_ms, 120_000);
}

#[tokio::test]
async fn purge_removes_old_keeps_recent() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    let now = 1_000_000_000_000_i64;
    let old = now - 15 * 86_400_000; // -15 j
    idx.insert_metric_samples(old, &[("a".to_string(), 1.0)])
        .await
        .unwrap();
    idx.insert_metric_samples(now, &[("a".to_string(), 2.0)])
        .await
        .unwrap();

    let cutoff = now - 14 * 86_400_000;
    let deleted = idx.purge_metric_samples(cutoff).await.expect("purge");
    assert_eq!(deleted, 1, "1 ligne > 14j supprimée");

    let pts = idx
        .query_metric_timeseries(&["a".to_string()], 0, now, 60_000)
        .await
        .unwrap();
    assert_eq!(pts.len(), 1, "seule la ligne récente reste");
    assert_eq!(pts[0].ts_ms, now);
}

#[tokio::test]
async fn distinct_series_lists_unique_keys() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    idx.insert_metric_samples(0, &[("b".to_string(), 1.0), ("a".to_string(), 1.0)])
        .await
        .unwrap();
    idx.insert_metric_samples(60_000, &[("a".to_string(), 2.0)])
        .await
        .unwrap();
    let mut series = idx.list_distinct_metric_series().await.expect("distinct");
    series.sort();
    assert_eq!(series, vec!["a".to_string(), "b".to_string()]);
}

#[tokio::test]
async fn empty_series_query_returns_empty() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    let pts = idx
        .query_metric_timeseries(&[], 0, 100, 60_000)
        .await
        .expect("query");
    assert!(pts.is_empty());
}
