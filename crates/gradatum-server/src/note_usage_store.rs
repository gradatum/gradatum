//! Compteur d'usage PAR NOTE persisté (salience per note).
//!
//! ## Design
//!
//! Jumeau per-note de [`crate::read_usage_store`] (granularité endpoint). `NoteUsageStore`
//! ouvre sa propre `rusqlite::Connection` sur la même base (`index.db`, mode WAL —
//! multi-connexion sûr, `busy_timeout` 5000 ms). La table `note_usage` est créée par la
//! migration `0029_note_usage.sql` (exécutée par `SqliteIndex::open` via `with_search_path`).
//!
//! ## Flush batch
//!
//! Les compteurs sont accumulés en mémoire (`NoteUsageAccumulators` dans `AppState`,
//! `Mutex<HashMap>` swappé atomiquement) et flushés toutes les 60 s via
//! [`NoteUsageStore::flush_batch`]. Chaque flush effectue un UPSERT cumulatif par clé
//! `(tenant_id, note_id, kind)` : `count += excluded.count`, `last_used_ms = MAX(...)`.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` n'est ni `Send` ni `Sync` — enveloppé dans
//! `Arc<Mutex<Connection>>` (Tokio mutex). Les verrous sont tenus le temps minimum
//! (relâchés avant tout `.await`).
//!
//! ## Dimension de scope — per-NAMESPACE (`VaultId`)
//!
//! `note_usage` compte l'usage **par note**. Une note est namespacée par son vault
//! (PK composite `(vault_id, id)` depuis la migration 0032) : l'usage d'une note
//! appartient donc au **namespace** (vault), pas au principal. Le job d'audit/rétention
//! ([`crate::audit_job`]) itère par vault actif et interroge ce store avec un `vault_id`,
//! ce qui confirme la dimension namespace.
//!
//! La colonne SQL reste nommée `tenant_id` (héritage 0029, pré-désambiguïsation) : la
//! renommer exigerait une migration ; le typage `VaultId` en Rust rend la dimension
//! explicite tout en restant byte-identical (`.as_str()`).
//!
//! **Conflation résolue (W2, `arch/01KXWMDDX1`)** : [`NoteUsageStore::counts_for_notes`]
//! prend un `&VaultId` (namespace), et le chemin salience de `logic.rs` l'interroge avec
//! le vault de lecture EFFECTIF (`read_vault.vault_id()`), jamais le principal (`TenantId`
//! porté par le JWT). La salience d'une note est donc keyée par son namespace, cohérent
//! avec le read-side du job d'audit. À flag OFF `principal == vault == "main"` → la clé est
//! identique sur les deux chemins → byte-identical.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use gradatum_core::scope::VaultId;
use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tokio::sync::Mutex;

#[cfg(test)]
use rusqlite::OptionalExtension;

/// Vocabulaire FERMÉ des 5 `kind` d'usage (colonne `kind` de `note_usage`).
///
/// Ordre fixe — ne pas réarranger (les tests et les points d'incrément utilisent
/// ces constantes). `search-hit-top3` s'incrémente EN PLUS de `search-hit`
/// (sous-compteur des rangs 1-3).
///
pub const KIND_READ: &str = "read";
pub const KIND_SEARCH_HIT: &str = "search-hit";
pub const KIND_SEARCH_HIT_TOP3: &str = "search-hit-top3";
pub const KIND_RECALL_SURFACED: &str = "recall-surfaced";
pub const KIND_RECALL_ACCEPTED: &str = "recall-accepted";

/// DDL autonome de `note_usage` — miroir exact de la migration 0029 (hors INSERT
/// `_schema_migrations`, réservé au runner de migrations). Utilisé par les ouvertures
/// autonomes (`open_or_create`, `open_in_memory`) qui n'exécutent pas le runner.
const NOTE_USAGE_DDL: &str = "CREATE TABLE IF NOT EXISTS note_usage (
    tenant_id    TEXT NOT NULL,
    note_id      TEXT NOT NULL,
    kind         TEXT NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    last_used_ms INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, note_id, kind)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_note_usage_last ON note_usage (tenant_id, last_used_ms);";

/// Clé d'accumulation d'usage : `(tenant_id, note_id, kind)`.
pub type UsageKey = (String, String, String);

/// Valeur accumulée pour une clé : `(count_delta, last_used_ms)`.
pub type UsageValue = (u64, i64);

