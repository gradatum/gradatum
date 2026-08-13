//! Split-brain des READS PUBLICS : `vault_read` / `vault_history` routés par le vault
//! EFFECTIF (finding P1 de revue).
//!
//! ## Le trou (≥2 vaults, flip-blocker)
//!
//! Un précédent correctif a routé 6 read-back internes, mais la classe « read instance-bound via le
//! singleton `state.vault` » était plus large. Trois reads PUBLICS restaient cloués au
//! singleton `main` :
//!
//! - `vault_read_impl` (`state.vault.read_note_by_id`) — le read public principal (MCP
//!   `vault_read`) ;
//! - `vault_history_impl` (`state.vault.history_versions`) ;
//! - `vault_history_get_impl` (`state.vault.history_get`).
//!
//! Ces trois méthodes du `Vault` sont instance-bound (`history_*` construit le chemin
//! `{self.vault_id}/.history/…`, `read_note_by_id` lit le md du handle). À
//! `multi_tenant.enabled = ON`, un principal dont le vault effectif est `vault-b` lisait
//! donc le corps / l'historique de `main` (fuite sur homonyme ULID, ou 404/500 sur read
//! own-vault légitime) — split-brain, flip-blocker.
//!
//! Ce correctif route ces reads via le handle du vault effectif obtenu du registre
//! (`read_back_reader` → `state.vaults.resolve`), fail-closed sur miss (jamais un repli
//! silencieux sur `main`).
//!
//! ## Régime & byte-identical
//!
//! Le routage est **gaté sur `multi_tenant.enabled`** (pattern read-path C2 déjà en
//! place). À OFF le chemin `state.vault` (singleton `main`) est inchangé
//! (byte-identical, un seul vault physique). À ON, la lecture est routée par le vault
//! effectif (dérivé de `effective_tenant`). Ce test exerce le régime ON local au harnais
//! (flip INTERDIT LIVE) via des appels DIRECTS aux `*_impl` publics.
//!
//! Deux `Vault` physiques (`main`, `vault-b`) adossés au MÊME `Arc<SqliteIndex>` (md
//! per-vault sous `<root>/<vault_id>/`, index partagé — topologie cible council
//! `01KXWMCR0N`), registre peuplé des deux handles.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{VaultHistoryGetRequest, VaultHistoryRequest, VaultReadRequest};
use gradatum_server::api_v1::logic::{vault_history_get_impl, vault_history_impl, vault_read_impl};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Preset ACL : `reader` en lecture sur `main/*`, `vault-b/*` et `vault-absent/*` (ce
/// dernier autorise l'ACL mais reste absent du registre → exerce le fail-closed).
const TEST_ACL: &str = r#"
[[consumer]]
identity = "reader"
read_patterns  = ["main/*", "vault-b/*", "vault-absent/*"]
write_patterns = []
"#;

struct Env {
    state: AppState,
    vault_main: Arc<Vault>,
    vault_b: Arc<Vault>,
    _tmp: TempDir,
}

/// Frontmatter minimal ciblant `vault_id` (section non protégée = pas de guard identité).
fn frontmatter_for(vault_id: &str) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section: Section::Feedback,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// TrustContext BearerToken pour `tenant`, identité `reader` (read scope).
fn bearer(tenant: &str) -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "reader".into(),
        scopes: vec!["read".into()],
        tenant_id: tenant.into(),
        jti: None,
    }
}

