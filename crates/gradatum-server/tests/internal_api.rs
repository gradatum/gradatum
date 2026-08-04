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
use gradatum_core::identity::NoteId;
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

// ── Tests F-41 — compare-and-swap sur le VRAI handle_persist_curated ──────────
//
// Reproduit le red-proof LIVE (`experiments/01KYX2DXTT…`) : une écriture in-place avec un
// `expected_sha256` volontairement altéré doit produire un job `Conflict` (409) et laisser
// la note INTACTE — pas un 202 qui écrase. Ce test exerce le chemin de PRODUCTION réel
// (`handle_persist_curated` via `build_internal_router`), PAS un double `TestInternalClient`
// qui câblerait lui-même le CAS que la prod jetait (cf. golden test worker, fausse confiance).

/// Lit le corps markdown et le hash hex courant d'une note directement depuis le vault réel.
async fn read_body_and_hash(env: &InternalTestEnv, note_id: &str) -> (String, String) {
    let id = NoteId(Ulid::from_string(note_id).expect("ULID valide"));
    let note = env
        ._vault
        .read_note(id)
        .await
        .expect("note présente dans le vault");
    (note.body.markdown.clone(), note.content_hash.hex())
}

fn curated_body(note_id: &str, body: &str, expected_sha256: Option<&str>) -> serde_json::Value {
    json!({
        "note_id": note_id,
        "tenant_id": "main",
        "section": "decisions",
        "status": "live",
        "title": "F-41 CAS",
        "body": body,
        "tags": [],
        "author": null,
        "provenance": null,
        "temporal": null,
        "links": [],
        "trust": null,
        "expected_sha256": expected_sha256,
    })
}

/// F-41 — CREATE (sha absent) écrit ; RMW avec sha PÉRIMÉ → 409 + note intacte ;
/// RMW avec sha COURANT → 200 + note mise à jour. Verrou câblé sur le chemin de prod.
#[tokio::test]
async fn internal_api_persist_curated_optimistic_lock_cas() {
    let env = build_internal_env().await;
    let router = env.router.clone();
    let note_id = Ulid::new().to_string();

    // 1. CREATE (expected_sha256 = None) → écriture inconditionnelle, MARQUEUR-V1.
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        curated_body(&note_id, "MARQUEUR-V1", None),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "CREATE doit retourner 200");

    let (body_v1, hash_v1) = read_body_and_hash(&env, &note_id).await;
    assert_eq!(body_v1, "MARQUEUR-V1", "CREATE a bien écrit V1");

    // 2. RMW avec un sha VOLONTAIREMENT ALTÉRÉ → 409 Conflict, note NON écrasée.
    //    On retourne le hash courant d'un nibble pour garantir la divergence.
    let stale_hash: String = {
        let mut c = hash_v1.chars();
        let first = c.next().expect("hash non vide");
        let flipped = if first == '0' { '1' } else { '0' };
        std::iter::once(flipped).chain(c).collect()
    };
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        curated_body(&note_id, "MARQUEUR-V3", Some(&stale_hash)),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "RMW avec sha périmé doit retourner 409 (compare-and-swap), pas 200"
    );

    // C2 red-proof : le corps 409 DOIT être un JSON exposant le hash courant (v1) — sinon le
    // client interne ne peut pas renseigner WriteConflictDto.current_sha256 (contrat gelé F-41).
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let conflict_json: serde_json::Value =
        serde_json::from_slice(&body).expect("le corps 409 doit être du JSON exploitable");
    assert_eq!(
        conflict_json["current_sha256"].as_str(),
        Some(hash_v1.as_str()),
        "le corps 409 doit exposer le hash courant (v1) pour la résolution RMW"
    );

    // La note DOIT être restée intacte : MARQUEUR-V1, jamais MARQUEUR-V3.
    let (body_after_conflict, hash_after_conflict) = read_body_and_hash(&env, &note_id).await;
    assert_eq!(
        body_after_conflict, "MARQUEUR-V1",
        "un conflit ne doit JAMAIS écraser la note (red-proof LIVE)"
    );
    assert_eq!(
        hash_after_conflict, hash_v1,
        "le hash courant est inchangé après le conflit"
    );

    // 3. RMW avec le sha COURANT correct → 200, note mise à jour vers MARQUEUR-V2.
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        curated_body(&note_id, "MARQUEUR-V2", Some(&hash_v1)),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "RMW avec le sha courant correct doit réussir (200)"
    );
    let (body_v2, _hash_v2) = read_body_and_hash(&env, &note_id).await;
    assert_eq!(
        body_v2, "MARQUEUR-V2",
        "RMW avec le bon sha met à jour la note"
    );
}

