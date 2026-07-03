//! Proactive recall store — sessions de rappel + feedback (F-46, Active Recall v0.7.1).
//!
//! ## Design
//!
//! Miroir de [`crate::proactive_surface_store::ProactiveSurfaceStore`] : connexion
//! `rusqlite::Connection` dédiée sur le même fichier `index.db` que `SqliteIndex`,
//! en mode WAL (multi-connexion safe — lectures non bloquantes, écritures sérialisées
//! par SQLite, `busy_timeout` 5000 ms).
//!
//! Les tables `proactive_recall_sessions` et `proactive_recall_feedback` sont créées
//! par la migration `0023_proactive_recall_sessions.sql`, exécutée par `SqliteIndex::open`
//! (via `with_search_path` dans `AppState`).
//!
//! ## Sémantique
//!
//! - [`ProactiveRecallStore::insert_session`] enregistre une session de rappel proactif
//!   (liste d'ULIDs surfacés sérialisée en JSON).
//! - [`ProactiveRecallStore::get_surfaced`] retourne la liste d'ULIDs pour un `recall_id`
//!   **et un tenant** donnés, ou `None` si aucune session ne correspond (filtre tenant
//!   obligatoire — isolation cross-tenant, anti-IDOR).
//! - [`ProactiveRecallStore::record_feedback`] fait un UPSERT idempotent sur le feedback :
//!   2× le même `recall_id` → 1 seule ligne, dernière valeur conservée.
//! - [`ProactiveRecallStore::purge`] supprime les lignes par âge (cutoff) puis cap sur
//!   les deux tables, suivant le pattern de [`crate::session_trace_store::SessionTraceStore`].
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` n'est ni `Send` ni `Sync` → enveloppé dans
//! `Arc<tokio::sync::Mutex<Connection>>`. Les verrous sont tenus au minimum.
//! Les opérations bloquantes s'exécutent dans `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tokio::sync::Mutex;

