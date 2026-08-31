//! Tests d'intégration HTTP pour `HttpVaultClient::existing_titles`.
//!
//! ## Objectif
//!
//! Les `MockVaultClient` unitaires court-circuitent le parsing JSON : ils prouvent que
//! la garde du titre bloque, pas qu'elle voit quoi que ce soit du serveur réel. C'est
//! précisément la faille qui a produit les doublons de juin 2026 — `marker_exists`
//! lisait `payload["results"]` alors que l'API répond `items`, si bien que la garde
//! était constamment vraie et constamment aveugle (commit 58f334b3).
//!
//! Ces tests exercent donc le vrai code de parsing contre un faux serveur HTTP, pour
//! que le champ `title` de `vault_read` soit vérifié et non supposé.
//!
//! ## Cas couverts
//!
//! 1. **existing_titles_parses_the_title_field** : `vault_list` liste les cartes,
//!    `vault_read` renvoie leur `title` → l'index est peuplé.
//! 2. **existing_titles_skips_a_note_without_title** : `title: null` → la note est
//!    écartée (elle ne peut pas collisionner sur l'axe du titre).
//! 3. **existing_titles_follows_vault_list_pagination** : la seconde page de
//!    `vault_list` est suivie, sinon une carte échapperait à la garde.
//!
//! ## Garde-fou
//!
//! Wiremock démarre un faux serveur sur un port éphémère : ces tests ne touchent
//! jamais le serveur LIVE.

use gradatum_admin::changelog_backfill::{HttpVaultClient, VaultWriteClient};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Monte la route `/auth/exchange`, requise par `HttpVaultClient::new`.
async fn mount_auth(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token",
            "ttl_secs": 86400,
            "scopes": ["read", "write"],
            "tenant_id": "main",
            "kid": "test-kid"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn existing_titles_parses_the_title_field() {
    let server = MockServer::start().await;
    mount_auth(&server).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{ "path": "project-map/01AAA", "size_bytes": 180 }],
            "next_cursor": null,
            "total": 1
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/01AAA",
            "title": "[PROJECT-MAP][gradatum] Warden: Network Access Control Layer — v0.1.0",
            "content": "…",
            "metadata": null,
            "size_bytes": 180,
            "sha256": "0".repeat(64)
        })))
        .mount(&server)
        .await;

    let client = HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new avec le mock /auth/exchange");

    let entries = client.existing_titles().await.expect("existing_titles");

    assert_eq!(entries.len(), 1, "la carte listée doit être indexée");
    assert_eq!(entries[0].0, "project-map/01AAA");
    assert_eq!(
        entries[0].1, "[PROJECT-MAP][gradatum] Warden: Network Access Control Layer — v0.1.0",
        "le champ lu doit être `title` — c'est ici que le bug de juin s'était logé"
    );
}

#[tokio::test]
async fn existing_titles_skips_a_note_without_title() {
    let server = MockServer::start().await;
    mount_auth(&server).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{ "path": "project-map/01BBB", "size_bytes": 12 }],
            "next_cursor": null,
            "total": 1
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/01BBB",
            "title": null,
            "content": "sans titre",
            "metadata": null,
            "size_bytes": 12,
            "sha256": "0".repeat(64)
        })))
        .mount(&server)
        .await;

    let client = HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let entries = client.existing_titles().await.expect("existing_titles");

    assert!(
        entries.is_empty(),
        "une note sans titre ne peut pas collisionner sur l'axe du titre"
    );
}

#[tokio::test]
async fn existing_titles_follows_vault_list_pagination() {
    let server = MockServer::start().await;
    mount_auth(&server).await;

    // Page 2 : la requête porte le curseur. Montée en premier — wiremock donne la
    // priorité au mock le plus spécifique déclaré en dernier, on borne donc par le
    // corps de la requête.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .and(body_string_contains("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{ "path": "project-map/01DDD", "size_bytes": 180 }],
            "next_cursor": null,
            "total": 2
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{ "path": "project-map/01CCC", "size_bytes": 180 }],
            "next_cursor": "01CCC",
            "total": 2
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/01CCC",
            "title": "[PROJECT-MAP][gradatum] Une carte — v1.0.0",
            "content": "…",
            "metadata": null,
            "size_bytes": 180,
            "sha256": "0".repeat(64)
        })))
        .mount(&server)
        .await;

    let client = HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let entries = client.existing_titles().await.expect("existing_titles");

    assert_eq!(
        entries.len(),
        2,
        "la seconde page doit être suivie, sinon une carte échappe à la garde"
    );
}
