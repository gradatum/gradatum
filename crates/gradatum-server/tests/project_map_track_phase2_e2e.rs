//! Preuves adossées au REGISTRE du rôle `track`.
//!
//! Vrai chemin, aucun mock : `Vault` réel (TempDir) dont l'index SQLite est PARTAGÉ avec
//! `state.search`, `SqliteQueueStore` réel pour l'enqueue. Les cartes de structure sont
//! semées via `vault.write_note` (qui dérive `role_kind` à l'indexation, colonne 0043
//! reconnaissant désormais ROADMAP/BACKLOG), puis résolues par les contrôles write-path.
//!
//! Couvre les preuves qui exigent le registre (les preuves PURES vivent dans les tests
//! unitaires de `gradatum-core::project_map`) :
//! - #1  une carte pointant un BACKLOG (ou une ROADMAP) est ACCEPTÉE ;
//! - #4  une cible inexistante est refusée ;
//! - #5  une cible qui est une carte de travail est refusée ;
//! - #6  un `project` divergent est refusé ;
//! - #8  rétrograder une ROADMAP ayant des enfants est refusé (RESTRICT scopé structure) ;
//! - déliv. 8 : le serveur INJECTE le track dérivé du `[[version:]]` quand la ROADMAP
//!   existe, et n'injecte RIEN sinon (ne casse pas les écritures avant la création des cartes de structure).

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::QueueStore;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::job::Job;
use gradatum_core::scope::{TenantId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{VaultDowngradeRequest, VaultWriteRequest};
use gradatum_server::api_v1::logic::{
    FeatureWriteAuthority, vault_downgrade_impl, vault_write_impl,
};
use gradatum_server::config::ServerConfig;
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

const ACL_RW: &str = r#"
[[consumer]]
identity = "writer"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// `TrustContext` BearerToken pour `main`, identité `writer` (read+write).
fn writer() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "writer".into(),
        scopes: vec!["read".into(), "write".into()],
        tenant_id: "main".into(),
        jti: None,
    }
}

/// AppState mono-vault : vault réel (Registry) + index partagé (`state.search`) + queue.
async fn build_state() -> (AppState, Arc<Vault>, TempDir) {
    use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};

    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index: Arc<dyn gradatum_core::index::Index> = vault.index().clone();

    let jobs_pool = QueueDb::open_in_memory().await.expect("jobs pool");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(ACL_RW).expect("preset ACL");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_server_config(ServerConfig::default())
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>)
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool);
    state.search = shared_index;

    (state, vault, tmp)
}

fn frontmatter_project_map() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::ProjectMap,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: chrono::Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Sème une carte project-map RÉELLE (indexée : `role_kind` dérivé). Rend son ULID.
async fn seed(vault: &Vault, body: &str) -> String {
    let note = vault
        .write_note(frontmatter_project_map(), body.to_string())
        .await
        .expect("write_note seed");
    note.id.to_string()
}

/// Requête `vault_write` project-map (création externe).
fn write_req(title: &str, body: &str) -> VaultWriteRequest {
    let mut req = VaultWriteRequest::new(title.to_string(), body.to_string());
    req.section_hint = Some("project-map".to_string());
    req.tenant_id = Some(TenantId::new("main"));
    req
}

async fn do_write(state: &AppState, req: VaultWriteRequest) -> Result<(), GradatumError> {
    vault_write_impl(
        state,
        &writer(),
        req,
        "test-req",
        FeatureWriteAuthority::External,
    )
    .await
    .map(|_| ())
}

// ── Preuve #1 — attachement à un BACKLOG (et à une ROADMAP) ACCEPTÉ ─────────────

#[tokio::test]
async fn attach_to_existing_backlog_is_accepted() {
    let (state, vault, _tmp) = build_state().await;
    seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:BACKLOG]] [[version:gradatum/backlog]]",
    )
    .await;

    // Carte de travail (changelog, pas de feature) pointant le BACKLOG.
    let res = do_write(
        &state,
        write_req(
            "Travail backlog",
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/backlog]] \
             [[track:gradatum/backlog]]",
        ),
    )
    .await;
    assert!(
        res.is_ok(),
        "une carte pointant un BACKLOG existant doit être ACCEPTÉE : {res:?}"
    );
}

