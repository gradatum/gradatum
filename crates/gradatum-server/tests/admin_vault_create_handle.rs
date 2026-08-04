//! `handle_admin_vault_create` instancie un `Vault` réel et l'enregistre
//! dans le registre de handles, **gaté `multi_tenant.enabled`**.
//!
//! - **Flag ON** : create("vault-b") → un handle `Vault` réel est instancié (adossé au pool
//!   `index.db` partagé, sous-répertoire md sibling) puis enregistré ; `state.vaults.resolve`
//!   le résout, et une lecture renvoie un `NoteNotFound` PROPRE (pas un fantôme
//!   `VaultNotFound`/500).
//! - **Flag OFF** (défaut LIVE) : comportement index-only actuel STRICTEMENT inchangé —
//!   aucun handle instancié, le registre n'est jamais muté en runtime (byte-identical).
//!
//! La registration runtime exploite la mutabilité intérieure (`RwLock`) du `VaultRegistry` :
//! le handler mute le registre PARTAGÉ derrière `Arc<VaultRegistry>`, la mutation est donc
//! visible sur un clone de l'`AppState` conservé par le test.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::index::Index;
use gradatum_core::scope::VaultId;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::internal;
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "test-admin-token-0123456789abcdef";

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40001))
}

/// Construit un `AppState` vault racine `main` réel (registre + `shared_index` peuplés,
/// `state.search` = même pool) et retourne `(state, TempDir)`. `multi_tenant` selon `on`.
async fn build_state(on: bool) -> (AppState, TempDir) {
    let tmp = TempDir::new().expect("TempDir admin_vault_create_handle");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let idx = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str("").expect("preset ACL vide valide");
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut app_state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_admin_api_token(secrecy::SecretString::from(ADMIN_TOKEN.to_string()))
        .with_server_config(ServerConfig {
            multi_tenant: MultiTenantConfig { enabled: on },
            ..ServerConfig::default()
        });
    app_state.search = Arc::clone(&idx) as Arc<dyn Index>;
    // Câblage cible (identique à `with_vault_path` au boot) : registre singleton {main} +
    // `shared_index` = pool `index.db` concret.
    app_state.vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));
    app_state.shared_index = Some(idx);
    (app_state, tmp)
}

/// POST admin loopback create → StatusCode.
async fn post_create(app: &Router, vault_id: &str) -> StatusCode {
    let body = serde_json::json!({ "vault_id": vault_id });
    let req = Request::builder()
        .method("POST")
        .uri("/internal/v1/admin/vaults/create")
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Admin", format!("Bearer {ADMIN_TOKEN}"))
        .extension(ConnectInfo(loopback()))
        .body(Body::from(serde_json::to_vec(&body).expect("json")))
        .expect("request");
    app.clone().oneshot(req).await.expect("service").status()
}

/// Flag ON : create instancie + enregistre un handle réel, résoluble, lecture propre.
#[tokio::test]
async fn admin_create_vault_registers_resolvable_handle() {
    let (app_state, _tmp) = build_state(true).await;
    // Avant create : vault-b non résoluble (fail-closed).
    assert!(
        app_state.vaults.resolve(&VaultId::new("vault-b")).is_err(),
        "vault-b ne doit pas exister avant create"
    );

    let app = internal::build_internal_router(app_state.clone());
    let status = post_create(&app, "vault-b").await;
    assert_eq!(status, StatusCode::OK, "create vault-b doit réussir (200)");

    // La mutation du registre PARTAGÉ (RwLock) est visible via le clone conservé.
    let handle = app_state
        .vaults
        .resolve(&VaultId::new("vault-b"))
        .expect("vault-b doit être résoluble après create (handle réel enregistré)");

    // Lecture sur vault-b : NoteNotFound PROPRE (pas un fantôme VaultNotFound/500).
    let read = handle.read_note_by_id("01ARZ3NDEKTSV4RRFFQ69G5FAV").await;
    assert!(
        matches!(read, Err(GradatumError::NoteNotFound(_))),
        "lecture sur vault-b doit renvoyer NoteNotFound propre, obtenu : {read:?}"
    );
}

/// Flag OFF (byte-identical) : create reste index-only — aucun handle instancié, registre
/// jamais muté en runtime, réponse 200 inchangée.
#[tokio::test]
async fn admin_create_flag_off_unchanged() {
    let (app_state, _tmp) = build_state(false).await;
    assert_eq!(app_state.vaults.len(), 1, "registre = {{main}} au départ");

    let app = internal::build_internal_router(app_state.clone());
    let status = post_create(&app, "vault-b").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create reste 200 à flag OFF (provision index-only)"
    );

    // À OFF : AUCUN handle instancié — vault-b reste non résoluble, registre inchangé.
    assert!(
        app_state.vaults.resolve(&VaultId::new("vault-b")).is_err(),
        "flag OFF : aucun handle vault-b (registre non muté)"
    );
    assert_eq!(
        app_state.vaults.len(),
        1,
        "flag OFF : registre reste {{main}} (byte-identical)"
    );
}
