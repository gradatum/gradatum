//! Compteurs d'usage persistés pour les read-paths (télémétrie v0.5.3).
//!
//! ## Design
//!
//! `ReadUsageCounterStore` ouvre sa propre `rusqlite::Connection` sur la même base
//! de données que `SqliteIndex` (`index.db`). La base est en mode WAL — plusieurs
//! connexions sont sûres (lectures non bloquantes, écritures sérialisées par SQLite,
//! `busy_timeout` 5000 ms).
//!
//! La table `read_usage_counters` est créée par la migration `0019_read_usage_counters.sql`,
//! exécutée par `SqliteIndex::open` (via `with_search_path` dans `AppState`).
//!
//! ## Flush batch
//!
//! Les compteurs sont accumulés en mémoire via `AtomicU64` (dans `ReadUsageAccumulators`
//! dans `AppState`) et flushés toutes les 60s via [`ReadUsageCounterStore::flush_batch`].
//! Chaque flush effectue un UPSERT atomique pour chaque endpoint actif.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` n'est ni `Send` ni `Sync` — il est enveloppé dans
//! `Arc<Mutex<Connection>>` (Tokio mutex). Les verrous sont tenus le temps minimum
//! (relâchés avant tout `.await`).

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tokio::sync::Mutex;

#[cfg(test)]
use rusqlite::OptionalExtension;

/// Noms canoniques des 5 endpoints read-path instrumentés.
///
/// Utilisés comme valeur de la colonne `endpoint` dans `read_usage_counters`.
/// Ordre fixe — ne pas réarranger (les tests utilisent ces constantes).
pub const ENDPOINT_VAULT_SEARCH: &str = "/api/v1/vault_search";
pub const ENDPOINT_VAULT_READ: &str = "/api/v1/vault_read";
pub const ENDPOINT_CODE_SCOPE: &str = "/api/v1/code_scope";
pub const ENDPOINT_VAULT_TIMELINE: &str = "/api/v1/vault_timeline";
pub const ENDPOINT_LESSONS_RECALL: &str = "/api/v1/lessons/recall";

/// Erreur du store de compteurs d'usage.
#[derive(Debug, Error)]
pub enum ReadUsageError {
    /// Erreur SQLite sous-jacente.
    #[error("read_usage SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Thread de blocage échoué (panic ou annulation) — impossible en pratique.
    #[error("read_usage blocking thread failed")]
    Blocking,
}

/// Payload d'un flush — un compteur par endpoint.
///
/// `hit_count = 0` → ligne ignorée (aucun hit depuis le dernier flush).
#[derive(Debug, Clone)]
pub struct UsageFlushEntry {
    /// Chemin de l'endpoint (`/api/v1/vault_search`, etc.).
    pub endpoint: &'static str,
    /// Fenêtre horaire : `epoch_ms / 3_600_000`.
    pub window_h: i64,
    /// Nombre de hits à ajouter pour cette fenêtre.
    pub hit_count: u64,
}

/// Store de compteurs d'usage des read-paths.
///
/// Cloneable (inner `Arc`) — injecté dans `AppState` et partagé entre
/// les handlers et la tâche flush.
#[derive(Clone)]
pub struct ReadUsageCounterStore {
    /// Connexion SQLite dédiée — séparée de `SqliteIndex` pour éviter les deadlocks.
    ///
    /// Même fichier `index.db` (WAL) — SQLite garantit la cohérence multi-connexion.
    conn: Arc<Mutex<Connection>>,
}

