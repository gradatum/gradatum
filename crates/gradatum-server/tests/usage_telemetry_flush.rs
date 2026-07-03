//! Tests d'intégration — flush télémétrie usage (feat/usage-telemetry-19091).
//!
//! Couvre :
//! - Task 5 : `flush_once` persiste les deltas (read-path + MCP) et incrémente les familles Prometheus.
//! - Task 6 : `seed_metrics_from_db` relit la DB et seed les familles (P1-3 ordonnancement boot).
//! - P1-2 (reviewer) : clé non routable → ignorée (warn, aucun incrément Prometheus).
//! - T3 (v0.7.5 F-85) : `flush_once` retourne `Result` et propage les erreurs.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use prometheus_client::encoding::text::encode;
use tempfile::TempDir;

use gradatum_server::mcp_usage::McpToolCounters;
use gradatum_server::metrics::AppMetrics;
use gradatum_server::read_usage_store::{ReadUsageCounterStore, UsageFlushEntry};
use gradatum_server::state::ReadUsageAccumulators;
use gradatum_server::telemetry_flush::{flush_once, seed_metrics_from_db};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode le registre Prometheus en texte.
fn encode_metrics(m: &AppMetrics) -> String {
    let mut buf = String::new();
    encode(&mut buf, &m.registry).expect("encode métriques");
    buf
}

/// Ouvre un `ReadUsageCounterStore` sur un fichier temporaire.
///
/// `open_in_memory()` est `#[cfg(test)]` (gated dans le crate source, inaccessible depuis
/// les tests d'intégration qui consomment la lib). On utilise `TempDir` + `open_or_create()`
/// qui crée le fichier et le DDL si absent (autonome, sans SqliteIndex).
async fn open_test_store(dir: &TempDir) -> Arc<ReadUsageCounterStore> {
    let path = dir.path().join("usage_test.db");
    Arc::new(
        ReadUsageCounterStore::open_or_create(&path)
            .await
            .expect("ReadUsageCounterStore::open_or_create"),
    )
}

/// Cherche la somme pour un endpoint dans le résultat de `sum_by_endpoint`.
fn find_sum(sums: &[(String, u64)], endpoint: &str) -> u64 {
    sums.iter()
        .find(|(k, _)| k == endpoint)
        .map(|(_, n)| *n)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Task 5 — flush_once
// ---------------------------------------------------------------------------

/// `flush_once` persiste les deltas read-path + MCP et incrémente les familles Prometheus.
///
/// Test normatif (plan Task 5, step 1) : les deltas read-path et MCP sont persistés en DB
/// et les familles Prometheus sont incrémentées.
#[tokio::test]
async fn flush_once_persists_and_increments_metrics() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_test_store(&dir).await;
    let accs = Arc::new(ReadUsageAccumulators::default());
    let mcp = Arc::new(McpToolCounters::new());
    let metrics = AppMetrics::new();

    // Simuler 3 hits vault_search (read-path) + 1 hit vault_list (MCP).
    accs.vault_search.fetch_add(3, Ordering::Relaxed);
    mcp.record("vault_list");

    flush_once(&accs, &mcp, &store, &metrics, 200)
        .await
        .expect("flush_once");

    // Vérification DB.
    let sums = store.sum_by_endpoint().await.expect("sum_by_endpoint");
    assert_eq!(
        find_sum(&sums, "/api/v1/vault_search"),
        3,
        "read-path vault_search doit être persisté"
    );
    assert_eq!(
        find_sum(&sums, "mcp:vault_list"),
        1,
        "mcp vault_list doit être persisté"
    );

    // Vérification Prometheus.
    let buf = encode_metrics(&metrics);
    assert!(
        buf.contains("gradatum_read_usage_total"),
        "gradatum_read_usage_total doit apparaître"
    );
    assert!(
        buf.contains("tool=\"vault_list\""),
        "tool=vault_list doit apparaître dans mcp_tool_calls"
    );
}

/// Un deuxième flush cumule correctement (AtomicU64 swap reset → nouveaux deltas).
#[tokio::test]
async fn flush_once_accumulates_across_flushes() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_test_store(&dir).await;
    let accs = Arc::new(ReadUsageAccumulators::default());
    let mcp = Arc::new(McpToolCounters::new());
    let metrics = AppMetrics::new();

    // Flush 1 : 2 hits vault_search.
    accs.vault_search.fetch_add(2, Ordering::Relaxed);
    flush_once(&accs, &mcp, &store, &metrics, 100)
        .await
        .expect("flush_once 1");

    // Flush 2 : 5 hits vault_search supplémentaires.
    accs.vault_search.fetch_add(5, Ordering::Relaxed);
    flush_once(&accs, &mcp, &store, &metrics, 101)
        .await
        .expect("flush_once 2");

    // La DB doit avoir 2+5 = 7 hits au total.
    let sums = store.sum_by_endpoint().await.expect("sum_by_endpoint");
    assert_eq!(find_sum(&sums, "/api/v1/vault_search"), 7);
}

// ---------------------------------------------------------------------------
// Task 6 — seed_metrics_from_db
// ---------------------------------------------------------------------------

