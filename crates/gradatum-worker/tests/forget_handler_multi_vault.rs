//! A2-bis — `handle_forget` en **multi-vault** (`MultiTenantCfg { enabled: true }`).
//!
//! # Le trou fermé
//!
//! `handle_forget` avait DEUX sources de vérité de vault :
//!
//! ```text
//! listing (ForgetScope.vault)  →  get_note(ULID)  →  persist_forget(vault_id)
//!        (scopé vault-b)           (NON scopé → main)      (scopé vault-b)
//! ```
//!
//! `InternalClient::get_note` était la dernière lecture de note non scopée du trait ;
//! côté serveur `resolve_read_back_reader` fait `vault.unwrap_or("main")`, donc la note
//! LUE venait de `main` tandis que la note MUTÉE venait du vault du job. Les trois appels
//! de `handle_forget` portaient des **gardes** : contrôle de section protégée,
//! re-vérification TOCTOU, et lecture de la section transmise à `persist_forget`. Une
//! garde qui juge une autre note que celle qu'elle protège ne protège rien.
//!
//! Le lot rend `get_note` scopé (`?vault_id=…`) et fait de `vault_id` — issu de
//! `JobSpec.scope` — la **source unique** du listing ET de la mutation ;
//! `ForgetScope.vault` est rétrogradé en assertion de cohérence.
//!
//! # Couverture
//!
//! Sur les 13 fichiers de test du worker, un seul tournait à `enabled: true`
//! (`purge_toctou_vault_scoped.rs`) ; `forget_handler.rs` était intégralement à OFF —
//! c'est ce trou de couverture qui a laissé passer la classe.
//!
//! # Limite du harnais
//!
//! Un seul vault **physique** (`main`). Ce qui distingue les vaults est l'INDEX, dont la
//! clé est composite `(vault_id, id)` (migration 0032) — la même source que les listings.
//! `TestInternalClient::get_note` lit donc l'index pour les métadonnées scopées.

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::{
    ForgetScope, ForgetSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
    JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{MultiTenantCfg, handle_forget};
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
    let tmp = TempDir::new().expect("TempDir — forget multi-vault");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create — forget multi-vault"),
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

/// Job `Forget` scopé `JobScope::Vault(vault_id)`.
fn make_scoped_forget_job(vault_id: &str, spec: ForgetSpec) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Human;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Forget(spec),
                class,
                mode: JobMode::Batch,
                scope: JobScope::Vault(vault_id.to_string()),
                priority: JobPriority::Normal,
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

fn client_for(fixture: &Fixture) -> Arc<dyn InternalClient> {
    Arc::new(TestInternalClient::new(
        Arc::clone(&fixture.vault),
        Arc::clone(&fixture.index),
    )) as Arc<dyn InternalClient>
}

// ─────────────────────────────────────────────────────────────────────────────
// Test — la garde « section protégée » juge la note du BON vault
// ─────────────────────────────────────────────────────────────────────────────

/// ULID X collisionné : `decisions` (oubliable) dans `vault-b`, `council` (PROTÉGÉE)
/// dans `main`.
///
/// Le job de forget de `vault-b` DOIT juger X **dans `vault-b`** → section `decisions`
/// → éligible. Il ne doit pas consulter `main`, où l'homonyme est protégé.
///
/// **Discriminance** — avec l'appel non scopé d'avant le lot (`get_note(ULID)`, résolu
/// sur `main` par le serveur), la section lue est `council`, donc `PROTECTED_FORGET`,
/// donc `is_protected = true` : X est exclu et la preview annonce **0 éligible**.
/// L'assertion `1 eligible note(s)` tombe. Le sens du défaut est le pire des deux : la
/// commande rendait `Ok` en n'oubliant rien, sans le dire.
#[tokio::test]
async fn forget_judges_protected_section_in_the_job_vault_not_main() {
    let fixture = make_fixture().await;

    let note_id = NoteId::new();
    let x = note_id.0.to_string();

    // ── vault-b : X en section `decisions` (NON protégée) sous le locus ciblé.
    fixture
        .index
        .seed_note_with_fts_vault(
            &x,
            "vault-b",
            "decisions",
            Some("inbox/old/note"),
            "corps vault-b oubliable",
        )
        .await
        .expect("seed vault-b");

    // ── main : X en section `council` (PROTECTED_FORGET) — l'homonyme piège.
    fixture
        .vault
        .write_note_with_id(
            make_frontmatter("main", Section::Council, NoteStatus::Live),
            format!("# main {x}\n\ncorps main protege"),
            note_id,
        )
        .await
        .expect("write_note_with_id main");

    // Sanity : les deux lignes coexistent bien, avec des sections différentes.
    let seen_b = fixture
        .index
        .get_note("vault-b", &x)
        .await
        .expect("index vault-b")
        .expect("ligne vault-b");
    assert_eq!(seen_b.section, "decisions");
    let seen_main = fixture
        .index
        .get_note("main", &x)
        .await
        .expect("index main")
        .expect("ligne main");
    assert_eq!(seen_main.section, "council");

    // ── Dry-run scopé vault-b, multi-tenant ON.
    // Dry-run : la mutation `.md` est mono-vault dans ce harnais, la preview suffit à
    // prouver quel vault a été JUGÉ.
    let spec = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "vault-b".to_string(),
            locus: "inbox/old/".to_string(),
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };

    let out = handle_forget(
        make_scoped_forget_job("vault-b", spec),
        Data::new(client_for(&fixture)),
        Data::new(MultiTenantCfg { enabled: true }),
    )
    .await
    .expect("handle_forget vault-b ne doit pas échouer");

    assert!(
        out.result_note_md.contains("1 eligible note(s)"),
        "X doit être jugé dans vault-b (section decisions → éligible), pas dans main \
         (section council → protégée) : {}",
        out.result_note_md
    );
    assert!(
        !out.result_note_md.contains("1 excluded"),
        "aucune exclusion attendue : la section protégée de `main` ne concerne pas ce job : {}",
        out.result_note_md
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test — invariant « un job = exactement un vault »
// ─────────────────────────────────────────────────────────────────────────────

/// Un `ForgetScope` désignant un autre vault que celui du job est refusé terminalement.
///
/// Avant le lot, ce job listait dans `main` et mutait dans `vault-b` — deux vaults, un
/// seul job, aucune erreur.
#[tokio::test]
async fn forget_rejects_a_scope_pointing_at_another_vault() {
    let fixture = make_fixture().await;

    let spec = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "main".to_string(), // ≠ vault du job
            locus: "inbox/old/".to_string(),
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };

    let result = handle_forget(
        make_scoped_forget_job("vault-b", spec),
        Data::new(client_for(&fixture)),
        Data::new(MultiTenantCfg { enabled: true }),
    )
    .await;

    let err = result.expect_err("un scope divergent doit être refusé");
    assert!(
        format!("{err}").contains("scope vault mismatch"),
        "l'erreur doit nommer la divergence de vault : {err}"
    );
}

/// Un `ForgetScope::Agent` multi-vault est refusé : le fan-out (N vaults ⇒ N jobs)
/// appartient au site d'enqueue, pas au handler.
#[tokio::test]
async fn forget_rejects_a_multi_vault_agent_scope() {
    let fixture = make_fixture().await;

    let spec = ForgetSpec {
        scope: ForgetScope::Agent {
            agent_id: "curator".to_string(),
            vaults: vec!["vault-b".to_string(), "main".to_string()],
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };

    let result = handle_forget(
        make_scoped_forget_job("vault-b", spec),
        Data::new(client_for(&fixture)),
        Data::new(MultiTenantCfg { enabled: true }),
    )
    .await;

    let err = result.expect_err("un Agent multi-vault doit être refusé");
    assert!(
        format!("{err}").contains("multi-vault"),
        "l'erreur doit nommer le fan-out manquant : {err}"
    );
}

/// Le job enqueué par `gradatum-admin vault forget` (`JobScope::Vault(tenant)`) est
/// exécutable à ON — c'était le bloquant deploy : `VaultWide` en dur était refusé
/// terminalement par `resolve_job_vault` dès le flag levé.
#[tokio::test]
async fn forget_runs_on_a_vault_scoped_job_at_multi_tenant_on() {
    let fixture = make_fixture().await;

    let spec = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "vault-b".to_string(),
            locus: "aucun/prefixe/existant/".to_string(),
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };

    let out = handle_forget(
        make_scoped_forget_job("vault-b", spec),
        Data::new(client_for(&fixture)),
        Data::new(MultiTenantCfg { enabled: true }),
    )
    .await
    .expect("un job Vault(v) doit être exécutable à ON");

    assert!(
        out.result_note_md.contains("no eligible note"),
        "preview vide attendue pour un locus inexistant : {}",
        out.result_note_md
    );
}
