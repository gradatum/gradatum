//! Élection leader via SQLite CAS sur la table `worker_leadership`.
//!
//! Mécanisme : slot unique (slot=0). Chaque worker tente :
//! 1. `INSERT OR IGNORE` pour créer le slot s'il n'existe pas encore.
//! 2. `UPDATE WHERE expires_at < now OR holder = self` pour prendre le slot
//!    si le leader actuel a expiré ou si c'est déjà nous.
//!
//! Les timestamps `expires_at` sont stockés en **millisecondes Unix** pour
//! permettre les tests avec des durées sub-second (< 1 seconde).
//!
//! Le renewal loop tourne en tâche Tokio séparée et maintient le bail en vie.
//! Si le renewal échoue (DB inaccessible, lost lock), la tâche se termine
//! silencieusement — le worker principal doit surveiller le `JoinHandle`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;

/// Configuration de l'élection leader.
#[derive(Debug, Clone)]
pub struct LeaderConfig {
    /// Fréquence de renouvellement du bail.
    pub renew_every: Duration,
    /// Durée de validité du bail (doit être > `renew_every` pour éviter la famine).
    pub expires_after: Duration,
}

impl Default for LeaderConfig {
    fn default() -> Self {
        Self {
            renew_every: Duration::from_secs(30),
            expires_after: Duration::from_secs(60),
        }
    }
}

/// Handle d'élection leader pour un worker.
///
/// Clone-able : le même handle peut être passé à la renewal loop et au
/// code de dispatch sans duplication de la connexion DB.
#[derive(Clone)]
pub struct LeaderElection {
    pool: Arc<SqlitePool>,
    /// Identifiant unique de ce worker (ULID généré à la création).
    holder: String,
    cfg: LeaderConfig,
}

impl LeaderElection {
    /// Crée un nouveau handle d'élection.
    ///
    /// Le `pool` doit déjà avoir le schéma `worker_leadership` appliqué
    /// (via [`gradatum_queue::schema::SCHEMA_V1`]).
    pub async fn new(pool: Arc<SqlitePool>, cfg: LeaderConfig) -> anyhow::Result<Self> {
        let holder = ulid::Ulid::new().to_string();
        Ok(Self { pool, holder, cfg })
    }

    /// Timestamp courant en millisecondes Unix.
    ///
    /// Utilise des millisecondes pour permettre les tests avec des durées
    /// sub-second (ex. `expires_after = Duration::from_millis(300)`).
    ///
    /// # Panics
    ///
    /// Impossible en pratique : panique uniquement si l'horloge système est
    /// antérieure à l'epoch Unix, ce qui ne peut se produire sur un système
    /// correctement configuré.
    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("horloge système valide (post-epoch)")
            .as_millis() as i64
    }

    /// Tente d'acquérir (ou de reconfirmer) le leadership.
    ///
    /// Retourne `true` si ce worker est désormais leader, `false` sinon.
    ///
    /// # Algorithme
    ///
    /// 1. `INSERT OR IGNORE` : si le slot n'existe pas, on devient leader.
    /// 2. `UPDATE WHERE expires_at < now OR holder = self` : si le leader
    ///    actuel a expiré OU si c'est déjà nous, on prend/renouvelle le slot.
    pub async fn try_acquire(&self) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let expires = now + self.cfg.expires_after.as_millis() as i64;

        // Tentative 1 : INSERT OR IGNORE (slot inexistant → on gagne).
        let r = sqlx::query(
            "INSERT OR IGNORE INTO worker_leadership (slot, holder, expires_at) VALUES (0, ?, ?)",
        )
        .bind(&self.holder)
        .bind(expires)
        .execute(self.pool.as_ref())
        .await?;

        if r.rows_affected() == 1 {
            return Ok(true);
        }

        // Tentative 2 : CAS sur le slot existant (leader expiré OU déjà nous).
        let r = sqlx::query(
            "UPDATE worker_leadership
             SET holder = ?, expires_at = ?
             WHERE slot = 0 AND (expires_at < ? OR holder = ?)",
        )
        .bind(&self.holder)
        .bind(expires)
        .bind(now)
        .bind(&self.holder)
        .execute(self.pool.as_ref())
        .await?;

        Ok(r.rows_affected() == 1)
    }

    /// Renouvelle le bail du leader actuel.
    ///
    /// Retourne `true` si le renouvellement a réussi (on est toujours leader),
    /// `false` si le slot a été repris par un autre worker entre-temps.
    pub async fn renew(&self) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let expires = now + self.cfg.expires_after.as_millis() as i64;
        let r = sqlx::query(
            "UPDATE worker_leadership SET expires_at = ? WHERE slot = 0 AND holder = ?",
        )
        .bind(expires)
        .bind(&self.holder)
        .execute(self.pool.as_ref())
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Libère la lease leadership de ce worker.
    ///
    /// Supprime la row `worker_leadership` conditionnée par `holder = self.holder`
    /// pour éviter de libérer une lease prise par un autre worker entre-temps
    /// (race-safe).
    ///
    /// ## Effets de bord
    ///
    /// - Si la row n'existe pas (déjà expirée ou prise), l'opération est un no-op.
    /// - Si la DB est inaccessible (pool fermé, locked), l'erreur est propagée —
    ///   l'appelant doit décider si elle est critique (`.ok()` pour best-effort).
    pub async fn release(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM worker_leadership WHERE holder = ?")
            .bind(&self.holder)
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// Démarre la boucle de renouvellement en arrière-plan.
    ///
    /// La tâche se termine automatiquement si :
    /// - Le renouvellement retourne `false` (leadership perdu).
    /// - Une erreur DB survient.
    ///
    /// Le `JoinHandle` retourné permet d'`abort()` la tâche lors du shutdown.
    pub fn spawn_renewal(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.cfg.renew_every);
            // Consommer le premier tick immédiat.
            interval.tick().await;
            loop {
                interval.tick().await;
                match self.renew().await {
                    Ok(true) => {
                        tracing::debug!(holder = %self.holder, "leadership renouvelé");
                    }
                    Ok(false) => {
                        tracing::warn!(
                            holder = %self.holder,
                            "leadership perdu — arrêt de la boucle de renouvellement"
                        );
                        break;
                    }
                    Err(e) => {
                        tracing::error!(
                            holder = %self.holder,
                            error = %e,
                            "erreur lors du renouvellement du leadership"
                        );
                        break;
                    }
                }
            }
        })
    }
}
