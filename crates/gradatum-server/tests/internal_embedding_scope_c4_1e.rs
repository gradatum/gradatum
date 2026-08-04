//! Scope cross-vault de `POST /internal/v1/persist/embedding` (C4-1e, Slice B / B2).
//!
//! B2 = **EXPAND** : `PersistEmbeddingRequest` gagne un champ `vault_id: Option<String>`
//! (`#[serde(default)]`, défaut `"main"`). Le handler le passe à
//! `insert_note_embedding(vault_id, …)`, qui utilise ce paramètre comme clé de partition
//! ANN au lieu d'un `SELECT vault_id FROM notes WHERE id = ?` (id-only, vecteur de fuite
//! cross-vault sur collision d'ULID — PK composite `(vault_id, id)` depuis 0032).
//!
//! - OFF (byte-identical) : POST SANS `vault_id` → défaut `"main"` → 200 + read-back OK
//!   (un worker antérieur, sans le champ, continue de fonctionner).
//! - ON (param honoré)    : POST avec `vault_id = "vault-b"` (note homonyme main+vault-b) →
//!   200 + read-back OK ; le paramètre route la partition ANN.
//!
//! ## Limite d'observabilité (documentée)
//!
//! L'isolation de la **partition ANN** (`note_embeddings_ann`, vec0) n'est PAS observable
//! dans le harnais CI : l'extension sqlite-vec est une dépendance runtime gatée
//! `#[ignore = "requiert libvec0"]` (cf `ann_routing.rs`), donc `upsert_ann` est un no-op
//! en mode dégradé. De plus, `note_embeddings` n'a pas encore de colonne `vault_id`
//! (PK `(note_id, embedder_id)` — vault-scoping = Slice D2). Ces tests verrouillent donc
//! l'**EXPAND** (compat OFF + param accepté sur le chemin réel) ; la preuve d'isolation de
//! partition relève de D2 + un banc vec0 (`ann_recall.rs`, gaté).

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
        "title": "Note embedding scope",
        "body": "# Note\n\nCorps.",
        "tags": [],
        "author": null,
        "provenance": null,
        "temporal": null,
        "links": [],
        "trust": null
    })
}

/// OFF (expand byte-identical) : POST embedding SANS `vault_id` → 200, read-back OK.
/// Prouve qu'un worker antérieur (payload sans le champ) reste fonctionnel.
#[tokio::test]
async fn persist_embedding_without_vault_id_defaults_main() {
    let env = build_env().await;
    let note_id = Ulid::new().to_string();

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/curated",
            curated(&note_id, "main"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed main");

    // Payload SANS champ vault_id — schéma pré-B2.
    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/embedding",
            json!({
                "note_id": note_id,
                "embedder_id": "noop-internal",
                "dim": 4,
                "vector": [0.1, 0.2, 0.3, 0.4]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST embedding sans vault_id doit rester 200 (expand-safe)"
    );

    let resp = env
        .router
        .oneshot(get(&format!(
            "/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "read-back OK");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["dim"], 4);
}

/// ON (param honoré) : note homonyme main+vault-b ; POST embedding avec `vault_id=vault-b`
/// → 200 + read-back OK. Le paramètre atteint `insert_note_embedding` (clé de partition ANN).
/// L'isolation de partition elle-même n'est pas observable sans libvec0 (cf en-tête module).
#[tokio::test]
async fn persist_embedding_accepts_vault_id_param() {
    let env = build_env().await;
    let note_id = Ulid::new().to_string(); // même ULID, deux vaults

    for tenant in ["main", "vault-b"] {
        let resp = env
            .router
            .clone()
            .oneshot(post(
                "/internal/v1/persist/curated",
                curated(&note_id, tenant),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "seed {tenant}");
    }

    let resp = env
        .router
        .clone()
        .oneshot(post(
            "/internal/v1/persist/embedding",
            json!({
                "note_id": note_id,
                "embedder_id": "noop-internal",
                "dim": 4,
                "vector": [0.5, 0.6, 0.7, 0.8],
                "vault_id": "vault-b"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST embedding avec vault_id=vault-b doit être 200"
    );

    // C4-1e (Slice E) : la LECTURE est désormais scopée par vault_id (défaut "main").
    // L'embedding a été persisté sous `vault-b` uniquement — le read-back doit donc cibler
    // `?vault_id=vault-b`. Avant Slice E, un GET sans param trouvait le vecteur de vault-b
    // par lookup non scopé (note_id, embedder_id) : c'est précisément la fuite cross-vault
    // que Slice E ferme (lire `main` ne doit plus révéler l'embedding de `vault-b`).
    let resp = env
        .router
        .oneshot(get(&format!(
            "/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal&vault_id=vault-b"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "read-back OK");
}
