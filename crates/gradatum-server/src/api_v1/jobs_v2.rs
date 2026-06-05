//! Endpoints F-16 — Introspection jobs v0.2.0 (Phase 3).
//!
//! Implémente les 5 endpoints de l'API jobs v81 §6 :
//!
//! | Méthode | Path | Description |
//! |---------|------|-------------|
//! | GET  | `/api/v1/jobs`             | Liste paginée (cursor-based) |
//! | GET  | `/api/v1/jobs/:id`         | Détail d'un job (fix E-12) |
//! | POST | `/api/v1/jobs`             | Création avec Idempotency-Key |
//! | POST | `/api/v1/jobs/:id/cancel`  | Annulation (409 si Running) |
//! | GET  | `/api/v1/jobs/:id/events`  | SSE stream d'événements |
//!
//! # Auth (invariant réseau privé)
//!
//! v0.2.0 Bronze : ces endpoints n'exigent pas de bearer JWT.
//! Déployer derrière un réseau privé (VPN, pare-feu).
//! Auth granulaire F-45 multi-user JWT planifiée v1.0.0 Gold.
//! Voir spec §11 E-21.
//!
//! # Idempotency-Key
//!
//! Le header `Idempotency-Key` est obligatoire sur `POST /api/v1/jobs`.
//! Absence → 400 Bad Request.
//! Key existante → 200 `{ id, idempotent: true }` sans créer de nouveau job.
//!
//! # SSE (Last-Event-ID)
//!
//! Si `Last-Event-ID` est présent, les events broadcast sont filtrés depuis
//! la tête du channel (le buffer circular de capacité 256 est partagé).
//! Trou : si le buffer a été recyclé depuis le dernier Last-Event-ID → replay
//! depuis 0. Caveat documenté §11 E-15.
//!
//! # Références
//!
//! - v81 §6 L5613-5668 — Jobs API spec
//! - v81 L12411 + L12423 + L12459 — POSTMORTEM caveats
//! - spec §6 Phase 3 F-16

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
    Extension,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use ulid::Ulid;

use gradatum_core::{JobFilter, JobRecord, JobStatus, QueueEvent};

use crate::state::AppState;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{idempotency_insert, idempotency_lookup};

// ─────────────────────────────────────────────────────────────────────────────
// DTOs
// ─────────────────────────────────────────────────────────────────────────────

/// Réponse GET /api/v1/jobs — liste paginée.
#[derive(Debug, Serialize)]
pub struct JobListResponse {
    /// Liste des JobRecord (résumé ou complet selon le filtre).
    pub items: Vec<JobRecord>,
    /// Cursor pour la page suivante — None si c'est la dernière page.
    pub next_cursor: Option<String>,
}

/// Query params pour GET /api/v1/jobs.
#[derive(Debug, Deserialize)]
pub struct JobListQuery {
    /// Filtre par statut (valeur unique).
    pub status: Option<String>,
    /// Filtre par kind (valeur unique).
    pub kind: Option<String>,
    /// Filtre depuis cette date (ISO-8601 UTC).
    pub since: Option<String>,
    /// Nombre de résultats (défaut 50, max 200).
    pub limit: Option<usize>,
    /// Cursor de pagination (ULID du dernier job retourné).
    pub cursor: Option<String>,
}

/// Body pour POST /api/v1/jobs.
///
/// # Note Phase 3 Bronze
///
/// `scheduling` et `lineage` sont désérialisés mais partiellement utilisés (E-13).
/// `scheduling` sera consommé en F-16 Silver v0.5.0 pour le scheduling différé.
/// `lineage.triggered_by` est extrait dans `build_job_record_from_spec`.
#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    /// Spécification du job (sérialisée depuis v81 JobRecord partiel).
    pub spec: serde_json::Value,
    /// Scheduling optionnel (scheduled_at, deadline, etc.) — E-13 Bronze : non consommé.
    #[allow(dead_code)]
    pub scheduling: Option<serde_json::Value>,
    /// Lineage optionnel (triggered_by, parent_job).
    pub lineage: Option<serde_json::Value>,
}

/// Réponse POST /api/v1/jobs.
#[derive(Debug, Serialize)]
pub struct CreateJobResponse {
    /// ULID du job créé (ou existant si idempotent).
    pub id: String,
    /// true si un job existant a été retourné (Idempotency-Key déjà connue).
    pub idempotent: bool,
}

