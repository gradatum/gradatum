//! SSOT des 8 tâches récurrentes in-process du serveur gradatum.
//!
//! Ce module est la **source unique de vérité** (SSOT) pour :
//! - les noms des tâches (`TASK_*` constantes)
//! - leurs intervalles réels (`task_interval_secs`) tels que câblés dans `main.rs`
//!
//! L'endpoint `GET /api/v1/system/scheduled` ET les boucles `tokio::spawn` de `main.rs`
//! appellent `task_interval_secs` — garantie de zéro divergence entre intervalles réels
//! et intervalles rapportés (badges « en retard » fiables).
//!
//! ## Ajout d'une nouvelle tâche
//!
//! 1. Déclarer une constante `TASK_*`.
//! 2. L'ajouter à `ALL_SCHEDULED_TASKS`.
//! 3. Ajouter le case dans `task_interval_secs`.
//! 4. Seeder au boot dans `main.rs` + envelopper le tick.

use crate::config::ServerConfig;

/// Nom canonique de la tâche de flush télémétrie usage (60s hardcodé).
pub const TASK_TELEMETRY_FLUSH: &str = "telemetry-flush";

/// Nom canonique de la tâche de rétention du journal d'événements.
pub const TASK_PURGE_EVENT_LOG: &str = "purge-event-log";

/// Nom canonique de la tâche de rétention des traces de session.
pub const TASK_PURGE_SESSION_TRACE: &str = "purge-session-trace";

/// Nom canonique de la tâche de rétention des compteurs read_usage.
/// Réutilise `cfg.session_trace.purge_interval_secs` (même TTL 90j).
pub const TASK_PURGE_READ_USAGE: &str = "purge-read-usage";

/// Nom canonique de la tâche d'auto-promotion review → live.
pub const TASK_REVIEW_PROMOTE: &str = "review-promote";

/// Nom canonique de la tâche de rafraîchissement proactif (F-46).
pub const TASK_PROACTIVE_REFRESH: &str = "proactive-refresh";

/// Nom canonique de la tâche de rétention du store proactive recall (F-46).
/// Réutilise `cfg.session_trace.purge_interval_secs`.
pub const TASK_ACTIVE_RECALL_PURGE: &str = "active-recall-purge";

/// Canonical name of the curated metrics timeseries sampling task (since v0.7.5).
pub const TASK_METRIC_SAMPLE: &str = "metric-sample";

/// Liste ordonnée des 8 tâches récurrentes in-process.
///
/// Utilisée pour le seed au boot et par l'endpoint `/api/v1/system/scheduled`.
pub const ALL_SCHEDULED_TASKS: [&str; 8] = [
    TASK_TELEMETRY_FLUSH,
    TASK_PURGE_EVENT_LOG,
    TASK_PURGE_SESSION_TRACE,
    TASK_PURGE_READ_USAGE,
    TASK_REVIEW_PROMOTE,
    TASK_PROACTIVE_REFRESH,
    TASK_ACTIVE_RECALL_PURGE,
    TASK_METRIC_SAMPLE,
];

/// Retourne l'intervalle réel (en secondes) de la tâche `name`, tel que câblé dans `main.rs`.
///
/// # SSOT
///
/// Cette fonction est la source unique de vérité pour les intervalles des tâches.
/// Elle DOIT rester strictement synchronisée avec le câblage `interval(Duration::from_secs(...))`
/// de chaque `tokio::spawn` dans `main.rs`. Tout désalignement = badges « en retard » incorrects.
///
/// # Tableau des intervalles
///
/// | Tâche | Intervalle |
/// |---|---|
/// | telemetry-flush | 60s hardcodé |
/// | purge-event-log | `event_log.purge_interval_secs.max(60)` |
/// | purge-session-trace | `session_trace.purge_interval_secs.max(60)` |
/// | purge-read-usage | `session_trace.purge_interval_secs.max(60)` (même TTL) |
/// | review-promote | `review_promote.interval_secs.max(60)` |
/// | proactive-refresh | `proactive_recall.refresh_interval_secs.max(60)` |
/// | active-recall-purge | `session_trace.purge_interval_secs.max(60)` (même TTL) |
/// | metric-sample | 60s hardcodé |
///
/// # Fallback
///
/// Un nom inconnu retourne 60 (plancher minimal).
pub fn task_interval_secs(name: &str, cfg: &ServerConfig) -> u64 {
    match name {
        TASK_TELEMETRY_FLUSH => 60,
        TASK_PURGE_EVENT_LOG => cfg.event_log.purge_interval_secs.max(60),
        TASK_PURGE_SESSION_TRACE => cfg.session_trace.purge_interval_secs.max(60),
        TASK_PURGE_READ_USAGE => cfg.session_trace.purge_interval_secs.max(60),
        TASK_REVIEW_PROMOTE => cfg.review_promote.interval_secs.max(60),
        TASK_PROACTIVE_REFRESH => cfg.proactive_recall.refresh_interval_secs.max(60),
        TASK_ACTIVE_RECALL_PURGE => cfg.session_trace.purge_interval_secs.max(60),
        TASK_METRIC_SAMPLE => 60,
        _ => 60,
    }
}