/// F-41 — un `expected_sha256` syntaxiquement invalide sur le listener interne → 400,
/// avant toute écriture (fail-closed, parité avec la garde publique).
#[tokio::test]
async fn internal_api_persist_curated_malformed_expected_sha_is_400() {
    let env = build_internal_env().await;
    let note_id = Ulid::new().to_string();
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        curated_body(&note_id, "corps", Some("pas-un-hash")),
        TEST_TOKEN,
    );
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "expected_sha256 malformé doit retourner 400 avant tout write"
    );
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

    // persist (section NON protégée — la note doit être supprimable)
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "feedback",
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

/// F-100 P1-1 — le DELETE interne (chemin exact du job Purge) refuse une section
/// protégée avec **403** et préserve la note. Prouve la garde system-wide au niveau
/// du choke point via l'endpoint HTTP réel (mapping `Forbidden` → 403).
#[tokio::test]
async fn internal_api_delete_protected_section_is_403() {
    let env = build_internal_env().await;
    let router = env.router;
    let note_id = Ulid::new().to_string();

    // persist d'une note council (PROTECTED_DELETE). La garde cascade refuse quel que
    // soit le statut — l'endpoint interne DELETE ne gate pas sur `garbage` (c'est le
    // job Purge qui filtre en amont), donc une note council `live` suffit à la prouver.
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "council",
            "status": "live",
            "title": "Verdict à préserver",
            "body": "# Verdict\n\nCorps gouvernance.",
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

    // DELETE → 403 (refus dur de la garde, distinct d'un 404/500).
    let req = make_delete(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "DELETE d'une note council doit retourner 403 (PROTECTED_DELETE)"
    );

    // GET → 200 : la note est TOUJOURS présente (aucune mutation).
    let req = make_get(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "la note council refusée doit rester présente (GET 200)"
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

// ── Test tenant_id HONORÉ — routage vault (C4-1b, P0 security review) ─────────

/// persist/curated avec `tenant_id` ≠ "main" → écriture DANS CE VAULT (plus dans "main").
///
/// RÉGRESSION du P0 security review (C4-1b) : le handler hardcodait
/// `vault_id = INTERNAL_TENANT_ID = "main"` en IGNORANT `req.tenant_id` — une écriture d'un
/// tenant tiers (propagée par le worker depuis un `vault_write` authentifié) atterrissait dans
/// `main` (vecteur write tiers→main). Le fix dérive `frontmatter.vault_id` + l'index du
/// `req.tenant_id` propagé. Ce listener est loopback + token interne : `req.tenant_id` est fixé
/// par le worker APRÈS `effective_write_vault` (ACL) + `ensure_job_tenant` (impose `main` à flag
/// OFF). On vérifie ici que la note atterrit sous `research`, PAS sous `main`.
#[tokio::test]
async fn internal_api_persist_curated_routes_to_request_tenant_vault() {
    let env = build_internal_env().await;
    let index_path = gradatum_core::paths::vault_dir_index_path(&env._tmp.path().join("vault"));

    // Lot REG : le vault cible doit être inscrit au registre de données pour accepter une
    // note. Ce seed est le PRÉREQUIS que la garde exige — pas un contournement : sans lui,
    // `research` serait un 6e orphelin du type de ceux mesurés sur le LIVE.
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index.db");
        conn.execute(
            "INSERT OR IGNORE INTO tenants (id, status, created_at) VALUES ('research', 'active', 0)",
            [],
        )
        .expect("seed tenants research");
    }

    let note_id = Ulid::new().to_string();
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "research",
            "section": "decisions",
            "status": "live",
            "title": "Note vault research",
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

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/curated doit réussir"
    );

    // La note DOIT être indexée sous `research`, et ABSENTE de `main` (pas de vecteur tiers→main).
    let conn = rusqlite::Connection::open(&index_path).expect("open index.db");
    let vault_id: String = conn
        .query_row(
            "SELECT vault_id FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("note présente");
    assert_eq!(
        vault_id, "research",
        "la note doit atterrir dans le vault du tenant requête, pas 'main'"
    );
    let in_main: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1 AND vault_id = 'main'",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("count main");
    assert_eq!(
        in_main, 0,
        "aucune écriture dans 'main' (vecteur tiers→main fermé)"
    );
}

/// Byte-identical : persist/curated avec `tenant_id = "main"` atterrit dans `main` (parc actuel
/// flag OFF — le worker impose `main` en amont, donc ce chemin reste strictement inchangé).
#[tokio::test]
async fn internal_api_persist_curated_main_lands_in_main() {
    let env = build_internal_env().await;
    let index_path = gradatum_core::paths::vault_dir_index_path(&env._tmp.path().join("vault"));

    let note_id = Ulid::new().to_string();
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "Note main",
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

    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let conn = rusqlite::Connection::open(&index_path).expect("open index.db");
    let vault_id: String = conn
        .query_row(
            "SELECT vault_id FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("note présente");
    assert_eq!(
        vault_id, "main",
        "tenant main → vault main (byte-identical)"
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

// ── Test persist/distill tags (F-43 Task 3) ──────────────────────────────────

/// POST persist/distill with `tags: ["quality-low"]` on a NEW note → re-read → frontmatter
/// contains tag `quality-low`.
///
/// Verifies that `PersistDistillRequest.tags` is forwarded to `parse_tags` and stored in
/// the note frontmatter on creation (first call, note absent from vault).
#[tokio::test]
async fn persist_distill_tags() {
    let env = build_internal_env().await;
    let router = env.router;
    let note_id = Ulid::new().to_string();

    // POST /internal/v1/persist/distill — new note with tags: ["quality-low"]
    let req = make_request(
        "POST",
        "/internal/v1/persist/distill",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "reference",
            "title": "Distilled synthesis",
            "body": "# Synthesis\n\nContent.",
            "trust": 0.5,
            "expected_sha256": null,
            "mark_processed": false,
            "derived_into": null,
            "derived_from": [],
            "tags": ["quality-low"]
        }),
        TEST_TOKEN,
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/distill with tags must return 200"
    );

    // Re-read the note via GET /internal/v1/note/:ulid and verify the tag is present.
    let req = make_get(&format!("/internal/v1/note/{note_id}"), TEST_TOKEN);
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET note after distill must return 200"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let note: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tags = note["tags"].as_array().expect("tags must be an array");
    assert!(
        tags.iter().any(|t| t.as_str() == Some("quality-low")),
        "frontmatter must contain tag 'quality-low', got: {tags:?}"
    );
}

// ── Tests F-112 — GET /internal/v1/notes/count-unprocessed ───────────────────

/// Seed une note curated (status paramétrable) puis pose son locus index-level.
async fn seed_note_with_locus(
    env: &InternalTestEnv,
    section: &str,
    status: &str,
    locus: &str,
) -> String {
    let note_id = Ulid::new().to_string();
    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": section,
            "status": status,
            "title": format!("Note {note_id}"),
            "body": format!("# Note {note_id}\n\nCorps."),
            "tags": [],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null
        }),
        TEST_TOKEN,
    );
    let resp = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "seed persist/curated");

    set_locus(env, &note_id, locus).await;
    note_id
}

