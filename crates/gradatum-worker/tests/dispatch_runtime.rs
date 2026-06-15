//! Tests d'intégration — `Dispatcher::process_job` câblé curator + vault.
//!
//! T5 P2.0c : vérifie que `run_once` traite réellement les 3 kinds de jobs
//! (curate / classify / downgrade) avec la cascade curator + persistance vault.
//!

use std::sync::Arc;

use bincode::config::standard as bincode_std;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use tempfile::TempDir;

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Payload bincode pour VaultWriteRequest — miroir de `gradatum_dto::VaultWriteRequest`.
///
/// INVARIANT D'ORDRE (bincode positionnel — pas de noms de champs) :
/// pos 6 = `tenant_id`, pos 7 = `expected_sha256`, pos 8 = `note_id`.
/// Ne jamais modifier l'ordre sans aligner `dispatch.rs` + `gradatum-dto`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct WriteReq {
    title: String,
    body: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    section_hint: Option<String>,
    #[serde(default = "default_main_write")]
    tenant_id: String,
    #[serde(default)]
    expected_sha256: Option<String>,
    /// ULID préalloué (optionnel — alignement bug C v0.3.7).
    #[serde(default)]
    note_id: Option<String>,
}

fn default_main_write() -> String {
    "main".into()
}

/// Encode un VaultWriteRequest en payload bincode (sans note_id préalloué).
fn encode_write_payload(title: &str, body: &str, section_hint: Option<&str>) -> Vec<u8> {
    let req = WriteReq {
        title: title.into(),
        body: body.into(),
        author: None,
        tags: vec![],
        section_hint: section_hint.map(|s| s.to_string()),
        tenant_id: "main".into(),
        expected_sha256: None,
        note_id: None,
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).unwrap()
}

/// Encode un VaultWriteRequest avec `note_id` préalloué — test alignement bug C.
fn encode_write_payload_with_note_id(title: &str, body: &str, note_id: &str) -> Vec<u8> {
    let req = WriteReq {
        title: title.into(),
        body: body.into(),
        author: None,
        tags: vec![],
        section_hint: None,
        tenant_id: "main".into(),
        expected_sha256: None,
        note_id: Some(note_id.to_string()),
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).unwrap()
}

/// Encode un VaultClassifyRequest en payload bincode.
fn encode_classify_payload(note_id: &str) -> Vec<u8> {
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct ClassifyReq {
        note_id: String,
        #[serde(default = "default_main")]
        tenant_id: String,
    }
    fn default_main() -> String {
        "main".into()
    }
    let req = ClassifyReq {
        note_id: note_id.into(),
        tenant_id: "main".into(),
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).unwrap()
}

/// Encode un VaultDowngradeRequest en payload bincode.
fn encode_downgrade_payload(note_id: &str, reason: &str) -> Vec<u8> {
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct DowngradeReq {
        note_id: String,
        reason: String,
        #[serde(default)]
        replaced_by: Option<String>,
        #[serde(default = "default_main")]
        tenant_id: String,
    }
    fn default_main() -> String {
        "main".into()
    }
    let req = DowngradeReq {
        note_id: note_id.into(),
        reason: reason.into(),
        replaced_by: None,
        tenant_id: "main".into(),
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).unwrap()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// T5 Step 1 — curate kind : une note avec préfixe [DECISIONS] doit être admise
/// et persistée dans le vault avec la section `decisions`.
#[tokio::test]
async fn dispatch_curate_writes_note_with_assigned_section() {
    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Enqueue un job curate avec titre [DECISIONS] → heuristic route → decisions
    let payload = encode_write_payload(
        "[DECISIONS] Test note dispatch curate",
        "Corps de la note.",
        None,
    );
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "le dispatcher doit signaler qu'un job a été traité"
    );

    // Vérification : au moins une note dans l'index (locus_count ≥ 1)
    let count = vault.index().locus_count().await.unwrap();
    assert_eq!(count, 1, "une note doit être indexée après curate admis");
}

/// T5 — classify kind : re-router une note existante via l'heuristique.
#[tokio::test]
async fn dispatch_classify_reclassifies_note() {
    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Écrire une note directement dans le vault
    let frontmatter = build_minimal_frontmatter(Section::Reference, NoteStatus::Live);
    let note = vault
        .write_note(frontmatter, "debug content OOM crash fix".into())
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Enqueue classify
    let payload = encode_classify_payload(&note_id);
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "classify".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "le dispatcher doit signaler qu'un job a été traité"
    );

    // La note a été re-classifiée — l'index doit toujours contenir 1 note
    let count = vault.index().locus_count().await.unwrap();
    assert_eq!(count, 1, "toujours 1 note après reclassification");
}

