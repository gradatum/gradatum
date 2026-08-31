//! `gradatum-admin drift-scan` sub-command (F-174, critère 3).
//!
//! Runs the coherence drift scan (`Vault::drift_check` → `scan_phase_a`) and **exposes**
//! its counters, alerting when any drift is present. Before F-174 the scan had no operator
//! surface at all — only a benchmark called it — so a vault could drift silently while the
//! control reported nothing. This command closes that gap: it is the "exposé et alerte
//! au-delà de zéro" of the card.
//!
//! ## Read-only
//!
//! The scan only reads (`file_checksums`, `storage.list`, a `count(*)` on `notes`). It
//! **never repairs** — reconstruction stays behind its dedicated, gated entry point. A
//! control that triggered a mass write on a live service would be an incident in waiting.
//!
//! ## What counts as drift
//!
//! Three coherence violations, plus confirmed content divergence:
//! - `level3_full_hash_mismatch` — a tracked file whose content diverged (index → disque),
//! - `missing` — a `file_checksums` entry whose file is gone (index → disque),
//! - `untracked` — a note `.md` on disk absent from `file_checksums` (disque → index),
//! - `embeddable_notes_without_vector` — an embeddable note (live/pending-review/staging)
//!   with no embedding (dimension vecteur).
//!
//! `level2_prefix_match` and `level3_full_hash_match` are **not** drift (stable or purely
//! cosmetic) and never raise the alert.
//!
//! ## Usage
//! ```text
//! gradatum-admin drift-scan --root /var/lib/gradatum
//! ```
//! Exit code is non-zero when [`DriftScanCliReport::has_drift`] holds, so the
//! command is usable as a health gate in a cron or CI step.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gradatum_vault::Vault;

/// Arguments for the `drift-scan` sub-command.
#[derive(Debug, Clone)]
pub struct DriftScanCliArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`) — holds `vault/`.
    pub root: PathBuf,
}

/// Exposed drift counters — the surface the card's critère 3 requires.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[must_use = "the report carries the drift counters and the alert verdict"]
pub struct DriftScanCliReport {
    /// Tracked files whose full hash diverged (confirmed drift, index → disque).
    pub mismatch: u64,
    /// `file_checksums` entries whose file is absent on disk (index → disque).
    pub missing: u64,
    /// Note `.md` on disk absent from `file_checksums` (disque → index).
    pub untracked: u64,
    /// Embeddable notes (live/pending-review/staging) with no embedding (dimension vecteur).
    pub embeddable_notes_without_vector: u64,
    /// Files verified stable (size+prefix or cosmetic full-hash match). Not drift —
    /// reported for context only, never contributes to the alert.
    pub stable: u64,
}

impl DriftScanCliReport {
    /// Total number of coherence violations across all directions and representations.
    ///
    /// Sums the four drift classes. `stable` is deliberately excluded: a stable file is the
    /// absence of drift, not a violation.
    #[must_use]
    pub fn total_drift(&self) -> u64 {
        self.mismatch + self.missing + self.untracked + self.embeddable_notes_without_vector
    }

    /// `true` when any drift class is non-zero — the alert condition (critère 3).
    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.total_drift() > 0
    }
}

/// Runs the drift scan against the vault under `args.root` and returns the exposed counters.
///
/// Read-only: opens the vault, calls [`Vault::drift_check`], maps the raw `DriftScanResult`
/// into the operator-facing [`DriftScanCliReport`]. Never writes, never repairs.
///
/// # Errors
///
/// - The vault directory (`<root>/vault`) cannot be opened.
/// - The drift scan itself fails (storage or index error).
pub async fn run(args: DriftScanCliArgs) -> Result<DriftScanCliReport> {
    let vault_dir = args.root.join("vault");
    if !vault_dir.exists() {
        anyhow::bail!(
            "vault directory not found: {} — the server must have initialised it",
            vault_dir.display()
        );
    }

    let vault = Vault::open(&vault_dir)
        .await
        .map_err(|e| anyhow::anyhow!("opening vault {}: {e}", vault_dir.display()))?;

    let scan = vault
        .drift_check()
        .await
        .context("running the coherence drift scan")?;

    let report = DriftScanCliReport {
        mismatch: scan.level3_full_hash_mismatch,
        missing: scan.missing.len() as u64,
        untracked: scan.untracked.len() as u64,
        embeddable_notes_without_vector: scan.embeddable_notes_without_vector,
        stable: scan.level2_prefix_match + scan.level3_full_hash_match,
    };

    tracing::info!(
        mismatch = report.mismatch,
        missing = report.missing,
        untracked = report.untracked,
        embeddable_notes_without_vector = report.embeddable_notes_without_vector,
        stable = report.stable,
        has_drift = report.has_drift(),
        "drift-scan complete"
    );

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::VaultId;

    // ── has_drift : chaque classe est seule à pouvoir déclencher l'alerte ────────
    // Domaine par domaine — un report où SEUL un champ est non nul doit alerter,
    // sinon le champ ne compte pas dans total_drift (garde masquée).
    #[test]
    fn has_drift_fires_on_each_class_alone_and_ignores_stable() {
        // Vault sain : uniquement du stable → aucune alerte.
        let clean = DriftScanCliReport {
            stable: 42,
            ..Default::default()
        };
        assert!(!clean.has_drift(), "un vault stable ne doit pas alerter");

        // Chaque classe, seule, déclenche.
        for report in [
            DriftScanCliReport {
                mismatch: 1,
                ..Default::default()
            },
            DriftScanCliReport {
                missing: 1,
                ..Default::default()
            },
            DriftScanCliReport {
                untracked: 1,
                ..Default::default()
            },
            DriftScanCliReport {
                embeddable_notes_without_vector: 1,
                ..Default::default()
            },
        ] {
            assert!(
                report.has_drift(),
                "chaque classe de dérive, seule, doit alerter : {report:?}"
            );
        }
    }

    // ── run : le scan est câblé bout-en-bout et remonte l'untracked ──────────────
    // Frontière : un vault fraîchement créé est sain ; un .md déposé hors entonnoir
    // fait basculer has_drift à vrai via le champ untracked (disque → index).
    #[tokio::test]
    async fn run_reports_untracked_and_flips_the_alert() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        std::fs::create_dir_all(vault_dir.join("main")).unwrap();

        // Vault vide → aucune dérive.
        let clean = run(DriftScanCliArgs {
            root: root.to_path_buf(),
        })
        .await
        .unwrap();
        assert!(
            !clean.has_drift(),
            "un vault vide ne doit pas alerter : {clean:?}"
        );

        // Dépose un .md hors entonnoir → untracked (disque → index).
        let orphan = gradatum_core::identity::NoteId::new();
        std::fs::write(
            vault_dir.join(format!("main/{orphan}.md")),
            "# Hors entonnoir\n",
        )
        .unwrap();

        let drifted = run(DriftScanCliArgs {
            root: root.to_path_buf(),
        })
        .await
        .unwrap();
        assert!(
            drifted.untracked >= 1,
            "le .md hors entonnoir est untracked"
        );
        assert!(
            drifted.has_drift(),
            "l'untracked doit faire basculer l'alerte : {drifted:?}"
        );
    }
}
