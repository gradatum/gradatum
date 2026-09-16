//! Tests d'intégration F-280 — `/health` face aux livraisons de messages.
//!
//! Deux propriétés, mesurées sur le rapport rendu par [`health::probe_once`] :
//!
//! - une livraison `Job::Message` en file de rebut (DLQ) ne vieillit PAS
//!   `dlq_oldest_age_secs` : elle attend un acquittement de son destinataire, ce n'est
//!   pas du travail mort dont l'abandon serait un signal — tandis que `dlq_depth`
//!   continue de la compter (le détail par kind vit dans `dlq_by_kind`) ;
//! - le rapport détaillé PUBLIE `mailbox_pending`, le nombre de livraisons en attente,
//!   jusque-là confiné à un `tracing::debug!` interne.
//!
//! Fixture : `AppState::default()` + un VRAI `SqliteQueueStore` sur FICHIER (`TempDir`,
//! pas d'in-memory) — le contrat de `list`/`count_jobs_by_status`/`fail_dlq` observé est
//! celui du LIVE. `embed.enabled = false` : sans cela la sonde viserait le gateway LIVE
//! (`embed.endpoint` par défaut = `localhost:8436`).
//!
//! `default_curator_backend() == "heuristic"` rend la capacité curator `Unreachable`,
//! donc [`health::probe_once`] rend toujours `unhealthy` sur une config de test :
//! AUCUNE assertion sur `status` ici — le rollup est couvert par les tests unitaires de
//! `health.rs`. On asserte les CHAMPS.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gradatum_core::{
    CurateSpec, JOB_KIND_MESSAGE, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, MessageDeliverySpec,
    QueueStore, RetryBackoff, TriggerSource, job_kind_str,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::ServerConfig;
use gradatum_server::health;
use gradatum_server::state::AppState;
use tempfile::TempDir;
use ulid::Ulid;

/// Antidatage des entrées DLQ. `DLQ_MAX_AGE_SECS` vaut 24 h : à −25 h l'âge mesuré est
/// non ambigu, et une exclusion accidentellement trop large (qui renverrait `0`) se voit.
const BACKDATED_HOURS: i64 = 25;

/// Plancher d'âge attendu pour une entrée antidatée de 25 h (marge sous 25 × 3600).
const OLD_AGE_FLOOR_SECS: u64 = 89_000;

/// Délai de sonde des tests : large (les capacités externes sont hors-jeu) — le bornage
/// lui-même est couvert par `tests/health.rs`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

struct HealthEnv {
    state: AppState,
    _dir: TempDir,
}

/// Monte l'état de test : queue SQLite sur fichier + config à embed désactivé.
async fn build_health_env() -> HealthEnv {
    let dir = TempDir::new().expect("tempdir health F-280");
    let queue_path = dir.path().join("queue.db");

    let jobs_pool = QueueDb::open(&queue_path)
        .await
        .expect("queue db fichier — invariant test");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let mut state = AppState::default().with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool);
    state.server_config = Arc::new(cfg_embed_off());

    HealthEnv { state, _dir: dir }
}

/// Config minimale : embed désactivé — la sonde n'atteint jamais le gateway.
fn cfg_embed_off() -> ServerConfig {
    let mut cfg = ServerConfig::default();
    cfg.embed.enabled = false;
    cfg
}

/// Instant antidaté de [`BACKDATED_HOURS`] — `created_at` porte l'âge mesuré.
fn backdated_now() -> DateTime<Utc> {
    Utc::now() - ChronoDuration::hours(BACKDATED_HOURS)
}

