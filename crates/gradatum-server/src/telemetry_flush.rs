//! Télémétrie usage — helper de flush et seed Prometheus (feat/usage-telemetry-19091).
//!
//! ## Responsabilités
//!
//! - `route_metric` : mapping fail-closed clé→famille Prometheus (P1-2 reviewer).
//! - `flush_once` : swap-reset des AtomicU64 + concat MCP + flush DB + fan-out Prometheus.
//! - `seed_metrics_from_db` : lecture DB au boot + seed familles (P1-3 — appelé avant spawn flush).
//!
//! ## Design
//!
//! Ces fonctions sont extraites de `main.rs` pour être testables via les tests d'intégration
//! (`tests/usage_telemetry_flush.rs`). La boucle flush dans `main.rs` appelle `flush_once`,
//! et le boot appelle `seed_metrics_from_db` **avant** le `tokio::spawn` de la boucle.
//!
//! ## Cardinalité
//!
//! Bornée : 5 clés `/api/v1/` + 21 clés `mcp:` = 26 séries max.
//! `route_metric` rejette toute clé hors-préfixe → aucune pollution de cardinalité.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::mcp_usage::McpToolCounters;
use crate::metrics::{AppMetrics, McpToolLabel, NoteUsageKindLabel, UsageEndpointLabel};
use crate::note_usage_store::{NoteUsageError, NoteUsageStore};
use crate::read_usage_store::ReadUsageCounterStore;
use crate::read_usage_store::UsageFlushEntry;
use crate::read_usage_store::{
    ENDPOINT_CODE_SCOPE, ENDPOINT_LESSONS_RECALL, ENDPOINT_VAULT_READ, ENDPOINT_VAULT_SEARCH,
    ENDPOINT_VAULT_TIMELINE,
};
use crate::state::{NoteUsageAccumulators, ReadUsageAccumulators};

/// Erreur retournée par `seed_metrics_from_db`.
///
/// Wraps `ReadUsageError` pour signaler une erreur de lecture DB au seed boot.
pub type SeedError = crate::read_usage_store::ReadUsageError;

/// Mapping fail-closed : clé usage → famille Prometheus.
///
/// - `key` commençant par `/api/v1/` → incrémente `metrics.read_usage` (endpoint = key).
/// - `key` commençant par `mcp:` → incrémente `metrics.mcp_tool_calls` (tool = key sans préfixe).
/// - **Sinon** : `tracing::warn!` + **aucun incrément** (P1-2 reviewer fail-closed).
///
/// Les appelants (`flush_once`, `seed_metrics_from_db`) filtrent `delta > 0` avant
/// d'appeler cette fonction — `delta = 0` n'arrive donc jamais ici.
/// Les séries non-incrémentées n'apparaissent pas dans `/metrics` (prometheus_client lazy).
///
/// Shared between `flush_once` and `seed_metrics_from_db`.
pub(crate) fn route_metric(metrics: &AppMetrics, key: &str, delta: u64) {
    if key.starts_with("/api/v1/") {
        metrics
            .read_usage
            .get_or_create(&UsageEndpointLabel {
                endpoint: key.to_owned(),
            })
            .inc_by(delta);
    } else if let Some(tool) = key.strip_prefix("mcp:") {
        metrics
            .mcp_tool_calls
            .get_or_create(&McpToolLabel {
                tool: tool.to_owned(),
            })
            .inc_by(delta);
    } else {
        tracing::warn!(
            key = key,
            "non-routable usage key, skipped (route_metric fail-closed)"
        );
    }
}

