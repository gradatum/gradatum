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
/// Point d'entrée du tick review-promote (C2, EX-C2-3).
///
/// - `multi_tenant_enabled = false` → [`promote_once`] inchangé (scan global legacy,
///   même requête `find_promotable`, même plan de tick, même
///   handle singleton `vault`).
/// - `true` → itération explicite PAR vault actif (`list_active_vaults`) avec
///   `find_promotable_in_vault` — aucun tick ne croise les vaults (INV-JOB-SCOPE). Le
///   write de chaque vault est routé (A1, caveat pré-flip) vers le handle du vault CIBLE
///   résolu par `vaults.resolve(&vault_id)` — plus le singleton `main` : sans ce routage,
///   `promote_batch` construisait `for_system_task(vault_id)` puis muterait le Vault `main`
///   (`ensure_witness_owns_vault` → `NoteNotFound`, note jamais promue).
///
/// # Non-fatal
/// Mêmes garanties que [`promote_once`] : erreurs par note comptées, jamais fatales ;
/// échec de listing des vaults → `errors = 1`, tick ignoré ; un vault actif absent du
/// registre (`resolve` fail-closed `VaultNotFound`) est compté en `errors` et sauté, sans
/// interrompre le tick des autres vaults.
pub async fn promote_tick(
    index: &Arc<dyn Index>,
    vault: &Arc<dyn Registry>,
    vaults: &Arc<crate::state::VaultRegistry>,
    metrics: &AppMetrics,
    server_cfg: &crate::config::ServerConfig,
    now_ms: i64,
    multi_tenant_enabled: bool,
) -> PromoteStats {
    if !multi_tenant_enabled {
        // Chemin OFF (byte-identical) : config review-promote GLOBALE, mono-vault `main`.
        return promote_once(index, vault, metrics, &server_cfg.review_promote, now_ms).await;
    }
    // Gate GLOBAL (master switch, byte-identical à l'ancien `cfg.enabled`) : quand la promotion
    // globale est OFF, aucun tick. Le per-vault (L6) ne fait que RAFFINER les params d'un vault
    // quand la promotion globale est ON ; la désactivation per-vault d'une promotion globalement
    // active est honorée dans la boucle (`cfg_eff.enabled` ⇒ `continue`), mais l'activation
    // per-vault d'une promotion globalement inactive n'est pas dans le scope L6.
    if !server_cfg.review_promote.enabled {
        return PromoteStats::default();
    }

    let active_vault_ids = match index.list_active_vaults().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "review_promote: list_active_vaults failed — tick skipped");
            return PromoteStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut stats = PromoteStats::default();
    for vault_id in active_vault_ids {
        // L6 : config review-promote EFFECTIVE de CE vault (override A6 `[per_vault]` ou global).
        // `cfg_eff.enabled = false` ⇒ ce vault a explicitement désactivé la promotion → sauté.
        let cfg_eff = server_cfg.review_promote_for(vault_id.as_str());
        if !cfg_eff.enabled {
            continue;
        }
        let cutoff_ms = now_ms - (cfg_eff.age_days as i64) * 86_400_000;
        let promotable = match index
            .find_promotable_in_vault(&vault_id, cutoff_ms, cfg_eff.max_per_tick)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    vault_id = %vault_id,
                    error = %e,
                    "review_promote: find_promotable_in_vault failed — vault skipped"
                );
                stats.errors += 1;
                continue;
            }
        };
        // A1 (caveat pré-flip) : router le write vers le handle du vault CIBLE, pas le
        // singleton `main`. Fail-closed `VaultNotFound` (vault actif non enregistré) →
        // non-fatal, vault sauté (le bootstrap N vaults garantit la cohérence à flag ON).
        let vault_handle = match vaults.resolve(&vault_id) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    vault_id = %vault_id,
                    error = %e,
                    "review_promote: vault registry resolve failed — vault skipped"
                );
                stats.errors += 1;
                continue;
            }
        };
        let batch = promote_batch(&vault_handle, &vault_id, metrics, cfg_eff, promotable).await;
        stats.staging += batch.staging;
        stats.pending_review += batch.pending_review;
        stats.errors += batch.errors;
    }
    stats
}

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
            tracing::warn!(error = %e, "review_promote: find_promotable failed — tick skipped");
            // Incrémenter errors pour que main.rs mappe en TaskOutcome::Error.
            // PromoteStats::default() aurait errors=0 → fausse impression de succès.
            return PromoteStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    // Chemin global (flag OFF, mono-vault) : le Vault injecté est lié à `main`.
    promote_batch(
        vault,
        &gradatum_core::scope::VaultId::new("main"),
        metrics,
        cfg,
        promotable,
    )
    .await
}

/// Applique la promotion d'un lot `(ulid, from_status)` — boucle commune aux chemins
/// global (OFF) et per-vault (ON). Sémantique inchangée : erreurs par note non-fatales.
async fn promote_batch(
    vault: &Arc<dyn Registry>,
    vault_id: &gradatum_core::scope::VaultId,
    metrics: &AppMetrics,
    cfg: &ReviewPromoteConfig,
    promotable: Vec<(String, NoteStatus)>,
) -> PromoteStats {
    let mut stats = PromoteStats::default();
    let reason = format!("auto-promote: review aged > {}d", cfg.age_days);

    // C4 (caveat C1 HAUTE, council 01KXTRART) : témoin système du vault promu — la boucle
    // orchestrateur itère par vault actif (INV-JOB-SCOPE), le scope est garanti hors ACL
    // par-requête. Épingle la transition couche-Vault au vault courant (parité tenant-facing).
    let checked = gradatum_core::scope::AclCheckedVaultId::for_system_task(vault_id.clone());

    for (ulid_str, from_status) in promotable {
        match vault
            .update_note_status(&checked, &ulid_str, NoteStatus::Live, Some(reason.clone()))
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
                    "review_promote: update_note_status failed (likely TOCTOU) — note skipped"
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
            "review_promote: tick complete"
        );
    }

    stats
}