/// Erreur du store de compteurs d'usage par note.
#[derive(Debug, Error)]
pub enum NoteUsageError {
    /// Erreur SQLite sous-jacente.
    #[error("note_usage SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Thread de blocage échoué (panic ou annulation) — impossible en pratique.
    #[error("note_usage blocking thread failed")]
    Blocking,
}

/// Store de compteurs d'usage par note.
///
/// Cloneable (inner `Arc`) — injecté dans `AppState` et partagé entre les handlers
/// (indirectement, via l'accumulateur) et la tâche flush.
#[derive(Clone)]
pub struct NoteUsageStore {
    /// Connexion SQLite dédiée — séparée de `SqliteIndex` pour éviter les deadlocks.
    ///
    /// Même fichier `index.db` (WAL) — SQLite garantit la cohérence multi-connexion.
    conn: Arc<Mutex<Connection>>,
}

impl NoteUsageStore {
    /// Ouvre une connexion WAL dédiée à `path` pour la table `note_usage`.
    ///
    /// Les PRAGMAs WAL et `busy_timeout` sont appliqués immédiatement.
    /// La migration 0029 doit déjà avoir été exécutée par `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Retourne `NoteUsageError::Sqlite` si le fichier est inaccessible ou si les
    /// PRAGMAs échouent.
    pub async fn open(path: &Path) -> Result<Self, NoteUsageError> {
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
        .map_err(|_| NoteUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre ou crée une connexion sur `path`, en créant la table si absente.
    ///
    /// Contrairement à `open()` (qui exige que le fichier et la table existent déjà via
    /// la migration 0029 de `SqliteIndex`), cette méthode est autonome : elle crée le
    /// fichier SQLite si absent et applique le DDL directement.
    ///
    /// Usage : tests d'intégration qui ont besoin d'un store isolé sans `SqliteIndex`.
    ///
    /// # Errors
    ///
    /// Retourne `NoteUsageError::Sqlite` si le fichier est inaccessible ou si le DDL échoue.
    // Utilisé par les tests d'intégration via la lib crate (non callable depuis le binaire).
    #[allow(dead_code)]
    pub async fn open_or_create(path: &Path) -> Result<Self, NoteUsageError> {
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
            conn.execute_batch(NOTE_USAGE_DDL)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre une connexion en mémoire pour les tests unitaires.
    ///
    /// Crée la table `note_usage` directement (sans migration runner).
    /// NE DOIT PAS être utilisé en production.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, NoteUsageError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(NOTE_USAGE_DDL)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Flush un lot de compteurs en un seul UPSERT par ligne dans une transaction.
    ///
    /// Clé : `(tenant_id, note_id, kind)`. Valeur : `(count_delta, last_used_ms)`.
    /// Les entrées avec `count_delta = 0` sont ignorées (pas d'écriture inutile).
    /// Retourne le nombre de lignes effectivement écrites.
    ///
    /// L'UPSERT (`ON CONFLICT DO UPDATE SET count = count + excluded.count,
    /// last_used_ms = MAX(last_used_ms, excluded.last_used_ms)`) garantit l'agrégation
    /// cumulative : plusieurs flush sur la même clé s'accumulent, `last_used_ms` = max.
    ///
    /// # Errors
    ///
    /// `NoteUsageError::Sqlite` sur erreur de base de données.
    /// `NoteUsageError::Blocking` si le thread de blocage échoue.
    pub async fn flush_batch(
        &self,
        batch: HashMap<UsageKey, UsageValue>,
    ) -> Result<usize, NoteUsageError> {
        // Filtrer les entrées à zéro count — aucune écriture nécessaire.
        let entries: Vec<(UsageKey, UsageValue)> = batch
            .into_iter()
            .filter(|(_, (count, _))| *count > 0)
            .collect();

        if entries.is_empty() {
            return Ok(0);
        }

        let conn = Arc::clone(&self.conn);
        let written = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let tx = conn.unchecked_transaction()?;
            let mut written = 0usize;

            for ((tenant_id, note_id, kind), (count, last_used_ms)) in &entries {
                // count est stocké en INTEGER SQLite (i64 max). Les compteurs u64 ne
                // dépassent jamais i64::MAX dans une fenêtre de 60 s en pratique.
                let count_i64 = (*count).min(i64::MAX as u64) as i64;
                tx.execute(
                    "INSERT INTO note_usage (tenant_id, note_id, kind, count, last_used_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (tenant_id, note_id, kind)
                     DO UPDATE SET count = count + excluded.count,
                                   last_used_ms = MAX(last_used_ms, excluded.last_used_ms)",
                    params![tenant_id, note_id, kind, count_i64, last_used_ms],
                )?;
                written += 1;
            }

            tx.commit()?;
            Ok::<usize, rusqlite::Error>(written)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;

        Ok(written)
    }

    /// Batch read of per-kind usage counts for a set of notes.
    ///
    /// Returns `note_id → [(kind, count)]`; notes with no usage rows are simply
    /// absent from the map. Bounded by the RRF buffer (≤ 50 ids) at the call site.
    ///
    /// # Errors
    ///
    /// `NoteUsageError::Blocking` if the blocking thread fails, or the underlying
    /// SQLite error (wrapped) on query failure.
    ///
    /// # Dimension (`note_usage`)
    ///
    /// `note_usage` est scopé per-**NAMESPACE** (vault), cohérent avec le read-side du
    /// job d'audit (`min_last_used`/`last_used_map`, tous deux `&VaultId`) : la
    /// colonne SQL legacy `tenant_id` porte le `vault_id` effectif. Le principal (JWT
    /// `TenantId`) ne clé JAMAIS cette table — c'est le vault CIBLE (namespace) qui compte.
    /// À flag OFF `principal == vault == "main"` → byte-identical. Réf `arch/01KXWMDDX1`.
    pub async fn counts_for_notes(
        &self,
        vault_id: &VaultId,
        note_ids: &[String],
    ) -> Result<HashMap<String, Vec<(String, u64)>>, NoteUsageError> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = Arc::clone(&self.conn);
        // La colonne SQL reste nommée `tenant_id` (legacy) mais lie le vault namespace.
        let tenant_id = vault_id.as_str().to_owned();
        let note_ids: Vec<String> = note_ids.to_vec();
        let map = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let placeholders = std::iter::repeat_n("?", note_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT note_id, kind, count FROM note_usage
                 WHERE tenant_id = ?1 AND note_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&tenant_id];
            for id in &note_ids {
                params.push(id);
            }
            let mut map: HashMap<String, Vec<(String, u64)>> = HashMap::new();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (note_id, kind, count) = row?;
                map.entry(note_id)
                    .or_default()
                    .push((kind, count.max(0) as u64));
            }
            Ok::<_, rusqlite::Error>(map)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;
        Ok(map)
    }

    /// F-111 — `note_id → MAX(last_used_ms)` (tous kinds confondus) pour un tenant.
    ///
    /// Fournit à la règle d'oubli gradué la dernière utilisation observée par note.
    /// Une note absente de la map n'a **aucun** événement `note_usage` (usage nul
    /// depuis le début de collecte).
    ///
    /// # Errors
    ///
    /// Erreur SQLite / échec du thread bloquant.
    pub async fn last_used_map(
        &self,
        vault_id: &VaultId,
    ) -> Result<HashMap<String, i64>, NoteUsageError> {
        let conn = Arc::clone(&self.conn);
        // Byte-identical : `vault_id` lie la colonne SQL legacy `tenant_id`.
        let tenant_id = vault_id.as_str().to_owned();
        let map = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT note_id, MAX(last_used_ms) FROM note_usage
                 WHERE tenant_id = ?1 GROUP BY note_id",
            )?;
            let rows = stmt.query_map(params![tenant_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut map: HashMap<String, i64> = HashMap::new();
            for row in rows {
                let (note_id, last_used_ms) = row?;
                map.insert(note_id, last_used_ms);
            }
            Ok::<_, rusqlite::Error>(map)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;
        Ok(map)
    }

    /// F-111 — `MIN(last_used_ms)` global du tenant : proxy T0 du début de collecte.
    ///
    /// Table vide → `None`. Alimente la garde de fenêtre F-111
    /// ([`gradatum_curator::audit::window_covered`]) : tant que
    /// `now − T0 < usage_window_days`, le signal « 0 usage » est intenable et
    /// l'exécuteur reste inerte.
    ///
    /// # Invariant
    ///
    /// `note_usage.last_used_ms` ne contient QUE des événements postérieurs au
    /// début réel de la collecte. Tout backfill historique (import de dates
    /// antérieures) défausserait silencieusement la garde de fenêtre F-111 §4 :
    /// **interdit sans révision de la spec F-111**.
    ///
    /// # Errors
    ///
    /// Erreur SQLite / échec du thread bloquant.
    pub async fn min_last_used(&self, vault_id: &VaultId) -> Result<Option<i64>, NoteUsageError> {
        let conn = Arc::clone(&self.conn);
        // Byte-identical : `vault_id` lie la colonne SQL legacy `tenant_id`.
        let tenant_id = vault_id.as_str().to_owned();
        let min = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // SQLite retourne une ligne unique NULL sur agrégat de table vide →
            // `Option<i64>` mappe ce NULL en `None`.
            let min: Option<i64> = conn.query_row(
                "SELECT MIN(last_used_ms) FROM note_usage WHERE tenant_id = ?1",
                params![tenant_id],
                |r| r.get::<_, Option<i64>>(0),
            )?;
            Ok::<Option<i64>, rusqlite::Error>(min)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;
        Ok(min)
    }

    /// Retourne `(count, last_used_ms)` pour une clé `(tenant_id, note_id, kind)`, ou
    /// `None` si absente. Utilisé dans les tests pour vérifier l'accumulation.
    #[cfg(test)]
    pub async fn get(
        &self,
        vault_id: &VaultId,
        note_id: &str,
        kind: &str,
    ) -> Result<Option<(u64, i64)>, NoteUsageError> {
        let conn = Arc::clone(&self.conn);
        // Byte-identical : `vault_id` lie la colonne SQL legacy `tenant_id`.
        let (tenant_id, note_id, kind) = (
            vault_id.as_str().to_owned(),
            note_id.to_owned(),
            kind.to_owned(),
        );
        let row = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let row: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT count, last_used_ms FROM note_usage
                     WHERE tenant_id = ?1 AND note_id = ?2 AND kind = ?3",
                    params![tenant_id, note_id, kind],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok::<Option<(i64, i64)>, rusqlite::Error>(row)
        })
        .await
        .map_err(|_| NoteUsageError::Blocking)??;

        Ok(row.map(|(count, last_used_ms)| (count.max(0) as u64, last_used_ms)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn batch(
        entries: &[(&str, &str, &str, u64, i64)],
    ) -> HashMap<(String, String, String), (u64, i64)> {
        entries
            .iter()
            .map(|(t, n, k, c, ms)| ((t.to_string(), n.to_string(), k.to_string()), (*c, *ms)))
            .collect()
    }

    // F-111 : MAX(last_used_ms) tous kinds confondus par note
    #[tokio::test]
    async fn last_used_map_returns_max_across_kinds() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        store
            .flush_batch(batch(&[
                ("main", "n1", KIND_READ, 1, 1_000),
                ("main", "n1", KIND_SEARCH_HIT, 3, 5_000),
                ("main", "n2", KIND_READ, 1, 2_000),
            ]))
            .await
            .expect("flush");
        let map = store
            .last_used_map(&VaultId::new("main"))
            .await
            .expect("map");
        assert_eq!(map.get("n1"), Some(&5_000));
        assert_eq!(map.get("n2"), Some(&2_000));
        assert_eq!(map.len(), 2);
    }

    // F-111 : MIN(last_used_ms) global = proxy T0 collecte, tenant-scoped
    #[tokio::test]
    async fn min_last_used_is_global_t0_and_tenant_scoped() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        assert_eq!(
            store
                .min_last_used(&VaultId::new("main"))
                .await
                .expect("empty"),
            None
        );
        store
            .flush_batch(batch(&[
                ("main", "n1", KIND_READ, 1, 7_000),
                ("main", "n2", KIND_READ, 1, 3_000),
            ]))
            .await
            .expect("flush");
        assert_eq!(
            store
                .min_last_used(&VaultId::new("main"))
                .await
                .expect("t0"),
            Some(3_000)
        );
        assert_eq!(
            store
                .min_last_used(&VaultId::new("autre"))
                .await
                .expect("scoped"),
            None
        );
    }

    // F-110 Phase 2 : lecture batch — agrégats par note, notes sans usage absentes
    #[tokio::test]
    async fn counts_for_notes_returns_per_note_kind_counts() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        store
            .flush_batch(batch(&[
                ("main", "n1", KIND_READ, 2, 1_000),
                ("main", "n1", KIND_SEARCH_HIT, 5, 1_000),
                ("main", "n2", KIND_READ, 1, 1_000),
            ]))
            .await
            .expect("flush");

        let ids = vec![
            "n1".to_string(),
            "n2".to_string(),
            "n3-sans-usage".to_string(),
        ];
        let map = store
            .counts_for_notes(&VaultId::new("main"), &ids)
            .await
            .expect("batch read");
        assert_eq!(map.len(), 2); // n3 absent
        let mut n1 = map.get("n1").expect("n1").clone();
        n1.sort();
        assert_eq!(
            n1,
            vec![
                (KIND_READ.to_string(), 2u64),
                (KIND_SEARCH_HIT.to_string(), 5u64)
            ]
        );
        assert_eq!(
            map.get("n2").expect("n2"),
            &vec![(KIND_READ.to_string(), 1u64)]
        );
    }

