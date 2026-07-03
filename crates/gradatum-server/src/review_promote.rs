//! Job périodique d'auto-promotion des notes en review backlog.
//!
//! Promeut en `Live` les notes restées en `staging` ou `pending-review` au-delà
//! de N jours. Miroir de la tâche de rétention event_log (tokio interval, non-fatal).

use std::sync::Arc;

use gradatum_core::index::Index;
use gradatum_core::status::NoteStatus;
use gradatum_vault::Registry;

use crate::config::ReviewPromoteConfig;
use crate::metrics::{AppMetrics, FromStatusLabel};

/// Statistiques d'un tick d'auto-promotion.
#[derive(Debug, Default, PartialEq)]
pub struct PromoteStats {
    /// Notes promues depuis `staging`.
    pub staging: usize,
    /// Notes promues depuis `pending_review`.
    pub pending_review: usize,
    /// Échecs non-fatals (ex : NoteNotFound TOCTOU).
    pub errors: usize,
}

/// Exécute un tick d'auto-promotion.
///
/// Promeut en `Live` les notes dans `staging` ou `pending-review` dont l'âge
/// dépasse `cfg.age_days`. Retourne des statistiques pour les tests et le log.
///
/// # Non-fatal
/// Les erreurs par note (`NoteNotFound` TOCTOU) sont loggées en WARN et comptées
/// dans `metrics.review_promote_errors` — jamais fatales. Le batch continue.
///
/// # Errors
/// Si `find_promotable` échoue (panne DB), retourne `PromoteStats { errors: 1, .. }`
/// (erreur loggée en WARN, tick ignoré). `stats.errors > 0` permet à `main.rs` de
/// mapper en `TaskOutcome::Error` et de le reporter dans `scheduled_task_health`.
pub async fn promote_once(
    index: &Arc<dyn Index>,
    vault: &Arc<dyn Registry>,
    metrics: &AppMetrics,
    cfg: &ReviewPromoteConfig,
    now_ms: i64,
) -> PromoteStats {
    if !cfg.enabled {
        return PromoteStats::default();
    }

    let cutoff_ms = now_ms - (cfg.age_days as i64) * 86_400_000;
    let promotable = match index.find_promotable(cutoff_ms, cfg.max_per_tick).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "review_promote: find_promotable échoué — tick ignoré");
            // Incrémenter errors pour que main.rs mappe en TaskOutcome::Error.
            // PromoteStats::default() aurait errors=0 → fausse impression de succès.
            return PromoteStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut stats = PromoteStats::default();
    let reason = format!("auto-promote: review aged > {}d", cfg.age_days);

    for (ulid_str, from_status) in promotable {
        match vault
            .update_note_status(&ulid_str, NoteStatus::Live, Some(reason.clone()))
            .await
        {
            Ok(()) => {
                let label_str: &'static str = match from_status {
                    NoteStatus::Staging => "staging",
                    NoteStatus::PendingReview => "pending-review",
                    _ => "unknown",
                };
                metrics
                    .review_promoted
                    .get_or_create(&FromStatusLabel {
                        from_status: label_str,
                    })
                    .inc();
                match from_status {
                    NoteStatus::Staging => stats.staging += 1,
                    NoteStatus::PendingReview => stats.pending_review += 1,
                    _ => {}
                }
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid_str,
                    error = %e,
                    "review_promote: update_note_status échoué (TOCTOU probable) — note ignorée"
                );
                metrics.review_promote_errors.inc();
                stats.errors += 1;
            }
        }
    }

    if stats.staging + stats.pending_review > 0 {
        tracing::info!(
            staging = stats.staging,
            pending_review = stats.pending_review,
            errors = stats.errors,
            age_days = cfg.age_days,
            "review_promote: tick terminé"
        );
    }

    stats
}