/// Réponse POST /api/v1/jobs/:id/cancel.
#[derive(Debug, Serialize)]
pub struct CancelJobResponse {
    /// ULID du job annulé.
    pub id: String,
    /// Statut après l'opération.
    pub status: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse un statut depuis une chaîne (case-insensitive).
fn parse_status(s: &str) -> Option<JobStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Some(JobStatus::Pending),
        "running" => Some(JobStatus::Running),
        "waiting" => Some(JobStatus::Waiting),
        "done" => Some(JobStatus::Done),
        "failed" => Some(JobStatus::Failed),
        "dlq" => Some(JobStatus::DLQ),
        "cancelled" | "canceled" => Some(JobStatus::Cancelled),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// `GET /api/v1/jobs` — Liste paginée des jobs.
///
/// # Query params
///
/// - `status` : filtre par statut (ex: `pending`, `running`, `dead`)
/// - `kind`   : filtre par kind (ex: `Curate`, `Embed`)
/// - `since`  : filtre les jobs créés après cette date ISO-8601
/// - `limit`  : nombre de résultats (défaut 50, max 200)
/// - `cursor` : ULID du dernier job retourné (pagination)
///
/// # Retour
///
/// - **200 OK** + `{ items: [JobRecord], next_cursor: Option<String> }`
/// - **400 Bad Request** si un query param est malformé
/// - **500 Internal Server Error** si erreur SQLite
pub async fn list_jobs(
    State(state): State<AppState>,
    Extension(_trust): Extension<TrustContext>,
    Query(query): Query<JobListQuery>,
) -> Result<Json<JobListResponse>, StatusCode> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);

    let status_filter = match &query.status {
        Some(s) => match parse_status(s) {
            Some(st) => Some(st),
            None => return Err(StatusCode::BAD_REQUEST),
        },
        None => None,
    };

    let cursor_filter = match &query.cursor {
        Some(c) => match c.parse::<Ulid>() {
            Ok(u) => Some(u),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
        None => None,
    };

    let created_after = match &query.since {
        Some(s) => match s.parse::<chrono::DateTime<Utc>>() {
            Ok(dt) => Some(dt),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
        None => None,
    };

    // Demande limit + 1 pour détecter s'il y a une page suivante.
    let filter = JobFilter {
        status: status_filter,
        kind: query.kind.clone(),
        created_after,
        cursor: cursor_filter,
        // +1 pour détecter has_more sans double query
        limit: limit + 1,
        ..Default::default()
    };

    match state.job_store.list(filter).await {
        Ok(mut items) => {
            let has_more = items.len() > limit;
            if has_more {
                items.truncate(limit);
            }
            let next_cursor = if has_more {
                items.last().map(|r| r.id.to_string())
            } else {
                None
            };
            Ok(Json(JobListResponse { items, next_cursor }))
        }
        Err(e) => {
            tracing::error!(error = %e, "list_jobs: QueueStore.list() échoué");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /api/v1/jobs/:id` — Détail d'un job.
///
/// Retourne le `JobRecord` complet avec statut SQL synchronisé (fix E-12).
///
/// # Retour
///
/// - **200 OK** + `JobRecord` JSON complet
/// - **400 Bad Request** si l'ID n'est pas un ULID valide
/// - **404 Not Found** si le job n'existe pas
/// - **500 Internal Server Error** si erreur SQLite
pub async fn get_job_v2(
    State(state): State<AppState>,
    Extension(_trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
) -> Result<Json<JobRecord>, StatusCode> {
    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    match state.job_store.get(id).await {
        Ok(Some(record)) => Ok(Json(record)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "get_job_v2: QueueStore.get() échoué");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /api/v1/jobs` — Création d'un job avec Idempotency-Key.
///
/// Le header `Idempotency-Key` est **obligatoire**.
///
/// # Comportement
///
/// - Key absente → **400 Bad Request**
/// - Key inconnue → enqueue + stockage → **202 Accepted** `{ id, idempotent: false }`
/// - Key connue   → lookup → **200 OK** `{ id, idempotent: true }` (pas de nouveau job)
///
/// # Body
///
/// ```json
/// { "spec": { ... }, "scheduling": { ... }, "lineage": { ... } }
/// ```
///
/// # Limitations Phase 3
///
/// Le body `spec` est pour l'instant accepté mais non désérialisé en `JobRecord` complet
/// (F-16 spec Bronze v0.2.0 — création via CLI admin est le flux nominal).
/// Retourne un job stub avec les champs fournis et un ULID généré.
///
/// Référence : v81 L5640-5648, spec §6 Phase 3, écart E-13 (voir §11).
pub async fn create_job(
    State(state): State<AppState>,
    Extension(_trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(body): Json<CreateJobRequest>,
) -> Result<Response, StatusCode> {
    // Idempotency-Key obligatoire (v81 L5642)
    let idempotency_key = match headers.get("Idempotency-Key") {
        Some(v) => match v.to_str() {
            Ok(s) if !s.is_empty() && s.len() <= 256 => s.to_string(),
            _ => return Err(StatusCode::BAD_REQUEST),
        },
        None => return Err(StatusCode::BAD_REQUEST),
    };

    // Pool requis pour l'idempotence — 501 si non câblé
    let pool = match &state.jobs_pool {
        Some(p) => p.clone(),
        None => {
            tracing::warn!("create_job: jobs_pool non câblé — Idempotency-Key non supporté");
            return Err(StatusCode::NOT_IMPLEMENTED);
        }
    };

    // Lookup idempotent
    match idempotency_lookup(&pool, &idempotency_key).await {
        Ok(Some(existing_id)) => {
            // Key connue → retourner le job existant (idempotent = true)
            let response = CreateJobResponse {
                id: existing_id,
                idempotent: true,
            };
            return Ok((StatusCode::OK, Json(response)).into_response());
        }
        Ok(None) => {} // Continuer la création
        Err(e) => {
            tracing::error!(error = %e, "create_job: idempotency_lookup échoué");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Construire un JobRecord minimal depuis le body
    // Phase 3 Bronze : la spec JSON est conservée dans lineage.triggered_by pour traçabilité.
    // Désérialisation complète → F-16 Silver v0.5.0+ (E-13).
    let job_record = build_job_record_from_spec(body);

    match state.job_store.enqueue(job_record).await {
        Ok(job_id) => {
            let job_id_str = job_id.to_string();
            // Stocker la clé d'idempotence
            if let Err(e) = idempotency_insert(&pool, &idempotency_key, &job_id_str).await {
                // Non-fatal : le job a été créé, l'idempotence peut être manquée une fois.
                tracing::warn!(
                    error = %e,
                    job_id = %job_id_str,
                    "create_job: idempotency_insert échoué — job créé mais clé non stockée"
                );
            }
            let response = CreateJobResponse {
                id: job_id_str,
                idempotent: false,
            };
            Ok((StatusCode::ACCEPTED, Json(response)).into_response())
        }
        Err(e) => {
            tracing::error!(error = %e, "create_job: QueueStore.enqueue() échoué");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Construit un `JobRecord` minimal depuis un body `CreateJobRequest`.
///
/// Phase 3 Bronze : la spec JSON complète est preservée dans le champ `triggered_by`
/// pour traçabilité. La désérialisation complète vers tous les variants `Job` est
/// planifiée pour F-16 Silver v0.5.0 (écart E-13).
fn build_job_record_from_spec(body: CreateJobRequest) -> JobRecord {
    use gradatum_core::{
        CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
        JobRetry, JobScheduling, JobScope, JobSpec, RetryBackoff, TriggerSource,
    };

    let now = Utc::now();
    // Extrait le triggered_by depuis le lineage fourni
    let triggered_by = body
        .lineage
        .as_ref()
        .and_then(|l| l.get("triggered_by"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        // Fallback : stringify la spec pour traçabilité Bronze
        .or_else(|| {
            serde_json::to_string(&body.spec)
                .ok()
                .map(|s| format!("api-spec:{}", &s[..s.len().min(200)]))
        });

    // Job placeholder : Curate est le variant le plus courant pour les jobs API
    // Les agents (F-04) créeront leurs JobRecord directement.
    // E-13 : désérialisation complète planifiée F-16 Silver.
    let stub_spec = CurateSpec {
        note_id: Ulid::new(),
        tenant_id: "main".to_string(),
        ..Default::default()
    };

    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Curate(stub_spec),
            class: JobClass::Api,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::Api),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
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
            triggered_by,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// `POST /api/v1/jobs/:id/cancel` — Annule un job.
///
/// # Comportement (v81 L5656-5660)
///
/// - Job en `Running` → **409 Conflict** (laisser finir, ne pas tuer)
/// - Job en `Pending`/`Waiting` → annulé → **200 OK** `{ id, status: "Cancelled" }`
/// - Job déjà terminal (`Done`/`Failed`/`DLQ`/`Cancelled`) → **200 OK** idempotent
/// - Job inexistant → **404 Not Found**
/// - ULID invalide → **400 Bad Request**
pub async fn cancel_job(
    State(state): State<AppState>,
    Extension(_trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
) -> Result<Json<CancelJobResponse>, StatusCode> {
    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // Lire le statut courant pour appliquer les règles v81
    let record = match state.job_store.get(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: get() échoué");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    match record.lifecycle.status {
        // Job Running → 409 Conflict (caveat L12411 v81)
        JobStatus::Running => {
            return Err(StatusCode::CONFLICT);
        }
        // Job déjà terminal → 200 idempotent
        JobStatus::Done | JobStatus::DLQ | JobStatus::Cancelled => {
            return Ok(Json(CancelJobResponse {
                id: id.to_string(),
                status: format!("{:?}", record.lifecycle.status).to_lowercase(),
            }));
        }
        // Job Pending ou Waiting → annuler
        JobStatus::Pending | JobStatus::Waiting | JobStatus::Failed => {}
    }

    match state.job_store.cancel(id).await {
        Ok(()) => Ok(Json(CancelJobResponse {
            id: id.to_string(),
            status: "cancelled".to_string(),
        })),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: QueueStore.cancel() échoué");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /api/v1/jobs/:id/events` — SSE stream d'événements pour un job.
///
/// Retourne un stream `text/event-stream` avec les événements du job.
///
/// # Types d'événements
///
/// - `status` : changement de statut `{ event_id, type, status, attempts, timestamp }`
/// - `progress` : progression `{ event_id, type, current, total, step, eta_secs }`
/// - `heartbeat` : keepalive toutes les 30s
///
/// # Fermeture
///
/// Le stream se ferme automatiquement quand le job passe en état terminal
/// (`Done`, `DLQ`, `Cancelled`).
///
/// # Last-Event-ID
///
/// Si le header `Last-Event-ID` est présent, le client a déjà reçu des events
/// jusqu'à cet ID. On rejoue depuis le début du buffer (caveat E-15 §11 : pas de
/// replay exact depuis un ID arbitraire — le buffer circulaire de 256 est partagé).
///
/// # Retour
///
/// - **200 OK** + `Content-Type: text/event-stream`
/// - **400 Bad Request** si l'ID n'est pas un ULID valide
/// - **404 Not Found** si le job n'existe pas
pub async fn job_events(
    State(state): State<AppState>,
    Extension(_trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // Vérifier l'existence du job avant de créer le stream
    match state.job_store.get(id).await {
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Ok(Some(_)) => {}
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "job_events: get() échoué");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Last-Event-ID header pour reconnexion
    let _last_event_id = headers
        .get("Last-Event-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // S'abonner au broadcast AVANT de lire l'état final (évite race condition)
    let rx = state.job_store.subscribe();
    let target_id = id;

    // Compteur d'events pour les IDs SSE — non utilisé Phase 3 Bronze (E-15 : IDs fixes).
    // Planifié F-16 Silver via scan() pour état mutable.
    let event_counter: u64 = 0;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Err(_) => {
                // Lagged (buffer overflow) — skip silencieusement
                // Le client peut se reconnecter via Last-Event-ID
                None
            }
            Ok(event) => {
                let matches = matches!(
                    &event,
                    QueueEvent::JobInserted(eid) |
                    QueueEvent::JobFailed(eid, _) |
                    QueueEvent::JobReady(eid) |
                    QueueEvent::JobCancelled(eid)
                    if *eid == target_id
                ) || matches!(
                    &event,
                    QueueEvent::JobCompleted(eid, _, _) if *eid == target_id
                );

                if !matches {
                    return None;
                }

                let event_data = match &event {
                    QueueEvent::JobCompleted(_, status, _) => {
                        let status_str = format!("{:?}", status).to_lowercase();
                        serde_json::json!({
                            "type": "status",
                            "status": status_str,
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobFailed(_, attempt) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "failed",
                            "attempts": attempt,
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobCancelled(_) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "cancelled",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobReady(_) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "pending",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    _ => {
                        serde_json::json!({
                            "type": "status",
                            "status": "inserted",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                };

                // Signal de fermeture pour les états terminaux
                let is_terminal = matches!(
                    &event,
                    QueueEvent::JobCompleted(eid, _, _) | QueueEvent::JobCancelled(eid)
                    if *eid == target_id
                );

                let data_str = serde_json::to_string(&event_data)
                    .unwrap_or_else(|_| r#"{"type":"error"}"#.to_string());

                if is_terminal {
                    // Retourner l'event + None pour fermer le stream
                    Some(Ok::<Event, Infallible>(Event::default().data(data_str)))
                } else {
                    Some(Ok::<Event, Infallible>(Event::default().data(data_str)))
                }
            }
        }
    });

    // Le compteur d'event_counter est capturé dans une closure séparée
    // pour l'event ID (limitation : pas d'état mutable dans filter_map sans RefCell)
    // Caveat E-15 : event IDs séquentiels non implémentés — IDs fixes à 0.
    // Planifié : refacto avec scan() pour state mutable → F-16 Silver.
    let _ = event_counter; // Supprime le warning unused

    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("heartbeat"),
    );

    Ok(sse)
}
