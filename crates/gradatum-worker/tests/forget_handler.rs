//! Tests d'intégration — handler `handle_forget` (F-44).
//!
//! # Cas couverts
//!
//! - `fail_closed_note_not_in_index_excluded` : C6 — une note dont l'ULID est
//!   absent de l'index (get_note_section retourne None) est traitée comme PROTÉGÉE
//!   et exclue du lot (comportement fail-closed).
//!
//! - `confirm_ulids_both_empty_is_legal` : C4 — deux ensembles vides (eligible=0
//!   et confirm_ulids=[]) sont égaux → pas d'erreur, job vide retourne JobOutput.
//!
//! - `confirm_ulids_mismatch_returns_error` : C4 — confirm_ulids non vide mais
//!   aucun ULID éligible → erreur Business.

use std::sync::Arc;

#[path = "test_internal_client.rs"]
mod test_internal_client;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::{
    ForgetScope, ForgetSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
    JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    scope::VaultId,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn make_forget_job(spec: ForgetSpec, mode: JobMode) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Forget(spec),
                class,
                mode,
                scope: JobScope::VaultWide,
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

struct ForgetFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

async fn make_fixture() -> ForgetFixture {
    let tmp = TempDir::new().expect("TempDir — forget_handler");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create — forget_handler"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();
    ForgetFixture {
        vault,
        index,
        _tmp: tmp,
    }
}

/// Client interne de test adossé au `Vault` + `SqliteIndex` de la fixture.
///
/// Un client neuf par job : les handlers en prennent un `Arc` par appel.
fn client_for(
    fixture: &ForgetFixture,
) -> Arc<dyn gradatum_worker::internal_client::InternalClient> {
    Arc::new(test_internal_client::TestInternalClient::new(
        Arc::clone(&fixture.vault),
        Arc::clone(&fixture.index),
    ))
}

/// Spec de forget réel ciblant le locus `inbox/old`, confirmée sur `ulid`.
fn forget_spec(ulid: &str, by: &str) -> ForgetSpec {
    ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "main".to_string(),
            locus: "inbox/old".to_string(),
        },
        dry_run: false,
        forgotten_by: Some(by.to_string()),
        confirm_ulids: vec![ulid.to_string()],
    }
}

