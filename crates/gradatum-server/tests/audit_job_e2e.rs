//! Tests e2e dry-run pour `audit_job::audit_once` (F-51, Option A).
//!
//! Seede un vault SQLite en mémoire reproduisant les catégories du corpus pilote
//! (A sonde, C doublon exact, E note vide, F collision de titre) + contrôles négatifs
//! (notes distinctes légitimes) + garde-fou invariant (section protégée jamais scannée).
//! Exécute la passe et vérifie le rapport écrit + les métriques. Aucune mutation du vault.

use std::path::PathBuf;
use std::sync::Arc;

use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use gradatum_server::audit_job::audit_once;
use gradatum_server::config::AuditConfig;
use gradatum_server::metrics::AppMetrics;

fn tmp_storage_root() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("f51-audit-e2e-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create tmp storage root");
    dir
}

async fn seed(idx: &SqliteIndex, id: &str, section: &str, body: &str) {
    idx.seed_note(id, section, body)
        .await
        .unwrap_or_else(|e| panic!("seed_note {id}: {e}"));
}

#[tokio::test]
async fn audit_once_reports_categories_and_excludes_protected() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // C — doublon exact (corps identique) en debug.
    seed(
        &idx,
        "01C0",
        "debug",
        "# dup note\n\nidentical body content here now",
    )
    .await;
    seed(
        &idx,
        "01C1",
        "debug",
        "# dup note\n\nidentical body content here now",
    )
    .await;
    // A — sonde : titre-sonde + corps court en debug.
    seed(&idx, "01A0", "debug", "# tagprobe status open\n\ntag probe").await;
    // E — note vide en debug.
    seed(&idx, "01E0", "debug", "").await;
    // F — collision de titre, corps distincts (non exact, non near-dup) en debug.
    seed(
        &idx,
        "01F0",
        "debug",
        "# Description 2026\n\nexternal-agent ci cargo install recompile fix alpha",
    )
    .await;
    seed(
        &idx,
        "01F1",
        "debug",
        "# Description 2026\n\nllm commons content block vec extend beta gamma",
    )
    .await;
    // Contrôles négatifs — notes distinctes légitimes (pas de flag attendu).
    seed(
        &idx,
        "01N0",
        "reference",
        "# REX field notes\n\nllama cpp gateway AMD tableau modèles lignée fork",
    )
    .await;
    seed(
        &idx,
        "01N1",
        "architecture",
        "# Gap analysis v81\n\ncouverture bronze divergences planifié live",
    )
    .await;
    // Garde-fou invariant : 2 doublons exacts en section PROTÉGÉE — jamais scannés.
    seed(
        &idx,
        "01P0",
        "decisions",
        "# protected dup\n\nidentical protected body here now",
    )
    .await;
    seed(
        &idx,
        "01P1",
        "decisions",
        "# protected dup\n\nidentical protected body here now",
    )
    .await;

    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let cfg = AuditConfig {
        enabled: true,
        interval_secs: 60,
        max_scan: 1000,
    };
    let root = tmp_storage_root();

    let stats = audit_once(
        &index,
        &metrics,
        &cfg,
        &gradatum_server::config::DowngradeConfig::default(),
        None,
        None,
        &root,
        "main",
        1_700_000_000_000,
    )
    .await;

    // 8 notes non protégées scannées (les 2 decisions exclues).
    assert_eq!(stats.scanned, 8, "sections protégées exclues du scan");
    assert_eq!(stats.errors, 0);
    assert!(
        stats.delete_tier >= 3,
        "exact + sonde + vide en tier delete"
    );
    assert!(stats.review_tier >= 1, "collision de titre en tier review");

    // Relire le rapport JSON écrit.
    let report_path = root.join("audit/audit-report-main-latest.json");
    let raw = std::fs::read_to_string(&report_path).expect("rapport JSON écrit");
    let report: gradatum_curator::audit::AuditReport =
        serde_json::from_str(&raw).expect("rapport JSON valide");

    assert_eq!(report.scanned, 8);
    // Catégories attendues présentes.
    assert!(
        report
            .counts_by_category
            .get("exact-duplicate")
            .copied()
            .unwrap_or(0)
            >= 1
    );
    assert!(
        report
            .counts_by_category
            .get("structural-junk")
            .copied()
            .unwrap_or(0)
            >= 2
    );
    assert!(
        report
            .counts_by_category
            .get("title-collision")
            .copied()
            .unwrap_or(0)
            >= 1
    );

    // Garde-fou : AUCUN candidat en section protégée, ni les contrôles négatifs.
    for c in &report.candidates {
        assert_ne!(c.section, "decisions", "note protégée jamais candidate");
        assert_ne!(c.note_id, "01N0", "contrôle négatif reference non flaggé");
        assert_ne!(
            c.note_id, "01N1",
            "contrôle négatif architecture non flaggé"
        );
    }
    // Le doublon protégé n'est pas dans le rapport.
    assert!(!report.candidates.iter().any(|c| c.note_id == "01P1"));

    // Commandes admin générées pour les candidats delete.
    let cmds = std::fs::read_to_string(root.join("audit/audit-commands-main-latest.sh"))
        .expect("commandes écrites");
    assert!(cmds.contains("gradatum-admin delete"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn audit_once_disabled_is_noop() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    seed(&idx, "01A0", "debug", "").await;
    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let cfg = AuditConfig {
        enabled: false,
        interval_secs: 60,
        max_scan: 1000,
    };
    let root = tmp_storage_root();

    let stats = audit_once(
        &index,
        &metrics,
        &cfg,
        &gradatum_server::config::DowngradeConfig::default(),
        None,
        None,
        &root,
        "main",
        1_700_000_000_000,
    )
    .await;
    assert_eq!(stats, gradatum_server::audit_job::AuditRunStats::default());
    // Aucun artefact écrit.
    assert!(!root.join("audit/audit-report-main-latest.json").exists());
    let _ = std::fs::remove_dir_all(&root);
}
