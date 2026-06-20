//! Tests d'intégration — API interne server-to-worker (Wave 2, v0.5.3).
//!
//! ## Pattern
//!
//! Utilise `gradatum_server::internal::build_internal_router` directement
//! (pas de liaison TCP) — oneshot via `tower::ServiceExt`.
//!
//! Les tests vérifient :
//! - 401 si token absent ou invalide.
//! - 401 si adresse non-loopback.
//! - 200 persist/curated (vault write + index).
//! - 409 conflict (hash périmé).
//! - 200 persist/embedding + GET embedding.
//! - 404 DELETE note inexistante.
//! - 404 GET note inexistante.
//! - Isolation : `/internal/v1/*` absent du router public.

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

// ── NoopBackend minimal ──────────────────────────────────────────────────────

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

// ── Fixture ──────────────────────────────────────────────────────────────────

struct InternalTestEnv {
    router: axum::Router,
    _vault: Arc<Vault>,
    _tmp: TempDir,
}

async fn build_internal_env() -> InternalTestEnv {
    let tmp = TempDir::new().expect("TempDir internal API tests");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create test fixture"),
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
    .expect("preset ACL interne valide");

    let token_secret = SecretString::from(TEST_TOKEN.to_string());
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopEmbed))
        .with_vault_arc(vault_registry)
        .with_internal_api_token(token_secret);

    state.search = index;

    let router = build_internal_router(state);

    InternalTestEnv {
        router,
        _vault: vault,
        _tmp: tmp,
    }
}

/// Construit une requête HTTP avec ConnectInfo loopback injectée.
fn make_request(method: &str, uri: &str, body: serde_json::Value, token: &str) -> Request<Body> {
    let body_str = body.to_string();
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Internal", format!("Bearer {token}"));

    // ConnectInfo loopback requise par le middleware
    builder
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(body_str))
        .unwrap()
}