/// Deux vaults physiques, index partagé, registre peuplé, flag ON.
async fn build_env() -> Env {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("vault");

    // `main` ouvre le pool ; `vault-b` le RÉUTILISE (index partagé, md distinct).
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index = Arc::clone(vault_main.index());
    let vault_b = Arc::new(
        Vault::with_shared_index(
            &root,
            VaultId::parse("vault-b").expect("vault-b valide"),
            Arc::clone(&shared_index),
        )
        .await
        .expect("Vault::with_shared_index vault-b"),
    );

    let registry = VaultRegistry::new();
    registry
        .insert(VaultId::new("main"), Arc::clone(&vault_main))
        .expect("insert main");
    registry
        .insert(VaultId::new("vault-b"), Arc::clone(&vault_b))
        .expect("insert vault-b");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    // Flip local au harnais — active le routage read par vault effectif (ON-only).
    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig { enabled: true },
        ..ServerConfig::default()
    };

    let facade: Arc<dyn gradatum_vault::Registry> = Arc::clone(&vault_main) as _;
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(facade)
        .with_server_config(cfg);
    let search_index: Arc<dyn gradatum_core::index::Index> = shared_index.clone();
    state.search = search_index;
    state.vaults = Arc::new(registry);

    Env {
        state,
        vault_main,
        vault_b,
        _tmp: tmp,
    }
}

/// Écrit une note homonyme (même ULID) dans les DEUX vaults avec des CORPS distincts.
async fn seed_homonym(env: &Env, id: NoteId) {
    env.vault_main
        .write_note_with_id(frontmatter_for("main"), "# Note\n\nCORPS-MAIN".into(), id)
        .await
        .expect("write main");
    env.vault_b
        .write_note_with_id(
            frontmatter_for("vault-b"),
            "# Note\n\nCORPS-VAULT-B".into(),
            id,
        )
        .await
        .expect("write vault-b");
}

// ── vault_read ────────────────────────────────────────────────────────────────

/// Split-brain fermé : `vault_read` d'un principal `vault-b` lit le CORPS de `vault-b`,
/// jamais celui de `main` (homonyme). Avant le fix (singleton `main`) → corps de `main`.
#[tokio::test]
async fn vault_read_reads_target_vault_body_not_main() {
    let env = build_env().await;
    let id = NoteId::new();
    seed_homonym(&env, id).await;

    let mut req = VaultReadRequest::new(id.0.to_string());
    req.tenant_id = Some("vault-b".into());
    let resp = vault_read_impl(&env.state, &bearer("vault-b"), req)
        .await
        .expect("vault_read vault-b → Ok");

    assert!(
        resp.content.contains("CORPS-VAULT-B"),
        "vault_read ciblant vault-b DOIT lire le corps de vault-b, obtenu : {:?}",
        resp.content
    );
    assert!(
        !resp.content.contains("CORPS-MAIN"),
        "vault_read ciblant vault-b ne DOIT JAMAIS renvoyer le corps de main (split-brain), obtenu : {:?}",
        resp.content
    );
}

/// Parité : un principal `main` lit le corps de `main` — le routage n'a pas cassé le
/// chemin nominal.
#[tokio::test]
async fn vault_read_main_reads_main_body() {
    let env = build_env().await;
    let id = NoteId::new();
    seed_homonym(&env, id).await;

    let mut req = VaultReadRequest::new(id.0.to_string());
    req.tenant_id = Some("main".into());
    let resp = vault_read_impl(&env.state, &bearer("main"), req)
        .await
        .expect("vault_read main → Ok");

    assert!(
        resp.content.contains("CORPS-MAIN"),
        "vault_read ciblant main DOIT lire le corps de main, obtenu : {:?}",
        resp.content
    );
    assert!(
        !resp.content.contains("CORPS-VAULT-B"),
        "vault_read main ne DOIT PAS renvoyer le corps de vault-b, obtenu : {:?}",
        resp.content
    );
}