#[tokio::test]
async fn attach_to_existing_roadmap_is_accepted() {
    let (state, vault, _tmp) = build_state().await;
    seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] [[version:gradatum/2.2.0]] \
         [[visibilite:public]]",
    )
    .await;

    let res = do_write(
        &state,
        write_req(
            "Travail 2.2.0",
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/2.2.0]] \
             [[track:gradatum/2.2.0]]",
        ),
    )
    .await;
    assert!(
        res.is_ok(),
        "une carte pointant une ROADMAP existante doit être ACCEPTÉE : {res:?}"
    );
}

// ── Preuve #4 — cible inexistante refusée ───────────────────────────────────────

#[tokio::test]
async fn attach_to_nonexistent_target_is_rejected() {
    let (state, _vault, _tmp) = build_state().await;
    let res = do_write(
        &state,
        write_req(
            "Cible fantôme",
            "[[project:gradatum]] [[status:OPEN]] [[kind:FIX]] [[version:gradatum/backlog]] \
             [[track:gradatum/9.9.9]]",
        ),
    )
    .await;
    match res {
        Err(GradatumError::InvalidInput(m)) => assert!(
            m.contains("does not resolve"),
            "cible inexistante → message §3.3 : {m}"
        ),
        other => panic!("une cible inexistante doit être refusée (400) : {other:?}"),
    }
}

// ── Preuve #5 — cible qui est une carte de travail refusée ──────────────────────

#[tokio::test]
async fn attach_to_work_card_target_is_rejected() {
    let (state, vault, _tmp) = build_state().await;
    // Une carte de TRAVAIL porte version 9.9.9 (aucune ROADMAP pour 9.9.9).
    seed(
        &vault,
        "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/9.9.9]]",
    )
    .await;

    let res = do_write(
        &state,
        write_req(
            "Track vers travail",
            "[[project:gradatum]] [[status:OPEN]] [[kind:FIX]] [[version:gradatum/backlog]] \
             [[track:gradatum/9.9.9]]",
        ),
    )
    .await;
    match res {
        Err(GradatumError::InvalidInput(m)) => assert!(
            m.contains("work card"),
            "cible = carte de travail → message dédié : {m}"
        ),
        other => panic!("une cible carte-de-travail doit être refusée (400) : {other:?}"),
    }
}

// ── Preuve #6 — project divergent refusé ────────────────────────────────────────

#[tokio::test]
async fn attach_with_divergent_project_is_rejected() {
    let (state, vault, _tmp) = build_state().await;
    // La ROADMAP gradatum/2.2.0 existe : le rejet vient du MISMATCH projet, pas de l'absence.
    seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] [[version:gradatum/2.2.0]] \
         [[visibilite:public]]",
    )
    .await;

    let res = do_write(
        &state,
        write_req(
            "Carte system infiltrée",
            "[[project:system]] [[status:DONE]] [[kind:FIX]] [[version:system/backlog]] \
             [[track:gradatum/2.2.0]]",
        ),
    )
    .await;
    match res {
        Err(GradatumError::InvalidInput(m)) => assert!(
            m.contains("track project") && m.contains("card project"),
            "project divergent → message dédié : {m}"
        ),
        other => panic!("un project divergent doit être refusé (400) : {other:?}"),
    }
}

// ── Preuve #8 — rétrograder une ROADMAP ayant des enfants refusé (RESTRICT) ──────

#[tokio::test]
async fn downgrade_structure_card_with_children_is_rejected() {
    let (state, vault, _tmp) = build_state().await;
    let roadmap_id = seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] [[version:gradatum/2.2.0]] \
         [[visibilite:public]]",
    )
    .await;
    // Un enfant pointe la ROADMAP via track.
    seed(
        &vault,
        "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/2.2.0]] \
         [[track:gradatum/2.2.0]]",
    )
    .await;

    let req = VaultDowngradeRequest::new(roadmap_id, "test restrict".to_string());
    let res = vault_downgrade_impl(&state, &writer(), req).await;
    match res {
        Err(GradatumError::InvalidInput(m)) => assert!(
            m.contains("RESTRICT") && m.contains("children"),
            "ROADMAP avec enfants → RESTRICT : {m}"
        ),
        other => panic!("rétrograder une ROADMAP avec enfants doit être refusé : {other:?}"),
    }
}