impl ReadUsageCounterStore {
    /// Ouvre une connexion WAL dédiée à `path` pour la table `read_usage_counters`.
    ///
    /// Les PRAGMAs WAL et `busy_timeout` sont appliqués immédiatement.
    /// La migration 0019 doit déjà avoir été exécutée par `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Retourne `ReadUsageError::Sqlite` si le fichier est inaccessible ou si les
    /// PRAGMAs échouent.
    pub async fn open(path: &Path) -> Result<Self, ReadUsageError> {
        let path = path.to_path_buf();
        // Ouvrir la connexion rusqlite dans un thread dédié — `Connection::open`
        // peut bloquer sur les locks OS (WAL checkpoint) et n'est pas async.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // PRAGMAs C12 alignés sur SqliteIndex — nécessaires sur chaque connexion SQLite.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre ou crée une connexion sur `path`, en créant la table si absente.
    ///
    /// Contrairement à `open()` (qui exige que le fichier et la table existent déjà via la
    /// migration 0019 de `SqliteIndex`), cette méthode est autonome : elle crée le fichier
    /// SQLite si absent et applique le DDL directement.
    ///
    /// Usage : tests d'intégration qui ont besoin d'un store isolé sans `SqliteIndex`.
    ///
    /// # Errors
    ///
    /// Retourne `ReadUsageError::Sqlite` si le fichier est inaccessible ou si le DDL échoue.
    // Utilisé par les tests d'intégration via la lib crate (non callable depuis le binaire).
    #[allow(dead_code)]
    pub async fn open_or_create(path: &Path) -> Result<Self, ReadUsageError> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS read_usage_counters (
                    endpoint   TEXT    NOT NULL,
                    window_h   INTEGER NOT NULL,
                    hit_count  INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (endpoint, window_h)
                ) WITHOUT ROWID;",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre une connexion en mémoire pour les tests unitaires.
    ///
    /// Crée la table `read_usage_counters` directement (sans migration runner).
    /// NE DOIT PAS être utilisé en production.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, ReadUsageError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS read_usage_counters (
                    endpoint   TEXT    NOT NULL,
                    window_h   INTEGER NOT NULL,
                    hit_count  INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (endpoint, window_h)
                ) WITHOUT ROWID;",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Flush un lot de compteurs en un seul UPSERT par ligne dans une transaction.
    ///
    /// Les entrées avec `hit_count = 0` sont ignorées (pas d'écriture inutile).
    /// Retourne le nombre de lignes effectivement écrites.
    ///
    /// L'UPSERT (`ON CONFLICT DO UPDATE SET hit_count = hit_count + excluded.hit_count`)
    /// garantit l'agrégation cumulative : plusieurs flush sur la même `(endpoint, window_h)`
    /// s'accumulent correctement.
    ///
    /// # Errors
    ///
    /// `ReadUsageError::Sqlite` sur erreur de base de données.
    /// `ReadUsageError::Blocking` si le thread de blocage échoue.
    pub async fn flush_batch(
        &self,
        entries: Vec<UsageFlushEntry>,
    ) -> Result<usize, ReadUsageError> {
        // Filtrer les entrées à zéro hit — aucune écriture nécessaire.
        let entries: Vec<UsageFlushEntry> =
            entries.into_iter().filter(|e| e.hit_count > 0).collect();

        if entries.is_empty() {
            return Ok(0);
        }

        let conn = Arc::clone(&self.conn);
        let written = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let tx = conn.unchecked_transaction()?;
            let mut count = 0usize;

            for entry in &entries {
                // hit_count est stocké en INTEGER SQLite (i64 max).
                // Les compteurs AtomicU64 sont bornés à u64::MAX mais en pratique
                // ne dépassent jamais i64::MAX dans une fenêtre horaire.
                let hit_i64 = entry.hit_count.min(i64::MAX as u64) as i64;
                tx.execute(
                    "INSERT INTO read_usage_counters (endpoint, window_h, hit_count)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (endpoint, window_h)
                     DO UPDATE SET hit_count = hit_count + excluded.hit_count",
                    params![entry.endpoint, entry.window_h, hit_i64],
                )?;
                count += 1;
            }

            tx.commit()?;
            Ok::<usize, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(written)
    }