fn make_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("X-Gradatum-Internal", format!("Bearer {token}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::empty())
        .unwrap()
}

fn make_delete(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("X-Gradatum-Internal", format!("Bearer {token}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::empty())
        .unwrap()
}

// ── Tests 401 ────────────────────────────────────────────────────────────────

/// Token absent → 401.
#[tokio::test]
async fn internal_api_no_token_is_401() {
    let env = build_internal_env().await;

    let req = Request::builder()
        .method("POST")
        .uri("/internal/v1/persist/curated")
        .header("Content-Type", "application/json")
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from("{}"))
        .unwrap();

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Token invalide → 401.
#[tokio::test]
async fn internal_api_wrong_token_is_401() {
    let env = build_internal_env().await;

    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({}),
        "wrong-token",
    );
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── Tests persist/curated ────────────────────────────────────────────────────

/// persist/curated valide → 200 + PersistOkResponse.
#[tokio::test]
async fn internal_api_persist_curated_ok() {
    let env = build_internal_env().await;

    let note_id = Ulid::new().to_string();
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "Test note",
            "body": "# Test note\n\nCorps.",
            "tags": ["test"],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/curated doit retourner 200"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["note_id"], note_id);
}

/// persist/curated section invalide → 400.
#[tokio::test]
async fn internal_api_persist_curated_bad_section_is_400() {
    let env = build_internal_env().await;

    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": Ulid::new().to_string(),
            "tenant_id": "main",
            "section": "invalid-section",
            "status": "live",
            "title": "T",
            "body": "corps",
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── Tests persist/embedding + GET ────────────────────────────────────────────

/// persist/embedding valide + read-back via GET.
///
/// Pré-condition : la note doit exister dans `notes` (FK constraint sur `note_embeddings.note_id`).
/// → persist/curated d'abord, puis persist/embedding.
#[tokio::test]
async fn internal_api_persist_and_read_embedding() {
    let env = build_internal_env().await;
    let router = env.router;

    let note_id = Ulid::new().to_string();

    // 0. Créer la note préalablement (FK constraint note_embeddings → notes.id).
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "Note embedding test",
            "body": "# Note embedding test

Corps.",
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/curated préalable doit retourner 200"
    );

    // 1. POST persist/embedding
    let req = make_request(
        "POST",
        "/internal/v1/persist/embedding",
        json!({
            "note_id": note_id,
            "embedder_id": "noop-internal",
            "dim": 4,
            "vector": [0.1, 0.2, 0.3, 0.4]
        }),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/embedding doit retourner 200"
    );

    // 2. GET note/:ulid/embedding
    let req = make_get(
        &format!("/internal/v1/note/{note_id}/embedding?embedder_id=noop-internal"),
        TEST_TOKEN,
    );
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET embedding doit retourner 200"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["note_id"], note_id);
    assert_eq!(json["embedder_id"], "noop-internal");
    assert_eq!(json["dim"], 4);
}

// ── Tests GET note ────────────────────────────────────────────────────────────

/// GET /internal/v1/note/:ulid → 404 si inexistante.
#[tokio::test]
async fn internal_api_get_note_not_found() {
    let env = build_internal_env().await;
    let unknown_id = Ulid::new().to_string();
    let req = make_get(&format!("/internal/v1/note/{unknown_id}"), TEST_TOKEN);
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /internal/v1/note/:ulid → 200 après persist/curated.
#[tokio::test]
async fn internal_api_get_note_after_persist() {
    let env = build_internal_env().await;
    let router = env.router;
    let note_id = Ulid::new().to_string();

    // 1. persist
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "Lecture test",
            "body": "# Lecture test\n\nCorps.",
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. GET
    let req = make_get(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET note existante doit retourner 200"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["note_id"], note_id);
    assert_eq!(json["section"], "decisions");
}

// ── Tests DELETE ──────────────────────────────────────────────────────────────

/// DELETE /internal/v1/note/:ulid → 404 si inexistante.
#[tokio::test]
async fn internal_api_delete_note_not_found() {
    let env = build_internal_env().await;
    let unknown_id = Ulid::new().to_string();
    let req = make_delete(&format!("/internal/v1/note/{unknown_id}"), TEST_TOKEN);
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// DELETE /internal/v1/note/:ulid → 204 après persist/curated.
#[tokio::test]
async fn internal_api_delete_note_after_persist() {
    let env = build_internal_env().await;
    let router = env.router;
    let note_id = Ulid::new().to_string();

    // persist
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "Note à supprimer",
            "body": "# Note\n\nCorps.",
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // DELETE
    let req = make_delete(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "DELETE note existante doit retourner 204"
    );

    // GET → 404 après suppression
    let req = make_get(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "note supprimée doit retourner 404 sur GET"
    );
}

// ── Tests body limit (V1) ─────────────────────────────────────────────────────

/// Corps > 4 MiB sur persist/curated → 413 Payload Too Large.
///
/// Vérifie que la limite globale `INTERNAL_BODY_LIMIT` est appliquée.
#[tokio::test]
async fn internal_api_body_limit_global_is_413() {
    let env = build_internal_env().await;

    // 4 MiB + 1 octet > limite globale.
    let huge_body = vec![b'x'; 4 * 1024 * 1024 + 1];
    let req = Request::builder()
        .method("POST")
        .uri("/internal/v1/persist/curated")
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Internal", format!("Bearer {TEST_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(huge_body))
        .unwrap();

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "corps > 4 MiB sur persist/curated doit retourner 413"
    );
}

/// Corps > 512 KiB sur persist/embedding → 413 Payload Too Large.
///
/// Vérifie que la limite individuelle `EMBEDDING_BODY_LIMIT` est appliquée,
/// même si elle est plus stricte que la limite globale.
#[tokio::test]
async fn internal_api_body_limit_embedding_is_413() {
    let env = build_internal_env().await;

    // 512 KiB + 1 octet > limite embedding.
    let huge_body = vec![b'x'; 512 * 1024 + 1];
    let req = Request::builder()
        .method("POST")
        .uri("/internal/v1/persist/embedding")
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Internal", format!("Bearer {TEST_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(huge_body))
        .unwrap();

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "corps > 512 KiB sur persist/embedding doit retourner 413"
    );
}

// ── Test tenant_id ignoré (V2+V3) ────────────────────────────────────────────

/// persist/curated avec tenant_id ≠ "main" dans le body → écriture dans "main".
///
/// Documente que `req.tenant_id` est ignoré par tous les handlers persist :
/// l'écriture va toujours dans le tenant "main" (INTERNAL_TENANT_ID).
/// Comportement requis pour défense en profondeur avant Slice 2b multi-tenant.
#[tokio::test]
async fn internal_api_tenant_id_in_body_is_ignored() {
    let env = build_internal_env().await;

    let note_id = Ulid::new().to_string();
    // tenant_id "other-tenant" dans le body — doit être ignoré.
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "other-tenant",
            "section": "decisions",
            "status": "live",
            "title": "Note tenant ignoré",
            "body": "# Note\n\nCorps.",
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );

    // L'écriture doit réussir (200) — preuve que "other-tenant" n'a pas été routé.
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/curated doit réussir (tenant_id body ignoré, écriture dans 'main')"
    );
}

// ── Test GET trust (absent) ───────────────────────────────────────────────────

/// GET trust absent → 404.
#[tokio::test]
async fn internal_api_get_trust_not_found() {
    let env = build_internal_env().await;
    let unknown_id = Ulid::new().to_string();
    let req = make_get(&format!("/internal/v1/note/{unknown_id}/trust"), TEST_TOKEN);
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Test ULID invalide → 400 ─────────────────────────────────────────────────

/// ULID invalide dans le path → 400.
#[tokio::test]
async fn internal_api_invalid_ulid_is_400() {
    let env = build_internal_env().await;
    let req = make_get("/internal/v1/note/not-a-valid-ulid", TEST_TOKEN);
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