/// Erreur du store `proactive_recall`.
#[derive(Debug, Error)]
pub enum ProactiveRecallError {
    /// Erreur SQLite sous-jacente.
    #[error("proactive_recall SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Erreur de (dé)sérialisation JSON de la liste d'ULIDs.
    #[error("proactive_recall JSON : {0}")]
    Json(#[from] serde_json::Error),
    /// Le thread bloquant a échoué (panic ou annulation) — impossible en pratique.
    #[error("proactive_recall thread blocking échoué")]
    Blocking,
}

/// Store sessions + feedback pour le rappel proactif (F-46, Active Recall v0.7.1).
///
/// Cloneable (inner `Arc`) — injecté dans `AppState` et partagé entre la tâche
/// de refresh proactif et les handlers de rappel.
#[derive(Clone)]
pub struct ProactiveRecallStore {
    /// Connexion SQLite dédiée — séparée de `SqliteIndex` pour éviter les deadlocks.
    ///
    /// Même fichier `index.db` (WAL) — SQLite garantit la cohérence multi-connexion.
    #[allow(dead_code)]
    conn: Arc<Mutex<Connection>>,
}

impl ProactiveRecallStore {
    /// Ouvre une connexion WAL dédiée à `path` pour les tables `proactive_recall_*`.
    ///
    /// Les PRAGMAs WAL et `busy_timeout` sont appliqués immédiatement.
    /// La migration 0023 doit déjà avoir été exécutée par `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Retourne `ProactiveRecallError::Sqlite` si le fichier est inaccessible ou si
    /// les PRAGMAs échouent.
    pub async fn open(path: &Path) -> Result<Self, ProactiveRecallError> {
        let path = path.to_path_buf();
        // Ouvrir la connexion dans un thread dédié — `Connection::open` peut bloquer
        // sur les locks OS (WAL checkpoint) et n'est pas async.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // PRAGMAs alignés sur ProactiveSurfaceStore / SessionTraceStore.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre une connexion SQLite en mémoire (dev/test uniquement).
    ///
    /// Crée les tables `proactive_recall_sessions` et `proactive_recall_feedback`
    /// directement (sans runner de migration). Le DDL est copié de
    /// `0023_proactive_recall_sessions.sql`.
    ///
    /// **Ne pas utiliser en production** — utiliser [`ProactiveRecallStore::open`]
    /// avec un chemin fichier. Méthode disponible sans gate `cfg(test)` pour permettre
    /// son usage depuis les tests d'intégration externes (`tests/`).
    #[allow(dead_code)] // Utilisée dans tests/mcp_native.rs — non visible du bin.
    pub async fn open_in_memory() -> Result<Self, ProactiveRecallError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS proactive_recall_sessions (
                    recall_id     TEXT    PRIMARY KEY,
                    tenant        TEXT    NOT NULL,
                    mode          TEXT    NOT NULL,
                    surfaced_json TEXT    NOT NULL,
                    created_ms    INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS proactive_recall_feedback (
                    recall_id     TEXT    PRIMARY KEY,
                    accepted_json TEXT    NOT NULL,
                    created_ms    INTEGER NOT NULL
                );",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Enregistre une session de rappel proactif.
    ///
    /// Sérialise `surfaced` (liste d'ULIDs) en JSON puis insère le rang.
    /// `recall_id` est la clé primaire : un doublon retourne une erreur SQLite
    /// (UNIQUE constraint) — l'appelant doit garantir l'unicité du `recall_id`.
    ///
    /// Called by `proactive_refresh_once`. Not yet used outside tests.
    ///
    /// # Errors
    ///
    /// - `ProactiveRecallError::Json` si la sérialisation échoue.
    /// - `ProactiveRecallError::Sqlite` sur erreur de base de données (dont doublon).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn insert_session(
        &self,
        recall_id: &str,
        tenant: &str,
        mode: &str,
        surfaced: &[String],
        now_ms: i64,
    ) -> Result<(), ProactiveRecallError> {
        let json = serde_json::to_string(surfaced)?;
        let recall_id = recall_id.to_owned();
        let tenant = tenant.to_owned();
        let mode = mode.to_owned();
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO proactive_recall_sessions
                    (recall_id, tenant, mode, surfaced_json, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![recall_id, tenant, mode, json, now_ms],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        Ok(())
    }

    /// Retourne la liste d'ULIDs surfacés pour `recall_id` **dans le tenant donné**,
    /// ou `None` si aucune session ne correspond aux deux critères.
    ///
    /// Désérialise le JSON stocké en `Vec<String>`. Le filtre `tenant` est obligatoire
    /// (isolation cross-tenant, anti-IDOR) : une session appartenant à un autre tenant
    /// renvoie `None` même si le `recall_id` existe.
    ///
    /// Called by the proactive recall handlers. Not yet used outside tests.
    ///
    /// # Errors
    ///
    /// - `ProactiveRecallError::Json` si la désérialisation échoue (données corrompues).
    /// - `ProactiveRecallError::Sqlite` sur erreur de base de données.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_surfaced(
        &self,
        recall_id: &str,
        tenant: &str,
    ) -> Result<Option<Vec<String>>, ProactiveRecallError> {
        let recall_id = recall_id.to_owned();
        let tenant = tenant.to_owned();
        let conn = Arc::clone(&self.conn);

        let maybe_json = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT surfaced_json FROM proactive_recall_sessions
                 WHERE recall_id = ?1 AND tenant = ?2",
            )?;
            let mut rows = stmt.query(params![recall_id, tenant])?;
            match rows.next()? {
                Some(row) => {
                    let json: String = row.get(0)?;
                    Ok::<Option<String>, rusqlite::Error>(Some(json))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        match maybe_json {
            None => Ok(None),
            Some(json) => {
                let ulids: Vec<String> = serde_json::from_str(&json)?;
                Ok(Some(ulids))
            }
        }
    }

    /// Enregistre le feedback pour une session de rappel (UPSERT idempotent).
    ///
    /// Fait un `INSERT … ON CONFLICT(recall_id) DO UPDATE` : un deuxième appel avec
    /// le même `recall_id` écrase silencieusement le feedback précédent (dernière
    /// valeur conservée). Pas d'erreur sur doublon.
    ///
    /// Called by the proactive recall feedback handlers. Not yet used outside tests.
    ///
    /// # Errors
    ///
    /// - `ProactiveRecallError::Json` si la sérialisation de `accepted` échoue.
    /// - `ProactiveRecallError::Sqlite` sur erreur de base de données.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn record_feedback(
        &self,
        recall_id: &str,
        accepted: &[String],
        now_ms: i64,
    ) -> Result<(), ProactiveRecallError> {
        let json = serde_json::to_string(accepted)?;
        let recall_id = recall_id.to_owned();
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO proactive_recall_feedback (recall_id, accepted_json, created_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(recall_id) DO UPDATE SET
                     accepted_json = excluded.accepted_json,
                     created_ms    = excluded.created_ms",
                params![recall_id, json, now_ms],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        Ok(())
    }

    /// Purge les lignes périmées selon deux critères, sur les deux tables.
    ///
    /// Pour chaque table (`proactive_recall_sessions`, `proactive_recall_feedback`) :
    /// 1. **Âge** : supprime les lignes où `created_ms < cutoff_ms`.
    /// 2. **Cap** : si le nombre de lignes restantes dépasse `max_rows`, supprime les
    ///    plus anciennes (ordre `created_ms ASC`) jusqu'à atteindre exactement `max_rows`.
    ///
    /// Retourne le nombre total de lignes supprimées sur les deux tables.
    ///
    /// Internal maintenance — called by the retention task;
    /// not yet wired in production (limited scope).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn purge(
        &self,
        cutoff_ms: i64,
        max_rows: usize,
    ) -> Result<usize, ProactiveRecallError> {
        let conn = Arc::clone(&self.conn);
        // Conversion sûre : max_rows ne dépassera pas i64::MAX en pratique (rétention).
        let max_rows_i64 = i64::try_from(max_rows).unwrap_or(i64::MAX);

        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut total_deleted = 0usize;

            // --- Table proactive_recall_sessions ---

            // Passe 1a : suppression par âge sur sessions.
            let del_sessions_age = conn.execute(
                "DELETE FROM proactive_recall_sessions WHERE created_ms < ?1",
                params![cutoff_ms],
            )?;
            total_deleted += del_sessions_age;

            // Passe 1b : cap max_rows sur sessions.
            let sessions_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM proactive_recall_sessions", [], |r| {
                    r.get(0)
                })?;
            if sessions_count > max_rows_i64 {
                let excess = sessions_count - max_rows_i64;
                let del_sessions_cap = conn.execute(
                    "DELETE FROM proactive_recall_sessions WHERE recall_id IN (
                        SELECT recall_id FROM proactive_recall_sessions
                        ORDER BY created_ms ASC LIMIT ?1
                    )",
                    params![excess],
                )?;
                total_deleted += del_sessions_cap;
            }

