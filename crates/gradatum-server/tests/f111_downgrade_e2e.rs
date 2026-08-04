//! Tests e2e F-111 — axe pertinence de la passe d'audit + exécuteur auto-downgrade.
//!
//! Isole la POLITIQUE (audit_once : détection + garde fenêtre + cap + dry-run) du
//! MÉCANISME (le vrai downgrade→restore est couvert par les tests `vault_downgrade`
//! de gradatum-index). Un `FakeDowngrader` enregistre les appels dans un `Mutex`.
//!
//! Invariants vérifiés : dry-run (enabled=false) = ZÉRO appel downgrader ; garde
//! fenêtre bloque même enabled=true ; cap `max_per_run` ; échec unitaire ne stoppe
//! pas la passe ; ordre déterministe (les plus vieilles d'abord).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gradatum_core::error::GradatumError;
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use gradatum_server::audit_job::{AuditRunStats, NoteDowngrader, audit_once};
use gradatum_server::config::{AuditConfig, DowngradeConfig};
use gradatum_server::metrics::AppMetrics;
use gradatum_server::note_usage_store::NoteUsageStore;

const NOW: i64 = 1_784_200_000_000;
const DAY_MS: i64 = 86_400_000;

fn tmp_storage_root() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("f111-downgrade-e2e-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create tmp storage root");
    dir
}

/// Faux downgrader : enregistre les ULID appelés ; échoue optionnellement sur un ULID.
#[derive(Default)]
struct FakeDowngrader {
    calls: Mutex<Vec<String>>,
    fail_on: Option<String>,
}

impl FakeDowngrader {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("mutex").clone()
    }
}

#[async_trait]
impl NoteDowngrader for FakeDowngrader {
    async fn downgrade(
        &self,
        _tenant: &str,
        note_id: &str,
        _reason: &str,
    ) -> Result<(), GradatumError> {
        if self.fail_on.as_deref() == Some(note_id) {
            return Err(GradatumError::Storage("fake downgrade failure".into()));
        }
        self.calls.lock().expect("mutex").push(note_id.to_string());
        Ok(())
    }
}

/// Seede une note candidate (vieille, debug, trust NULL→0.5, live) via `created` explicite.
async fn seed_old(idx: &SqliteIndex, id: &str, age_days: i64) {
    idx.seed_note_with_created(
        id,
        "debug",
        "# vieille note\n\ncontenu peu pertinent daté",
        NOW - age_days * DAY_MS,
    )
    .await
    .unwrap_or_else(|e| panic!("seed {id}: {e}"));
}

/// Store usage avec une ligne d'ancrage T0 (proxy début de collecte) — la note d'ancrage
/// n'existe pas dans l'index, donc n'est jamais candidate ; les candidats restent « 0 usage ».
async fn usage_with_t0(root: &std::path::Path, t0_age_days: i64) -> NoteUsageStore {
    let store = NoteUsageStore::open_or_create(&root.join("note_usage.db"))
        .await
        .expect("usage open");
    let mut batch: HashMap<(String, String, String), (u64, i64)> = HashMap::new();
    batch.insert(
        (
            "main".to_string(),
            "zzanchor".to_string(),
            "read".to_string(),
        ),
        (1, NOW - t0_age_days * DAY_MS),
    );
    store.flush_batch(batch).await.expect("flush anchor");
    store
}

fn audit_enabled() -> AuditConfig {
    AuditConfig {
        enabled: true,
        interval_secs: 60,
        max_scan: 1000,
    }
}