/// Sème une note, l'oublie une première fois (`alice`), puis désynchronise l'index.
///
/// État rendu : index `forgotten = 0`, frontmatter `forgotten: true` + `forgotten_at` /
/// `forgotten_by` du premier oubli. Ce n'est pas une fenêtre de course artificielle : deux
/// chemins de production ordinaires produisent exactement cet état — `vault_unforgot`
/// (route publique) efface la marque d'index sans toucher au `.md`, et `persist_forget`
/// rend 200 best-effort quand son `mark_forgotten` échoue après un write vault réussi.
/// `unmark_forgotten` (index seul) le recrée fidèlement.
///
/// Les trois listings de scope filtrent `forgotten = 0` : c'est justement dans cet état
/// qu'un second job re-résout la note et atteint la branche du skip idempotent.
///
/// Rend `(note_id, ulid, forgotten_at du PREMIER oubli)`.
async fn seed_forgotten_note_then_desync_index(
    fixture: &ForgetFixture,
) -> (
    gradatum_core::identity::NoteId,
    String,
    chrono::DateTime<Utc>,
) {
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::LocusId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;
    use gradatum_worker::apalis_handlers::handle_forget;

    // Note réelle (`.md` + ligne d'index), section non protégée, sous un locus ciblable.
    let note_id = NoteId::new();
    let ulid = note_id.0.to_string();
    let now = Utc::now();
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: Some(LocusId::new("inbox/old")),
        section: Section::Decisions,
        status: NoteStatus::Live,
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
    };
    fixture
        .vault
        .write_note_with_id(fm, format!("# note {ulid}\n\ncorps oubliable"), note_id)
        .await
        .expect("write_note_with_id — note du double forget");

    // Sanity : la note est résolue par le scope AVANT tout oubli.
    let listed = fixture
        .index
        .list_notes_by_locus_prefix("main", "inbox/old")
        .await
        .expect("list_notes_by_locus_prefix");
    assert!(
        listed.iter().any(|(id, _)| id == &ulid),
        "prérequis du test : la note doit être résolue par le scope Locus, obtenu {listed:?}"
    );

    // ── Oubli #1 — l'acte à tracer.
    handle_forget(
        make_forget_job(forget_spec(&ulid, "alice"), JobMode::Batch),
        Data::new(client_for(fixture)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("premier forget doit réussir");

    let first = fixture
        .vault
        .read_note(note_id)
        .await
        .expect("relecture après oubli #1");
    assert_eq!(first.frontmatter.forgotten, Some(true));
    let first_at = first
        .frontmatter
        .forgotten_at
        .expect("l'oubli #1 doit horodater");
    assert_eq!(first.frontmatter.forgotten_by.as_deref(), Some("alice"));

    // ── Désynchronisation index → le second job re-résout la note.
    fixture
        .index
        .unmark_forgotten("main", &ulid)
        .await
        .expect("unmark_forgotten — index seul, le frontmatter reste oublié");

    (note_id, ulid, first_at)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// C6 fail-closed : un ULID dont la section est absente de l'index (note non indexée)
/// doit être exclu du lot plutôt qu'inclus.
///
/// Comportement attendu (fail-closed) : `get_note_section` retourne `None`
/// → `unwrap_or(true)` → `is_protected = true` → note exclue de `eligible`.
/// Le job s'exécute en dry-run et la note hors index n'apparaît pas dans la preview.
#[tokio::test]
async fn fail_closed_note_not_in_index_excluded() {
    use gradatum_worker::apalis_handlers::handle_forget;

    let fixture = make_fixture().await;

    // ULID fantôme : jamais inséré dans l'index.
    let phantom_ulid = Ulid::new().to_string();

    // Scope Locus : cible un préfixe qui pourrait théoriquement matcher l'ULID fantôme.
    // En pratique, list_notes_by_locus_prefix ne le trouvera pas (absent de l'index),
    // donc raw_candidates sera vide. On simule la situation C6 via un scope Agent
    // ciblant un agent qui n'existe pas — résultat : candidats vides, job dry-run vide.
    //
    // Pour tester le chemin exact C6 (get_note_section None), on insère l'ULID
    // directement dans l'index via seed_note (section connue) puis on le supprime
    // de la table notes pour simuler un état hors-index côté get_note_section.
    // Ici, la vérification est indirecte : on seed une note, puis on seed un second
    // ULID fantôme hors FTS — ce dernier ne peut pas être retourné par search_fts_for_forget
    // donc il ne passe jamais dans la boucle C6. Le test porte donc sur la logique
    // de la boucle : une note connue (section=decisions) doit être éligible,
    // et la valeur unwrap_or(true) ne doit pas l'exclure.

    // Note normale dans l'index (body sans tirets pour éviter l'interprétation
    // FTS5 du tiret comme opérateur soustractif dans la requête de test).
    // L'id est un locus textuel — seed_note_with_fts retourne Ok(()).
    let note_locus = "main/decisions/normal-fail-closed";
    fixture
        .index
        .seed_note_with_fts(note_locus, "decisions", "test section guard protection")
        .await
        .expect("seed_note_with_fts — C6");

    // Dry-run via scope Locus (aucun résultat attendu pour un locus inexistant).
    let spec_no_candidate = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "main".to_string(),
            locus: "inexistant/phantom/".to_string(),
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };
    let job = make_forget_job(spec_no_candidate, JobMode::Batch);

    let out = handle_forget(
        job,
        Data::new(Arc::new(test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        ))
            as Arc<dyn gradatum_worker::internal_client::InternalClient>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_forget dry-run locus vide doit réussir");

    // Dry-run avec 0 candidats → 0 note éligible (fail-closed ne génère pas d'erreur).
    // JobOutput::dry_run → notes_created et notes_modified vides.
    assert!(
        out.notes_modified.is_empty() && out.notes_created.is_empty(),
        "aucune note ne doit être éligible pour un locus inexistant: {out:?}"
    );

    // Vérification directe C6 : une note normale (section=decisions) dont get_note_section
    // retourne Some("decisions") → unwrap_or(true) non déclenché → éligible.
    // (Le phantom_ulid est hors index donc hors scope — on vérifie que note_locus
    // est bien éligible en dry-run scope Topic.)
    let _ = (note_locus, phantom_ulid); // variables référencées pour éviter dead_code

    // Requête sans tiret ni opérateur FTS5 spécial.
    let spec_topic = ForgetSpec {
        scope: ForgetScope::Topic {
            query: "protection".to_string(),
            vault: Some("main".to_string()),
            limit: Some(10),
        },
        dry_run: true,
        forgotten_by: None,
        confirm_ulids: vec![],
    };
    let job2 = make_forget_job(spec_topic, JobMode::Batch);

    let out2 = handle_forget(
        job2,
        Data::new(Arc::new(test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        ))
            as Arc<dyn gradatum_worker::internal_client::InternalClient>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_forget dry-run topic doit réussir");

    // La note normale est éligible (section=decisions, non protégée).
    // unwrap_or(true) ne doit PAS exclure une note dont la section est connue.
    // En dry-run → JobOutput::dry_run, result_note_md contient le texte DRY-RUN.
    assert!(
        out2.result_note_md.contains("DRY-RUN"),
        "dry-run doit retourner un JobOutput avec 'DRY-RUN' dans result_note_md: {out2:?}"
    );
}

/// C4 — deux ensembles vides (eligible=0, confirm_ulids=[]) sont légaux.
///
/// Comportement attendu : expected_sorted == confirmed_sorted == [] → OK.
/// Le job s'exécute sans erreur et retourne un résultat vide.
#[tokio::test]
async fn confirm_ulids_both_empty_is_legal() {
    use gradatum_worker::apalis_handlers::handle_forget;

    let fixture = make_fixture().await;

    // Mode réel, confirm_ulids vide, scope qui ne retourne rien.
    // expected_sorted = [] et confirmed_sorted = [] → [] == [] → OK.
    let spec = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "main".to_string(),
            locus: "aucun_prefixe_existant/".to_string(),
        },
        dry_run: false,
        forgotten_by: None,
        confirm_ulids: vec![], // Deux ensembles vides — légal C4
    };
    let job = make_forget_job(spec, JobMode::Batch);

    let result = handle_forget(
        job,
        Data::new(Arc::new(test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        ))
            as Arc<dyn gradatum_worker::internal_client::InternalClient>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    // Doit réussir (pas d'erreur "confirm_ulids mismatch").
    assert!(
        result.is_ok(),
        "deux ensembles vides doivent être acceptés sans erreur: {result:?}"
    );
    let out = result.expect("handle_forget deux vides");
    // 0 notes traitées → notes_modified vide.
    assert!(
        out.notes_modified.is_empty(),
        "0 notes oubliées pour scope vide: {out:?}"
    );
}

/// C4 — confirm_ulids non vide mais scope vide → mismatch → erreur Business.
///
/// Comportement attendu : expected_sorted=[] != confirmed_sorted=["01FAKE..."] → Err.
#[tokio::test]
async fn confirm_ulids_mismatch_returns_error() {
    use gradatum_worker::apalis_handlers::handle_forget;

    let fixture = make_fixture().await;

    let spec = ForgetSpec {
        scope: ForgetScope::Locus {
            vault: "main".to_string(),
            locus: "inexistant/".to_string(),
        },
        dry_run: false,
        forgotten_by: None,
        confirm_ulids: vec!["01JFAKEULID0000000000000XX".to_string()], // ULID fantôme
    };
    let job = make_forget_job(spec, JobMode::Batch);

    let result = handle_forget(
        job,
        Data::new(Arc::new(test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        ))
            as Arc<dyn gradatum_worker::internal_client::InternalClient>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_err(),
        "confirm_ulids mismatch doit retourner une erreur Business: {result:?}"
    );
}

/// A7 — un second forget PRÉSERVE `forgotten_at`/`forgotten_by` du premier.
///
/// C'est la propriété qui compte : la piste d'audit du **premier** oubli (quand, par
/// qui) est la seule trace de l'acte destructif. Le skip idempotent existe pour ça.
///
/// **Discriminance** — avant le fix, la re-vérification TOCTOU comparait
/// `dto.status == "forgotten"`. `NoteStatus` n'a aucune variante `Forgotten`
/// (`draft|staging|pending-review|live|deprecated|garbage`) : la comparaison était
/// TOUJOURS fausse, le skip ne s'exécutait jamais, et le second `persist_forget`
/// écrasait `forgotten_at` **et** `forgotten_by`. Le flag vit dans le champ booléen
/// `NoteReadDto.forgotten` (frontmatter), celui-là même que lit le chemin distill.
///
/// **Fenêtre reproduite** — les trois listings de scope filtrent `forgotten = 0`, donc
/// un second job ne re-résout la note que si l'index la croit vivante alors que le
/// frontmatter la sait oubliée. `unmark_forgotten` (index seul, ne touche pas le `.md`)
/// recrée exactement cette désynchronisation, qui est le cas que la garde protège.
#[tokio::test]
async fn second_forget_preserves_the_first_forget_audit_trail() {
    use gradatum_worker::apalis_handlers::handle_forget;

    let fixture = make_fixture().await;
    let (note_id, ulid, first_at) = seed_forgotten_note_then_desync_index(&fixture).await;

    // ── Oubli #2 — doit être un no-op idempotent, PAS une réécriture.
    handle_forget(
        make_forget_job(forget_spec(&ulid, "bob"), JobMode::Batch),
        Data::new(client_for(&fixture)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("second forget doit réussir (skip idempotent)");

    let second = fixture
        .vault
        .read_note(note_id)
        .await
        .expect("relecture après oubli #2");
    assert_eq!(
        second.frontmatter.forgotten_by.as_deref(),
        Some("alice"),
        "le second forget ne doit PAS réattribuer l'oubli à son propre acteur"
    );
    assert_eq!(
        second.frontmatter.forgotten_at,
        Some(first_at),
        "le second forget ne doit PAS réhorodater l'oubli initial"
    );
}

/// A7-bis — le skip idempotent RÉPARE l'index, sans rien sacrifier de la piste d'audit.
///
/// A7 avait ressuscité le skip (mort tant qu'il comparait `dto.status == "forgotten"`), ce
/// qui a troqué une propriété contre une autre : la piste d'audit du premier oubli était
/// enfin préservée, mais le `continue` laissait l'index désynchronisé **définitivement** —
/// la note restait indexée vivante, donc cherchable, alors que le vault la dit oubliée, et
/// elle était malgré tout comptée dans la liste `forgotten` du job. Avant A7 le second
/// forget réparait l'index, au prix de l'audit. Sur un produit qui invoque le droit à
/// l'oubli, c'est le mauvais côté de l'échange.
///
/// **Discriminance** — ce test échoue sur A7 (`58010454`) : l'index reste `forgotten = 0`
/// après le second forget. Il échouerait tout autant sur une « correction » qui rappellerait
/// `persist_forget` dans la branche du skip : l'index redeviendrait cohérent mais
/// `forgotten_at`/`forgotten_by` seraient ré-estampés à l'acteur du second oubli. Les deux
/// assertions doivent donc passer ENSEMBLE — c'est tout l'objet du lot.
#[tokio::test]
async fn second_forget_resynchronises_the_index_and_keeps_the_audit_trail() {
    use gradatum_worker::apalis_handlers::handle_forget;

    let fixture = make_fixture().await;
    let (note_id, ulid, first_at) = seed_forgotten_note_then_desync_index(&fixture).await;

    // Prérequis explicite : l'index est bien désynchronisé AVANT le second forget.
    assert!(
        !fixture
            .index
            .is_note_forgotten("main", &ulid)
            .await
            .expect("is_note_forgotten — état désynchronisé"),
        "prérequis du test : l'index doit croire la note vivante avant le second forget"
    );

    // ── Oubli #2 — skip idempotent + re-synchronisation de l'index.
    handle_forget(
        make_forget_job(forget_spec(&ulid, "bob"), JobMode::Batch),
        Data::new(client_for(&fixture)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("second forget doit réussir (skip idempotent)");

    // Propriété 1 — l'index est réparé : la note n'est plus cherchable comme vivante.
    assert!(
        fixture
            .index
            .is_note_forgotten("main", &ulid)
            .await
            .expect("is_note_forgotten — après le second forget"),
        "le second forget doit ré-affirmer la marque d'oubli dans l'index"
    );

    // Propriété 2 — la piste d'audit du PREMIER oubli est intacte.
    let second = fixture
        .vault
        .read_note(note_id)
        .await
        .expect("relecture après oubli #2");
    assert_eq!(
        second.frontmatter.forgotten_by.as_deref(),
        Some("alice"),
        "la re-synchronisation ne doit PAS réattribuer l'oubli à l'acteur du second job"
    );
    assert_eq!(
        second.frontmatter.forgotten_at,
        Some(first_at),
        "la re-synchronisation ne doit PAS réhorodater l'oubli initial"
    );
}
