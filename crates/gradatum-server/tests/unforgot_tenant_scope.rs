//! Isolation cross-tenant de `POST /api/v1/vault/unforgot/{ulid}` (`vault_unforgot`,
//! forget.rs) — jumeau P0-8 le plus sérieux (J3, ÉCRITURE).
//!
//! ## Le trou (famille cross-tenant, flip-blocker sécurité)
//!
//! Le handler clouait le vault à `let vault_id = "main"` (hardcode inconditionnel) ET
//! n'appliquait que `write_scope_allowed`, SANS `require_write_grant` — il était donc le
//! SEUL write-path vault MOINS protégé que `vault_forget`. À `multi_tenant.enabled = ON`, un
//! tenant `≠ main` porteur d'un grant ACL couvrant `main/*` + d'un token write pouvait
//! RESTAURER une note oubliée de `main` (`unmark_forgotten("main", …)`) → tampering + droit
//! à l'oubli défait.
//!
//! ## Le fix
//!
//! Parité stricte avec `vault_forget` : vault dérivé du JWT via `effective_write_vault`
//! (effective_tenant + write-scope + `require_write_grant`), GATÉ sur `multi_tenant.enabled`
//! (byte-identical à OFF). Un tenant `≠ main` sans grant write sur `main` est refusé 403 et
//! ne peut plus toucher les notes de `main`.
//!
//! Deux `Vault` physiques (`main`, `vault-b`) adossés au MÊME `Arc<SqliteIndex>` ; flag ON
//! local au harnais (flip INTERDIT LIVE). Appels DIRECTS au handler axum public.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_server::api_v1::forget::vault_unforgot;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;

const MAIN_ACTOR: &str = "main-actor";
const VAULT_B_ACTOR: &str = "vaultb-actor";

/// Preset ACL `writer` : write sur `main/*` ET `vault-b/*`. Le grant cross-vault
/// (`main/*` pour un principal `vault-b`) modélise la précondition de l'exploit : ACL Write
/// `Allow` sur le locus de `main` — la seule barrière restante côté fix est le grant tenant.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "writer"
read_patterns  = ["main/*", "vault-b/*"]
write_patterns = ["main/*", "vault-b/*"]
"#;

struct Env {
    state: AppState,
    vault_main: Arc<Vault>,
    vault_b: Arc<Vault>,
    _tmp: TempDir,
}

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

/// TrustContext BearerToken pour `tenant`, identité `writer` (write scope).
fn bearer_write(tenant: &str) -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "writer".into(),
        scopes: vec!["write".into()],
        tenant_id: tenant.into(),
        jti: None,
    }
}

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

/// Écrit une note dans `vault_id`, puis la marque `forgotten`.
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

/// Vrai si `note_id` figure toujours dans la liste des oubliées de `vault_id`.
async fn is_forgotten(env: &Env, vault_id: &str, note_id: &str) -> bool {
    let rows = env
        .state
        .search
        .list_forgotten(vault_id, 500, None)
        .await
        .expect("list_forgotten");
    rows.iter().any(|(id, _, _, _, _)| id == note_id)
}

/// ISOLATION (échoue avant le fix — hardcode `"main"` + absence de `require_write_grant`) :
/// un principal `vault-b` (write scope + ACL write couvrant `main/*`, MAIS aucun grant tenant
/// sur `main`) NE peut PAS restaurer une note oubliée de `main` ; la note reste oubliée.
#[tokio::test]
async fn unforgot_cannot_restore_main_note_from_vault_b() {
    let env = build_env().await;
    let main_note = seed_forgotten(&env, "main", MAIN_ACTOR).await;

    let result = vault_unforgot(
        State(env.state.clone()),
        Extension(bearer_write("vault-b")),
        Path(main_note.clone()),
    )
    .await;

    // Avant fix : Ok(restored) (hardcode main + pas de grant) → la note de main serait restaurée.
    assert!(
        result.is_err(),
        "vault-b NE doit PAS pouvoir restaurer une note oubliée de main"
    );
    assert_eq!(
        result.err(),
        Some(StatusCode::FORBIDDEN),
        "refus attendu : 403 (require_write_grant absent pour vault-b sur son propre vault)"
    );
    assert!(
        is_forgotten(&env, "main", &main_note).await,
        "la note de main DOIT rester oubliée (droit à l'oubli préservé)"
    );
}

/// PARITÉ : le tenant racine `main` (write scope + grant seed 0030 `main↔main`) restaure
/// bien sa propre note oubliée (le routage write-path n'a pas cassé le chemin nominal).
#[tokio::test]
async fn unforgot_main_restores_own_note() {
    let env = build_env().await;
    let main_note = seed_forgotten(&env, "main", MAIN_ACTOR).await;

    let resp = vault_unforgot(
        State(env.state.clone()),
        Extension(bearer_write("main")),
        Path(main_note.clone()),
    )
    .await
    .expect("main → Ok (restore own note)");

    assert_eq!(resp.0.status, "restored");
    assert!(
        !is_forgotten(&env, "main", &main_note).await,
        "main DOIT avoir restauré sa propre note (plus oubliée)"
    );
}