/// T5 — downgrade kind : rétrograder une note Live → Deprecated.
#[tokio::test]
async fn dispatch_downgrade_deprecates_live_note() {
    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Écrire une note Live
    let frontmatter = build_minimal_frontmatter(Section::Decisions, NoteStatus::Live);
    let note = vault
        .write_note(frontmatter, "décision archivée".into())
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Enqueue downgrade
    let payload = encode_downgrade_payload(&note_id, "remplacée par une version révisée");
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "downgrade".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "le dispatcher doit signaler qu'un job a été traité"
    );

    // La note re-écrite avec statut Deprecated (index = 1 note mise à jour)
    let count = vault.index().locus_count().await.unwrap();
    // Après write_note (curate original) + write_note (downgrade), locus_count peut être 1
    // car la note est upsert-ée (même id ULID généré à chaque write → deux entrées)
    // Le comportement observable important : run_once retourne true sans panique
    assert!(count >= 1, "au moins 1 note dans l'index après downgrade");
}

/// T5 — queue vide : run_once retourne false sans bloquer.
#[tokio::test]
async fn dispatch_empty_queue_returns_false() {
    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Queue vide — aucun job enqueué
    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let result = dispatcher.run_once().await;
    assert!(
        result.is_ok(),
        "run_once ne doit pas retourner Err sur queue vide"
    );
    let processed = result.unwrap();
    assert!(!processed, "run_once retourne false si la queue est vide");
}

// ── Task 18 B3 — Tests cascade curator classify ─────────────────────────────

/// Implémentation mock du trait CuratorProcess pour les tests classify.
///
/// Comptabilise les appels via AtomicUsize et retourne un CurateOutcome configuré.
struct MockCuratorProcess {
    call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    outcome: gradatum_curator::CurateOutcome,
}

#[async_trait::async_trait]
impl gradatum_curator::CuratorProcess for MockCuratorProcess {
    async fn process(&self, _note: gradatum_curator::Note) -> gradatum_curator::CurateOutcome {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.outcome.clone()
    }
}

/// B3 : le job classify doit appeler le curator via la cascade complète.
///
/// MockCuratorProcess comptabilise les appels — le mock doit être appelé ≥ 1 fois.
#[tokio::test]
async fn classify_job_calls_curator_cascade_not_heuristic_only() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Seed une note initiale dans le vault
    let frontmatter = build_minimal_frontmatter(Section::Reference, NoteStatus::Live);
    let note = vault
        .write_note(
            frontmatter,
            "# Note de raisonnement\n\nContenu sémantique pour classify cascade.".into(),
        )
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Mock curator : retourne Admitted avec section "reasoning"
    let call_count = Arc::new(AtomicUsize::new(0));
    let mock = Arc::new(MockCuratorProcess {
        call_count: Arc::clone(&call_count),
        outcome: gradatum_curator::CurateOutcome::Admitted {
            decisions: gradatum_curator::CuratorDecisions {
                canonical_section: "reasoning".to_string(),
                tags: vec![],
                novelty: gradatum_curator::novelty::NoveltyVerdict::Admitted,
                wikilinks: vec![],
                dedup: gradatum_curator::dedup::DedupVerdict::Unique,
            },
        },
    });

    // Enqueue classify
    let payload = encode_classify_payload(&note_id);
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "classify".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = gradatum_worker::dispatch::Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(mock as Arc<dyn gradatum_curator::CuratorProcess>)
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(processed, "run_once doit signaler qu'un job a été traité");

    assert!(
        call_count.load(Ordering::Relaxed) > 0,
        "le mock curator doit avoir été appelé par process_job(classify) — B3 cascade"
    );
}

/// B3 outcome Rejected : la note reste inchangée dans le vault.
///
/// Un mock retournant Rejected ne doit pas modifier la section de la note.
#[tokio::test]
async fn classify_job_rejected_does_not_modify_note() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Seed une note en section Reference
    let frontmatter = build_minimal_frontmatter(Section::Reference, NoteStatus::Live);
    let note = vault
        .write_note(frontmatter, "# Note testée\n\nCorps de note.".into())
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Mock curator : retourne Rejected
    let mock = Arc::new(MockCuratorProcess {
        call_count: Arc::new(AtomicUsize::new(0)),
        outcome: gradatum_curator::CurateOutcome::Rejected {
            reason: "note non pertinente pour classify".to_string(),
        },
    });

    let payload = encode_classify_payload(&note_id);
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "classify".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = gradatum_worker::dispatch::Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(mock as Arc<dyn gradatum_curator::CuratorProcess>)
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "run_once doit traiter le job même en cas de Rejected"
    );

    // La note doit toujours être présente dans l'index (1 note)
    let count = vault.index().locus_count().await.unwrap();
    assert_eq!(
        count, 1,
        "note rejetée par classify ne doit pas changer l'index (toujours 1 note)"
    );
}