            // --- Table proactive_recall_feedback ---

            // Passe 2a : suppression par âge sur feedback.
            let del_feedback_age = conn.execute(
                "DELETE FROM proactive_recall_feedback WHERE created_ms < ?1",
                params![cutoff_ms],
            )?;
            total_deleted += del_feedback_age;

            // Passe 2b : cap max_rows sur feedback.
            let feedback_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM proactive_recall_feedback", [], |r| {
                    r.get(0)
                })?;
            if feedback_count > max_rows_i64 {
                let excess = feedback_count - max_rows_i64;
                let del_feedback_cap = conn.execute(
                    "DELETE FROM proactive_recall_feedback WHERE recall_id IN (
                        SELECT recall_id FROM proactive_recall_feedback
                        ORDER BY created_ms ASC LIMIT ?1
                    )",
                    params![excess],
                )?;
                total_deleted += del_feedback_cap;
            }

            Ok::<usize, rusqlite::Error>(total_deleted)
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;

        Ok(deleted)
    }

    /// Retourne le nombre total de lignes dans `proactive_recall_sessions`.
    ///
    /// Utilisé par les tests (assertions post-insert/purge).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn count_sessions(&self) -> Result<i64, ProactiveRecallError> {
        let conn = Arc::clone(&self.conn);
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM proactive_recall_sessions", [], |r| {
                    r.get(0)
                })?;
            Ok::<i64, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;
        Ok(count)
    }

    /// Retourne le nombre total de lignes dans `proactive_recall_feedback`.
    ///
    /// Utilisé par les tests (assertions post-feedback/purge).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn count_feedback(&self) -> Result<i64, ProactiveRecallError> {
        let conn = Arc::clone(&self.conn);
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM proactive_recall_feedback", [], |r| {
                    r.get(0)
                })?;
            Ok::<i64, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| ProactiveRecallError::Blocking)??;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `insert_session` suivi de `get_surfaced` retourne exactement la liste d'ULIDs insérée.
    #[tokio::test]
    async fn insert_session_then_get_surfaced_returns_ulids() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        let ulids = vec![
            "01JABC000000000000000000".to_owned(),
            "01JDEF000000000000000000".to_owned(),
        ];
        store
            .insert_session("recall-001", "main", "salience", &ulids, 1_000_000)
            .await
            .expect("insert_session");

        let result = store
            .get_surfaced("recall-001", "main")
            .await
            .expect("get_surfaced");
        assert_eq!(
            result,
            Some(ulids),
            "get_surfaced doit retourner exactement les ULIDs insérés"
        );
    }

    /// `get_surfaced` sur un `recall_id` absent retourne `None`.
    #[tokio::test]
    async fn get_surfaced_absent_recall_id_returns_none() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        let result = store
            .get_surfaced("recall-inexistant", "main")
            .await
            .expect("get_surfaced absent");
        assert_eq!(result, None, "recall_id absent → None");
    }

    /// FIX 1 (anti-IDOR) : `get_surfaced` filtre par tenant — une session du tenant A
    /// est invisible pour le tenant B même quand le `recall_id` existe.
    #[tokio::test]
    async fn get_surfaced_filters_by_tenant() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        let ulids = vec!["01JABC000000000000000000".to_owned()];
        store
            .insert_session("recall-xt", "tenant-a", "salience", &ulids, 1_000)
            .await
            .expect("insert_session tenant-a");

        // Tenant correct → la session est lisible.
        let same = store
            .get_surfaced("recall-xt", "tenant-a")
            .await
            .expect("get_surfaced tenant-a");
        assert_eq!(same, Some(ulids), "tenant propriétaire → session lisible");

        // Tenant différent, même recall_id → None (pas de fuite cross-tenant).
        let cross = store
            .get_surfaced("recall-xt", "tenant-b")
            .await
            .expect("get_surfaced tenant-b");
        assert_eq!(cross, None, "tenant étranger → None (anti-IDOR)");
    }

    /// FIX 2 (rétention) : `purge` par âge supprime aussi les lignes de la table
    /// `proactive_recall_feedback` (couverture du chemin câblé dans `main.rs`).
    #[tokio::test]
    async fn purge_removes_old_feedback_by_age() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Un feedback ancien (created_ms=0) et un récent.
        store
            .record_feedback("recall-old", &[], 0)
            .await
            .expect("record old");
        store
            .record_feedback("recall-new", &[], 2_000_000)
            .await
            .expect("record new");

        // cutoff=1_000_000 : old (0) en dessous, new (2M) au-dessus ; cap désactivé.
        let removed = store
            .purge(1_000_000, usize::MAX)
            .await
            .expect("purge feedback age");
        assert_eq!(removed, 1, "le feedback old doit être supprimé par âge");

        let remaining = store.count_feedback().await.expect("count_feedback");
        assert_eq!(
            remaining, 1,
            "il doit rester 1 feedback après purge par âge"
        );
    }

    /// `record_feedback` est idempotent : 2 appels avec le même `recall_id` →
    /// 1 seule ligne dans la table, dernière valeur conservée, pas d'erreur.
    #[tokio::test]
    async fn record_feedback_idempotent_upsert() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        let accepted_v1 = vec!["01JABC000000000000000000".to_owned()];
        let accepted_v2 = vec![
            "01JABC000000000000000000".to_owned(),
            "01JDEF000000000000000000".to_owned(),
        ];

        // Premier enregistrement.
        store
            .record_feedback("recall-001", &accepted_v1, 1_000)
            .await
            .expect("record_feedback v1");

        // Deuxième enregistrement — même recall_id, valeur différente.
        store
            .record_feedback("recall-001", &accepted_v2, 2_000)
            .await
            .expect("record_feedback v2 idempotent");

        // Vérifie qu'il n'y a qu'une seule ligne (pas d'erreur, pas de doublon).
        let count = store.count_feedback().await.expect("count_feedback");
        assert_eq!(count, 1, "2× record_feedback → 1 seule ligne dans la table");
    }

    /// `purge` par âge : lignes avec `created_ms < cutoff_ms` sont supprimées.
    #[tokio::test]
    async fn purge_removes_old_sessions_by_age() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Une session ancienne (created_ms=0) et une récente.
        store
            .insert_session("recall-old", "main", "salience", &[], 0)
            .await
            .expect("insert old");
        store
            .insert_session("recall-new", "main", "salience", &[], 2_000_000)
            .await
            .expect("insert new");

        // cutoff=1_000_000 : la session old (0) est en dessous, la new (2M) est au-dessus.
        // max_rows très grand pour ne pas déclencher le cap.
        let removed = store.purge(1_000_000, usize::MAX).await.expect("purge age");
        assert_eq!(removed, 1, "la session old doit être supprimée par âge");

        let remaining = store.count_sessions().await.expect("count_sessions");
        assert_eq!(remaining, 1, "il doit rester 1 session après purge par âge");
    }

    /// `purge` par cap : si sessions > max_rows, les plus anciennes sont supprimées.
    #[tokio::test]
    async fn purge_caps_sessions_to_max_rows() {
        let store = ProactiveRecallStore::open_in_memory()
            .await
            .expect("open in-memory");

        // 4 sessions, toutes récentes (cutoff=0 → rien par âge).
        for i in 0..4_u64 {
            store
                .insert_session(
                    &format!("recall-{i:03}"),
                    "main",
                    "salience",
                    &[],
                    i64::try_from(100 + i).expect("i < i64::MAX"),
                )
                .await
                .expect("insert");
        }

        // cap=2 → les 2 plus anciennes doivent être supprimées.
        let removed = store.purge(0, 2).await.expect("purge cap");
        assert_eq!(removed, 2, "purge cap doit supprimer 4-2=2 sessions");

        let remaining = store.count_sessions().await.expect("count_sessions");
        assert_eq!(
            remaining, 2,
            "il doit rester exactement 2 sessions après cap"
        );
    }
}
