//! C4-1e (W3) — Fermeture du TROU RÉEL TOCTOU cross-vault de la purge.
//!
//! `handle_purge` re-vérifie le statut de chaque candidat avant destruction (mitigation
//! TOCTOU : une note restaurée `Garbage→Live` entre le listing et le delete est skip).
//! Avant ce fix, cette re-vérification passait par `InternalClient::get_note(id)` — résolu
//! **par ULID seul via le singleton `main`** — alors que le listing (`list_garbage`) est
//! scopé au vault du tick. Fenêtre :
//!
//! ```text
//! list_garbage(vault-b)  →  get_note(ULID)  →  delete
//!      (scopé vault-b)       (NON scopé, main)     (scopé vault-b)
//! ```
//!
//! Un candidat de `vault-b` (ULID X) voyait son statut re-vérifié dans `main` : si `main`
//! détenait un homonyme `X` Live, la note de `vault-b` était **skip à tort** (jamais
//! purgée) ; si `main` détenait `X` Garbage, la purge se fondait sur le mauvais vault.
//!
//! Le fix threade le `vault_id` du tick jusqu'au re-check : `get_note_status(vault_id, id)`
//! (index, `WHERE vault_id = ?1 AND id = ?2` — même source que `list_garbage`).
//!
//! Harnais : `TestInternalClient` enveloppe un `Vault` (`main`) + son `SqliteIndex`. La
//! clé composite `(vault_id, id)` (migration 0032) permet à l'ULID X de coexister dans
//! `main` (Live) et `vault-b` (Garbage) dans le même index. Les assertions portent sur
//! l'**état INDEX** (proprement scopé) ; le `.md` physique reste mono-vault dans ce
//! harnais (limite connue : un seul vault physique en test).

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::{
    GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
    JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, PurgeMode, PurgeSpec, TriggerSource,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{MultiTenantCfg, handle_purge};
use gradatum_worker::internal_client::InternalClient;
use tempfile::TempDir;
use test_internal_client::TestInternalClient;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

struct Fixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

async fn make_fixture() -> Fixture {
    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();
    Fixture {
        vault,
        index,
        _tmp: tmp,
    }
}

fn make_frontmatter(vault_id: &str, section: Section, status: NoteStatus) -> Frontmatter {
    let now = Utc::now();
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: Some(now),
        tags: Default::default(),
        author: None,
        created: now,
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Job `Purge` réel (dry_run=false, grace_days=None) scopé à `vault_id`.
fn make_scoped_purge_job(vault_id: &str) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::System;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Purge(PurgeSpec {
                    mode: PurgeMode::Lifecycle,
                    dry_run: false,
                    grace_days: None,
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::Vault(vault_id.to_string()),
                priority: JobPriority::Low,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Running,
                created_at: now,
                started_at: Some(now),
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test — TROU RÉEL : re-check TOCTOU scopé par le vault du candidat
// ─────────────────────────────────────────────────────────────────────────────

/// ULID X collisionné : Garbage dans `vault-b`, Live dans `main`.
///
/// Le tick de purge de `vault-b` DOIT re-vérifier le statut de X **dans `vault-b`**
/// (Garbage) et le purger — sans consulter `main`, où X est Live.
///
/// Avant fix (`get_note(X)` → `main` Live) : X de `vault-b` est skip à tort → jamais purgé.
/// Après fix (`get_note_status("vault-b", X)` → Garbage) : X de `vault-b` est purgé,
/// et l'homonyme Live de `main` reste intact.
#[tokio::test]
async fn purge_rechecks_candidate_status_in_its_own_vault() {
    let fixture = make_fixture().await;

    // ULID unique partagé par les deux vaults (clé composite `(vault_id, id)`).
    let note_id = NoteId::new();
    let x = note_id.0.to_string();

    // ── vault-b : X en Garbage (candidat à purger) — seedé AVANT main pour un fts propre.
    fixture
        .index
        .seed_note_with_fts_vault(&x, "vault-b", "feedback", None, "corps vault-b")
        .await
        .expect("seed vault-b");
    fixture
        .index
        .patch_note_status(
            &AclCheckedVaultId::for_system_task(VaultId::new("vault-b")),
            &note_id,
            Some("garbage"),
            None,
            None,
        )
        .await
        .expect("patch vault-b garbage");

    // ── main : X en Live (note légitime, .md réel) — l'homonyme à ne PAS toucher.
    fixture
        .vault
        .write_note_with_id(
            make_frontmatter("main", Section::Feedback, NoteStatus::Live),
            format!("# main {x}\n\ncorps main live"),
            note_id,
        )
        .await
        .expect("write_note_with_id main");

    // Sanity pré-purge : X = Garbage@vault-b, Live@main.
    assert_eq!(
        fixture
            .index
            .get_note_status("vault-b", &x)
            .await
            .expect("status vault-b"),
        Some(NoteStatus::Garbage),
        "pré-purge : X est Garbage dans vault-b"
    );
    assert_eq!(
        fixture
            .index
            .get_note_status("main", &x)
            .await
            .expect("status main"),
        Some(NoteStatus::Live),
        "pré-purge : X est Live dans main"
    );

    // ── Purge scopée vault-b, multi-tenant ON (route vers vault-b, sinon rejet fail-closed).
    let client = Arc::new(TestInternalClient::new(
        Arc::clone(&fixture.vault),
        Arc::clone(&fixture.index),
    )) as Arc<dyn InternalClient>;

    let result = handle_purge(
        make_scoped_purge_job("vault-b"),
        Data::new(client),
        Data::new(MultiTenantCfg { enabled: true }),
    )
    .await
    .expect("handle_purge vault-b ne doit pas échouer");

    // Le candidat de vault-b est purgé (re-check scopé → Garbage confirmé dans vault-b).
    assert!(
        result.result_note_md.contains("1 note(s) deleted"),
        "1 suppression attendue (X de vault-b) : {}",
        result.result_note_md
    );
    assert!(
        fixture
            .index
            .get_note_status("vault-b", &x)
            .await
            .expect("status vault-b post")
            .is_none(),
        "X de vault-b DOIT être purgé (re-check dans SON vault, pas dans main)"
    );

    // L'homonyme Live de main N'EST PAS touché (delete_note_from_index scopé vault-b).
    assert_eq!(
        fixture
            .index
            .get_note_status("main", &x)
            .await
            .expect("status main post"),
        Some(NoteStatus::Live),
        "X de main (Live) DOIT rester intact — aucun hijack cross-vault"
    );
}
