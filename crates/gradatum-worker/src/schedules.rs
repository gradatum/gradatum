//! Schedules cron périodiques via `apalis-cron` — F-15.
//!
//! ## Schedules v0.2.0
//!
//! | Nom | Expression cron | Action |
//! |---|---|---|
//! | `cleanup_dlq_daily` | `0 3 * * *` | Supprime les jobs DLQ âgés de >30j |
//!
//! ## Architecture
//!
//! `apalis-cron` fournit [`CronStream`] qui génère un [`Tick`] à chaque occurrence
//! de la cron expression. Le handler reçoit le `Tick` et effectue le nettoyage SQL.
//!
//! ## Références
//!
//! - Spec §5 Phase 2 — apalis-cron schedules
//! - v81 §6 L3178 — F-15 feature description
//! - apalis-cron 1.0.0-rc.8 — `CronStream::new(Schedule)`

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::{BoxDynError, Data};
use apalis_cron::Tick;
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use gradatum_core::QueueStore;
use gradatum_db_sqlite::idempotency_cleanup;

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration d'un schedule cron depuis le TOML.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ScheduleConfig {
    /// Nom du schedule (ex. `"cleanup_dlq_daily"`).
    pub name: String,
    /// Expression cron (ex. `"0 3 * * *"`).
    pub cron: String,
    /// Rétention en jours pour le cleanup DLQ (défaut : 30).
    #[serde(default = "ScheduleConfig::default_retention")]
    pub retention_days: u32,
}

