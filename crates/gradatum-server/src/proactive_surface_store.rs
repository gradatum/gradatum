//! Proactive surface store — latest-per-tenant UPSERT for the proactive recall surface.
//!
//! ## Design
//!
//! Miroir de [`crate::session_trace_store::SessionTraceStore`] : connexion
//! `rusqlite::Connection` dédiée sur le même fichier `index.db` que `SqliteIndex`,
//! en mode WAL (multi-connexion safe — lectures non bloquantes, écritures sérialisées
//! par SQLite, `busy_timeout` 5000 ms).
//!
//! La table `proactive_surface` est créée par la migration `0022_proactive_surface.sql`,
//! exécutée par `SqliteIndex::open` (via `with_search_path` dans `AppState`).
//!
//! ## Sémantique latest-par-tenant
//!
//! Un seul rang par `tenant_id` : [`ProactiveSurfaceStore::upsert_surface`] fait un
//! `INSERT … ON CONFLICT(tenant_id) DO UPDATE` qui écrase inconditionnellement.
//! [`ProactiveSurfaceStore::get_surface`] retourne `None` si le tenant est absent.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` n'est ni `Send` ni `Sync` → enveloppé dans
//! `Arc<tokio::sync::Mutex<Connection>>`. Les verrous sont tenus au minimum.
//! Les opérations bloquantes s'exécutent dans `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Arc;

use gradatum_dto::ProactiveHit;
use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tokio::sync::Mutex;

/// Erreur du store `proactive_surface`.
#[derive(Debug, Error)]
pub enum ProactiveSurfaceError {
    /// Erreur SQLite sous-jacente.
    #[error("proactive_surface SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Erreur de (dé)sérialisation JSON de la surface.
    #[error("proactive_surface JSON : {0}")]
    Json(#[from] serde_json::Error),
    /// Le thread bloquant a échoué (panic ou annulation) — impossible en pratique.
    #[error("proactive_surface thread blocking échoué")]
    Blocking,
}

/// Store UPSERT latest-par-tenant pour la surface proactive pré-calculée.
///
/// Cloneable (inner `Arc`) — injecté dans `AppState` et partagé entre la tâche
/// de refresh proactif et les handlers de rappel.
#[derive(Clone)]
pub struct ProactiveSurfaceStore {
    /// Connexion SQLite dédiée — séparée de `SqliteIndex` pour éviter les deadlocks.
    ///
    /// Même fichier `index.db` (WAL) — SQLite garantit la cohérence multi-connexion.
    /// Used by `upsert_surface` and `get_surface` (Active Recall).
    #[allow(dead_code)]
    conn: Arc<Mutex<Connection>>,
}

impl ProactiveSurfaceStore {
    /// Ouvre une connexion WAL dédiée à `path` pour la table `proactive_surface`.
    ///
    /// Les PRAGMAs WAL et `busy_timeout` sont appliqués immédiatement.
    /// La migration 0022 doit déjà avoir été exécutée par `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Retourne `ProactiveSurfaceError::Sqlite` si le fichier est inaccessible ou si
    /// les PRAGMAs échouent.
    pub async fn open(path: &Path) -> Result<Self, ProactiveSurfaceError> {
        let path = path.to_path_buf();
        // Ouvrir la connexion dans un thread dédié — `Connection::open` peut bloquer
        // sur les locks OS (WAL checkpoint) et n'est pas async.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // PRAGMAs alignés sur SessionTraceStore / EventLogStore.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ProactiveSurfaceError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre une connexion en mémoire pour les tests unitaires.
    ///
    /// Crée la table `proactive_surface` directement (sans runner de migration).
    /// Le DDL est copié de `0022_proactive_surface.sql`.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, ProactiveSurfaceError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS proactive_surface (
                    tenant_id    TEXT    PRIMARY KEY,
                    surface_json TEXT    NOT NULL,
                    updated_ms   INTEGER NOT NULL
                );",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| ProactiveSurfaceError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Écrase la surface proactive pour `tenant` (UPSERT latest-par-tenant).
    ///
    /// Sérialise `surface` en JSON puis fait un `INSERT … ON CONFLICT(tenant_id) DO UPDATE`.
    /// Un appel ultérieur avec le même `tenant` remplace entièrement la surface précédente.
    ///
    /// Called by `proactive_refresh_once`. Not yet used outside tests.
    ///
    /// # Errors
    ///
    /// - `ProactiveSurfaceError::Json` si la sérialisation échoue.
    /// - `ProactiveSurfaceError::Sqlite` sur erreur de base de données.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn upsert_surface(
        &self,
        tenant: &str,
        surface: &[ProactiveHit],
        now_ms: i64,
    ) -> Result<(), ProactiveSurfaceError> {
        let json = serde_json::to_string(surface)?;
        let tenant = tenant.to_owned();
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO proactive_surface (tenant_id, surface_json, updated_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(tenant_id) DO UPDATE SET
                     surface_json = excluded.surface_json,
                     updated_ms   = excluded.updated_ms",
                params![tenant, json, now_ms],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|_| ProactiveSurfaceError::Blocking)??;

        Ok(())
    }