/// `seed_metrics_from_db` relit la DB et seed les familles Prometheus.
///
/// Test normatif (plan Task 6, step 1) : seed depuis la DB → les familles reflètent les sommes.
#[tokio::test]
async fn seed_reflects_db_sum() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_test_store(&dir).await;
    let metrics = AppMetrics::new();

    // Pré-peupler la DB directement via flush_batch.
    store
        .flush_batch(vec![
            UsageFlushEntry {
                endpoint: "/api/v1/vault_read",
                window_h: 50,
                hit_count: 7,
            },
            UsageFlushEntry {
                endpoint: "mcp:vault_write",
                window_h: 50,
                hit_count: 9,
            },
        ])
        .await
        .expect("flush_batch");

    seed_metrics_from_db(&store, &metrics)
        .await
        .expect("seed_metrics_from_db");

    let buf = encode_metrics(&metrics);
    assert!(
        buf.contains("gradatum_read_usage_total"),
        "gradatum_read_usage_total doit apparaître"
    );
    assert!(
        buf.contains("endpoint=\"/api/v1/vault_read\""),
        "endpoint vault_read doit être dans read_usage"
    );
    assert!(
        buf.contains("gradatum_mcp_tool_calls_total"),
        "gradatum_mcp_tool_calls_total doit apparaître"
    );
    assert!(
        buf.contains("tool=\"vault_write\""),
        "tool vault_write doit être dans mcp_tool_calls"
    );
}

/// `seed_metrics_from_db` ignore les clés non routables (fail-closed P1-2).
///
/// Test normatif (plan Task 6, step 1b) : une clé legacy orpheline ne doit pas
/// apparaître dans les familles Prometheus.
#[tokio::test]
async fn seed_skips_unroutable_key() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_test_store(&dir).await;
    let metrics = AppMetrics::new();

    store
        .flush_batch(vec![UsageFlushEntry {
            endpoint: "legacy:weird",
            window_h: 50,
            hit_count: 99,
        }])
        .await
        .expect("flush_batch");

    seed_metrics_from_db(&store, &metrics)
        .await
        .expect("seed_metrics_from_db");

    let buf = encode_metrics(&metrics);
    // La clé orpheline ne doit apparaître ni dans read_usage ni dans mcp_tool_calls.
    assert!(
        !buf.contains("legacy:weird"),
        "clé orpheline ne doit pas apparaître dans les métriques"
    );
}

// ---------------------------------------------------------------------------
// T3 (v0.7.5 F-85) — flush_once retourne Result + propagation d'erreur
// ---------------------------------------------------------------------------

/// `flush_once` retourne `Ok(())` sur un store valide.
///
/// Vérifie que la signature `-> Result<(), ReadUsageError>` compile ET fonctionne
/// sur le chemin nominal.
#[tokio::test]
async fn flush_once_returns_ok_on_valid_store() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_test_store(&dir).await;
    let accs = Arc::new(ReadUsageAccumulators::default());
    let mcp = Arc::new(McpToolCounters::new());
    let metrics = AppMetrics::new();

    accs.vault_search.fetch_add(2, Ordering::Relaxed);

    // `flush_once` doit retourner `Ok(())` — signature vérifiée à la compilation.
    flush_once(&accs, &mcp, &store, &metrics, 500)
        .await
        .expect("flush_once doit retourner Ok sur store valide");

    // La persistance est vérifiée par les autres tests ; ici on vérifie uniquement le type.
}

/// `flush_once` propage `Err` quand `flush_batch` échoue.
///
/// Simulation : une seconde connexion rusqlite supprime la table `read_usage_counters`.
/// Le prochain flush tente un INSERT sur une table absente → SQLite error → `Err` propagé.
#[tokio::test]
async fn flush_once_propagates_err_on_flush_failure() {
    use rusqlite::Connection;

    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("usage_test.db");
    let store = Arc::new(
        ReadUsageCounterStore::open_or_create(&db_path)
            .await
            .expect("open store"),
    );
    let accs = Arc::new(ReadUsageAccumulators::default());
    let mcp = Arc::new(McpToolCounters::new());
    let metrics = AppMetrics::new();

    // Supprimer la table via une 2e connexion — la 1re connexion (store) est idle entre les flushes.
    // En WAL mode, le DDL est écrit dans le journal et visible par la 1re connexion dès sa prochaine
    // transaction → le prochain INSERT échouera avec "no such table: read_usage_counters".
    {
        let conn2 = Connection::open(&db_path).expect("2e connexion rusqlite");
        conn2
            .pragma_update(None, "journal_mode", "WAL")
            .expect("WAL mode sur conn2");
        conn2
            .execute("DROP TABLE IF EXISTS read_usage_counters", [])
            .expect("DROP TABLE via conn2");
        // conn2 droppée ici → transaction committée dans le WAL.
    }

    // Assurer au moins 1 hit > 0 pour que flush_batch ne court-circuite pas sur entries.is_empty().
    accs.vault_search.fetch_add(1, Ordering::Relaxed);

    let result = flush_once(&accs, &mcp, &store, &metrics, 600).await;
    assert!(
        result.is_err(),
        "flush_once doit retourner Err quand la table cible est absente"
    );
}
