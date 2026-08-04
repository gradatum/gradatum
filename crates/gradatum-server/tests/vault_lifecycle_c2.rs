//! Tests E2E C2 (F-18, EX-C2-4) — cycle de vie des vaults sur l'API admin interne.
//!
//! `POST /internal/v1/admin/vaults/create|suspend|delete` : provisioning idempotent,
//! suspend réversible, soft-delete, garde du vault racine `main`, 404 vault inconnu,
//! 400 vault_id mal formé, auth admin (token + loopback) fail-closed.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_core::index::Index;
use gradatum_core::scope::{TenantId, VaultId};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

use gradatum_server::{internal, state::AppState};

/// Token admin de test (≥ 32 caractères, longueur publique-par-design).
const ADMIN_TOKEN: &str = "test-admin-token-0123456789abcdef";

/// Adresse loopback synthétique injectée dans les extensions (ConnectInfo).
fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40001))
}

struct LifecycleEnv {
    app: Router,
    idx: Arc<SqliteIndex>,
    _tmp: TempDir,
}

/// Routeur interne (admin) + index réel partagé (migrations 0001-0031 appliquées).
async fn build_app() -> LifecycleEnv {
    use gradatum_auth::jwt::JwtService;

    let tmp = TempDir::new().expect("TempDir vault_lifecycle_c2");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create — vault_lifecycle_c2"),
    );
    let idx = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str("").expect("preset ACL vault_lifecycle_c2");

    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_admin_api_token(secrecy::SecretString::from(ADMIN_TOKEN.to_string()));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = internal::build_internal_router(state);
    LifecycleEnv {
        app,
        idx,
        _tmp: tmp,
    }
}

