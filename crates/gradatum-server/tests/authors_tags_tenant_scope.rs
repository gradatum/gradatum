//! Isolation cross-tenant de `GET /api/v1/vault_authors` et `GET /api/v1/vault_tags`
//! (`vault_authors_impl` / `vault_tags_impl`, logic.rs) — jumeaux P0-8 (J1/J2).
//!
//! ## Le trou (famille cross-tenant, flip-blocker sécurité)
//!
//! Les deux handlers clouaient le vault à la constante `"main"` (hardcode inconditionnel),
//! en ignorant `trust.tenant_id()` ET `multi_tenant.enabled` : l'ACL était évaluée sur le
//! locus fixe `main/main` et l'index interrogé pour le vault `main`.
//!
//! `vault_authors`/`vault_tags` sont des READ own-vault (aucun paramètre de vault cible) —
//! siblings exacts de `vault_status_impl` / `vault_forgotten_list`. À
//! `multi_tenant.enabled = ON`, un principal `tenant ≠ main` porteur d'un grant ACL couvrant
//! `main/*` listait les AUTEURS (identité, PII-adjacent) et les TAGS (topologie) des notes de
//! `main` → fuite cross-tenant.
//!
//! ## Le fix
//!
//! Résolution du vault EFFECTIF depuis le JWT (`effective_tenant`), GATÉE sur
//! `multi_tenant.enabled` (pattern read-path aligné sur `vault_forgotten_list`). À OFF le
//! chemin `"main"` est inchangé (byte-identical). À ON, un tenant ne voit QUE ses propres
//! auteurs/tags.
//!
//! Deux `Vault` physiques (`main`, `vault-b`) adossés au MÊME `Arc<SqliteIndex>` (topologie
//! cible council `01KXWMCR0N`) ; flag ON local au harnais (flip INTERDIT LIVE). Appels
//! DIRECTS aux fonctions métier publiques `*_impl`.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_core::trust::TrustContext;
use gradatum_dto::VaultTagsRequest;
use gradatum_server::api_v1::logic::{vault_authors_impl, vault_tags_impl};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Marqueur auteur (identité) semé dans une note de `main` — ne DOIT jamais fuiter vers `vault-b`.
const MAIN_AUTHOR: &str = "main-secret-author";
/// Marqueur tag semé dans une note de `main` — ne DOIT jamais fuiter vers `vault-b`.
const MAIN_TAG: &str = "confidential-main";
const VAULT_B_AUTHOR: &str = "vaultb-author";
const VAULT_B_TAG: &str = "vaultb-tag";

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

/// Frontmatter minimal ciblant `vault_id`, avec auteur + tag (section non protégée).
fn frontmatter_for(vault_id: &str, author: &str, tag: &str) -> Frontmatter {
    let tag = Tag::new(tag).expect("tag de test valide");
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section: Section::Feedback,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: [tag].into_iter().collect(),
        author: Some(AuthorRef {
            kind: AuthorKind::Human,
            id: author.to_string(),
            display_name: Some(author.to_string()),
        }),
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

/// Écrit une note (auteur + tag) dans `vault_id`.
async fn seed_note(env: &Env, vault_id: &str, author: &str, tag: &str) {
    let id = NoteId::new();
    let vault = match vault_id {
        "main" => &env.vault_main,
        "vault-b" => &env.vault_b,
        other => panic!("vault inconnu dans le harnais : {other}"),
    };
    vault
        .write_note_with_id(
            frontmatter_for(vault_id, author, tag),
            "# Note\n\nCorps.".into(),
            id,
        )
        .await
        .expect("write_note_with_id");
}

// ── J1 : vault_authors ─────────────────────────────────────────────────────────

/// ISOLATION (échoue avant le fix — hardcode `"main"`) : un principal `vault-b` porteur d'un
/// grant ACL couvrant `main/*` NE voit PAS les auteurs de `main`, et voit UNIQUEMENT les siens.
#[tokio::test]
async fn vault_authors_scopes_to_effective_tenant_not_main() {
    let env = build_env().await;
    seed_note(&env, "main", MAIN_AUTHOR, MAIN_TAG).await;
    seed_note(&env, "vault-b", VAULT_B_AUTHOR, VAULT_B_TAG).await;

    let resp = vault_authors_impl(&env.state, &bearer("vault-b"))
        .await
        .expect("vault-b → Ok");

    assert!(
        !resp.authors.iter().any(|a| a.name == MAIN_AUTHOR),
        "vault-b NE doit PAS voir l'auteur de main (avant fix : hardcode main → fuite identité)"
    );
    assert!(
        resp.authors.iter().any(|a| a.name == VAULT_B_AUTHOR),
        "vault-b DOIT voir son propre auteur"
    );
}

/// PARITÉ : un principal `main` voit ses propres auteurs, pas ceux de `vault-b`.
#[tokio::test]
async fn vault_authors_main_sees_own_authors() {
    let env = build_env().await;
    seed_note(&env, "main", MAIN_AUTHOR, MAIN_TAG).await;
    seed_note(&env, "vault-b", VAULT_B_AUTHOR, VAULT_B_TAG).await;

    let resp = vault_authors_impl(&env.state, &bearer("main"))
        .await
        .expect("main → Ok");

    assert!(
        resp.authors.iter().any(|a| a.name == MAIN_AUTHOR),
        "main DOIT voir son propre auteur"
    );
    assert!(
        !resp.authors.iter().any(|a| a.name == VAULT_B_AUTHOR),
        "main NE doit PAS voir l'auteur de vault-b (isolation symétrique)"
    );
}

// ── J2 : vault_tags ──────────────────────────────────────────────────────────

/// ISOLATION (échoue avant le fix — hardcode `"main"`) : un principal `vault-b` NE voit PAS
/// les tags de `main`, et voit UNIQUEMENT les siens.
#[tokio::test]
async fn vault_tags_scopes_to_effective_tenant_not_main() {
    let env = build_env().await;
    seed_note(&env, "main", MAIN_AUTHOR, MAIN_TAG).await;
    seed_note(&env, "vault-b", VAULT_B_AUTHOR, VAULT_B_TAG).await;

    let resp = vault_tags_impl(&env.state, &bearer("vault-b"), VaultTagsRequest::new())
        .await
        .expect("vault-b → Ok");

    assert!(
        !resp.tags.iter().any(|t| t.tag == MAIN_TAG),
        "vault-b NE doit PAS voir le tag de main (avant fix : hardcode main → fuite topologie)"
    );
    assert!(
        resp.tags.iter().any(|t| t.tag == VAULT_B_TAG),
        "vault-b DOIT voir son propre tag"
    );
}

/// PARITÉ : un principal `main` voit ses propres tags, pas ceux de `vault-b`.
#[tokio::test]
async fn vault_tags_main_sees_own_tags() {
    let env = build_env().await;
    seed_note(&env, "main", MAIN_AUTHOR, MAIN_TAG).await;
    seed_note(&env, "vault-b", VAULT_B_AUTHOR, VAULT_B_TAG).await;

    let resp = vault_tags_impl(&env.state, &bearer("main"), VaultTagsRequest::new())
        .await
        .expect("main → Ok");

    assert!(
        resp.tags.iter().any(|t| t.tag == MAIN_TAG),
        "main DOIT voir son propre tag"
    );
    assert!(
        !resp.tags.iter().any(|t| t.tag == VAULT_B_TAG),
        "main NE doit PAS voir le tag de vault-b (isolation symétrique)"
    );
}
