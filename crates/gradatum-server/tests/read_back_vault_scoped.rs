//! Split-brain read-back : `read_note_by_id` routé par le vault EFFECTIF.
//!
//! ## Le trou (≥2 vaults, flip-blocker)
//!
//! Sous le registre de handles, plusieurs read-back internes lisaient le corps de
//! la note via le **singleton `main`** (`state.vault.read_note_by_id(id)`), alors que le MARK
//! (`mark_forgotten`, garde overwrite index-level, …) était déjà scopé sur le vault cible.
//! Résultat sous 2 vaults au flip : **split-brain** — le mark frappe le bon vault mais le
//! read/write-back frappe `main` (corps d'un homonyme, ou `NoteNotFound` si `main` ne porte
//! pas l'ULID). Le routage envoie chaque read-back via le handle du vault effectif obtenu du
//! registre (`state.vaults.resolve`), fail-closed sur miss.
//!
//! ## Régime & byte-identical
//!
//! Le routage est **gaté sur `multi_tenant.enabled`** (pattern read-path C2 déjà en place :
//! `effective_read_vault` est ON-only, le legacy singleton reste inline à OFF). À OFF le
//! chemin `state.vault` (singleton `main`) est inchangé — byte-identical, un seul vault
//! physique, pas de split-brain possible. À ON, la lecture est routée par vault_id : ce test
//! démontre l'isolation sur `GET /internal/v1/note/{ulid}?vault_id=…`.
//!
//! Deux `Vault` physiques (`main`, `vault-b`) adossés au MÊME `Arc<SqliteIndex>` (md
//! per-vault sous `<root>/<vault_id>/`, index partagé — topologie cible council
//! `01KXWMCR0N`), registre peuplé des deux handles. `multi_tenant.enabled = true` est LOCAL
//! au harnais (flip INTERDIT LIVE).

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::internal::build_internal_router;
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use http_body_util::BodyExt;
use secrecy::SecretString;
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_TOKEN: &str = "test-internal-token-abc123";

struct Env {
    router: axum::Router,
    vault_main: Arc<Vault>,
    vault_b: Arc<Vault>,
    _tmp: TempDir,
}

/// Frontmatter minimal ciblant `vault_id` (section non protégée).
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

/// Deux vaults physiques, index partagé, registre peuplé, flag ON.
async fn build_env() -> Env {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("vault");

    // `main` ouvre le pool ; `vault-b` le RÉUTILISE (handle partagé, md distinct).
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
    let acl = AclEngine::from_preset_str(
        r#"
[[consumer]]
identity = "internal-test"
read_patterns  = ["main/*", "vault-b/*"]
write_patterns = ["main/*", "vault-b/*"]
"#,
    )
    .expect("preset ACL");

    // Flip local au harnais — active le routage read-back par vault effectif (ON-only).
    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig { enabled: true },
        ..ServerConfig::default()
    };

    let facade: Arc<dyn gradatum_vault::Registry> = Arc::clone(&vault_main) as _;
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(facade)
        .with_internal_api_token(SecretString::from(TEST_TOKEN.to_string()))
        .with_server_config(cfg);
    let search_index: Arc<dyn gradatum_core::index::Index> = shared_index.clone();
    state.search = search_index;
    state.vaults = Arc::new(registry);

    Env {
        router: build_internal_router(state),
        vault_main,
        vault_b,
        _tmp: tmp,
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("X-Gradatum-Internal", format!("Bearer {TEST_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::empty())
        .unwrap()
}

/// Corps JSON de la réponse `GET /internal/v1/note/{ulid}` (status attendu 200).
async fn read_body(env: &Env, uri: &str) -> (StatusCode, String) {
    let resp = env.router.clone().oneshot(get(uri)).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (status, body)
}

/// Écrit une note homonyme (même ULID) dans les DEUX vaults avec des CORPS distincts,
/// directement via chaque handle (md per-vault, index partagé).
async fn seed_homonym(env: &Env, id: NoteId) {
    env.vault_main
        .write_note_with_id(
            frontmatter_for("main"),
            "# Note main\n\nCORPS-MAIN".into(),
            id,
        )
        .await
        .expect("write main");
    env.vault_b
        .write_note_with_id(
            frontmatter_for("vault-b"),
            "# Note vault-b\n\nCORPS-VAULT-B".into(),
            id,
        )
        .await
        .expect("write vault-b");
}

/// Split-brain fermé : un read-back ciblant `vault-b` lit le CORPS de `vault-b`, jamais
/// celui de `main` (homonyme). Avant le fix (singleton `main`), `?vault_id=vault-b`
/// renvoyait le corps de `main`.
#[tokio::test]
async fn note_read_back_reads_target_vault_body_not_main() {
    let env = build_env().await;
    let id = NoteId::new();
    seed_homonym(&env, id).await;
    let id_str = id.0.to_string();

    let (status, body) = read_body(
        &env,
        &format!("/internal/v1/note/{id_str}?vault_id=vault-b"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read-back vault-b → 200");
    assert!(
        body.contains("CORPS-VAULT-B"),
        "le read-back ciblant vault-b DOIT lire le corps de vault-b, obtenu : {body:?}"
    );
    assert!(
        !body.contains("CORPS-MAIN"),
        "le read-back ciblant vault-b ne DOIT JAMAIS renvoyer le corps de main (split-brain), obtenu : {body:?}"
    );
}

/// Parité : `?vault_id=main` (et le défaut) lit le corps de `main` — le routage n'a pas
/// cassé le chemin nominal.
#[tokio::test]
async fn note_read_back_main_reads_main_body() {
    let env = build_env().await;
    let id = NoteId::new();
    seed_homonym(&env, id).await;
    let id_str = id.0.to_string();

    let (status, body) =
        read_body(&env, &format!("/internal/v1/note/{id_str}?vault_id=main")).await;
    assert_eq!(status, StatusCode::OK, "read-back main → 200");
    assert!(
        body.contains("CORPS-MAIN"),
        "le read-back ciblant main DOIT lire le corps de main, obtenu : {body:?}"
    );
    assert!(
        !body.contains("CORPS-VAULT-B"),
        "le read-back main ne DOIT PAS renvoyer le corps de vault-b, obtenu : {body:?}"
    );
}

/// Fail-closed : un vault_id absent du registre → erreur (500), JAMAIS un repli sur `main`.
#[tokio::test]
async fn note_read_back_unknown_vault_is_fail_closed() {
    let env = build_env().await;
    let id = NoteId::new();
    seed_homonym(&env, id).await;
    let id_str = id.0.to_string();

    let resp = env
        .router
        .clone()
        .oneshot(get(&format!("/internal/v1/note/{id_str}?vault_id=absent")))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "vault absent du registre → fail-closed 500 (jamais un repli sur main)"
    );
}
