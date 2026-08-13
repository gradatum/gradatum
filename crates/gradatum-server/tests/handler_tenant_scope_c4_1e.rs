//! Scope cross-vault des handlers internes destructifs/intégrité (C4-1e, Slice E).
//!
//! Avant Slice E, deux handlers `/internal/v1/*` clouaient le tenant à la constante
//! `INTERNAL_TENANT_ID = "main"` au lieu d'honorer le vault de la requête :
//!
//! - `DELETE /internal/v1/note/{ulid}` (`handle_delete_note`) appelait
//!   `cascade_delete_note(state, "main", …)` — à `multi_tenant.enabled = ON`, la purge
//!   d'un vault secondaire supprimait l'homonyme `main` (clobber) et laissait la note
//!   ciblée intacte (no-op) : classe delete-cross-vault.
//! - `POST /internal/v1/persist/forget` (`handle_persist_forget`) appelait
//!   `mark_forgotten("main", …)` — l'oubli d'une note d'un vault secondaire marquait
//!   l'homonyme `main` dans l'index.
//!
//! Slice E = **EXPAND propagation** :
//! - `handle_delete_note` accepte un query param `vault_id` OPTIONNEL (défaut `"main"`),
//!   comme `handle_note_trust` / `handle_note_embedding` (Slice B).
//! - `handle_persist_forget` scope `mark_forgotten` sur `req.tenant_id`.
//!
//! Garanties vérifiées ici :
//! - OFF (byte-identical) : requête SANS `?vault_id=` / `tenant_id = "main"` → cible `main`.
//! - ON (isolation)       : `?vault_id=vault-b` / `tenant_id = "vault-b"` → n'affecte QUE
//!   le vault secondaire, l'homonyme `main` est préservé.
//!
//! Le régime multi-vault est purement local au harnais (deux notes homonymes semées via
//! l'endpoint interne, qui honore `tenant_id`). Aucune config serveur n'est touchée :
//! `multi_tenant.enabled` reste OFF — seule la propagation du vault est exercée.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::scope::VaultId;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_server::{internal::build_internal_router, state::AppState};
use gradatum_vault::Vault;
use secrecy::SecretString;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

const TEST_TOKEN: &str = "test-internal-token-abc123";

struct NoopEmbed;

#[async_trait::async_trait]
impl Embedder for NoopEmbed {
    fn embedder_id(&self) -> &str {
        "noop-internal"
    }
    fn dim(&self) -> u16 {
        4
    }
    async fn embed(&self, _: &str) -> Result<Vec<f32>, gradatum_embed::error::EmbedError> {
        Ok(vec![0.0f32; 4])
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, gradatum_embed::error::EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 4]).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

struct Env {
    router: axum::Router,
    vault: Arc<Vault>,
    _tmp: TempDir,
}

/// Construit le router interne avec un index réel. L'ACL n'est pas ré-appliquée sur
/// `tenant_id` par l'endpoint interne (loopback + token) — cf `internal_api.rs`.
async fn build_env() -> Env {
    let tmp = TempDir::new().expect("TempDir");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

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

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopEmbed))
        .with_vault_arc(vault_registry)
        .with_internal_api_token(SecretString::from(TEST_TOKEN.to_string()));
    state.search = index;

    // Lot REG : le second vault de la fixture doit exister au registre de DONNÉES avant
    // qu'une note puisse y naître. Passage par l'API de production `provision_vault`
    // plutôt que par un INSERT brut : le prérequis exigé par la garde est ainsi prouvé
    // atteignable par le chemin sanctionné.
    vault
        .index()
        .provision_vault("vault-b")
        .await
        .expect("provision vault-b (prérequis lot REG)");

    Env {
        router: build_internal_router(state),
        vault,
        _tmp: tmp,
    }
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Internal", format!("Bearer {TEST_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(body.to_string()))
        .unwrap()
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