// Test 1 — dry-run (enabled=false) : candidats rapportés, ZÉRO appel downgrader, statuts inchangés.
#[tokio::test]
async fn f111_dry_run_reports_without_mutation() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    seed_old(&idx, "01AAA", 210).await;
    seed_old(&idx, "01BBB", 200).await;
    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let root = tmp_storage_root();
    let usage = usage_with_t0(&root, 45).await; // fenêtre 30j couverte
    let fake = FakeDowngrader::default();

    let stats = audit_once(
        &index,
        &metrics,
        &audit_enabled(),
        &DowngradeConfig::default(), // enabled = false
        Some(&usage),
        Some(&fake),
        &root,
        "main",
        NOW,
    )
    .await;

    assert_eq!(stats.downgraded, 0, "dry-run : aucune mutation");
    assert!(
        fake.calls().is_empty(),
        "dry-run : downgrader jamais appelé"
    );

    let report: gradatum_curator::audit::AuditReport = serde_json::from_str(
        &std::fs::read_to_string(root.join("audit/audit-report-main-latest.json")).expect("json"),
    )
    .expect("parse report");
    assert_eq!(report.irrelevant.len(), 2, "2 candidats rapportés");
    assert!(report.downgrade_actions.is_empty());
    assert!(!report.downgrade_enabled);

    // Statuts DB inchangés : les 2 notes restent scannables (les downgradées sont exclues du scan SQL).
    let rows = index.audit_scan("main", 100).await.expect("rescan");
    let ids: Vec<&str> = rows.iter().map(|r| r.note_id.as_str()).collect();
    assert!(
        ids.contains(&"01AAA") && ids.contains(&"01BBB"),
        "notes toujours live"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Test 2 — enabled=true + fenêtre couverte : downgrade capé, actions au rapport, plus vieilles d'abord.
#[tokio::test]
async fn f111_executor_downgrades_capped_and_traced() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    seed_old(&idx, "01C300", 300).await;
    seed_old(&idx, "01C250", 250).await;
    seed_old(&idx, "01C200", 200).await;
    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let root = tmp_storage_root();
    let usage = usage_with_t0(&root, 45).await;
    let fake = FakeDowngrader::default();

    let cfg = DowngradeConfig {
        enabled: true,
        max_per_run: 2,
        ..Default::default()
    };
    let stats = audit_once(
        &index,
        &metrics,
        &audit_enabled(),
        &cfg,
        Some(&usage),
        Some(&fake),
        &root,
        "main",
        NOW,
    )
    .await;

    assert_eq!(stats.downgraded, 2, "cap max_per_run=2 respecté");
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    // Les 2 plus vieilles (300j, 250j), dans l'ordre âge décroissant.
    assert_eq!(calls, vec!["01C300".to_string(), "01C250".to_string()]);

    let report: gradatum_curator::audit::AuditReport = serde_json::from_str(
        &std::fs::read_to_string(root.join("audit/audit-report-main-latest.json")).expect("json"),
    )
    .expect("parse report");
    assert_eq!(report.downgrade_actions.len(), 2);
    assert!(
        report
            .downgrade_actions
            .iter()
            .all(|a| a.outcome == "downgraded")
    );
    assert!(report.downgrade_enabled && report.downgrade_window_covered);

    let _ = std::fs::remove_dir_all(&root);
}

// Test 3 — enabled=true + fenêtre NON couverte (T0 récent) : exécuteur inerte.
#[tokio::test]
async fn f111_executor_inert_when_window_uncovered() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    seed_old(&idx, "01AAA", 210).await;
    seed_old(&idx, "01BBB", 200).await;
    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let root = tmp_storage_root();
    let usage = usage_with_t0(&root, 5).await; // T0 = now-5j < fenêtre 30j → non couverte
    let fake = FakeDowngrader::default();

    let cfg = DowngradeConfig {
        enabled: true,
        ..Default::default()
    };
    let stats = audit_once(
        &index,
        &metrics,
        &audit_enabled(),
        &cfg,
        Some(&usage),
        Some(&fake),
        &root,
        "main",
        NOW,
    )
    .await;

    assert_eq!(stats.downgraded, 0, "fenêtre non couverte : rien downgradé");
    assert!(fake.calls().is_empty());

    let report: gradatum_curator::audit::AuditReport = serde_json::from_str(
        &std::fs::read_to_string(root.join("audit/audit-report-main-latest.json")).expect("json"),
    )
    .expect("parse report");
    assert!(!report.downgrade_window_covered);
    assert!(
        report.irrelevant.iter().all(|c| !c.actionable),
        "candidats non-actionnables"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Test 4 — échec unitaire ne stoppe pas la passe.
#[tokio::test]
async fn f111_executor_continues_on_item_error() {
    let idx = SqliteIndex::open_in_memory().await.expect("open");
    seed_old(&idx, "01OLD", 300).await; // la plus vieille → traitée en 1er, échoue
    seed_old(&idx, "01NEW", 200).await;
    let index: Arc<dyn Index> = Arc::new(idx);
    let metrics = AppMetrics::new();
    let root = tmp_storage_root();
    let usage = usage_with_t0(&root, 45).await;
    let fake = FakeDowngrader {
        fail_on: Some("01OLD".to_string()),
        ..Default::default()
    };

    let cfg = DowngradeConfig {
        enabled: true,
        ..Default::default()
    };
    let stats = audit_once(
        &index,
        &metrics,
        &audit_enabled(),
        &cfg,
        Some(&usage),
        Some(&fake),
        &root,
        "main",
        NOW,
    )
    .await;

    assert_eq!(stats.downgraded, 1, "1 succès malgré 1 échec");
    assert_eq!(
        fake.calls(),
        vec!["01NEW".to_string()],
        "2e note traitée après l'échec"
    );

    let report: gradatum_curator::audit::AuditReport = serde_json::from_str(
        &std::fs::read_to_string(root.join("audit/audit-report-main-latest.json")).expect("json"),
    )
    .expect("parse report");
    let outcomes: Vec<&str> = report
        .downgrade_actions
        .iter()
        .map(|a| a.outcome.as_str())
        .collect();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes[0].starts_with("error:"), "1er échec tracé");
    assert_eq!(outcomes[1], "downgraded");

    let _ = std::fs::remove_dir_all(&root);
}

// Garde-fou : `AuditRunStats` par défaut a `downgraded = 0` (dérive-guard sur le champ).
#[tokio::test]
async fn f111_default_stats_has_zero_downgraded() {
    assert_eq!(AuditRunStats::default().downgraded, 0);
}
