//! Tests handler `POST /api/v1/vault_timeline` (F-55 zone D).
//!
//! Harness partagé `helpers/mod.rs` : `build_app` (Vault réel sur TempDir + ACL
//! `alpha13-tester` read scope) + `sign_token`. Cas : 401 (sans auth), 200
//! (items triés DESC), 400 (fenêtre inversée), 403 (ACL deny consumer inconnu).

#[path = "helpers/mod.rs"]
mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_auth::jwt::TokenScope;
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use http_body_util::BodyExt;
use tower::ServiceExt;

use helpers::{build_app, sign_token};

/// Effectue `POST /api/v1/vault_timeline` avec un body JSON et retourne la `Response`.
async fn post_timeline(
    app: axum::Router,
    token: Option<&str>,
    body: serde_json::Value,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .uri("/api/v1/vault_timeline")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.expect("vault_timeline oneshot")
}

#[tokio::test]
async fn timeline_requires_auth() {
    let env = build_app().await;
    let res = post_timeline(env.app.clone(), None, serde_json::json!({})).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn timeline_returns_items_desc() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Seed 2 notes temporelles via Vault::write_note (fichier .md + index) puis
    // write_temporal_entry sur l'index partagé (state.search == vault.index()).
    let id1 = env
        .write_note_in_section("decisions", "Note ancienne", "corps a")
        .await;
    let id2 = env
        .write_note_in_section("decisions", "Note récente", "corps b")
        .await;
    for (id, anchor) in [(&id1, 1_000_i64), (&id2, 2_000_i64)] {
        env.state
            .search
            .write_temporal_entry(&TemporalEntry {
                note_id: id.0.to_string(),
                vault_id: "main".to_string(),
                anchor_ms: anchor,
                anchor_src: AnchorSrc::Created,
                doc_kind: "Event".to_string(),
                valid_until_ms: None,
            })
            .await
            .expect("write_temporal_entry seed");
    }

    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "limit": 10 }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "deux notes temporelles seedées");
    // Tri anchor_ms DESC : la note récente (2000) d'abord.
    assert_eq!(items[0]["anchor_ms"].as_i64(), Some(2_000));
    assert_eq!(items[1]["anchor_ms"].as_i64(), Some(1_000));
    assert!(body.get("next_cursor").is_some(), "next_cursor présent");
    assert!(
        body["next_cursor"].is_null(),
        "next_cursor null (page non pleine)"
    );
}

#[tokio::test]
async fn timeline_rejects_inverted_window() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "from_ms": 3000, "to_ms": 1000 }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// M-1 — un doc_kind hors whitelist (token valide) → 400 (jamais 200/500).
#[tokio::test]
async fn timeline_rejects_unknown_doc_kind() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "doc_kind": ["BogusKind"] }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// V4 wire — un cursor malformé (token valide) est rejeté en 400 par le handler.
#[tokio::test]
async fn timeline_rejects_malformed_cursor() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "cursor": "pasvalide" }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn timeline_acl_deny_returns_403() {
    let env = build_app().await;
    // Token authentifié mais identité inconnue du preset ACL → default deny (403).
    let deny_token = env
        .state
        .jwt
        .sign(
            "consumer-inconnu-403",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign deny token");
    let res = post_timeline(env.app.clone(), Some(&deny_token), serde_json::json!({})).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

// ── Tests Lot 3 — DTO as_of_ms + include_expired + rétrocompat (v0.5.1) ─────

/// Cas h — rétrocompat : timeline sans as_of_ms ni include_expired → 200 identique v0.5.0.
#[tokio::test]
async fn timeline_retrocompat_no_validity_params_returns_200() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    // Seed une note
    let id1 = env
        .write_note_in_section("decisions", "Note rétrocompat", "corps rétro")
        .await;
    env.state
        .search
        .write_temporal_entry(&TemporalEntry {
            note_id: id1.0.to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 1_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Event".to_string(),
            valid_until_ms: None,
        })
        .await
        .expect("seed temporal entry");

    // Appel sans as_of_ms ni include_expired → comportement v0.5.0
    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "limit": 10 }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = body["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .any(|i| i["note_id"].as_str() == Some(&id1.0.to_string())),
        "cas h : note visible sans filtrage validité (rétrocompat v0.5.0)"
    );
}

/// as_of filtre une note expirée : elle doit être exclue.
#[tokio::test]
async fn timeline_as_of_excludes_expired_notes() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let id_live = env
        .write_note_in_section("decisions", "Note live", "corps live")
        .await;
    let id_expired = env
        .write_note_in_section("decisions", "Note expirée", "corps expiré")
        .await;

    // Note live : anchor=1000, valid_until=None (toujours valide)
    env.state
        .search
        .write_temporal_entry(&TemporalEntry {
            note_id: id_live.0.to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 1_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Event".to_string(),
            valid_until_ms: None,
        })
        .await
        .expect("seed live");
    // Note expirée : anchor=1000, valid_until=2000 ; as_of=3000 → exclue
    env.state
        .search
        .write_temporal_entry(&TemporalEntry {
            note_id: id_expired.0.to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 1_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Event".to_string(),
            valid_until_ms: Some(2_000),
        })
        .await
        .expect("seed expired");

    let res = post_timeline(
        env.app.clone(),
        Some(&token),
        serde_json::json!({ "as_of_ms": 3_000_i64, "limit": 10 }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = body["items"].as_array().expect("items array");
    let live_present = items
        .iter()
        .any(|i| i["note_id"].as_str() == Some(&id_live.0.to_string()));
    let expired_present = items
        .iter()
        .any(|i| i["note_id"].as_str() == Some(&id_expired.0.to_string()));
    assert!(live_present, "note live doit être présente avec as_of");
    assert!(!expired_present, "note expirée doit être exclue avec as_of");
}