fn del(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("X-Gradatum-Internal", format!("Bearer {TEST_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::empty())
        .unwrap()
}

/// Sème une note `feedback` (section NON protégée = hard-delete autorisé) via l'endpoint
/// interne, avec ULID + vault imposés. `trust` sert d'observable scopé (`get_trust`).
fn curated(note_id: &str, tenant: &str, trust: f32) -> serde_json::Value {
    json!({
        "note_id": note_id,
        "tenant_id": tenant,
        "section": "feedback",
        "status": "live",
        "title": "Note tenant scope",
        "body": "# Note\n\nCorps.",
        "tags": [],
        "author": null,
        "provenance": null,
        "temporal": null,
        "links": [],
        "trust": trust
    })
}

/// Corps `persist/forget` pour une note d'un vault donné.
fn forget(note_id: &str, tenant: &str) -> serde_json::Value {
    json!({
        "note_id": note_id,
        "tenant_id": tenant,
        "body": "# Note\n\nCorps oublié.",
        "section": "feedback",
        "forgotten_by": "test-agent"
    })
}

/// Vrai si `get_trust?vault_id=<vault>` renvoie 200 (la note existe dans ce vault).
async fn note_present(env: &Env, note_id: &str, vault: &str) -> bool {
    let resp = env
        .router
        .clone()
        .oneshot(get(&format!(
            "/internal/v1/note/{note_id}/trust?vault_id={vault}"
        )))
        .await
        .unwrap();
    resp.status() == StatusCode::OK
}

// ── #1 DELETE — chemin DESTRUCTIF ─────────────────────────────────────────────

/// OFF (byte-identical) : DELETE sans `?vault_id=` → défaut `"main"` → supprime main.
#[tokio::test]
async fn delete_without_vault_id_defaults_main() {
    let env = build_env().await;
    let note_id = Ulid::generate().to_string();

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "main", 0.9),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed main");
    assert!(note_present(&env, &note_id, "main").await, "main semé");

    let resp = env
        .router
        .clone()
        .oneshot(del(&format!("/internal/v1/note/{note_id}")))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "DELETE défaut main → 204"
    );

    assert!(
        !note_present(&env, &note_id, "main").await,
        "la note main doit être supprimée (défaut byte-identical)"
    );
}

/// ON (isolation) : deux notes homonymes ; DELETE `?vault_id=vault-b` supprime vault-b
/// et PRÉSERVE l'homonyme main (pas de clobber cross-vault).
#[tokio::test]
async fn delete_scopes_by_vault_id_query_preserves_main() {
    let env = build_env().await;
    let note_id = Ulid::generate().to_string(); // même ULID, deux vaults

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "main", 0.9),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed main");

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "vault-b", 0.2),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed vault-b");

    assert!(note_present(&env, &note_id, "main").await, "main présent");
    assert!(
        note_present(&env, &note_id, "vault-b").await,
        "vault-b présent"
    );

    // DELETE ciblé vault-b.
    let resp = env
        .router
        .clone()
        .oneshot(del(&format!(
            "/internal/v1/note/{note_id}?vault_id=vault-b"
        )))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "DELETE vault-b → 204 : {:?}",
        resp.status()
    );

    // vault-b supprimé, main préservé.
    assert!(
        !note_present(&env, &note_id, "vault-b").await,
        "la note vault-b doit être supprimée"
    );
    assert!(
        note_present(&env, &note_id, "main").await,
        "l'homonyme main NE doit PAS être touché (anti-clobber cross-vault)"
    );
}

// ── #2 forget — mark_forgotten scopé ──────────────────────────────────────────

/// OFF (byte-identical) : forget `tenant_id = "main"` → l'index main est marqué forgotten.
#[tokio::test]
async fn forget_main_marks_main_index() {
    let env = build_env().await;
    let note_id = Ulid::generate().to_string();

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "main", 0.9),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed main");

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/forget",
            forget(&note_id, "main"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "forget main → 200");

    let forgotten = env
        .vault
        .index()
        .is_note_forgotten("main", &note_id)
        .await
        .expect("is_note_forgotten main");
    assert!(forgotten, "la note main doit être marquée forgotten");
}

/// ON (isolation) : deux notes homonymes ; forget `tenant_id = "vault-b"` marque l'index
/// vault-b et NON l'homonyme main.
#[tokio::test]
async fn forget_scopes_mark_forgotten_by_tenant() {
    let env = build_env().await;
    let note_id = Ulid::generate().to_string();

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "main", 0.9),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed main");

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "vault-b", 0.2),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed vault-b");

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/forget",
            forget(&note_id, "vault-b"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "forget vault-b → 200");

    let idx = env.vault.index();
    let forgotten_b = idx
        .is_note_forgotten("vault-b", &note_id)
        .await
        .expect("is_note_forgotten vault-b");
    let forgotten_main = idx
        .is_note_forgotten("main", &note_id)
        .await
        .expect("is_note_forgotten main");

    assert!(
        forgotten_b,
        "la note vault-b doit être marquée forgotten (mark_forgotten scopé)"
    );
    assert!(
        !forgotten_main,
        "l'homonyme main NE doit PAS être marqué forgotten (anti-cross-vault)"
    );
}