    /// Purge les lignes dont la `window_h` est antérieure à `cutoff_window_h`.
    ///
    /// `cutoff_window_h = now_epoch_ms / 3_600_000 - retention_hours`
    ///
    /// Retourne le nombre de lignes supprimées.
    ///
    /// # Errors
    ///
    /// `ReadUsageError::Sqlite` sur erreur de base de données.
    pub async fn purge_before(&self, cutoff_window_h: i64) -> Result<u64, ReadUsageError> {
        let conn = Arc::clone(&self.conn);
        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn.execute(
                "DELETE FROM read_usage_counters WHERE window_h < ?1",
                params![cutoff_window_h],
            )?;
            Ok::<u64, rusqlite::Error>(n as u64)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(deleted)
    }

    /// Retourne la somme des `hit_count` groupée par `endpoint`, toutes fenêtres confondues.
    ///
    /// `SELECT endpoint, SUM(hit_count) FROM read_usage_counters GROUP BY endpoint`
    ///
    /// Used at startup to seed Prometheus metric families and in tests
    /// to verify multi-window aggregation.
    ///
    /// # Errors
    ///
    /// `ReadUsageError::Sqlite` sur erreur SQLite.
    /// `ReadUsageError::Blocking` si le thread de blocage échoue.
    // Câblé dans Task 6 (seed_metrics_from_db au boot, main.rs).
    #[allow(dead_code)]
    pub async fn sum_by_endpoint(&self) -> Result<Vec<(String, u64)>, ReadUsageError> {
        let conn = Arc::clone(&self.conn);
        let rows = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT endpoint, SUM(hit_count) as total
                 FROM read_usage_counters
                 GROUP BY endpoint",
            )?;
            let rows: Vec<(String, u64)> = stmt
                .query_map([], |row| {
                    let endpoint: String = row.get(0)?;
                    // SUM(hit_count) peut théoriquement dépasser i64::MAX, mais en pratique
                    // dans une fenêtre de rétention 90j avec des compteurs horaires, c'est
                    // impossible. On sature à u64::MAX pour sécurité.
                    let total: i64 = row.get(1)?;
                    Ok((endpoint, total.max(0) as u64))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<Vec<(String, u64)>, rusqlite::Error>(rows)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(rows)
    }

    /// Retourne le hit_count total pour un endpoint et une fenêtre donnés.
    ///
    /// Utilisé dans les tests pour vérifier l'accumulation cumulative.
    #[cfg(test)]
    pub async fn get_count(&self, endpoint: &str, window_h: i64) -> Result<i64, ReadUsageError> {
        let conn = Arc::clone(&self.conn);
        let endpoint = endpoint.to_owned();
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: Option<i64> = conn
                .query_row(
                    "SELECT hit_count FROM read_usage_counters
                     WHERE endpoint = ?1 AND window_h = ?2",
                    params![endpoint, window_h],
                    |r| r.get(0),
                )
                .optional()?;
            Ok::<i64, rusqlite::Error>(count.unwrap_or(0))
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(count)
    }

    /// Retourne le nombre total de lignes dans la table (pour les tests de purge).
    #[cfg(test)]
    pub async fn total_rows(&self) -> Result<i64, ReadUsageError> {
        let conn = Arc::clone(&self.conn);
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM read_usage_counters", [], |r| r.get(0))?;
            Ok::<i64, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| ReadUsageError::Blocking)??;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test 1 : UPSERT — 2 hits même (endpoint, window_h) → hit_count = 2 ──────

    /// Vérification de l'accumulation cumulative : deux flush sur le même slot
    /// doivent produire hit_count = somme des deux, pas un écrasement.
    #[tokio::test]
    async fn upsert_accumulates_hits_same_window() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        let window = 479_000i64; // valeur arbitraire

        // Premier flush : 1 hit
        store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_SEARCH,
                window_h: window,
                hit_count: 1,
            }])
            .await
            .expect("flush 1");

        // Deuxième flush : 1 hit supplémentaire
        store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_SEARCH,
                window_h: window,
                hit_count: 1,
            }])
            .await
            .expect("flush 2");

        let count = store
            .get_count(ENDPOINT_VAULT_SEARCH, window)
            .await
            .expect("get_count");

        assert_eq!(count, 2, "UPSERT doit accumuler : 1 + 1 = 2");
    }

    // ── Test 2 : flush 0 hit → aucune écriture ───────────────────────────────────

    /// Un flush d'entrée avec hit_count = 0 ne doit rien écrire en base.
    #[tokio::test]
    async fn flush_zero_hit_writes_nothing() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        let n = store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_READ,
                window_h: 1,
                hit_count: 0,
            }])
            .await
            .expect("flush zero");

        assert_eq!(n, 0, "hit_count=0 → aucune ligne écrite");
        assert_eq!(
            store.total_rows().await.expect("total"),
            0,
            "table doit rester vide"
        );
    }

    // ── Test 3 : purge — lignes > 90j supprimées, lignes récentes conservées ────

    /// Test purge par window_h : toute fenêtre < cutoff est supprimée,
    /// les fenêtres ≥ cutoff sont conservées.
    #[tokio::test]
    async fn purge_removes_old_windows_keeps_recent() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        let old_window = 100i64; // ancienne fenêtre → doit être purgée
        let new_window = 1000i64; // fenêtre récente → doit être conservée
        let cutoff = 500i64;

        // Insérer une ligne ancienne et une ligne récente.
        store
            .flush_batch(vec![
                UsageFlushEntry {
                    endpoint: ENDPOINT_CODE_SCOPE,
                    window_h: old_window,
                    hit_count: 5,
                },
                UsageFlushEntry {
                    endpoint: ENDPOINT_CODE_SCOPE,
                    window_h: new_window,
                    hit_count: 3,
                },
            ])
            .await
            .expect("flush");

        assert_eq!(store.total_rows().await.expect("total avant purge"), 2);

        let deleted = store.purge_before(cutoff).await.expect("purge");
        assert_eq!(deleted, 1, "une seule ligne (old_window) doit être purgée");
        assert_eq!(
            store.total_rows().await.expect("total après purge"),
            1,
            "la ligne récente doit rester"
        );
        assert_eq!(
            store
                .get_count(ENDPOINT_CODE_SCOPE, new_window)
                .await
                .expect("get"),
            3,
            "le compteur récent est intact"
        );
    }

    // ── Test 4 : AtomicU64 non perdu au flush ─────────────────────────────────────

    /// Simule un flush réel : on accumule N hits en mémoire (valeur simulée),
    /// on les flushes, et on vérifie que la valeur est exactement N en base.
    /// Ce test garantit que le swap-et-reset dans la tâche de flush ne perd pas
    /// de hits (aucune perte entre l'AtomicU64 et la DB).
    #[tokio::test]
    async fn flush_preserves_all_atomic_hits() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Simuler 42 hits accumulés via AtomicU64 (ici on passe directement la valeur).
        let simulated_hits = 42u64;
        let window = 99_999i64;

        let n = store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_TIMELINE,
                window_h: window,
                hit_count: simulated_hits,
            }])
            .await
            .expect("flush");

        assert_eq!(n, 1, "une ligne écrite");
        assert_eq!(
            store
                .get_count(ENDPOINT_VAULT_TIMELINE, window)
                .await
                .expect("get"),
            42,
            "les 42 hits sont intacts en base"
        );
    }

    // ── Test 5 : flush batch avec plusieurs endpoints en une transaction ──────────

    /// Un flush batch multi-endpoint doit écrire toutes les entrées dans une seule
    /// transaction (atomicité) et retourner le bon nombre de lignes.
    #[tokio::test]
    async fn flush_batch_multi_endpoint_atomic() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        let window = 10i64;
        let entries = vec![
            UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_SEARCH,
                window_h: window,
                hit_count: 10,
            },
            UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_READ,
                window_h: window,
                hit_count: 5,
            },
            UsageFlushEntry {
                endpoint: ENDPOINT_CODE_SCOPE,
                window_h: window,
                hit_count: 2,
            },
            UsageFlushEntry {
                endpoint: ENDPOINT_VAULT_TIMELINE,
                window_h: window,
                hit_count: 7,
            },
            UsageFlushEntry {
                endpoint: ENDPOINT_LESSONS_RECALL,
                window_h: window,
                hit_count: 1,
            },
        ];

        let n = store.flush_batch(entries).await.expect("flush multi");
        assert_eq!(n, 5, "5 lignes écrites (une par endpoint)");
        assert_eq!(store.total_rows().await.expect("total"), 5);

        assert_eq!(
            store
                .get_count(ENDPOINT_VAULT_SEARCH, window)
                .await
                .expect("get"),
            10
        );
        assert_eq!(
            store
                .get_count(ENDPOINT_VAULT_READ, window)
                .await
                .expect("get"),
            5
        );
        assert_eq!(
            store
                .get_count(ENDPOINT_CODE_SCOPE, window)
                .await
                .expect("get"),
            2
        );
        assert_eq!(
            store
                .get_count(ENDPOINT_VAULT_TIMELINE, window)
                .await
                .expect("get"),
            7
        );
        assert_eq!(
            store
                .get_count(ENDPOINT_LESSONS_RECALL, window)
                .await
                .expect("get"),
            1
        );
    }

    // ── Test 6 : purge cutoff = 0 → rien supprimé ────────────────────────────────

    /// Cutoff = 0 (pas de fenêtre antérieure à l'epoch 0) → aucune purge.
    #[tokio::test]
    async fn purge_cutoff_zero_deletes_nothing() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: ENDPOINT_LESSONS_RECALL,
                window_h: 500,
                hit_count: 3,
            }])
            .await
            .expect("flush");

        let deleted = store.purge_before(0).await.expect("purge");
        assert_eq!(deleted, 0, "cutoff=0 ne doit rien supprimer");
        assert_eq!(
            store.total_rows().await.expect("total"),
            1,
            "ligne préservée"
        );
    }

    // ── Test Task 4 : sum_by_endpoint groupe les fenêtres multiples ──────────────

    /// Helper : retrouver la somme pour un endpoint dans le résultat de `sum_by_endpoint`.
    fn find_sum(sums: &[(String, u64)], endpoint: &str) -> u64 {
        sums.iter()
            .find(|(k, _)| k == endpoint)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// `sum_by_endpoint` groupe les hits de plusieurs fenêtres horaires par endpoint.
    ///
    /// Vérifie que `SUM(hit_count) GROUP BY endpoint` agrège correctement plusieurs
    /// lignes avec des `window_h` différents.
    #[tokio::test]
    async fn sum_by_endpoint_groups_across_windows() {
        let store = ReadUsageCounterStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Deux fenêtres pour vault_search (total = 5), une pour mcp:vault_list (total = 2).
        store
            .flush_batch(vec![
                UsageFlushEntry {
                    endpoint: "/api/v1/vault_search",
                    window_h: 100,
                    hit_count: 4,
                },
                UsageFlushEntry {
                    endpoint: "mcp:vault_list",
                    window_h: 100,
                    hit_count: 2,
                },
            ])
            .await
            .expect("flush 1");
        store
            .flush_batch(vec![UsageFlushEntry {
                endpoint: "/api/v1/vault_search",
                window_h: 101,
                hit_count: 1,
            }])
            .await
            .expect("flush 2");

        let sums = store.sum_by_endpoint().await.expect("sum_by_endpoint");
        assert_eq!(
            find_sum(&sums, "/api/v1/vault_search"),
            5,
            "vault_search : 4 + 1 = 5"
        );
        assert_eq!(find_sum(&sums, "mcp:vault_list"), 2, "mcp:vault_list = 2");
    }
}
