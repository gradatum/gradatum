//! `GET /api/v1/system/scheduled` — health of recurring scheduled tasks (since v0.7.5).
//!
//! Expose l'état des 8 tâches tokio::interval in-process pour le studio.
//!
//! # Contrat
//!
//! | Méthode | Path | Réponse | Codes |
//! |---|---|---|---|
//! | GET | `/api/v1/system/scheduled` | [`ScheduledResponse`] | 200 / 401 / 403 / 500 |
//!
//! Auth : même groupe que `/dashboard` (TrustContext + scope Read sur `main/dashboard`).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::trust::TrustContext;
use serde::Serialize;

use crate::scheduled_tasks::{ALL_SCHEDULED_TASKS, task_interval_secs};
use crate::state::AppState;

/// Sanitise un message d'erreur avant exposition API.
///
/// Masque les tokens pouvant révéler des informations d'infrastructure (chemins FS,
/// URLs LLM internes) par des tokens génériques, puis tronque à 120 caractères.
/// La valeur originale complète reste stockée en base de données.
///
/// Règles appliquées (sans dépendance regex) :
/// - Token commençant par `/` → `[path]` (chemin absolu FS).
/// - Token commençant par `http://` ou `https://` → `[url]` (URL interne/LLM).
/// - Résultat tronqué à 120 caractères (avec `…` si tronqué).
fn sanitize_last_error(msg: &str) -> String {
    let masked: String = msg
        .split_whitespace()
        .map(|token| {
            if token.starts_with("https://") || token.starts_with("http://") {
                "[url]"
            } else if token.starts_with('/') {
                "[path]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if masked.chars().count() > 120 {
        let truncated: String = masked.chars().take(120).collect();
        format!("{truncated}\u{2026}")
    } else {
        masked
    }
}

/// Tenant système unique (identique au dashboard).
const TENANT: &str = "main";

/// DTO d'une tâche récurrente dans la réponse JSON.
#[derive(Debug, Serialize)]
pub struct ScheduledTaskSummary {
    /// Nom canonique de la tâche (ex. `"telemetry-flush"`).
    pub name: String,
    /// Epoch ms du dernier tick. `null` si la tâche n'a jamais tourné.
    pub last_run_ms: Option<i64>,
    /// Outcome du dernier tick : `"ok"` | `"error"` | `null` (jamais tourné).
    pub last_outcome: Option<String>,
    /// Durée du dernier tick en millisecondes. `null` si jamais tourné.
    pub last_duration_ms: Option<i64>,
    /// Message d'erreur du dernier tick en erreur. `null` sinon.
    pub last_error: Option<String>,
    /// Nombre total de ticks accomplis depuis le dernier seed.
    pub run_count: i64,
    /// Nombre d'erreurs dans les dernières 24h (fenêtre glissante).
    pub errors_24h: i64,
    /// Intervalle configuré en secondes — via SSOT [`task_interval_secs`].
    pub interval_secs: u64,
}

/// Réponse de `GET /api/v1/system/scheduled`.
#[derive(Debug, Serialize)]
pub struct ScheduledResponse {
    /// Santé des 8 tâches récurrentes in-process.
    pub tasks: Vec<ScheduledTaskSummary>,
}

/// `GET /api/v1/system/scheduled`
///
/// Retourne la santé des 8 tâches récurrentes in-process du serveur gradatum.
///
/// # Errors
///
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusé sur `main/dashboard`.
/// - `500 Internal Server Error` : erreur de stockage (lecture `scheduled_task_health`).
pub async fn get_scheduled(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<ScheduledResponse>, StatusCode> {
    // ── Authentification — miroir exact de /dashboard ─────────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Lecture santé tâches ──────────────────────────────────────────────────
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let rows = state
        .search
        .list_scheduled_health(now_ms)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "get_scheduled: list_scheduled_health failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Index par nom pour O(1) lookup lors du parcours ALL_SCHEDULED_TASKS.
    let row_map: std::collections::HashMap<&str, _> =
        rows.iter().map(|r| (r.task_name.as_str(), r)).collect();

    // Construction de la réponse — ordre canonique ALL_SCHEDULED_TASKS.
    // Les tâches non encore seedées (absentes du store) apparaissent avec valeurs nulles.
    let tasks = ALL_SCHEDULED_TASKS
        .iter()
        .map(|&name| {
            let row = row_map.get(name).copied();
            ScheduledTaskSummary {
                name: name.to_string(),
                last_run_ms: row.and_then(|r| r.last_run_ms),
                last_outcome: row.and_then(|r| r.last_outcome.clone()),
                last_duration_ms: row.and_then(|r| r.last_duration_ms),
                last_error: row.and_then(|r| r.last_error.as_deref().map(sanitize_last_error)),
                run_count: row.map(|r| r.run_count).unwrap_or(0),
                errors_24h: row.map(|r| r.errors_24h).unwrap_or(0),
                interval_secs: task_interval_secs(name, &state.server_config),
            }
        })
        .collect();

    Ok(Json(ScheduledResponse { tasks }))
}

// ────────────────────────────────────────────────────────────────────────────
// Metrics catalog + timeseries (v0.7.5 Slice 2a F-85)
// ────────────────────────────────────────────────────────────────────────────

use crate::curated_metrics::{series_meta, stub_catalog_entries};

/// Entrée du catalog métrique.
///
/// Exposée par `GET /api/v1/system/metrics/catalog`.
#[derive(Debug, Serialize)]
pub struct CatalogEntry {
    /// Clé de série curée (ex. `"read_usage.search"`).
    pub key: String,
    /// Groupe : `"usage"` | `"context"` | `"server"` | `"write"`.
    pub group: String,
    /// Type : `"counter"` | `"gauge"` | `"histogram_sum"` | `"histogram_count"`.
    pub kind: String,
    /// Unité indicative (`"calls"`, `"seconds"`, `"rows"`, …).
    pub unit: String,
    /// `false` pour les familles stub non encore alimentées (curator/llm).
    pub instrumented: bool,
}

/// Réponse de `GET /api/v1/system/metrics/catalog`.
#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    /// Toutes les séries curées connues (instrumentées + stubs).
    pub series: Vec<CatalogEntry>,
}

/// Point d'une timeseries.
#[derive(Debug, Serialize)]
pub struct TimeseriesPoint {
    /// Epoch ms (MIN(ts_ms) du bucket si downsamplé).
    pub ts_ms: i64,
    /// Valeur brute cumulée, ou moyenne du bucket.
    pub value: f64,
}

/// Une série dans la réponse timeseries.
#[derive(Debug, Serialize)]
pub struct TimeseriesSeries {
    /// Clé de série curée.
    pub key: String,
    /// Points ordonnés par ts_ms croissant.
    pub points: Vec<TimeseriesPoint>,
}

/// Réponse de `GET /api/v1/system/metrics/timeseries`.
#[derive(Debug, Serialize)]
pub struct TimeseriesResponse {
    /// Epoch ms du début de la plage demandée.
    pub from_ms: i64,
    /// Epoch ms de la fin de la plage demandée.
    pub to_ms: i64,
    /// Taille effective du bucket en secondes (60 si pas de downsample).
    pub bucket_secs: i64,
    /// Séries dans l'ordre des clés demandées.
    pub series: Vec<TimeseriesSeries>,
}

/// Query params de `GET /api/v1/system/metrics/timeseries`.
#[derive(Debug, serde::Deserialize)]
pub struct TimeseriesQuery {
    /// Liste CSV de clés curées (ex. `"read_usage.search,http.requests_total"`).
    pub series: String,
    /// Epoch ms de début de plage (inclusif).
    pub from_ms: i64,
    /// Epoch ms de fin de plage (inclusif).
    pub to_ms: i64,
    /// Nombre maximal de points par série (clampé entre 1 et 2000, défaut 500).
    #[serde(default)]
    pub max_points: Option<i64>,
}

/// Plafond absolu de points par série (protection DoS — safety cap).
const MAX_POINTS_CAP: i64 = 2000;

/// Valeur par défaut de `max_points`.
const MAX_POINTS_DEFAULT: i64 = 500;

/// Nombre maximal de séries acceptées par requête (protection DoS — safety cap, ADN 5).
const MAX_SERIES: usize = 32;

/// Plage temporelle maximale acceptée (≈ 1 an).
///
/// Borne le scan SQL sur `metric_sample` et défend contre les requêtes DoS
/// à plage extrême (`MAX_SPAN_MS` = 366 j × 86 400 s × 1 000 ms/s).
const MAX_SPAN_MS: i64 = 366 * 24 * 60 * 60 * 1_000; // 31_622_400_000 ms

/// Calcule la taille de bucket (ms) pour borner le nombre de points par série.
///
/// Si `span_ms / 60_000 <= max_points` → `60_000` (pas de downsample, points bruts).
/// Sinon → plus petit multiple de 60_000 tel que `span_ms / bucket_ms <= max_points`.
///
/// # Note
///
/// Les buckets SQL sont alignés sur l'epoch absolu (`ts_ms / bucket_ms`), pas sur
/// `from_ms` — le compte réel peut donc atteindre `max_points + 1` points/série.
/// Acceptable pour un graphe de tendance (cap dur `MAX_POINTS_CAP = 2000`).
///
/// La fonction est **totale** pour tout `span_ms` i64 légal : les additions internes
/// utilisent `saturating_add` — aucune panique ni overflow, même en build debug
/// avec overflow-checks actifs (C1bis security-reviewer).
pub fn compute_bucket_ms(span_ms: i64, max_points: i64) -> i64 {
    let max_points = max_points.max(1);
    let minutes = span_ms / 60_000;
    if minutes <= max_points {
        return 60_000;
    }
    // ceil(span_ms / max_points) arrondi au multiple de 60_000 supérieur.
    // saturating_add : évite l'overflow pour span_ms proche de i64::MAX.
    let raw = span_ms.saturating_add(max_points - 1) / max_points; // ceil ms/bucket
    let mult = raw.saturating_add(59_999) / 60_000; // ceil en minutes
    mult.max(1) * 60_000
}

/// `GET /api/v1/system/metrics/catalog`
///
/// Retourne l'univers des séries curées avec leurs métadonnées.
/// Inclut toujours les familles stub (curator/llm) même si jamais échantillonnées.
///
/// # Errors
///
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusé sur `main/dashboard`.
/// - `500 Internal Server Error` : erreur de stockage.
pub async fn get_metrics_catalog(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<CatalogResponse>, StatusCode> {
    // ── Authentification — miroir exact de get_scheduled ─────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Séries distinctes présentes en base ───────────────────────────────────
    let distinct = state
        .search
        .list_distinct_metric_series()
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "get_metrics_catalog: list_distinct_metric_series failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Convertit les clés connues en CatalogEntry (filtre les séries hors allowlist).
    let mut series: Vec<CatalogEntry> = distinct
        .into_iter()
        .filter_map(|key| {
            series_meta(&key).map(|m| CatalogEntry {
                key: m.key,
                group: m.group.to_string(),
                kind: m.kind.to_string(),
                unit: m.unit.to_string(),
                instrumented: m.instrumented,
            })
        })
        .collect();

    // Toujours inclure les familles stub (curator/llm) même si jamais échantillonnées.
    for stub in stub_catalog_entries() {
        if !series.iter().any(|e| e.key == stub.key) {
            series.push(CatalogEntry {
                key: stub.key,
                group: stub.group.to_string(),
                kind: stub.kind.to_string(),
                unit: stub.unit.to_string(),
                instrumented: stub.instrumented,
            });
        }
    }
    series.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(Json(CatalogResponse { series }))
}

/// `GET /api/v1/system/metrics/timeseries`
///
/// Retourne les points downsamplés par série sur une plage de temps.
/// Les séries demandées doivent être dans l'allowlist curée.
///
/// # Errors
///
/// - `400 Bad Request` : plage invalide (`from_ms >= to_ms`), overflow de span
///   (ex. `i64::MIN..i64::MAX`), plage supérieure à `MAX_SPAN_MS` (~1 an), série inconnue,
///   ou nombre de séries (après déduplication) supérieur à `MAX_SERIES` (32).
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusé sur `main/dashboard`.
/// - `500 Internal Server Error` : erreur de stockage.
pub async fn get_metrics_timeseries(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Query(q): axum::extract::Query<TimeseriesQuery>,
) -> Result<Json<TimeseriesResponse>, StatusCode> {
    // ── Authentification — miroir exact de get_scheduled ─────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Validation de la plage ────────────────────────────────────────────────
    if q.from_ms >= q.to_ms {
        return Err(StatusCode::BAD_REQUEST);
    }
    // checked_sub : protège l'overflow silencieux en release (ex. MIN..MAX → wrap → -1).
    // En debug, l'arithmétique brute aurait paniqué — même protection, tout mode.
    let span_ms = q
        .to_ms
        .checked_sub(q.from_ms)
        .ok_or(StatusCode::BAD_REQUEST)?;
    // Borne le scan SQL et défend contre les requêtes DoS à plage extrême (ADN 5).
    if span_ms > MAX_SPAN_MS {
        return Err(StatusCode::BAD_REQUEST);
    }

    // ── Parse + validation des clés contre l'allowlist ───────────────────────
    // Déduplication en préservant l'ordre : ferme l'asymétrie DoS cardinalité CSV
    // (clés répétées produisaient des entrées vides en doublon) — ADN 5.
    let mut seen = std::collections::HashSet::new();
    let keys: Vec<String> = q
        .series
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect();
    if keys.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Borne la cardinalité après déduplication (safety cap DoS — ADN 5).
    if keys.len() > MAX_SERIES {
        return Err(StatusCode::BAD_REQUEST);
    }
    for k in &keys {
        if series_meta(k).is_none() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let max_points = q
        .max_points
        .unwrap_or(MAX_POINTS_DEFAULT)
        .clamp(1, MAX_POINTS_CAP);
    let bucket_ms = compute_bucket_ms(span_ms, max_points);

    // ── Requête timeseries ────────────────────────────────────────────────────
    let rows = state
        .search
        .query_metric_timeseries(&keys, q.from_ms, q.to_ms, bucket_ms)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "get_metrics_timeseries: query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Regroupe les rows plates en séries (préserve l'ordre des clés demandées).
    let mut by_key: std::collections::HashMap<String, Vec<TimeseriesPoint>> =
        std::collections::HashMap::new();
    for r in rows {
        by_key.entry(r.series).or_default().push(TimeseriesPoint {
            ts_ms: r.ts_ms,
            value: r.value,
        });
    }
    let series = keys
        .into_iter()
        .map(|key| {
            let points = by_key.remove(&key).unwrap_or_default();
            TimeseriesSeries { key, points }
        })
        .collect();

    Ok(Json(TimeseriesResponse {
        from_ms: q.from_ms,
        to_ms: q.to_ms,
        bucket_secs: bucket_ms / 1000,
        series,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Session traces (v0.7.5 Slice 3 F-85)
// ────────────────────────────────────────────────────────────────────────────

use crate::session_trace_store::{TraceCursor, TraceQuery};

/// Nombre de traces retournées par défaut.
const TRACES_LIMIT_DEFAULT: u32 = 50;

/// Plafond absolu de traces par requête (safety cap — protection DoS, ADN 5).
const TRACES_LIMIT_MAX: u32 = 200;

/// Query params de `GET /api/v1/system/traces`. Tous optionnels.
///
/// `tenant_id` n'est PAS dans les params — il provient du JWT via [`TrustContext`].
#[derive(Debug, serde::Deserialize)]
pub struct TracesQuery {
    /// Filtre optionnel sur `action_type`.
    pub action_type: Option<String>,
    /// Filtre optionnel sur `agent_id`.
    pub agent_id: Option<String>,
    /// Filtre optionnel sur `session_id`.
    pub session_id: Option<String>,
    /// Borne inférieure inclusive sur `ts_ms`.
    pub from_ms: Option<i64>,
    /// Borne supérieure inclusive sur `ts_ms`.
    pub to_ms: Option<i64>,
    /// Curseur opaque `<created_at>_<id>` pour la page suivante.
    pub cursor: Option<String>,
    /// Nombre maximal de traces (défaut 50, max 200).
    pub limit: Option<u32>,
}

/// DTO d'une trace dans la réponse JSON.
#[derive(Debug, Serialize)]
pub struct TraceRowDto {
    /// Rowid SQLite.
    pub id: i64,
    /// Session ULID.
    pub session_id: String,
    /// Identité de l'agent émetteur.
    pub agent_id: String,
    /// Horodatage epoch ms fourni par le client.
    pub ts_ms: i64,
    /// Type d'action.
    pub action_type: String,
    /// Cible de l'action.
    pub target: Option<String>,
    /// Intention courte.
    pub intent: Option<String>,
    /// Résultat.
    pub outcome: Option<String>,
    /// Référence (sha7 | ULID | section/ULID).
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
    /// Horodatage d'insertion serveur epoch ms.
    pub created_at: i64,
}

/// Réponse de `GET /api/v1/system/traces`.
#[derive(Debug, Serialize)]
pub struct TracesResponse {
    /// Traces de la page courante.
    pub traces: Vec<TraceRowDto>,
    /// Curseur opaque pour la page suivante, ou `null` si dernière page.
    pub next_cursor: Option<String>,
}

/// Parse un curseur opaque `"<created_at>_<id>"` en `TraceCursor`.
///
/// Retourne `None` si le format est invalide (le handler renvoie 400).
fn parse_trace_cursor(s: &str) -> Option<TraceCursor> {
    let (created, id) = s.split_once('_')?;
    Some(TraceCursor {
        created_at: created.parse().ok()?,
        id: id.parse().ok()?,
    })
}

/// `GET /api/v1/system/traces` — paginated read of `session_trace` records.
///
/// # Errors
///
/// - `400 Bad Request` : `from_ms > to_ms` OU curseur malformé.
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusé sur `main/dashboard`.
/// - `500 Internal Server Error` : erreur SQLite OU store non câblé.
pub async fn get_traces(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Query(q): axum::extract::Query<TracesQuery>,
) -> Result<Json<TracesResponse>, StatusCode> {
    // ── Authentification — miroir exact de get_scheduled ─────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Validation de la plage temporelle ────────────────────────────────────
    if let (Some(f), Some(t)) = (q.from_ms, q.to_ms)
        && f > t
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    // ── Validation + parse du curseur ─────────────────────────────────────────
    let cursor = match q.cursor.as_deref() {
        None => None,
        Some(s) => Some(parse_trace_cursor(s).ok_or(StatusCode::BAD_REQUEST)?),
    };

    // ── Limit cappé [1, TRACES_LIMIT_MAX] (P2-2 plan-review) ─────────────────
    let limit = q
        .limit
        .unwrap_or(TRACES_LIMIT_DEFAULT)
        .clamp(1, TRACES_LIMIT_MAX);

    // ── Accès au store session_trace ──────────────────────────────────────────
    let store = state
        .session_trace
        .as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // ── Tenant depuis le JWT (jamais du body/query) ───────────────────────────
    // SINGLE-TENANT: fallback acceptable, voir roadmap multi-tenant.
    let tenant_id = match trust.tenant_id() {
        Some(t) => t.to_owned(),
        None => {
            // JWT authentifié sans claim tenant_id — ne devrait pas arriver en prod
            // (le studio émet toujours tenant="main"). Logué pour détection anomalie.
            tracing::warn!(
                sub = ?trust.subject(),
                "get_traces: JWT authentifié sans tenant_id — fallback sur TENANT"
            );
            TENANT.to_owned()
        }
    };

    let tq = TraceQuery {
        tenant_id,
        action_type: q.action_type,
        agent_id: q.agent_id,
        session_id: q.session_id,
        from_ms: q.from_ms,
        to_ms: q.to_ms,
        cursor,
        limit,
    };

    let mut rows = store.query_traces(&tq).await.map_err(|e| {
        tracing::error!(err = %e, "get_traces: query_traces failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // ── Pagination : rows contient jusqu'à limit+1 ────────────────────────────
    let next_cursor = if rows.len() as u32 > limit {
        rows.truncate(limit as usize);
        rows.last().map(|r| format!("{}_{}", r.created_at, r.id))
    } else {
        None
    };

    let traces = rows
        .into_iter()
        .map(|r| TraceRowDto {
            id: r.id,
            session_id: r.session_id,
            agent_id: r.agent_id,
            ts_ms: r.ts_ms,
            action_type: r.action_type,
            target: r.target,
            intent: r.intent,
            outcome: r.outcome,
            ref_: r.ref_,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(TracesResponse {
        traces,
        next_cursor,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Notes by status (F-85 bug drill-down "Deprecated vide")
// ────────────────────────────────────────────────────────────────────────────

/// Nombre de notes retournées par défaut pour `GET /api/v1/notes/by-status`.
const NOTES_BY_STATUS_LIMIT_DEFAULT: u32 = 50;

/// Plafond absolu de notes par requête (safety cap — protection DoS, ADN 5).
const NOTES_BY_STATUS_LIMIT_MAX: u32 = 200;

/// Statuts reconnus pour le listing par statut (allowlist anti-injection).
///
/// `downgraded` est intentionnellement inclus : c'est l'objet de cet endpoint
/// (les notes downgraded sont exclues de `vault_search`/FTS5, mais listables ici).
const KNOWN_NOTE_STATUSES: &[&str] = &[
    "live",
    "staging",
    "pending-review",
    "draft",
    "deprecated",
    "downgraded",
    "garbage",
];

/// Query params de `GET /api/v1/notes/by-status`. `status` est obligatoire.
///
/// `tenant_id` n'est PAS dans les params — il provient du JWT via [`TrustContext`].
#[derive(Debug, serde::Deserialize)]
pub struct NotesByStatusQuery {
    /// CSV de statuts (ex. `"deprecated,downgraded"`). Obligatoire, non vide.
    pub status: String,
    /// Filtre optionnel sur la section.
    pub section: Option<String>,
    /// Curseur ULID exclusif (dernier `ulid` reçu de la page précédente).
    pub cursor: Option<String>,
    /// Nombre maximal de notes (défaut 50, max 200).
    pub limit: Option<u32>,
}

/// DTO d'une note dans la réponse `GET /api/v1/notes/by-status`.
#[derive(Debug, Serialize)]
pub struct NoteByStatusDto {
    /// ULID de la note.
    pub ulid: String,
    /// Section thématique (e.g. `"decisions"`, `"architecture"`).
    pub section: String,
    /// Titre H1 extrait (peut être absent).
    pub title: Option<String>,
    /// Statut courant (e.g. `"downgraded"`, `"deprecated"`).
    pub status: String,
    /// Extrait du corps tronqué à ~160 caractères (UTF-8 safe).
    pub snippet: String,
    /// Horodatage de dernière modification ISO-8601 UTC.
    pub modified_at: String,
}

/// Réponse de `GET /api/v1/notes/by-status`.
#[derive(Debug, Serialize)]
pub struct NotesByStatusResponse {
    /// Notes de la page courante.
    pub entries: Vec<NoteByStatusDto>,
    /// Dernier ULID de la page (curseur pour la page suivante), ou `null` si dernière page.
    pub next_cursor: Option<String>,
    /// Nombre total de notes correspondant aux filtres (toutes pages confondues).
    pub total: u64,
}

/// Parse et valide le CSV de statuts contre l'allowlist [`KNOWN_NOTE_STATUSES`].
///
/// Retourne `None` si la liste est vide (après trim) ou si un statut est hors
/// allowlist (rejet global — le handler renvoie 400).
/// Déduplique les statuts identiques pour éviter les doublons dans la requête SQL.
fn parse_status_csv(raw: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let s = part.trim();
        if s.is_empty() {
            continue;
        }
        if !KNOWN_NOTE_STATUSES.contains(&s) {
            // Statut inconnu → rejet global (anti-injection + erreur client claire).
            return None;
        }
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// `GET /api/v1/notes/by-status` — listing paginé des notes par statut (métadonnées).
///
/// Inclut les notes `downgraded` (exclues de `vault_search`/FTS5) —
/// fix du drill-down "Deprecated" du dashboard studio qui affichait une liste vide.
///
/// Pagination keyset ULID ASC. `limit` cappé à `[1, 200]`. Tenant depuis le JWT.
///
/// # Errors
///
/// - `400` : `status` vide ou contient un statut hors allowlist.
/// - `401` : non authentifié.
/// - `403` : ACL Read refusé sur `main/dashboard`.
/// - `500` : erreur SQLite.
pub async fn get_notes_by_status(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Query(q): axum::extract::Query<NotesByStatusQuery>,
) -> Result<Json<NotesByStatusResponse>, StatusCode> {
    // ── Authentification ──────────────────────────────────────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // ── ACL Read sur main/dashboard (miroir exact de get_traces) ─────────────
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Validation + parse du CSV de statuts ──────────────────────────────────
    let statuses = parse_status_csv(&q.status).ok_or(StatusCode::BAD_REQUEST)?;

    // ── Limit cappé [1, NOTES_BY_STATUS_LIMIT_MAX] ───────────────────────────
    let limit = q
        .limit
        .unwrap_or(NOTES_BY_STATUS_LIMIT_DEFAULT)
        .clamp(1, NOTES_BY_STATUS_LIMIT_MAX) as usize;

    // ── Tenant depuis le JWT (jamais du body/query) ───────────────────────────
    // SINGLE-TENANT: fallback acceptable, voir roadmap multi-tenant.
    let tenant_id = match trust.tenant_id() {
        Some(t) => t.to_owned(),
        None => {
            tracing::warn!(
                sub = ?trust.subject(),
                "get_notes_by_status: JWT authentifié sans tenant_id — fallback sur TENANT"
            );
            TENANT.to_owned()
        }
    };

    let status_refs: Vec<&str> = statuses.iter().map(String::as_str).collect();

    let (records, total) = state
        .search
        .list_notes_by_status(
            &tenant_id,
            &status_refs,
            q.section.as_deref(),
            limit,
            q.cursor.as_deref(),
        )
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "get_notes_by_status: list_notes_by_status failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Guard identité (parité stricte `vault_list_impl`) : un appelant non privilégié ne
    // doit pas exfiltrer le titre (`identity/<agent>`) ni le snippet (corps) des âmes
    // d'agents via ce listing. Le param `section` est attaquant-contrôlé et l'allowlist
    // de statuts couvre tout → sans ce filtre, `?status=live&section=identity` fuiterait
    // les âmes cross-agent. Exclusion sur la section RÉELLE de chaque record (résolue
    // server-side, jamais depuis l'input). No-op pour Studio / main-agent / owner.
    let identity_privileged = crate::api_v1::logic::is_identity_privileged(&trust);

    // Curseur de la page suivante : calculé sur le nombre BRUT de records (avant filtre),
    // pour que la pagination avance même si une page entière est masquée (idem `vault_list_impl`).
    let next_cursor = if records.len() == limit {
        records.last().map(|r| r.id.clone())
    } else {
        None
    };

    let entries = records
        .into_iter()
        .filter(|r| !crate::api_v1::logic::identity_section_hidden(identity_privileged, &r.section))
        .map(|r| {
            // Horodatage ISO-8601 UTC depuis epoch ms (updated si dispo, sinon created).
            let modified_at = {
                let ms = r.updated.unwrap_or(r.created);
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_default()
            };
            // Snippet : tronque body_text à ~160 chars sur frontière char (UTF-8 safe).
            let snippet: String = r.body_text.chars().take(160).collect();
            NoteByStatusDto {
                ulid: r.id,
                section: r.section,
                title: r.title,
                status: r.status,
                snippet,
                modified_at,
            }
        })
        .collect();

    Ok(Json(NotesByStatusResponse {
        entries,
        next_cursor,
        total,
    }))
}
