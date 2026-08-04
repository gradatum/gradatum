//! F-51 — passe d'audit / déduplication rétrospective du vault (Option A : rapport pur).
//!
//! Miroir structurel de [`crate::review_promote`] (tokio interval, non-fatal). À chaque tick,
//! la passe scanne le vault (hors sections `PROTECTED_DELETE`, exclues côté SQL), détecte les
//! candidats déchet / doublon via [`gradatum_curator::audit`], et écrit un **rapport** sous la
//! racine de stockage. Elle **ne mute jamais** le vault (invariant fondateur F-100 :
//! `decisions/01KXAP7Z61` + `01KXANRX89`).
//!
//! Hôte retenu : **tâche tokio interval côté serveur** (et non worker apalis-cron). Rationale :
//! Option A est purement rapport, le corpus `main` est petit (~2k notes), le fenêtrage borne
//! l'O(n²), et le serveur est le propriétaire unique du filesystem du vault (écriture de
//! l'artefact sans franchir la frontière `/internal`). Le worker resterait justifié si une
//! variante future auto-mutait (elle ne le fait pas ici).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::index::Index;
use gradatum_curator::audit::{
    self, AuditRecord, AuditThresholds, DowngradeAction, IrrelevanceInput,
};
use gradatum_index::extract_h1_title;

use crate::config::{AuditConfig, DowngradeConfig};
use crate::metrics::{AppMetrics, AuditCategoryLabel};
use crate::note_usage_store::NoteUsageStore;

/// Statistiques d'un tick d'audit (tests + log + télémétrie).
#[derive(Debug, Default, PartialEq)]
pub struct AuditRunStats {
    /// Nombre de notes scannées (hors sections protégées).
    pub scanned: usize,
    /// Candidats tier `Delete`.
    pub delete_tier: usize,
    /// Candidats tier `Review`.
    pub review_tier: usize,
    /// Notes downgradées par l'exécuteur F-111 (0 en dry-run).
    pub downgraded: usize,
    /// Échecs non-fatals (scan ou écriture rapport).
    pub errors: usize,
}

/// Abstraction de la mutation `downgrade` — découple la politique F-111 (audit_once)
/// du mécanisme concret (index SQLite). Testable via un faux enregistreur.
#[async_trait]
pub trait NoteDowngrader: Send + Sync {
    /// Déclasse une note (statut `downgraded`, réversible). `reason` est la raison
    /// lisible F-111 ; l'implémentation la préfixe pour tracer l'origine automatique.
    ///
    /// # Errors
    ///
    /// Propager toute erreur de mutation (note absente, storage) — l'appelant continue
    /// la passe (best-effort par item).
    async fn downgrade(
        &self,
        tenant: &str,
        note_id: &str,
        reason: &str,
    ) -> Result<(), GradatumError>;
}

/// Implémentation de production : mute directement l'index (même appel bas-niveau que
/// `vault_downgrade_impl` post-guards — `Index: DocumentStore` expose `downgrade_note`).
///
/// Le contexte système single-tenant n'a pas de garde HTTP (JWT/ACL) à reproduire ;
/// les sections `identity`/`agent-issues` sont déjà exclues par `PROTECTED_DOWNGRADE`.
pub struct IndexDowngrader(pub Arc<dyn Index>);

#[async_trait]
impl NoteDowngrader for IndexDowngrader {
    async fn downgrade(
        &self,
        tenant: &str,
        note_id: &str,
        reason: &str,
    ) -> Result<(), GradatumError> {
        let id = ulid::Ulid::from_string(note_id).map(NoteId).map_err(|_| {
            GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                "invalid note_id (ULID expected)".into(),
            ))
        })?;
        let status_reason = format!("auto-downgrade F-111: {reason}");
        // C3a (EX-C3a P0) : le job d'audit itère par vault actif — `tenant` est le vault
        // scanné (`audit_scan(vault_id)`), garanti par l'orchestrateur (contexte système,
        // hors requête HTTP). Le filtre `AND vault_id = tenant` est byte-identical (les
        // candidats proviennent déjà de ce vault) et durcit la mutation par ULID.
        let checked = gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new(tenant),
        );
        // Pas de `replaced_by` (pas de note survivante — c'est un retrait de pertinence).
        self.0
            .downgrade_note(&checked, &id, &status_reason, None)
            .await
    }
}

