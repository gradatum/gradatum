//! Tests du client HTTP admin (F-100 1.6) contre un serveur mock (wiremock).
//!
//! Vérifient : lecture du token depuis fichier, envoi du header `X-Gradatum-Admin`,
//! sérialisation de la requête, désérialisation de la réponse.

use std::io::Write as _;

use gradatum_admin::admin_client::AdminClient;
use gradatum_dto::{
    VaultArchivesListRequest, VaultArchivesPurgeRequest, VaultArchivesRestoreRequest,
    VaultDeleteRequest,
};
use wiremock::matchers::{body_json_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Écrit un token dans un fichier temporaire et renvoie son handle (à garder vivant).
fn token_file(token: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile token");
    writeln!(f, "{token}").expect("write token");
    f
}

#[tokio::test]
async fn delete_sends_admin_header_and_parses_result() {
    let server = MockServer::start().await;
    let tf = token_file("secret-admin-token-abc");

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/delete"))
        .and(header("X-Gradatum-Admin", "Bearer secret-admin-token-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_id": "01HTEST00000000000000000AB",
            "deleted": true,
            "archived_path": ".archive/main/01HTEST00000000000000000AB.md"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let req = VaultDeleteRequest {
        note_id: "01HTEST00000000000000000AB".to_string(),
        dry_run: false,
        confirm_ulids: vec!["01HTEST00000000000000000AB".to_string()],
        tenant_id: Some("main".to_string().into()),
    };
    let resp = client.delete(&req).await.expect("delete ok");
    assert_eq!(resp["deleted"], true);
    assert_eq!(
        resp["archived_path"],
        ".archive/main/01HTEST00000000000000000AB.md"
    );
}

#[tokio::test]
async fn archives_list_parses_typed_response() {
    let server = MockServer::start().await;
    let tf = token_file("tok");

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/archives/list"))
        .and(header("X-Gradatum-Admin", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "note_id": "01HTEST00000000000000000AB",
                "section": "feedback",
                "archive_path": ".archive/main/01HTEST00000000000000000AB.md",
                "archived_at": 1000,
                "gc_due": 2000
            }],
            "limit": 50,
            "offset": 0,
            "count": 1
        })))
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let resp = client
        .archives_list(&VaultArchivesListRequest {
            vault_filter: None,
            section: None,
            since_ms: None,
            until_ms: None,
            include_gc: false,
            include_restored: false,
            limit: 50,
            offset: 0,
            tenant_id: Some("main".to_string().into()),
        })
        .await
        .expect("archives_list ok");
    assert_eq!(resp.count, 1);
    assert_eq!(resp.entries[0].section, "feedback");
}

#[tokio::test]
async fn purge_dry_run_sends_expected_body() {
    let server = MockServer::start().await;
    let tf = token_file("tok");

    // Le corps envoyé par un purge dry-run (execute=false → confirm_ulids omis).
    let expected_body =
        r#"{"note_id":"01HTEST00000000000000000AB","dry_run":true,"tenant_id":"main"}"#;

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/archives/purge"))
        .and(header("X-Gradatum-Admin", "Bearer tok"))
        .and(body_json_string(expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_id": "01HTEST00000000000000000AB",
            "dry_run": true,
            "purged": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let resp = client
        .archives_purge(&VaultArchivesPurgeRequest {
            note_id: "01HTEST00000000000000000AB".to_string(),
            dry_run: true,
            confirm_ulids: Vec::new(),
            tenant_id: Some("main".to_string().into()),
        })
        .await
        .expect("purge ok");
    assert!(resp.dry_run);
    assert!(!resp.purged);
}

#[tokio::test]
async fn restore_dry_run_sends_expected_body() {
    let server = MockServer::start().await;
    let tf = token_file("tok");

    // Le corps envoyé par un restore dry-run (execute=false → confirm_ulids omis).
    let expected_body =
        r#"{"note_id":"01HTEST00000000000000000AB","dry_run":true,"tenant_id":"main"}"#;

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/archives/restore"))
        .and(header("X-Gradatum-Admin", "Bearer tok"))
        .and(body_json_string(expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_id": "01HTEST00000000000000000AB",
            "dry_run": true,
            "restored": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let resp = client
        .archives_restore(&VaultArchivesRestoreRequest {
            note_id: "01HTEST00000000000000000AB".to_string(),
            dry_run: true,
            confirm_ulids: Vec::new(),
            tenant_id: Some("main".to_string().into()),
        })
        .await
        .expect("restore ok");
    assert!(resp.dry_run);
    assert!(!resp.restored);
    assert!(resp.status.is_none());
}

#[tokio::test]
async fn restore_real_parses_status() {
    let server = MockServer::start().await;
    let tf = token_file("tok");

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/archives/restore"))
        .and(header("X-Gradatum-Admin", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_id": "01HTEST00000000000000000AB",
            "dry_run": false,
            "restored": true,
            "status": "pending-review",
            "restored_path": "main/01HTEST00000000000000000AB.md"
        })))
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let resp = client
        .archives_restore(&VaultArchivesRestoreRequest {
            note_id: "01HTEST00000000000000000AB".to_string(),
            dry_run: false,
            confirm_ulids: vec!["01HTEST00000000000000000AB".to_string()],
            tenant_id: Some("main".to_string().into()),
        })
        .await
        .expect("restore ok");
    assert!(resp.restored);
    assert_eq!(resp.status.as_deref(), Some("pending-review"));
    assert_eq!(
        resp.restored_path.as_deref(),
        Some("main/01HTEST00000000000000000AB.md")
    );
}

#[tokio::test]
async fn non_2xx_is_error() {
    let server = MockServer::start().await;
    let tf = token_file("tok");

    Mock::given(method("POST"))
        .and(path("/internal/v1/admin/delete"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = AdminClient::new(&server.uri(), tf.path()).expect("AdminClient::new");
    let err = client
        .delete(&VaultDeleteRequest {
            note_id: "x".to_string(),
            dry_run: true,
            confirm_ulids: Vec::new(),
            tenant_id: Some("main".to_string().into()),
        })
        .await
        .expect_err("401 doit être une erreur");
    assert!(
        err.to_string().contains("401"),
        "erreur porte le statut: {err}"
    );
}

#[tokio::test]
async fn empty_token_file_is_error() {
    let tf = token_file("   ");
    let err =
        AdminClient::new("http://127.0.0.1:19092", tf.path()).expect_err("token vide doit échouer");
    assert!(err.to_string().contains("empty"), "message: {err}");
}