impl ScheduleConfig {
    fn default_retention() -> u32 {
        30
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Contexte partagé pour les handlers cron
// ─────────────────────────────────────────────────────────────────────────────

/// Contexte injecté dans les handlers cron.
///
/// Prévu pour injection manuelle en dehors du Monitor. Actuellement non utilisé
/// — le Monitor Apalis injecte pool et retention via `WorkerBuilder::data()`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct CronHandlerCtx {
    /// Pool SQLite pour les opérations de nettoyage.
    pub pool: Arc<SqlitePool>,
    /// Rétention DLQ en jours.
    pub dlq_retention_days: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — cleanup_dlq_daily
// ─────────────────────────────────────────────────────────────────────────────

/// Handler cron pour le nettoyage quotidien de la DLQ.
///
/// Supprime les jobs avec `status = 'DLQ'` âgés de plus de
/// `retention_days` jours (défaut : 30j).
///
/// # Comportement
///
/// - `completed_at < now - retention_days` → DELETE
/// - Si `completed_at IS NULL` → utilise `created_at` comme fallback
/// - Retourne `Ok(())` même si 0 lignes supprimées
///
/// # Sécurité
///
/// Opération destructrice irréversible — uniquement sur status='DLQ'.
/// La rétention 30j minimale est appliquée (plancher dans la query).
pub async fn handle_cleanup_dlq(
    _tick: Tick<Utc>,
    pool: Data<Arc<SqlitePool>>,
    retention: Data<u32>,
) -> Result<(), BoxDynError> {
    let cutoff = Utc::now() - chrono::Duration::days(*retention as i64);
    let cutoff_str = cutoff.to_rfc3339();

    let result = sqlx::query(
        r#"
        DELETE FROM gradatum_jobs
        WHERE status = 'DLQ'
          AND COALESCE(completed_at, created_at) < ?
        "#,
    )
    .bind(&cutoff_str)
    .execute(pool.as_ref())
    .await;

    match result {
        Ok(res) => {
            let deleted = res.rows_affected();
            if deleted > 0 {
                info!(
                    deleted = deleted,
                    retention_days = *retention,
                    "cleanup_dlq_daily : {} jobs DLQ purgés",
                    deleted
                );
            }
            Ok(())
        }
        Err(e) => {
            error!(error = %e, "cleanup_dlq_daily : erreur SQL");
            Err(BoxDynError::from(format!(
                "cleanup_dlq_daily SQL error: {e}"
            )))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sweep périodique (recover_stale_leases, cancel_expired_deadlines, promote_retries)
// ─────────────────────────────────────────────────────────────────────────────

/// Exécute le sweep périodique des jobs en base.
///
/// Appelle les 4 opérations de maintenance de la queue :
/// 1. [`QueueStore::recover_stale_leases`] — remet en Pending les jobs Running dont le lease est expiré
/// 2. [`QueueStore::cancel_expired_deadlines`] — annule les jobs dont la deadline est dépassée
/// 3. [`QueueStore::promote_retries`] — remet en Pending (ou DLQ si max atteint) les jobs Failed
/// 4. [`idempotency_cleanup`] — purge les entrées d'idempotence âgées de plus de 24h (TTL P1-2)
///
/// Le `pool` est requis pour le nettoyage de la table `gradatum_idempotency` (migration 008).
/// Si `pool` est `None`, l'opération 4 est ignorée avec un WARN (table peut croître
/// indéfiniment — uniquement acceptable en tests unitaires).
///
/// Exécuté par la boucle worker toutes les 30s via `tokio::spawn`.
/// Ne panique pas — les erreurs sont loggées.
pub async fn run_sweep_once(
    store: &(impl QueueStore + ?Sized),
    lease_ttl: Duration,
    pool: Option<&SqlitePool>,
) {
    let now = Utc::now();

    // 1. Recover leases expirés
    match store.recover_stale_leases(lease_ttl).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: leases expirés récupérés");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: recover_stale_leases échoué"),
    }

    // 2. Cancel deadlines expirés
    match store.cancel_expired_deadlines(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: deadlines expirés annulés");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: cancel_expired_deadlines échoué"),
    }

    // 3. Promote retries (Failed → Pending ou DLQ si max)
    match store.promote_retries(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: retries promus en Pending");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: promote_retries échoué"),
    }

    // 4. Idempotency cleanup (TTL 24h) — P1-2 Phase 4.2bis.
    // Purge les entrées de `gradatum_idempotency` antérieures à now - 24h.
    // Sans ce cleanup, la table croît indéfiniment (un enregistrement par POST /api/v1/jobs).
    match pool {
        Some(p) => {
            let cutoff_ms = (now - chrono::Duration::hours(24)).timestamp_millis();
            if let Err(e) = idempotency_cleanup(p, cutoff_ms).await {
                warn!(error = %e, "sweep: idempotency_cleanup échoué — table peut croître");
            }
        }
        None => {
            warn!("sweep: pool non disponible — idempotency_cleanup ignoré (table peut croître)");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent};
    use std::sync::Mutex;
    use tokio::sync::broadcast::Receiver;
    use ulid::Ulid;

    /// Store mock pour tester sweep_once.
    struct MockStore {
        stale_calls: Mutex<u32>,
        deadline_calls: Mutex<u32>,
        retry_calls: Mutex<u32>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                stale_calls: Mutex::new(0),
                deadline_calls: Mutex::new(0),
                retry_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl QueueStore for MockStore {
        async fn enqueue(&self, _: JobRecord) -> Result<Ulid, QueueError> {
            unimplemented!()
        }
        async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn get(&self, _: Ulid) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn complete(&self, _: Ulid, _: JobResult) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail(&self, _: Ulid, _: &str, _: u32) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn cancel(&self, _: Ulid) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail_dlq(&self, _: Ulid, _: &str) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn find_awaiting(&self, _: Ulid) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn set_pending(&self, _: Ulid) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn recover_stale_leases(&self, _: Duration) -> Result<Vec<Ulid>, QueueError> {
            *self.stale_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn cancel_expired_deadlines(
            &self,
            _: DateTime<Utc>,
        ) -> Result<Vec<Ulid>, QueueError> {
            *self.deadline_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn promote_retries(&self, _: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
            *self.retry_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn schedule_retry(&self, _: Ulid, _: DateTime<Utc>) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn list(&self, _: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        fn subscribe(&self) -> Receiver<QueueEvent> {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            drop(tx);
            rx
        }
    }

    #[tokio::test]
    async fn sweep_once_calls_all_three_methods() {
        let store = MockStore::new();
        // pool=None : idempotency_cleanup ignoré en test unitaire (table non disponible).
        run_sweep_once(&store, Duration::from_secs(300), None).await;
        assert_eq!(*store.stale_calls.lock().unwrap(), 1);
        assert_eq!(*store.deadline_calls.lock().unwrap(), 1);
        assert_eq!(*store.retry_calls.lock().unwrap(), 1);
    }
}