    /// Retourne la surface proactive du `tenant`, ou `None` si le tenant est absent.
    ///
    /// Désérialise le JSON stocké en `Vec<ProactiveHit>`.
    ///
    /// Called by the proactive recall handlers. Not yet used outside tests.
    ///
    /// # Errors
    ///
    /// - `ProactiveSurfaceError::Json` si la désérialisation échoue (données corrompues).
    /// - `ProactiveSurfaceError::Sqlite` sur erreur de base de données.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_surface(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<ProactiveHit>>, ProactiveSurfaceError> {
        let tenant = tenant.to_owned();
        let conn = Arc::clone(&self.conn);

        let maybe_json = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt =
                conn.prepare("SELECT surface_json FROM proactive_surface WHERE tenant_id = ?1")?;
            let mut rows = stmt.query(params![tenant])?;
            match rows.next()? {
                Some(row) => {
                    let json: String = row.get(0)?;
                    Ok::<Option<String>, rusqlite::Error>(Some(json))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|_| ProactiveSurfaceError::Blocking)??;

        match maybe_json {
            None => Ok(None),
            Some(json) => {
                let hits: Vec<ProactiveHit> = serde_json::from_str(&json)?;
                Ok(Some(hits))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit deux `ProactiveHit` distincts pour les tests.
    fn hit_a() -> ProactiveHit {
        ProactiveHit {
            ulid: "01JABC000000000000000000".into(),
            title: "Leçon A".into(),
            section: "lessons-learned".into(),
            snippet: "Corps de la leçon A.".into(),
            score: 0.9,
        }
    }

    fn hit_b() -> ProactiveHit {
        ProactiveHit {
            ulid: "01JDEF000000000000000000".into(),
            title: "Décision B".into(),
            section: "decisions".into(),
            snippet: "Corps de la décision B.".into(),
            score: 0.7,
        }
    }

    /// `upsert` puis `get` retourne la surface insérée (égalité structurelle).
    #[tokio::test]
    async fn upsert_then_get_returns_surface() {
        let store = ProactiveSurfaceStore::open_in_memory()
            .await
            .expect("open in-memory");
        let surface = vec![hit_a(), hit_b()];
        store
            .upsert_surface("main", &surface, 1_000_000)
            .await
            .expect("upsert");

        let result = store.get_surface("main").await.expect("get");
        assert_eq!(
            result,
            Some(surface),
            "get doit retourner exactement la surface upsertée"
        );
    }

    /// Ré-`upsert` avec le même tenant écrase la surface précédente.
    #[tokio::test]
    async fn reupsert_same_tenant_overwrites() {
        let store = ProactiveSurfaceStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Premier upsert : surface avec hit_a + hit_b.
        store
            .upsert_surface("main", &[hit_a(), hit_b()], 1_000)
            .await
            .expect("upsert 1");

        // Deuxième upsert : surface réduite à hit_b seulement.
        let nouvelle_surface = vec![hit_b()];
        store
            .upsert_surface("main", &nouvelle_surface, 2_000)
            .await
            .expect("upsert 2");

        let result = store.get_surface("main").await.expect("get");
        assert_eq!(
            result,
            Some(nouvelle_surface),
            "le deuxième upsert doit écraser le premier — get retourne la nouvelle surface"
        );
    }

    /// `get` sur un tenant absent retourne `None`.
    #[tokio::test]
    async fn get_absent_tenant_returns_none() {
        let store = ProactiveSurfaceStore::open_in_memory()
            .await
            .expect("open in-memory");

        let result = store
            .get_surface("tenant-inexistant")
            .await
            .expect("get absent");
        assert_eq!(result, None, "tenant absent → None");
    }

    /// Tenants distincts sont isolés — l'upsert sur l'un n'affecte pas l'autre.
    #[tokio::test]
    async fn distinct_tenants_are_isolated() {
        let store = ProactiveSurfaceStore::open_in_memory()
            .await
            .expect("open in-memory");

        store
            .upsert_surface("tenant-a", &[hit_a()], 1_000)
            .await
            .expect("upsert a");
        store
            .upsert_surface("tenant-b", &[hit_b()], 2_000)
            .await
            .expect("upsert b");

        let a = store.get_surface("tenant-a").await.expect("get a");
        let b = store.get_surface("tenant-b").await.expect("get b");
        assert_eq!(a, Some(vec![hit_a()]), "tenant-a doit retourner hit_a");
        assert_eq!(b, Some(vec![hit_b()]), "tenant-b doit retourner hit_b");
    }

    /// Une surface vide s'upserte et se retourne correctement.
    #[tokio::test]
    async fn upsert_empty_surface_roundtrip() {
        let store = ProactiveSurfaceStore::open_in_memory()
            .await
            .expect("open in-memory");
        store
            .upsert_surface("main", &[], 1_000)
            .await
            .expect("upsert vide");
        let result = store.get_surface("main").await.expect("get vide");
        assert_eq!(result, Some(vec![]), "surface vide → Some([])");
    }
}