/// Exécute un tick d'audit sur un vault. Non-fatal : toute erreur est loggée + comptée.
///
/// Écrit trois artefacts sous `{storage_root}/audit/` (horodatés + alias `-latest`) :
/// rapport JSON, rapport Markdown, et un script de commandes `gradatum-admin` préparées
/// (à exécuter par l'opérateur — jamais par un agent).
///
/// # Errors
///
/// N'échoue jamais (retourne des stats) : un échec de scan ou d'écriture est reporté via
/// `stats.errors` et la métrique `gradatum_audit_errors_total`, pour mapping `TaskOutcome::Error`.
#[allow(clippy::too_many_arguments)]
pub async fn audit_once(
    index: &Arc<dyn Index>,
    metrics: &AppMetrics,
    cfg: &AuditConfig,
    downgrade_cfg: &DowngradeConfig,
    usage: Option<&NoteUsageStore>,
    downgrader: Option<&dyn NoteDowngrader>,
    storage_root: &Path,
    vault_id: &str,
    now_ms: i64,
) -> AuditRunStats {
    if !cfg.enabled {
        return AuditRunStats::default();
    }

    let rows = match index.audit_scan(vault_id, cfg.max_scan).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, vault = %vault_id, "audit: scan failed — tick skipped");
            metrics.audit_errors.inc();
            return AuditRunStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    // F-111 : usage map + T0 collecte (garde fenêtre). Lus une fois par passe.
    let (t0_ms, last_used) = match usage {
        Some(store) => {
            // Frontière typée (Task 10) : `note_usage` est scopé per-NAMESPACE ; le job
            // d'audit itère par vault actif → `vault_id` (byte-identical, même chaîne).
            let vault = gradatum_core::scope::VaultId::new(vault_id);
            (
                store.min_last_used(&vault).await.unwrap_or(None),
                store.last_used_map(&vault).await.unwrap_or_default(),
            )
        }
        None => (None, Default::default()),
    };

    // Map rows→records en conservant les métadonnées F-111 (created/trust/status) AVANT
    // que `r` ne soit consommé dans `AuditRecord` (P2 : pas de find linéaire ultérieur).
    let mut records: Vec<AuditRecord> = Vec::with_capacity(rows.len());
    let mut irr_inputs: Vec<IrrelevanceInput> = Vec::with_capacity(rows.len());
    for r in rows {
        let title = r
            .title
            .filter(|t| !t.trim().is_empty())
            .or_else(|| extract_h1_title(&r.body_text))
            .unwrap_or_else(|| r.note_id.clone());
        irr_inputs.push(IrrelevanceInput {
            note_id: r.note_id.clone(),
            section: r.section.clone(),
            title: title.clone(),
            status: r.status,
            created_ms: r.created_ms,
            trust: r.trust,
            last_used_ms: last_used.get(&r.note_id).copied(),
        });
        records.push(AuditRecord {
            id: r.note_id,
            section: r.section,
            title,
            body: r.body_text,
            author_id: r.author_id,
            embedding: r.embedding,
            embedder_id: r.embedder_id,
        });
    }

    let scanned = records.len();
    let candidates = audit::detect(&records, &AuditThresholds::default());

    // F-111 : axe pertinence — règle conjonctive pure + garde fenêtre.
    let protected = downgrade_cfg.protected_sections();
    let irrelevant = audit::detect_irrelevant(
        &irr_inputs,
        &downgrade_cfg.thresholds(),
        &protected,
        t0_ms,
        now_ms,
    );

    let mut report = audit::build_report(vault_id, now_ms, scanned, candidates, irrelevant);
    // Étiquettes autoritatives (P2-3) : le rendu markdown en dérive DRY-RUN / fenêtre.
    report.downgrade_enabled = downgrade_cfg.enabled;
    report.downgrade_window_covered =
        audit::window_covered(t0_ms, now_ms, downgrade_cfg.usage_window_days);

    // Exécuteur F-111 : seulement flag ON + candidat actionnable + downgrader présent.
    // Dry-run (défaut) = ZÉRO mutation. Cap `max_per_run`. Échec unitaire → continue.
    let mut downgraded = 0usize;
    if downgrade_cfg.enabled
        && let Some(dg) = downgrader
    {
        let actionable: Vec<audit::IrrelevantCandidate> = report
            .irrelevant
            .iter()
            .filter(|c| c.actionable)
            .take(downgrade_cfg.max_per_run)
            .cloned()
            .collect();
        for cand in actionable {
            let outcome = match dg.downgrade(vault_id, &cand.note_id, &cand.reason).await {
                Ok(()) => {
                    downgraded += 1;
                    metrics.audit_downgraded.inc();
                    "downgraded".to_string()
                }
                Err(e) => {
                    tracing::warn!(error = %e, note = %cand.note_id, "audit F-111: downgrade failed");
                    format!("error: {e}")
                }
            };
            report.downgrade_actions.push(DowngradeAction {
                note_id: cand.note_id,
                title: cand.title,
                reason: cand.reason,
                outcome,
            });
        }
    }

    let delete_tier = report.counts_by_tier.get("delete").copied().unwrap_or(0);
    let review_tier = report.counts_by_tier.get("review").copied().unwrap_or(0);

    // Métriques : gauge par catégorie (remise à plat de la passe précédente d'abord).
    metrics.audit_candidates.clear();
    for (cat, count) in &report.counts_by_category {
        metrics
            .audit_candidates
            .get_or_create(&AuditCategoryLabel {
                category: cat.clone(),
            })
            .set(*count as i64);
    }

    let mut errors = 0usize;
    if let Err(e) = write_artifacts(storage_root, vault_id, now_ms, &report).await {
        tracing::warn!(error = %e, vault = %vault_id, "audit: report write failed");
        metrics.audit_errors.inc();
        errors += 1;
    } else {
        metrics.audit_last_run_ms.set(now_ms);
        tracing::info!(
            vault = %vault_id,
            scanned,
            delete_tier,
            review_tier,
            downgraded,
            "audit: tick completed, report written"
        );
    }

    AuditRunStats {
        scanned,
        delete_tier,
        review_tier,
        downgraded,
        errors,
    }
}