/// Pose le locus d'une note (mutation index-level, pattern move_locus F-37 S1.4).
async fn set_locus(env: &InternalTestEnv, note_id: &str, locus: &str) {
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::LocusId;
    let id = NoteId(Ulid::from_string(note_id).expect("ULID seedé valide"));
    env._vault
        .index()
        .update_note_locus(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            &id,
            &LocusId::new(locus),
        )
        .await
        .expect("update_note_locus fixture");
}

/// Marque une note `processed=true` via persist/distill (mark_processed).
async fn mark_processed(env: &InternalTestEnv, note_id: &str, section: &str) {
    let req = make_request(
        "POST",
        "/internal/v1/persist/distill",
        json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": section,
            "title": "Marquée processed",
            "body": "# Marquée\n\nCorps.",
            "trust": null,
            "expected_sha256": null,
            "mark_processed": true,
            "derived_into": null,
            "derived_from": [],
            "tags": []
        }),
        TEST_TOKEN,
    );
    let resp = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "mark_processed persist/distill"
    );
}

async fn get_count(env: &InternalTestEnv, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = make_get(uri, TEST_TOKEN);
    let resp = env.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// F-112 : count-unprocessed — live + !processed seulement, locus filtré.
#[tokio::test]
async fn count_unprocessed_counts_live_unprocessed_in_locus() {
    let env = build_internal_env().await;

    // n1 : live, processed absent, locus debug → comptée.
    seed_note_with_locus(&env, "debug", "live", "debug").await;
    // n2 : live, processed=true, locus debug → exclue.
    // (mark_processed réécrit le .md → poser le locus APRÈS, l'upsert hash-changé
    //  appliquerait excluded.locus=NULL sinon.)
    let n2 = seed_note_with_locus(&env, "debug", "live", "debug").await;
    mark_processed(&env, &n2, "debug").await;
    set_locus(&env, &n2, "debug").await;
    // n3 : draft (non-live), locus debug → exclue.
    seed_note_with_locus(&env, "debug", "draft", "debug").await;
    // n4 : live, autre locus → exclue.
    seed_note_with_locus(&env, "experiments", "live", "experiments").await;

    let (status, json) = get_count(
        &env,
        "/internal/v1/notes/count-unprocessed?vault=main&locus=debug",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["count"], 1, "1 seule note live non-processed : {json}");
}

/// F-112 : paramètre `locus` absent → 400.
#[tokio::test]
async fn count_unprocessed_missing_locus_is_400() {
    let env = build_internal_env().await;
    let (status, _) = get_count(&env, "/internal/v1/notes/count-unprocessed?vault=main").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// F-112 P2-4 : early-exit — `min` atteint plafonne le count retourné.
#[tokio::test]
async fn count_unprocessed_min_caps_count() {
    let env = build_internal_env().await;
    for _ in 0..3 {
        seed_note_with_locus(&env, "debug", "live", "debug").await;
    }

    let (status, json) = get_count(
        &env,
        "/internal/v1/notes/count-unprocessed?vault=main&locus=debug&min=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["count"], 2, "count plafonné à min=2 : {json}");
}

// ── Lot REG — invariant de registre au point d'écriture de note ──────────────

/// Lot REG : une note ne naît pas dans un vault absent des DEUX registres.
///
/// C'est le trou par lequel `default` et `test` sont entrés sur le LIVE (5 notes, aucun
/// registre). Le discriminant est le CODE : un vault non provisionné ferait de toute façon
/// échouer l'écriture vault en aval — mais en 500, après avoir engagé le pipeline. 403
/// prouve que le refus vient de la garde de registre, en tête de handler.
#[tokio::test]
async fn persist_curated_refuses_a_vault_absent_from_both_registries() {
    let env = build_internal_env().await;

    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": Ulid::new().to_string(),
            "tenant_id": "default",
            "section": "decisions",
            "status": "live",
            "title": "Note orpheline",
            "body": "# Note orpheline\n\nCorps.",
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
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "refus de registre attendu (403), pas un échec d'écriture en aval (500)"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let msg = String::from_utf8_lossy(&body);
    assert!(
        msg.contains("is not registered in any registry"),
        "le refus doit nommer sa cause, got: {msg}"
    );
}

/// Lot REG : une note de données n'atterrit jamais dans un vault de CODE.
///
/// Test distinct du précédent : il exerce l'autre barreau (préfixe `code-`), qui mord même
/// quand le vault EST inscrit quelque part — ici, au registre de code.
#[tokio::test]
async fn persist_curated_refuses_a_code_vault() {
    let env = build_internal_env().await;

    let req = make_request(
        "POST",
        "/internal/v1/persist/curated",
        json!({
            "note_id": Ulid::new().to_string(),
            "tenant_id": "code-gradatum",
            "section": "decisions",
            "status": "live",
            "title": "Note de donnees dans un vault de code",
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
    let resp = env.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let msg = String::from_utf8_lossy(&body);
    assert!(
        msg.contains("belongs to the code registry"),
        "le refus doit distinguer le registre de code, got: {msg}"
    );
}