#[tokio::test]
async fn downgrade_structure_card_without_children_is_allowed() {
    let (state, vault, _tmp) = build_state().await;
    let roadmap_id = seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] [[version:gradatum/3.0.0]] \
         [[visibilite:interne]]",
    )
    .await;

    let req = VaultDowngradeRequest::new(roadmap_id, "test no child".to_string());
    let res = vault_downgrade_impl(&state, &writer(), req).await;
    assert!(
        res.is_ok(),
        "une ROADMAP sans enfant doit pouvoir être rétrogradée (réversibilité Phase 3) : {res:?}"
    );
}

#[tokio::test]
async fn downgrade_work_card_even_if_referenced_is_allowed() {
    // Le RESTRICT est SCOPÉ aux cartes de structure : une carte de TRAVAIL référencée
    // (parent/backport) reste rétrogradable (réparation — caveat council).
    let (state, vault, _tmp) = build_state().await;
    let work_id = seed(
        &vault,
        "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/2.2.0]]",
    )
    .await;
    // Une autre carte la référence via parent (pas via track — track ne cible pas F-XX).
    seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:FIX]] [[version:gradatum/2.2.0]] \
         [[parent:F-07]]",
    )
    .await;

    let req = VaultDowngradeRequest::new(work_id, "repair".to_string());
    let res = vault_downgrade_impl(&state, &writer(), req).await;
    assert!(
        res.is_ok(),
        "rétrograder une carte de travail référencée reste permis (RESTRICT scopé structure) : {res:?}"
    );
}

// ── Déliv. 8 — injection serveur du track dérivé du [[version:]] ─────────────────

/// Rend le corps du dernier job Curate enqueue (pour inspecter le corps injecté).
async fn last_curate_body(state: &AppState) -> Option<String> {
    let job = state.job_store.dequeue(None).await.expect("dequeue")?;
    match job.spec.kind {
        Job::Curate(c) => c.body,
        _ => None,
    }
}

#[tokio::test]
async fn server_injects_track_when_roadmap_exists() {
    let (state, vault, _tmp) = build_state().await;
    seed(
        &vault,
        "[[project:gradatum]] [[status:OPEN]] [[kind:ROADMAP]] [[version:gradatum/2.2.0]] \
         [[visibilite:public]]",
    )
    .await;

    // Carte de travail SANS track, avec version dont la ROADMAP existe.
    do_write(
        &state,
        write_req(
            "Sans track explicite",
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/2.2.0]]",
        ),
    )
    .await
    .expect("write accepté");

    let body = last_curate_body(&state)
        .await
        .expect("un job Curate a été enqueue");
    assert!(
        body.contains("[[track:gradatum/2.2.0]]"),
        "le serveur doit injecter le track dérivé du [[version:]] : {body}"
    );
}

#[tokio::test]
async fn server_does_not_inject_track_before_roadmap_exists() {
    // Sécurité de déploiement pré-Phase-3 : aucune ROADMAP ⇒ aucune injection ⇒ l'écriture
    // vivante n'est pas cassée (la carte reste version-only, exportée comme aujourd'hui).
    let (state, _vault, _tmp) = build_state().await;

    do_write(
        &state,
        write_req(
            "Sans ROADMAP en face",
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/2.2.0]]",
        ),
    )
    .await
    .expect("write accepté (pas d'injection, pas de contrôle)");

    let body = last_curate_body(&state)
        .await
        .expect("un job Curate a été enqueue");
    assert!(
        !body.contains("[[track:"),
        "aucune ROADMAP ⇒ aucun track injecté (ne casse pas les écritures pré-Phase-3) : {body}"
    );
}
