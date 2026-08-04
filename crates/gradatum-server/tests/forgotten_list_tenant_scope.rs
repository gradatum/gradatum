//! Isolation cross-tenant de `GET /api/v1/vault/forgotten` (`vault_forgotten_list`).
//!
//! ## Le trou (P0-8, flip-blocker sécurité)
//!
//! Le handler clouait le vault à la constante `"main"` (hardcode inconditionnel), en
//! ignorant `trust.tenant_id()` ET `multi_tenant.enabled` : l'ACL était évaluée sur le
//! locus fixe `main/main` et l'index listé pour le vault `main`.
//!
//! `vault_forgotten_list` est un READ own-vault (aucun paramètre de vault cible) — sibling
//! exact de `vault_status_impl`, déjà migré. À `multi_tenant.enabled = ON`, un principal
//! `tenant ≠ main` porteur d'un grant ACL couvrant `main/*` (modélisé ici par le grant
//! global de l'exploit) listait les `forgotten_by` (PII, cf `SECURITY.md`) des notes de
//! `main` → fuite cross-tenant.
//!
//! ## Le fix
//!
//! Résolution du vault EFFECTIF depuis le JWT (`effective_tenant`), GATÉE sur
//! `multi_tenant.enabled` (pattern read-path aligné sur `vault_status_impl`). À OFF le
//! chemin `"main"` est inchangé (byte-identical). À ON, un tenant ne voit QUE ses propres
//! notes oubliées.
//!
//! Deux `Vault` physiques (`main`, `vault-b`) adossés au MÊME `Arc<SqliteIndex>` (topologie
//! cible council `01KXWMCR0N`) ; flag ON local au harnais (flip INTERDIT LIVE). Appels
//! DIRECTS au handler public.

#![allow(dead_code)]

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_server::api_v1::forget::{ForgottenListQuery, vault_forgotten_list};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;

/// PII marker semé dans le `forgotten_by` d'une note de `main` — ne DOIT jamais fuiter
/// vers un principal `vault-b`.
const MAIN_PII: &str = "main-secret-pii";
const VAULT_B_ACTOR: &str = "vaultb-actor";

/// Preset ACL : `reader` en lecture sur `main/*` ET `vault-b/*`. Le grant cross-vault
/// (`main/*` pour un principal `vault-b`) modélise la précondition de l'exploit : ACL
/// `Allow` sur le locus de `main` — la seule barrière restante est le scope de l'index.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "reader"
read_patterns  = ["main/*", "vault-b/*"]
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

/// Deux vaults physiques, index partagé, registre peuplé, flag multi_tenant ON.
async fn build_env() -> Env {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("vault");

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

/// Écrit une note dans `vault_id`, puis la marque `forgotten` avec l'acteur `by`.
async fn seed_forgotten(env: &Env, vault_id: &str, by: &str) -> String {
    let id = NoteId::new();
    let vault = match vault_id {
        "main" => &env.vault_main,
        "vault-b" => &env.vault_b,
        other => panic!("vault inconnu dans le harnais : {other}"),
    };
    vault
        .write_note_with_id(frontmatter_for(vault_id), "# Note\n\nCorps.".into(), id)
        .await
        .expect("write_note_with_id");
    let note_id = id.0.to_string();
    env.state
        .search
        .mark_forgotten(vault_id, &note_id, Some(by))
        .await
        .expect("mark_forgotten");
    note_id
}

/// ISOLATION (échoue avant le fix — hardcode `"main"`) : un principal `vault-b` porteur
/// d'un grant ACL couvrant `main/*` NE voit PAS les notes oubliées de `main` (ni la PII
/// `forgotten_by`), et voit UNIQUEMENT les siennes.
#[tokio::test]
async fn forgotten_list_scopes_to_effective_tenant_not_main() {
    let env = build_env().await;
    let id_main = seed_forgotten(&env, "main", MAIN_PII).await;
    let id_b = seed_forgotten(&env, "vault-b", VAULT_B_ACTOR).await;

    let resp = vault_forgotten_list(
        State(env.state.clone()),
        Extension(bearer("vault-b")),
        Query(ForgottenListQuery {
            limit: 50,
            cursor: None,
        }),
    )
    .await
    .expect("vault-b → 200")
    .0;

    // Aucune note de main, aucune fuite de PII.
    assert!(
        !resp.notes.iter().any(|n| n.ulid == id_main),
        "vault-b NE doit PAS voir la note oubliée de main (avant fix : hardcode main → fuite)"
    );
    assert!(
        !resp
            .notes
            .iter()
            .any(|n| n.forgotten_by.as_deref() == Some(MAIN_PII)),
        "le forgotten_by (PII) de main NE doit JAMAIS fuiter vers vault-b"
    );
    // Vault-b voit les SIENNES.
    assert!(
        resp.notes.iter().any(|n| n.ulid == id_b),
        "vault-b DOIT voir sa propre note oubliée"
    );
    assert_eq!(
        resp.total, 1,
        "le total est scopé à vault-b (1 note), pas au vault main"
    );
}

/// PARITÉ : un principal `main` voit ses propres notes oubliées (le routage n'a pas cassé
/// le chemin nominal own-vault).
#[tokio::test]
async fn forgotten_list_main_sees_own_notes() {
    let env = build_env().await;
    let id_main = seed_forgotten(&env, "main", MAIN_PII).await;
    let _id_b = seed_forgotten(&env, "vault-b", VAULT_B_ACTOR).await;

    let resp = vault_forgotten_list(
        State(env.state.clone()),
        Extension(bearer("main")),
        Query(ForgottenListQuery {
            limit: 50,
            cursor: None,
        }),
    )
    .await
    .expect("main → 200")
    .0;

    assert!(
        resp.notes.iter().any(|n| n.ulid == id_main),
        "main DOIT voir sa propre note oubliée"
    );
    assert!(
        !resp
            .notes
            .iter()
            .any(|n| n.forgotten_by.as_deref() == Some(VAULT_B_ACTOR)),
        "main NE doit PAS voir la note de vault-b (isolation symétrique)"
    );
    assert_eq!(resp.total, 1, "total scopé à main (1 note)");
}
