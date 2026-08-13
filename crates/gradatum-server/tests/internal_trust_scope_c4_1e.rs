//! Scope cross-vault de `GET /internal/v1/note/{ulid}/trust` (C4-1e, Slice B / B2).
//!
//! Avant B2, le handler `handle_note_trust` appelait `get_trust(&note_id)` — non scopé
//! (`SELECT trust FROM notes WHERE id = ?`). Sur collision d'ULID entre deux vaults
//! (PK composite `(vault_id, id)` depuis migration 0032), la requête renvoyait le trust
//! d'une ligne arbitraire, indépendamment du vault visé.
//!
//! B2 = **EXPAND** : le handler accepte un query param `vault_id` OPTIONNEL (défaut `"main"`).
//! - OFF (byte-identical) : requête SANS `?vault_id=` → défaut `"main"` → réponse inchangée.
//! - ON (isolation)       : `?vault_id=vault-b` → trust de `vault-b`, PAS de `main` (id colliding).
//!
//! Le régime multi-vault est purement local au harnais (deux notes homonymes semées via
//! l'endpoint interne, qui honore `tenant_id` — loopback + token interne). Aucune config
//! serveur n'est touchée : `multi_tenant.enabled` reste OFF.

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
use http_body_util::BodyExt;
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
    _vault: Arc<Vault>,
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
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
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
        _vault: vault,
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

/// Sème une note curated (ULID + vault + trust imposés) via l'endpoint interne.
fn curated(note_id: &str, tenant: &str, trust: f32) -> serde_json::Value {
    json!({
        "note_id": note_id,
        "tenant_id": tenant,
        "section": "decisions",
        "status": "live",
        "title": "Note trust scope",
        "body": "# Note\n\nCorps.",
        "tags": [],
        "author": null,
        "provenance": null,
        "temporal": null,
        "links": [],
        "trust": trust
    })
}

/// OFF (expand byte-identical) : GET sans `?vault_id=` → défaut `"main"` → trust de main.
#[tokio::test]
async fn get_trust_without_vault_id_defaults_main() {
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
        .oneshot(get(&format!("/internal/v1/note/{note_id}/trust")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let trust = json["trust"].as_f64().expect("trust f64");
    assert!(
        (trust - 0.9).abs() < 1e-6,
        "défaut vault=main → trust de main (0.9), obtenu {trust}"
    );
}

/// ON (isolation) : deux notes homonymes (main=0.9, vault-b=0.2) ; `?vault_id=vault-b`
/// renvoie 0.2, `?vault_id=main` renvoie 0.9 — pas de fuite cross-vault.
#[tokio::test]
async fn get_trust_scopes_by_vault_id_query() {
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

    // vault-b → 0.2
    let resp = env
        .router
        .clone()
        .oneshot(get(&format!(
            "/internal/v1/note/{note_id}/trust?vault_id=vault-b"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let b = json["trust"].as_f64().expect("trust vault-b");
    assert!(
        (b - 0.2).abs() < 1e-6,
        "vault-b doit renvoyer son propre trust (0.2), obtenu {b}"
    );

    // main → 0.9
    let resp = env
        .router
        .oneshot(get(&format!(
            "/internal/v1/note/{note_id}/trust?vault_id=main"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let m = json["trust"].as_f64().expect("trust main");
    assert!(
        (m - 0.9).abs() < 1e-6,
        "main doit renvoyer son propre trust (0.9), obtenu {m}"
    );
}