fn record(kind: Job, created_at: DateTime<Utc>) -> JobRecord {
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind,
            class: JobClass::Api,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: created_at,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: Some("test".to_string()),
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Livraison de message minimale (pointeur vers la note de boîte, jamais le corps).
fn message_kind() -> Job {
    Job::Message(MessageDeliverySpec {
        message_id: Ulid::generate(),
        recipient: "identity/agent/writer-main".to_string(),
    })
}

/// Job de traitement minimal (curate) — le contre-exemple « non-Message ».
fn processing_kind() -> Job {
    Job::Curate(CurateSpec {
        tenant_id: "main".to_string(),
        ..Default::default()
    })
}

/// Enqueue un job `Pending`, rend son identifiant.
async fn enqueue_pending(state: &AppState, kind: Job, created_at: DateTime<Utc>) -> Ulid {
    state
        .job_store
        .enqueue(record(kind, created_at))
        .await
        .expect("enqueue")
}

/// Enqueue puis pousse en DLQ — l'état observé par `probe_once`.
async fn enqueue_dlq(state: &AppState, kind: Job, created_at: DateTime<Utc>) -> Ulid {
    let id = enqueue_pending(state, kind, created_at).await;
    state
        .job_store
        .fail_dlq(id, "dead: max retries reached")
        .await
        .expect("fail_dlq");
    id
}

// ── (a) exclusion du vieillissement DLQ ───────────────────────────────────────

/// Une livraison `Message` en DLQ ne vieillit pas la file de rebut, mais reste comptée
/// dans sa profondeur.
///
/// Rouge avant F-280 : `dlq_oldest_age_secs` valait ≈ 90 000 s pour cette seule ligne.
#[tokio::test]
async fn dlq_message_delivery_is_excluded_from_dlq_oldest_age() {
    let env = build_health_env().await;
    enqueue_dlq(&env.state, message_kind(), backdated_now()).await;

    let report = health::probe_once(&env.state, PROBE_TIMEOUT).await;

    assert_eq!(
        report.dlq_depth, 1,
        "la livraison reste comptée dans la profondeur DLQ (décision de population F-280)"
    );
    assert_eq!(
        report.dlq_oldest_age_secs, 0,
        "une livraison de message en DLQ n'est pas du travail mort : elle ne vieillit pas dlq_oldest_age_secs"
    );
}

// ── (b) contrôle négatif : le vieillissement DLQ n'est pas vidé ───────────────

/// Un job non-Message antidaté en DLQ continue de vieillir la file de rebut.
#[tokio::test]
async fn dlq_processing_job_still_counts_for_oldest_age() {
    let env = build_health_env().await;
    enqueue_dlq(&env.state, processing_kind(), backdated_now()).await;

    let report = health::probe_once(&env.state, PROBE_TIMEOUT).await;

    assert_eq!(report.dlq_depth, 1);
    assert!(
        report.dlq_oldest_age_secs >= OLD_AGE_FLOOR_SECS,
        "un job de traitement mort doit vieillir la DLQ (≥ {OLD_AGE_FLOOR_SECS} s attendu, obtenu {})",
        report.dlq_oldest_age_secs
    );
}

// ── (c) exposition de `mailbox_pending` ──────────────────────────────────────

/// Le rapport détaillé PUBLIE `mailbox_pending` : 3 livraisons en attente.
///
/// Assertion sur le wire sérialisé — c'est la surface d'exposition du rapport détaillé,
/// et le seul accès qui compile avant que le champ n'existe (rouge : clé absente).
#[tokio::test]
async fn detailed_health_exposes_mailbox_pending() {
    let env = build_health_env().await;
    for _ in 0..3 {
        enqueue_pending(&env.state, message_kind(), Utc::now()).await;
    }

    let report = health::probe_once(&env.state, PROBE_TIMEOUT).await;
    let wire = serde_json::to_value(&report).expect("HealthReport sérialisable");

    assert_eq!(
        wire.get("mailbox_pending")
            .and_then(serde_json::Value::as_u64),
        Some(3),
        "le rapport détaillé doit exposer mailbox_pending (3 livraisons en attente)"
    );
}

// ── (d) âge DLQ exact au-delà de la fenêtre bornée ───────────────────────────

/// L'âge de la file de rebut reste EXACT au-delà de la fenêtre de balayage bornée.
///
/// 5 001 livraisons mortes antidatées de 25 h saturent la fenêtre (200 × 25 = 5 000
/// lignes) ; le seul job de traitement mort est récent et son ULID est le plus grand,
/// donc jamais atteint par un balayage borné — qui retombe alors sur l'âge de la plus
/// ancienne livraison (≈ 25 h), exactement la population que F-280 met à part.
///
/// Rouge avant la correction P2-1 : le balayage épuisait sa fenêtre et renvoyait ≈ 90 000 s.
#[tokio::test]
async fn dlq_age_stays_exact_past_the_scan_window() {
    let env = build_health_env().await;
    for _ in 0..5_001 {
        enqueue_dlq(&env.state, message_kind(), backdated_now()).await;
    }
    enqueue_dlq(&env.state, processing_kind(), Utc::now()).await;

    let report = health::probe_once(&env.state, PROBE_TIMEOUT).await;

    assert!(
        report.dlq_oldest_age_secs < 60,
        "un job de traitement mort récent doit donner l'âge DLQ, même derrière 5 001 livraisons (obtenu {} s)",
        report.dlq_oldest_age_secs
    );
}

/// Parité de clés prouvée, jamais supposée : l'âge DLQ exact se lit sur l'agrégat
/// `dlq_by_kind`, dont les clés viennent de la colonne SQL `kind` écrite à l'enqueue par
/// `job_kind_str`. La correction compare ces clés à [`JOB_KIND_MESSAGE`] pour décider
/// quelles entrées font vieillir la file — ce test est le verrou de cette égalité.
#[tokio::test]
async fn dlq_by_kind_keys_are_the_job_kind_str_values() {
    let env = build_health_env().await;
    enqueue_dlq(&env.state, message_kind(), backdated_now()).await;
    enqueue_dlq(&env.state, processing_kind(), backdated_now()).await;

    let by_kind = env
        .state
        .job_store
        .dlq_counts_by_kind()
        .await
        .expect("GROUP BY kind sur la file de rebut");

    assert_eq!(
        by_kind.get(JOB_KIND_MESSAGE),
        Some(&1),
        "la livraison est comptée sous la clé `JOB_KIND_MESSAGE` (parité colonne `kind`) : {by_kind:?}"
    );
    assert_eq!(
        by_kind.get(job_kind_str(&processing_kind())),
        Some(&1),
        "le job de traitement est compté sous la clé `job_kind_str` : {by_kind:?}"
    );
    assert_eq!(
        by_kind.len(),
        2,
        "aucun kind hors des deux enqueués : {by_kind:?}"
    );
}

/// Le champ typé et le builder qui le peuple : `with_mailbox_pending` renseigne
/// `mailbox_pending` sans toucher au reste du bloc file.
///
/// Verrou d'API typée — il ne peut pas compiler avant que le champ n'existe, d'où son
/// ajout dans la passe d'implémentation (le rouge de (c) tient la place du rouge
/// comportemental).
#[test]
fn with_mailbox_pending_sets_only_the_mailbox_field() {
    let report = gradatum_dto::HealthReport::new(gradatum_dto::HealthStatus::Ok, "2.4.0".into())
        .with_queue(7, 42)
        .with_mailbox_pending(3);

    assert_eq!(
        report.mailbox_pending,
        Some(3),
        "champ typé peuplé par le builder — `Some(v)` : seul le builder publie une mesure"
    );
    assert_eq!(report.queue_depth, 7, "bloc file intact");
    assert_eq!(report.queue_oldest_age_secs, 42, "bloc file intact");
    assert_eq!(
        report.dlq_depth, 0,
        "le builder boîte ne touche pas au bloc DLQ"
    );
}
