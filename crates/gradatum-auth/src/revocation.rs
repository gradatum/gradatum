//! RevocationStore — caveat C2 (design spec P2.0a).
//!
//! Trait + 2 impls :
//! - [`InMemoryRevocationStore`] : dev-only, émet un WARN au boot, pas de persistance.
//! - [`SqliteRevocationStore`] : production, WAL, GC périodique.
//!
//! [`boot_guard_check`] refuse le démarrage si bind est non-loopback ET store = "memory".

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dashmap::DashMap;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

/// Erreurs possibles du RevocationStore.
#[derive(Debug, thiserror::Error)]
pub enum RevocationError {
    /// Erreur SQLite (via sqlx).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),
    /// Erreur de calcul de temps système.
    #[error("system time error: {0}")]
    Time(#[from] std::time::SystemTimeError),
}

/// Trait de révocation de tokens JWT (jti).
///
/// Implémentations : [`InMemoryRevocationStore`] (dev) + [`SqliteRevocationStore`] (prod).
/// Toujours manipulé derrière un `Arc<dyn RevocationStore>` dans AppState.
#[async_trait]
pub trait RevocationStore: Send + Sync + 'static {
    /// Retourne `true` si le jti est révoqué ET pas encore expiré.
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError>;

    /// Révoque le jti jusqu'à `exp`.
    ///
    /// Idempotent : un second appel pour le même jti met à jour `exp`.
    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError>;

    /// Supprime les entrées expirées. Retourne le nombre de lignes supprimées.
    async fn gc(&self) -> Result<usize, RevocationError>;
}

// ─── InMemoryRevocationStore ─────────────────────────────────────────────────

/// Store mémoire — DEV ONLY.
///
/// Émet un WARN tracing au boot. Pas de persistance : un redémarrage efface toutes les révocations.
/// Interdit en bind non-loopback (voir [`boot_guard_check`]).
pub struct InMemoryRevocationStore {
    inner: DashMap<String, SystemTime>,
}

impl InMemoryRevocationStore {
    /// Crée un nouveau store en mémoire. Émet un WARN tracing.
    pub fn new() -> Self {
        tracing::warn!(
            "InMemoryRevocationStore activé — DEV ONLY. \
             Un redémarrage efface toutes les révocations. \
             Utiliser SqliteRevocationStore en production."
        );
        Self {
            inner: DashMap::new(),
        }
    }
}

impl Default for InMemoryRevocationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RevocationStore for InMemoryRevocationStore {
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError> {
        match self.inner.get(jti) {
            Some(exp) => Ok(*exp > SystemTime::now()),
            None => Ok(false),
        }
    }

    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError> {
        self.inner.insert(jti.to_string(), exp);
        Ok(())
    }

    async fn gc(&self) -> Result<usize, RevocationError> {
        let now = SystemTime::now();
        // Collecter d'abord pour éviter de tenir une référence DashMap pendant remove.
        let to_remove: Vec<String> = self
            .inner
            .iter()
            .filter(|kv| *kv.value() <= now)
            .map(|kv| kv.key().clone())
            .collect();
        let count = to_remove.len();
        for k in &to_remove {
            self.inner.remove(k);
        }
        Ok(count)
    }
}

// ─── SqliteRevocationStore ────────────────────────────────────────────────────

/// Store SQLite — production.
///
/// Schéma : `revoked(jti TEXT PK, exp INTEGER NOT NULL, revoked_at INTEGER NOT NULL)`.
/// WAL activé. Pool limité à 4 connexions (lectures concurrentes faibles attendues).
pub struct SqliteRevocationStore {
    pool: SqlitePool,
}

impl SqliteRevocationStore {
    /// Ouvre ou crée la base SQLite à `db_path`.
    ///
    /// Crée la table `revoked` si elle n'existe pas. Active le mode WAL.
    pub async fn new(db_path: &Path) -> Result<Self, RevocationError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        // Schéma idempotent.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS revoked (
                jti        TEXT    PRIMARY KEY,
                exp        INTEGER NOT NULL,
                revoked_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl RevocationStore for SqliteRevocationStore {
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let row: Option<(i64,)> = sqlx::query_as("SELECT exp FROM revoked WHERE jti = ?1")
            .bind(jti)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|(exp,)| exp > now_secs))
    }

    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError> {
        let exp_secs = exp.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        sqlx::query("INSERT OR REPLACE INTO revoked (jti, exp, revoked_at) VALUES (?1, ?2, ?3)")
            .bind(jti)
            .bind(exp_secs)
            .bind(now_secs)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn gc(&self) -> Result<usize, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let result = sqlx::query("DELETE FROM revoked WHERE exp <= ?1")
            .bind(now_secs)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() as usize)
    }
}

// ─── Boot guard ──────────────────────────────────────────────────────────────

/// Vérifie la combinaison bind × revocation_store au démarrage.
///
/// # Règle (caveat C2)
///
/// Interdit : bind non-loopback + `revocation_store == "memory"`.
/// Raison : un store en mémoire perd les révocations au restart et ne doit pas
/// être exposé sur une interface réseau publique/LAN.
///
/// # Arguments
///
/// - `bind_is_loopback` : `true` si `ServerConfig.server.bind.ip().is_loopback()`
/// - `revocation_store` : valeur du champ `ServerConfig.auth.revocation_store`
///
/// # Erreurs
///
/// Retourne `Err(&'static str)` avec le message explicatif — appelant doit `eprintln!` + exit(1).
pub fn boot_guard_check(
    bind_is_loopback: bool,
    revocation_store: &str,
) -> Result<(), &'static str> {
    if !bind_is_loopback && revocation_store == "memory" {
        Err(
            "revocation_store=memory est interdit quand bind est non-loopback (caveat C2). \
             Utilisez revocation_store=sqlite en production.",
        )
    } else {
        Ok(())
    }
}