/// Effectue un cycle flush : swap AtomicU64, concat entries MCP, UPSERT DB, fan-out Prometheus.
///
/// Appelée par la boucle flush 60s dans `main.rs` ET par les tests d'intégration.
///
/// ## Ordre des opérations
///
/// 1. Swap-reset les 5 accumulators read-path → `Vec<UsageFlushEntry>`.
/// 2. Swap-reset les 21 compteurs MCP via `mcp.swap_all(window_h)`.
/// 3. Concaténer et appeler `store.flush_batch(all_entries)`.
/// 4. Si 3 réussit, pour chaque entry avec `hit_count > 0` : `route_metric(metrics, key, delta)`.
///
/// ## Erreur flush
///
/// Si `flush_batch` échoue → `Err(ReadUsageError)` est retourné immédiatement.
/// Les AtomicU64 ont déjà été swappés à 0 ; les hits de cette fenêtre sont perdus.
/// L'appelant (boucle `main.rs`) logue le warn et continue — la boucle est infaillible.
/// Le fan-out Prometheus n'a pas lieu en cas d'erreur (cohérence partielle évitée).
///
/// # Errors
///
/// `ReadUsageError::Sqlite` si l'UPSERT DB échoue.
/// `ReadUsageError::Blocking` si le thread de blocage est annulé.
pub async fn flush_once(
    accumulators: &Arc<ReadUsageAccumulators>,
    mcp: &Arc<McpToolCounters>,
    store: &ReadUsageCounterStore,
    metrics: &AppMetrics,
    window_h: i64,
) -> Result<(), crate::read_usage_store::ReadUsageError> {
    // 1. Swap-reset les 5 accumulators read-path.
    let read_entries: Vec<UsageFlushEntry> = [
        (ENDPOINT_VAULT_SEARCH, &accumulators.vault_search),
        (ENDPOINT_VAULT_READ, &accumulators.vault_read),
        (ENDPOINT_CODE_SCOPE, &accumulators.code_scope),
        (ENDPOINT_VAULT_TIMELINE, &accumulators.vault_timeline),
        (ENDPOINT_LESSONS_RECALL, &accumulators.lessons_recall),
    ]
    .iter()
    .map(|(endpoint, counter)| {
        let hit_count = counter.swap(0, Ordering::Relaxed);
        UsageFlushEntry {
            endpoint,
            window_h,
            hit_count,
        }
    })
    .collect();

    // 2. Swap-reset les 21 compteurs MCP.
    let mcp_entries: Vec<UsageFlushEntry> = mcp.swap_all(window_h);

    // 3. Concaténer et UPSERT en DB.
    let mut all_entries = read_entries;
    all_entries.extend(mcp_entries);

    // Collecter les (endpoint, hit_count) AVANT de consommer all_entries dans flush_batch,
    // pour le fan-out Prometheus (étape 4) — seulement si le flush réussit.
    let deltas: Vec<(&'static str, u64)> = all_entries
        .iter()
        .map(|e| (e.endpoint, e.hit_count))
        .collect();

    // Propager l'erreur : si flush_batch échoue, Err est retourné immédiatement.
    // Le fan-out Prometheus n'a pas lieu, ce qui évite d'incrémenter des compteurs
    // pour des hits non persistés (cohérence partielle).
    let n = store.flush_batch(all_entries).await?;

    if n > 0 {
        tracing::debug!(
            written = n,
            window_h = window_h,
            "read_usage + mcp flush: counters persisted"
        );
    }

    // 4. Fan-out Prometheus : incrementer les familles pour les deltas > 0.
    // Fait uniquement si le flush a réussi (erreur → early return via `?` ci-dessus).
    for (key, delta) in deltas {
        if delta > 0 {
            route_metric(metrics, key, delta);
        }
    }

    Ok(())
}

/// Flush l'accumulateur d'usage PAR NOTE vers `note_usage`.
///
/// Greffé sur la boucle `telemetry-flush` (même tick 60 s que [`flush_once`], second
/// flush indépendant). Swap-reset l'accumulateur mémoire puis UPSERT le batch en DB.
///
/// Best-effort absolu : l'appelant (boucle `main.rs`) logue l'erreur en `warn!` et
/// continue — un échec ne fait jamais échouer le serveur. Les hits de la fenêtre en
/// cours sont perdus (télémétrie, pas de donnée métier).
///
/// Retourne le nombre de lignes UPSERT (0 si aucun usage accumulé depuis le dernier tick).
///
/// # Errors
///
/// `NoteUsageError::Sqlite` si l'UPSERT DB échoue.
/// `NoteUsageError::Blocking` si le thread de blocage est annulé.
pub async fn flush_note_usage(
    accumulators: &Arc<NoteUsageAccumulators>,
    store: &NoteUsageStore,
    metrics: &AppMetrics,
) -> Result<usize, NoteUsageError> {
    // Swap-reset atomique : la fenêtre en cours redémarre vide même si le flush échoue.
    let batch = accumulators.swap();

    // Somme des deltas par kind AVANT de consommer le batch (fan-out Prometheus).
    // Cardinalité bornée 5 (vocabulaire fermé KIND_*).
    let mut per_kind: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for ((_, _, kind), (count, _)) in &batch {
        *per_kind.entry(kind.clone()).or_insert(0) += *count;
    }

    let written = store.flush_batch(batch).await?;

    // Fan-out Prometheus uniquement si le flush a réussi (cohérence read_usage :
    // ne pas compter des usages non persistés).
    for (kind, delta) in per_kind {
        if delta > 0 {
            metrics
                .note_usage_total
                .get_or_create(&NoteUsageKindLabel { kind })
                .inc_by(delta);
        }
    }

    if written > 0 {
        tracing::debug!(written, "note_usage flush: per-note counters persisted");
    }

    Ok(written)
}

/// Seed les familles Prometheus depuis la DB au boot.
///
/// Lit `sum_by_endpoint()` (agrégat total DB) et incrémente chaque famille via `route_metric`.
///
/// ## INVARIANT P1-3 (critique)
///
/// Cette fonction DOIT être `await`ée à sa complétion **avant** le `tokio::spawn` de la
/// boucle flush dans `main.rs`. Ordre littéral exigé :
/// ```text
/// seed_metrics_from_db(...).await;   // complet
/// tokio::spawn(flush_loop);           // puis spawn
/// ```
/// Sinon un premier flush pourrait écrire en DB une donnée que le seed relit → double-count.
///
/// ## NON-IDEMPOTENT
///
/// `inc_by` est cumulatif. Cette fonction ne doit être appelée **qu'une seule fois** au boot.
///
/// # Errors
///
/// `SeedError` si la lecture DB échoue.
pub async fn seed_metrics_from_db(
    store: &ReadUsageCounterStore,
    metrics: &AppMetrics,
) -> Result<(), SeedError> {
    let sums = store.sum_by_endpoint().await?;

    for (key, total) in &sums {
        if *total > 0 {
            route_metric(metrics, key, *total);
        }
    }

    tracing::info!(
        series = sums.len(),
        "seed Prometheus from DB at boot (usage telemetry)"
    );

    Ok(())
}