/// Écrit les 3 artefacts (JSON / Markdown / commandes admin), horodatés + alias `-latest`.
async fn write_artifacts(
    storage_root: &Path,
    vault_id: &str,
    now_ms: i64,
    report: &audit::AuditReport,
) -> std::io::Result<()> {
    let dir = storage_root.join("audit");
    tokio::fs::create_dir_all(&dir).await?;

    let json = serde_json::to_string_pretty(report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let md = audit::render_markdown(report);
    let cmds = audit::render_admin_commands(report);

    // `vault_id` provient de la config (jamais d'entrée utilisateur) — pas de path traversal ici,
    // mais on reste défensif en n'interpolant que des composants de nom de fichier plats.
    for (suffix, content) in [
        (format!("audit-report-{vault_id}-{now_ms}.json"), &json),
        (format!("audit-report-{vault_id}-latest.json"), &json),
        (format!("audit-report-{vault_id}-{now_ms}.md"), &md),
        (format!("audit-report-{vault_id}-latest.md"), &md),
        (format!("audit-commands-{vault_id}-{now_ms}.sh"), &cmds),
        (format!("audit-commands-{vault_id}-latest.sh"), &cmds),
    ] {
        tokio::fs::write(dir.join(suffix), content).await?;
    }
    Ok(())
}