    // Isolation tenant : un autre tenant ne voit pas les compteurs
    #[tokio::test]
    async fn counts_for_notes_is_tenant_scoped() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        store
            .flush_batch(batch(&[("main", "n1", KIND_READ, 2, 1_000)]))
            .await
            .expect("flush");
        let map = store
            .counts_for_notes(&VaultId::new("autre-tenant"), &["n1".to_string()])
            .await
            .expect("batch read");
        assert!(map.is_empty());
    }

    // Liste vide ⇒ map vide, zéro requête utile
    #[tokio::test]
    async fn counts_for_notes_empty_ids_returns_empty() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        let map = store
            .counts_for_notes(&VaultId::new("main"), &[])
            .await
            .expect("batch read");
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn flush_batch_creates_rows() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        let n = store
            .flush_batch(batch(&[("main", "01AAA", KIND_READ, 1, 100)]))
            .await
            .expect("flush");
        assert_eq!(n, 1);
        assert_eq!(
            store
                .get(&VaultId::new("main"), "01AAA", KIND_READ)
                .await
                .expect("get"),
            Some((1, 100))
        );
    }

    #[tokio::test]
    async fn flush_batch_accumulates() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        store
            .flush_batch(batch(&[("main", "01AAA", KIND_READ, 2, 100)]))
            .await
            .expect("flush 1");
        store
            .flush_batch(batch(&[("main", "01AAA", KIND_READ, 3, 90)]))
            .await
            .expect("flush 2");
        // count cumulé (2 + 3 = 5), last_used_ms = max(100, 90) = 100.
        assert_eq!(
            store
                .get(&VaultId::new("main"), "01AAA", KIND_READ)
                .await
                .expect("get"),
            Some((5, 100))
        );
    }

    #[tokio::test]
    async fn flush_batch_multi_kind() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        let n = store
            .flush_batch(batch(&[
                ("main", "01AAA", KIND_READ, 1, 100),
                ("main", "01AAA", KIND_SEARCH_HIT, 4, 100),
                ("main", "01AAA", KIND_SEARCH_HIT_TOP3, 2, 100),
            ]))
            .await
            .expect("flush");
        assert_eq!(n, 3, "3 lignes distinctes (même note, 3 kinds)");
        assert_eq!(
            store
                .get(&VaultId::new("main"), "01AAA", KIND_SEARCH_HIT)
                .await
                .expect("get"),
            Some((4, 100))
        );
        assert_eq!(
            store
                .get(&VaultId::new("main"), "01AAA", KIND_SEARCH_HIT_TOP3)
                .await
                .expect("get"),
            Some((2, 100))
        );
    }

    #[tokio::test]
    async fn flush_batch_zero_count_writes_nothing() {
        let store = NoteUsageStore::open_in_memory().await.expect("open");
        let n = store
            .flush_batch(batch(&[("main", "01AAA", KIND_READ, 0, 100)]))
            .await
            .expect("flush");
        assert_eq!(n, 0);
        assert_eq!(
            store
                .get(&VaultId::new("main"), "01AAA", KIND_READ)
                .await
                .expect("get"),
            None
        );
    }

    #[tokio::test]
    async fn migration_0029_idempotente() {
        // Double open_or_create sur le même fichier temp : le second run ne doit pas échouer
        // (CREATE TABLE IF NOT EXISTS) et préserver les données.
        let dir = std::env::temp_dir().join(format!("f110_{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("index.db");

        let s1 = NoteUsageStore::open_or_create(&path).await.expect("open 1");
        s1.flush_batch(batch(&[("main", "01AAA", KIND_RECALL_SURFACED, 1, 42)]))
            .await
            .expect("flush");
        drop(s1);

        let s2 = NoteUsageStore::open_or_create(&path)
            .await
            .expect("open 2 (idempotent)");
        assert_eq!(
            s2.get(&VaultId::new("main"), "01AAA", KIND_RECALL_SURFACED)
                .await
                .expect("get"),
            Some((1, 42)),
            "les données survivent au ré-open"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
