//! Scope cross-vault de `GET /internal/v1/note/{ulid}/embedding` (C4-1e, Slice E).
//!
//! Avant Slice E, le handler `handle_note_embedding` appelait `get_note_embedding(note_id,
//! embedder_id)` — non scopé (`SELECT vector FROM note_embeddings WHERE note_id = ? AND
//! embedder_id = ?`). Sur collision d'ULID entre deux vaults (PK composite
//! `(note_id, embedder_id, vault_id)` de `note_embeddings` depuis migration 0033), la
//! requête renvoyait le vecteur d'une ligne arbitraire, indépendamment du vault visé —
//! au flip, le worker embed/distill lisait l'embedding du mauvais vault.
//!
//! Slice E = **EXPAND** : le handler accepte un query param `vault_id` OPTIONNEL (défaut
//! `"main"`), passé à `get_note_embedding(vault_id, …)` → clause `AND vault_id = ?`.
//! - OFF (byte-identical) : requête SANS `?vault_id=` → défaut `"main"` → vecteur de main.
//! - ON (isolation)       : `?vault_id=vault-b` → vecteur de `vault-b`, PAS de `main`
//!   (même ULID, deux vaults, vecteurs distincts).
//!
//! Contrairement au test EXPAND de l'insert (`internal_embedding_scope_c4_1e.rs`, gaté par
//! l'observabilité de la partition ANN vec0), l'isolation de la LECTURE est directement
//! observable ici : `note_embeddings` porte `vault_id` (D2, migration 0033) et le `SELECT
//! vector` scopé n'a besoin d'aucune extension runtime.
//!
//! Le régime multi-vault est purement local au harnais (deux notes homonymes + deux
//! embeddings semés via l'endpoint interne, qui honore `tenant_id`/`vault_id` — loopback +
//! token interne). Aucune config serveur n'est touchée : `multi_tenant.enabled` reste OFF.

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

fn curated(note_id: &str, tenant: &str) -> serde_json::Value {
    json!({
        "note_id": note_id,
        "tenant_id": tenant,
        "section": "decisions",
        "status": "live",
        "title": "Note embedding read scope",
        "body": "# Note\n\nCorps.",
        "tags": [],
        "author": null,
        "provenance": null,
        "temporal": null,
        "links": [],
        "trust": null
    })
}

/// Sème une note curated + son embedding dans `tenant`, avec un vecteur imposé.
async fn seed(env: &Env, note_id: &str, tenant: &str, vector: [f32; 4]) {
    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(note_id, tenant),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed note {tenant}");

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/embedding",
            json!({
                "note_id": note_id,
                "embedder_id": "noop-internal",
                "dim": 4,
                "vector": vector,
                "vault_id": tenant
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed embedding {tenant}");
}

async fn read_vector(env: &Env, uri: &str) -> Vec<f64> {
    let resp = env.router.clone().oneshot(get(uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "read {uri}");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["vector"]
        .as_array()
        .expect("vector array")
        .iter()
        .map(|v| v.as_f64().expect("f64"))
        .collect()
}

/// OFF (expand byte-identical) : GET sans `?vault_id=` → défaut `"main"` → vecteur de main.
/// Un worker antérieur (sans le param) lit toujours l'embedding de `main`.
#[tokio::test]
async fn get_embedding_without_vault_id_defaults_main() {
    let env = build_env().await;
    let note_id = Ulid::new().to_string();

    seed(&env, &note_id, "main", [0.1, 0.2, 0.3, 0.4]).await;

    let v = read_vector(
        &env,
        &format!("/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal"),
    )
    .await;
    assert_eq!(v.len(), 4, "dim 4");
    assert!(
        (v[0] - 0.1).abs() < 1e-6,
        "défaut vault=main → vecteur de main ([0.1, …]), obtenu {v:?}"
    );
}

/// ON (isolation) : deux notes/embeddings homonymes (main=[0.1,…], vault-b=[0.5,…]) ;
/// `?vault_id=vault-b` renvoie le vecteur de vault-b, `?vault_id=main` celui de main —
/// aucune fuite cross-vault sur la LECTURE.
#[tokio::test]
async fn get_embedding_scopes_by_vault_id_query() {
    let env = build_env().await;
    let note_id = Ulid::new().to_string(); // même ULID, deux vaults

    seed(&env, &note_id, "main", [0.1, 0.2, 0.3, 0.4]).await;
    seed(&env, &note_id, "vault-b", [0.5, 0.6, 0.7, 0.8]).await;

    // vault-b → [0.5, …]
    let vb = read_vector(
        &env,
        &format!(
            "/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal&vault_id=vault-b"
        ),
    )
    .await;
    assert!(
        (vb[0] - 0.5).abs() < 1e-6,
        "vault-b doit renvoyer son propre vecteur ([0.5, …]), obtenu {vb:?}"
    );

    // main → [0.1, …]
    let vm = read_vector(
        &env,
        &format!("/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal&vault_id=main"),
    )
    .await;
    assert!(
        (vm[0] - 0.1).abs() < 1e-6,
        "main doit renvoyer son propre vecteur ([0.1, …]), obtenu {vm:?}"
    );
}