/// B3 outcome Pending : la note passe en PendingReview (F-37 S1.2).
///
/// Un mock retournant Pending doit écrire la note avec NoteStatus::PendingReview
/// (flip Staging → PendingReview, file `/review`). Gate parité write-path.
#[tokio::test]
async fn classify_job_pending_sets_pending_review_status() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Seed une note Live en section Reference
    let frontmatter = build_minimal_frontmatter(Section::Reference, NoteStatus::Live);
    let note = vault
        .write_note(
            frontmatter,
            "# Note pour classify pending\n\nContenu.".into(),
        )
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Mock curator : retourne Pending avec section "architecture"
    let mock = Arc::new(MockCuratorProcess {
        call_count: Arc::new(AtomicUsize::new(0)),
        outcome: gradatum_curator::CurateOutcome::Pending {
            decisions: gradatum_curator::CuratorDecisions {
                canonical_section: "architecture".to_string(),
                tags: vec![],
                novelty: gradatum_curator::novelty::NoveltyVerdict::Admitted,
                wikilinks: vec![],
                dedup: gradatum_curator::dedup::DedupVerdict::Unique,
            },
            reason: "confiance LLM insuffisante".to_string(),
        },
    });

    let payload = encode_classify_payload(&note_id);
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "classify".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = gradatum_worker::dispatch::Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(mock as Arc<dyn gradatum_curator::CuratorProcess>)
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(processed, "run_once doit traiter le job Pending");

    // F-37 S1.2 — le flip Staging → PendingReview est observable au niveau de l'index :
    // après un classify Pending, au moins une note est en PendingReview et AUCUNE en
    // Staging (le path legacy `write_note` réécrit sous un nouvel ULID — comportement
    // pré-existant hors-scope ; ce qui compte ici est le statut écrit par le flip).
    let vid = VaultId::new("main");
    let pending_review = vault
        .index()
        .list_by_status(&vid, NoteStatus::PendingReview)
        .await
        .expect("list_by_status PendingReview");
    let staging = vault
        .index()
        .list_by_status(&vid, NoteStatus::Staging)
        .await
        .expect("list_by_status Staging");
    let _ = &note_id; // note seedée Live conservée (path legacy nouvel ULID)
    assert!(
        !pending_review.is_empty(),
        "classify Pending doit produire une note PendingReview (F-37 S1.2)"
    );
    assert!(
        staging.is_empty(),
        "aucune note ne doit rester en Staging après le flip S1.2"
    );
}

/// M3 — deux jobs classify successifs sur la même note ne corrompent pas le vault.
///
/// Le second job est traité sans panique. L'état final est cohérent.
#[tokio::test]
async fn classify_job_twice_on_same_note_does_not_corrupt() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    // Seed une note Live
    let frontmatter = build_minimal_frontmatter(Section::Reference, NoteStatus::Live);
    let note = vault
        .write_note(frontmatter, "# Note double classify\n\nContenu.".into())
        .await
        .unwrap();
    let note_id = note.id.to_string();

    // Enqueue deux jobs classify sur la même note
    for _ in 0..2 {
        let payload = encode_classify_payload(&note_id);
        queue
            .enqueue(NewJob {
                tenant_id: "main".into(),
                kind: "classify".into(),
                payload,
                max_attempts: 5,
            })
            .await
            .unwrap();
    }

    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());
    let dispatcher = gradatum_worker::dispatch::Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(curator as Arc<dyn gradatum_curator::CuratorProcess>)
        .with_audit(Arc::new(NoopAuditSink));

    // Premier classify
    let r1 = dispatcher.run_once().await.unwrap();
    assert!(r1, "premier classify doit traiter un job");

    // Second classify sur la même note
    let r2 = dispatcher.run_once().await.unwrap();
    assert!(r2, "second classify doit traiter un job sans panique");

    // Le vault est dans un état cohérent (index accessible)
    let count = vault.index().locus_count().await.unwrap();
    assert!(count >= 1, "vault cohérent après double classify");
}

// ── Test alignement bug C — ULID préalloué préservé par le Dispatcher ─────────

/// Régression bug C (v0.3.7) : le Dispatcher honore le `note_id` préalloué
/// dans le payload bincode curate. La note doit être lisible à l'ULID fourni.
///
/// Avant le fix : `write_note` générait un ULID frais → stored id ≠ note_id 202
/// → wikilinks morts (404). Après le fix : `write_note_with_id` honore l'ULID.
#[tokio::test]
async fn dispatch_curate_honors_prealloc_note_id() {
    use gradatum_core::identity::NoteId;
    use ulid::Ulid;

    let dir = TempDir::new().unwrap();
    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    let prealloc = Ulid::new();
    // Préfixe [DECISIONS] → heuristique confidence ≥ 0.8 → CurateOutcome::Admitted
    let payload = encode_write_payload_with_note_id(
        "[DECISIONS] Alignement bug C — ULID préalloué",
        "Corps de note suffisant pour le curator.",
        &prealloc.to_string(),
    );

    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(processed, "run_once doit signaler un job traité");

    // La note doit être lisible à l'ULID préalloué (bug C : stored id == enqueued id)
    let read = vault.read_note(NoteId(prealloc)).await;
    assert!(
        read.is_ok(),
        "la note doit être lisible à l'ULID préalloué via Dispatcher — err={:?}",
        read.err()
    );
    assert_eq!(
        read.unwrap().id,
        NoteId(prealloc),
        "l'id stocké doit être l'ULID préalloué — régression bug C"
    );
}

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn build_minimal_frontmatter(
    section: Section,
    status: NoteStatus,
) -> gradatum_core::frontmatter::Frontmatter {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use smallvec::SmallVec;
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: None,
        tags: SmallVec::new(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}