/// Fail-closed : un vault effectif absent du registre → `VaultNotFound`, JAMAIS un repli
/// sur `main` (même si `main` porte l'ULID).
#[tokio::test]
async fn vault_read_unknown_vault_is_fail_closed() {
    let env = build_env().await;
    let id = NoteId::new();
    // `main` porte l'ULID : avant le fix, le singleton renverrait Ok(corps main).
    env.vault_main
        .write_note_with_id(frontmatter_for("main"), "# Note\n\nCORPS-MAIN".into(), id)
        .await
        .expect("write main");

    let mut req = VaultReadRequest::new(id.0.to_string());
    req.tenant_id = Some("vault-absent".into());
    let err = vault_read_impl(&env.state, &bearer("vault-absent"), req)
        .await
        .expect_err("vault absent du registre → fail-closed (jamais Ok)");

    assert!(
        matches!(err, GradatumError::VaultNotFound(_)),
        "vault absent → VaultNotFound (jamais un repli sur main), obtenu : {err:?}"
    );
}

// ── vault_history / vault_history_get ──────────────────────────────────────────

/// `vault_history` compte les snapshots du vault EFFECTIF. vault-b a 1 snapshot (2
/// écritures à corps différents), main en a 0 (1 écriture). Avant le fix (singleton
/// main), le count serait 0 (split-brain).
#[tokio::test]
async fn vault_history_versions_scoped_to_target_vault() {
    let env = build_env().await;
    let id = NoteId::new();
    // vault-b : 2 écritures à corps distinct → 1 snapshot CoW.
    env.vault_b
        .write_note_with_id(frontmatter_for("vault-b"), "# H\n\nHIST-V1-B".into(), id)
        .await
        .expect("vault-b v1");
    env.vault_b
        .write_note_with_id(frontmatter_for("vault-b"), "# H\n\nHIST-V2-B".into(), id)
        .await
        .expect("vault-b v2");
    // main : 1 écriture → 0 snapshot.
    env.vault_main
        .write_note_with_id(frontmatter_for("main"), "# H\n\nHIST-MAIN".into(), id)
        .await
        .expect("main v1");

    let mut req = VaultHistoryRequest::new(id.0.to_string());
    req.tenant_id = Some("vault-b".into());
    let resp = vault_history_impl(&env.state, &bearer("vault-b"), req)
        .await
        .expect("vault_history vault-b → Ok");

    assert_eq!(
        resp.count, 1,
        "vault-b a 1 snapshot ; le singleton main en aurait 0 (split-brain), versions={:?}",
        resp.versions
    );
}

/// `vault_history_get` lit le CORPS du snapshot du vault EFFECTIF. Avant le fix, le
/// handle `main` (0 snapshot) ferait échouer la lecture du ts vault-b.
#[tokio::test]
async fn vault_history_get_reads_target_vault_snapshot() {
    let env = build_env().await;
    let id = NoteId::new();
    env.vault_b
        .write_note_with_id(frontmatter_for("vault-b"), "# H\n\nHIST-V1-B".into(), id)
        .await
        .expect("vault-b v1");
    env.vault_b
        .write_note_with_id(frontmatter_for("vault-b"), "# H\n\nHIST-V2-B".into(), id)
        .await
        .expect("vault-b v2");
    env.vault_main
        .write_note_with_id(frontmatter_for("main"), "# H\n\nHIST-MAIN".into(), id)
        .await
        .expect("main v1");

    // ts réel du snapshot vault-b (source de vérité = handle inhérent).
    let versions = env
        .vault_b
        .history_versions(id)
        .await
        .expect("history_versions vault-b");
    let ts = *versions.first().expect("au moins 1 snapshot vault-b");

    let mut req = VaultHistoryGetRequest::new(id.0.to_string(), ts);
    req.tenant_id = Some("vault-b".into());
    let resp = vault_history_get_impl(&env.state, &bearer("vault-b"), req)
        .await
        .expect("vault_history_get vault-b → Ok (avant le fix, main n'a pas ce snapshot)");

    assert!(
        resp.body.contains("HIST-V1-B"),
        "le snapshot lu DOIT être celui de vault-b, obtenu : {:?}",
        resp.body
    );
    assert!(
        !resp.body.contains("HIST-MAIN"),
        "ne DOIT JAMAIS renvoyer un snapshot de main (split-brain), obtenu : {:?}",
        resp.body
    );
}