/// POST admin authentifié → `(status, body_json_string)`.
async fn post_admin(
    app: &Router,
    path: &str,
    vault_id: &str,
    token: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json")
        .extension(ConnectInfo(loopback()));
    if let Some(t) = token {
        builder = builder.header("X-Gradatum-Admin", format!("Bearer {t}"));
    }
    let body = serde_json::json!({ "vault_id": vault_id });
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).expect("json")))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// POST admin authentifié avec body arbitraire (purge) → `(status, body_json_string)`.
async fn post_admin_json(
    app: &Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Admin", format!("Bearer {ADMIN_TOKEN}"))
        .extension(ConnectInfo(loopback()))
        .body(Body::from(serde_json::to_vec(&body).expect("json")))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// create : provisionne (200, changed=true) puis re-jeu idempotent (200, changed=false) ;
/// le tenant devient actif avec un self-grant write consultable.
#[tokio::test]
async fn create_is_idempotent_and_grants_self_write() {
    let env = build_app().await;

    let (status, body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create : {body}");
    assert!(
        body.contains("\"changed\":true"),
        "1er create : changed=true, {body}"
    );
    assert!(body.contains("\"status\":\"active\""));

    // Re-jeu : aucun effet, toujours 200.
    let (status2, body2) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status2, StatusCode::OK);
    assert!(
        body2.contains("\"changed\":false"),
        "re-jeu idempotent : changed=false, {body2}"
    );

    // Le tenant est actif + self-grant write (JOIN status='active' de C1).
    let vaults = env
        .idx
        .list_active_vaults()
        .await
        .expect("list_active_vaults");
    assert!(vaults.contains(&"research".to_string()));
    let grants = env
        .idx
        .tenant_grants(&TenantId::new("research"))
        .await
        .expect("tenant_grants");
    assert_eq!(grants.len(), 1);
    assert!(grants[0].access.allows_write(), "self-grant write attendu");
}

/// create : vault_id mal formé → 400 (`VaultId::parse`, P2-a).
#[tokio::test]
async fn create_malformed_vault_id_is_400() {
    let env = build_app().await;
    let (status, _body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "Bad Vault!",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// suspend : le tenant disparaît des vaults actifs ET ses grants ne sont plus
/// consultables (refus immédiat via le JOIN `status='active'`) ; idempotent au re-jeu.
#[tokio::test]
async fn suspend_removes_grants_immediately_and_is_idempotent() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;

    let (status, body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/suspend",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "suspend : {body}");
    assert!(body.contains("\"status\":\"suspended\""));
    assert!(body.contains("\"changed\":true"));

    let vaults = env
        .idx
        .list_active_vaults()
        .await
        .expect("list_active_vaults");
    assert!(
        !vaults.contains(&"research".to_string()),
        "suspendu = plus actif"
    );
    let grants = env
        .idx
        .tenant_grants(&TenantId::new("research"))
        .await
        .expect("tenant_grants");
    assert!(
        grants.is_empty(),
        "refus immédiat : plus aucun grant consultable"
    );

    // Re-jeu idempotent.
    let (status2, body2) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/suspend",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status2, StatusCode::OK);
    assert!(body2.contains("\"changed\":false"));
}

/// soft-delete : statut `deleted`, hors vaults actifs — la purge physique est différée.
#[tokio::test]
async fn soft_delete_marks_deleted_without_touching_notes() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    // Une note du vault research — le soft-delete ne doit PAS y toucher (A5).
    env.idx
        .seed_note_with_fts_vault(
            "01HRESEARCHAAAAAAAAAAAAAAA",
            "research",
            "reference",
            None,
            "corps",
        )
        .await
        .expect("seed note research");

    let (status, body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/delete",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete : {body}");
    assert!(body.contains("\"status\":\"deleted\""));

    let vaults = env
        .idx
        .list_active_vaults()
        .await
        .expect("list_active_vaults");
    assert!(!vaults.contains(&"research".to_string()));

    // La note existe toujours (purge différée, aucun ALTER/DELETE sur notes).
    let (count, _capped) = env
        .idx
        .count_fts_matches(
            &VaultId::new("research"),
            "\"corps\"",
            false,
            None,
            None,
            None,
        )
        .await
        .expect("count notes research");
    assert_eq!(count, 1, "soft-delete : les notes restent en place");
}

/// Le vault racine `main` est refusé sur suspend ET delete (403) — safety cap.
#[tokio::test]
async fn root_vault_main_cannot_be_suspended_or_deleted() {
    let env = build_app().await;
    for path in [
        "/internal/v1/admin/vaults/suspend",
        "/internal/v1/admin/vaults/delete",
    ] {
        let (status, body) = post_admin(&env.app, path, "main", Some(ADMIN_TOKEN)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} main : {body}");
    }
}

/// suspend/delete d'un vault inconnu → 404.
#[tokio::test]
async fn unknown_vault_is_404() {
    let env = build_app().await;
    let (status, _body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/suspend",
        "ghost",
        Some(ADMIN_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// purge : fail-closed sur le statut — un vault `active` puis `suspended` n'est
/// JAMAIS purgeable (409), même en dry-run.
#[tokio::test]
async fn purge_refuses_vault_not_soft_deleted() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;

    let body = serde_json::json!({ "vault_id": "research" });
    let (status, resp) =
        post_admin_json(&env.app, "/internal/v1/admin/vaults/purge", body.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "actif : {resp}");

    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/suspend",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    let (status2, resp2) = post_admin_json(&env.app, "/internal/v1/admin/vaults/purge", body).await;
    assert_eq!(status2, StatusCode::CONFLICT, "suspendu : {resp2}");
}

/// purge : garde `main` (403) et tenant inconnu (404) — mêmes gardes que suspend/delete.
#[tokio::test]
async fn purge_main_is_403_and_unknown_is_404() {
    let env = build_app().await;
    let (status, _b) = post_admin_json(
        &env.app,
        "/internal/v1/admin/vaults/purge",
        serde_json::json!({ "vault_id": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status2, _b2) = post_admin_json(
        &env.app,
        "/internal/v1/admin/vaults/purge",
        serde_json::json!({ "vault_id": "ghost" }),
    )
    .await;
    assert_eq!(status2, StatusCode::NOT_FOUND);
}

/// purge dry-run (défaut serde) : bilan d'éligibilité sans AUCUNE destruction.
#[tokio::test]
async fn purge_dry_run_reports_eligible_without_deleting() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    env.idx
        .seed_note_with_fts_vault(
            "01HRESEARCHAAAAAAAAAAAAAAA",
            "research",
            "reference",
            None,
            "corps",
        )
        .await
        .expect("seed note research");
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/delete",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;

    let (status, body) = post_admin_json(
        &env.app,
        "/internal/v1/admin/vaults/purge",
        serde_json::json!({ "vault_id": "research" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dry-run : {body}");
    assert!(body.contains("\"dry_run\":true"), "{body}");
    assert!(body.contains("\"eligible\":1"), "{body}");
    assert!(body.contains("\"deleted\":0"), "{body}");

    let (count, _capped) = env
        .idx
        .count_fts_matches(
            &VaultId::new("research"),
            "\"corps\"",
            false,
            None,
            None,
            None,
        )
        .await
        .expect("count notes research");
    assert_eq!(count, 1, "dry-run : la note reste en place");
}

/// purge réelle : exige la double confirmation `confirm_vault_id == vault_id` (400).
#[tokio::test]
async fn purge_real_without_confirm_is_400() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/delete",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    let (status, body) = post_admin_json(
        &env.app,
        "/internal/v1/admin/vaults/purge",
        serde_json::json!({ "vault_id": "research", "dry_run": false }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// purge réelle : vide le vault (index) puis re-jeu idempotent `eligible=0` ;
/// le tombstone `tenants.status='deleted'` reste en place (trace registre).
#[tokio::test]
async fn purge_real_empties_vault_and_is_idempotent() {
    let env = build_app().await;
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;
    env.idx
        .seed_note_with_fts_vault(
            "01HRESEARCHAAAAAAAAAAAAAAA",
            "research",
            "reference",
            None,
            "corps",
        )
        .await
        .expect("seed note research");
    post_admin(
        &env.app,
        "/internal/v1/admin/vaults/delete",
        "research",
        Some(ADMIN_TOKEN),
    )
    .await;

    let real = serde_json::json!({
        "vault_id": "research", "dry_run": false, "confirm_vault_id": "research"
    });
    let (status, body) =
        post_admin_json(&env.app, "/internal/v1/admin/vaults/purge", real.clone()).await;
    assert_eq!(status, StatusCode::OK, "purge réelle : {body}");
    assert!(body.contains("\"deleted\":1"), "{body}");
    assert!(body.contains("\"remaining\":0"), "{body}");

    let (count, _capped) = env
        .idx
        .count_fts_matches(
            &VaultId::new("research"),
            "\"corps\"",
            false,
            None,
            None,
            None,
        )
        .await
        .expect("count notes research");
    assert_eq!(count, 0, "purge réelle : index vidé");

    // Re-jeu idempotent : plus rien d'éligible, toujours 200.
    let (status2, body2) = post_admin_json(&env.app, "/internal/v1/admin/vaults/purge", real).await;
    assert_eq!(status2, StatusCode::OK);
    assert!(body2.contains("\"eligible\":0"), "{body2}");

    // Tombstone : le tenant reste `deleted` (jamais réactivé par la purge).
    let vaults = env
        .idx
        .list_active_vaults()
        .await
        .expect("list_active_vaults");
    assert!(!vaults.contains(&"research".to_string()));
}

/// Auth admin fail-closed : token absent → 401 (aucune mutation).
#[tokio::test]
async fn missing_admin_token_is_401() {
    let env = build_app().await;
    let (status, _body) = post_admin(
        &env.app,
        "/internal/v1/admin/vaults/create",
        "research",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let vaults = env
        .idx
        .list_active_vaults()
        .await
        .expect("list_active_vaults");
    assert!(
        !vaults.contains(&"research".to_string()),
        "aucune mutation sans token"
    );
}
