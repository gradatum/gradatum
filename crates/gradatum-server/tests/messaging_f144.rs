//! Tests d'intégration F-144 (messagerie inter-agents) — `message_send`.
//!
//! Vérifie le chemin d'envoi (T6) : note de boîte aux lettres écrite AVANT la ligne
//! de livraison, émetteur dérivé du credential, politique C4, et réconciliation de
//! la livraison manquante (réserve 5).
//!
//! Fixtures : `writer-main` (consumer ACL avec canal `messaging/identity/agent/*`) ·
//! `writer-reader` (consumer sans canal messaging → refus C4) · `writer-stranger`
//! (inconnu). Flag `multi_tenant` OFF (mono-vault `main`).

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::QueueStore;
use gradatum_core::job::{Job, JobStatus};
use gradatum_core::scope::{AgentId, TenantId, VaultId};
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_dto::MessageSendRequest;
use gradatum_server::api_v1::dto::VaultWriteRequest;
use gradatum_server::api_v1::logic::{FeatureWriteAuthority, vault_write_impl};
use gradatum_server::api_v1::messaging::{message_send_impl, reconcile_delivery_jobs};
use gradatum_server::config::ServerConfig;
use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use tempfile::TempDir;
use ulid::Ulid;

const TEST_ACL_F144: &str = r#"
[[consumer]]
identity = "writer-main"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main", "messaging/identity/agent/*"]

[[consumer]]
identity = "writer-reader"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

struct MsgEnv {
    state: AppState,
    queue_path: std::path::PathBuf,
    _dir: TempDir,
}

async fn build_msg_env() -> MsgEnv {
    let dir = TempDir::new().expect("tempdir F-144");
    let vault_path = dir.path().join("vault");
    let queue_path = dir.path().join("queue.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_F144).expect("preset ACL F-144 valide");

    let jobs_pool = QueueDb::open(&queue_path)
        .await
        .expect("queue db fichier — invariant test");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    // Vault réel sur TempDir : `state.vault` (registre d'écriture) et `state.search`
    // partagent le même SqliteIndex (`vault.index()`) — cohérence write/read.
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool)
        .with_server_config(ServerConfig::default());
    state.search = index;

    MsgEnv {
        state,
        queue_path,
        _dir: dir,
    }
}

fn bearer(sub: &str, tenant: &str) -> TrustContext {
    TrustContext::BearerToken {
        kid: "k-f144".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new(sub),
        scopes: vec!["write".to_owned()],
        tenant_id: TenantId::new(tenant),
        jti: None,
    }
}

/// Supprime la ligne de livraison (simule une perte d'enqueue — réserve 5).
fn delete_delivery_row(queue_path: &std::path::Path, message_id: Ulid) {
    let conn = rusqlite::Connection::open(queue_path).expect("open queue.db");
    conn.execute(
        "DELETE FROM gradatum_jobs WHERE id = ?1",
        [message_id.to_string()],
    )
    .expect("delete delivery row");
}

/// (a)+(d) `message_send` → 202-accepted, note écrite (émetteur dérivé du
/// credential, contenu + canal), livraison `Job::Message` Pending enfilée.
#[tokio::test]
async fn send_writes_mailbox_note_and_enqueues_delivery() {
    let env = build_msg_env().await;
    let trust = bearer("writer-main", "main");
    let recipient = "identity/agent/bob";

    let resp = message_send_impl(
        &env.state,
        &trust,
        MessageSendRequest::new(recipient.into(), Some("ping".into()), "hello".into()),
        "rid-1",
    )
    .await
    .expect("send accepté");

    assert_eq!(resp.status, "accepted");
    assert_eq!(
        resp.job_id, resp.message_id,
        "ligne état-porteuse id = message id"
    );

    let message_id = Ulid::from_string(&resp.message_id).expect("message_id ULID");
    let allow = vec!["main".to_string()];
    let (notes, total) = env
        .state
        .search
        .list_messaging_notes(&allow, recipient, 10, None)
        .await
        .expect("list_messaging_notes");
    assert_eq!(total, 1, "une note en boîte aux lettres");
    assert_eq!(notes[0].id, resp.message_id);
    assert_eq!(notes[0].vault_id, "main", "provenance vault_id");
    assert_eq!(notes[0].body_text, "hello");
    assert_eq!(
        notes[0].author.as_deref(),
        Some("identity/agent/writer-main"),
        "émetteur DÉRIVÉ du credential (a)"
    );
    assert_eq!(
        notes[0].locus.as_deref(),
        Some("messaging/identity/agent/bob"),
        "locus = canal destinataire (C4)"
    );

    let job = env
        .state
        .job_store
        .get(message_id, None)
        .await
        .expect("get livraison")
        .expect("livraison enfilée");
    assert!(
        matches!(job.spec.kind, Job::Message(_)),
        "Job::Message enfilé"
    );
    assert_eq!(
        job.lifecycle.status,
        JobStatus::Pending,
        "livraison Pending"
    );
}

/// (c) C4 — un consumer sans pattern canal `messaging/**` est REFUSÉ en silence
/// (aucune note écrite, aucune ligne enfilée).
#[tokio::test]
async fn send_refuses_sender_without_channel_coverage() {
    let env = build_msg_env().await;
    let trust = bearer("writer-reader", "main");

    let err = message_send_impl(
        &env.state,
        &trust,
        MessageSendRequest::new("identity/agent/bob".into(), None, "hello".into()),
        "rid-2",
    )
    .await
    .expect_err("writer-reader n'a aucun write_pattern messaging (C4)");

    assert!(
        format!("{err}").contains("denied"),
        "refus muet attendu (C2), obtenu {err}"
    );

    let (notes, _) = env
        .state
        .search
        .list_messaging_notes(&["main".to_string()], "identity/agent/bob", 10, None)
        .await
        .expect("list_messaging_notes");
    assert!(notes.is_empty(), "aucune note écrite sur refus C4");
}

/// Un principal non crédité (Studio) n'est pas un agent F-125 → refus.
#[tokio::test]
async fn send_refuses_uncredited_principal() {
    let env = build_msg_env().await;
    let studio = TrustContext::Studio {
        user: "human@example.com".to_owned(),
        scope: gradatum_core::trust::StudioScope::Operator,
        step_up_until: None,
    };
    let err = message_send_impl(
        &env.state,
        &studio,
        MessageSendRequest::new("identity/agent/bob".into(), None, "hello".into()),
        "rid-3",
    )
    .await
    .expect_err("Studio n'est pas un agent crédité F-125");
    assert!(
        matches!(err, gradatum_core::error::GradatumError::Forbidden(_)),
        "Forbidden attendu, obtenu {err}"
    );
}

/// (réserve 5) Livraison manquante (enqueue perdue) → la note reste pollable et la
/// réconciliation au sweep recrée la ligne `Job::Message` (pas de `delivered`
/// orphelin).
#[tokio::test]
async fn reconcile_recreates_missing_delivery_row() {
    let env = build_msg_env().await;
    let trust = bearer("writer-main", "main");
    let recipient = "identity/agent/bob";

    let resp = message_send_impl(
        &env.state,
        &trust,
        MessageSendRequest::new(recipient.into(), Some("ping".into()), "coucou".into()),
        "rid-4",
    )
    .await
    .expect("send accepté");
    let message_id = Ulid::from_string(&resp.message_id).expect("message_id ULID");

    // La note est écrite et pollable AVANT toute considération de livraison.
    let (notes, _) = env
        .state
        .search
        .list_messaging_notes(&["main".to_string()], recipient, 10, None)
        .await
        .expect("list_messaging_notes");
    assert_eq!(notes.len(), 1, "note écrite");

    // Perte de la ligne de livraison (simule enqueue échoué).
    delete_delivery_row(&env.queue_path, message_id);
    let gone = env
        .state
        .job_store
        .get(message_id, None)
        .await
        .expect("get")
        .is_none();
    assert!(gone, "la livraison a bien été perdue");

    // La note reste pollable en `accepted` (message présent, aucune livraison).
    let (notes, _) = env
        .state
        .search
        .list_messaging_notes(&["main".to_string()], recipient, 10, None)
        .await
        .expect("list_messaging_notes");
    assert_eq!(
        notes.len(),
        1,
        "message toujours pollable après perte d'enqueue"
    );

    // Réconciliation au sweep → la livraison est recréée.
    let recreated = reconcile_delivery_jobs(&env.state, &["main".to_string()], recipient)
        .await
        .expect("reconcile_delivery_jobs");
    assert_eq!(recreated, 1, "1 livraison recréée");

    let job = env
        .state
        .job_store
        .get(message_id, None)
        .await
        .expect("get livraison recréée")
        .expect("la livraison recréée existe");
    assert!(matches!(job.spec.kind, Job::Message(_)));
    assert_eq!(job.lifecycle.status, JobStatus::Pending);
}

// ── T7 — message_ack ──────────────────────────────────────────────────────────

/// `message_ack` du destinataire → `delivered`, ligne `Done` ; un second ack est
/// 404 muet (déjà terminale).
#[tokio::test]
async fn ack_by_recipient_delivers_and_second_ack_is_404() {
    let env = build_msg_env().await;
    let sender = bearer("writer-main", "main");
    let recipient_ident = "identity/agent/bob";

    let resp = message_send_impl(
        &env.state,
        &sender,
        MessageSendRequest::new(recipient_ident.into(), None, "hello".into()),
        "rid-ack-1",
    )
    .await
    .expect("send accepté");
    let message_id = resp.message_id.clone();

    // Bob (le destinataire) accuse réception.
    let ack = gradatum_server::api_v1::messaging::message_ack_impl(
        &env.state,
        &bearer("bob", "main"),
        &message_id,
    )
    .await
    .expect("ack destinataire");
    assert_eq!(ack.status, "delivered");

    // La ligne de livraison est terminale Done.
    let id = Ulid::from_string(&message_id).expect("ULID");
    let job = env
        .state
        .job_store
        .get(id, None)
        .await
        .expect("get")
        .expect("livraison existe");
    assert_eq!(job.lifecycle.status, JobStatus::Done, "Done après ack");

    // Double-ack → 404 muet.
    let err2 = gradatum_server::api_v1::messaging::message_ack_impl(
        &env.state,
        &bearer("bob", "main"),
        &message_id,
    )
    .await
    .expect_err("second ack doit être muet");
    assert!(
        format!("{err2}").contains("not found"),
        "double-ack 404 muet attendu, obtenu {err2}"
    );
}

/// Un principal autre que le destinataire ne peut PAS ack — 404 muet (C2), la
/// livraison reste Pending (rien n'est révélé).
#[tokio::test]
async fn ack_by_non_recipient_is_mute_404() {
    let env = build_msg_env().await;
    let sender = bearer("writer-main", "main");
    let recipient_ident = "identity/agent/bob";

    let resp = message_send_impl(
        &env.state,
        &sender,
        MessageSendRequest::new(recipient_ident.into(), None, "hello".into()),
        "rid-ack-2",
    )
    .await
    .expect("send accepté");
    let id = Ulid::from_string(&resp.message_id).expect("ULID");

    let err = gradatum_server::api_v1::messaging::message_ack_impl(
        &env.state,
        &bearer("alice", "main"),
        &resp.message_id,
    )
    .await
    .expect_err("alice n'est pas le destinataire");
    assert!(
        format!("{err}").contains("not found"),
        "404 muet attendu, obtenu {err}"
    );

    let job = env
        .state
        .job_store
        .get(id, None)
        .await
        .expect("get")
        .expect("livraison existe");
    assert_eq!(
        job.lifecycle.status,
        JobStatus::Pending,
        "livraison intacte (Pending) après refus muet"
    );
}

/// `message_ack` sur un id inconnu → 404 muet.
#[tokio::test]
async fn ack_unknown_message_is_mute_404() {
    let env = build_msg_env().await;
    let err = gradatum_server::api_v1::messaging::message_ack_impl(
        &env.state,
        &bearer("bob", "main"),
        &Ulid::generate().to_string(),
    )
    .await
    .expect_err("id inconnu");
    assert!(
        format!("{err}").contains("not found"),
        "404 muet attendu, obtenu {err}"
    );
}

// ── T8 — message_poll ─────────────────────────────────────────────────────────

use gradatum_server::api_v1::messaging::message_poll_impl;

/// Le destinataire poll sa propre boîte : scope Messaging, destinataire = identité
/// du caller (jamais paramètre), statut default `accepted`, provenance vault_id,
/// sujet porté. Un autre agent ne voit rien.
#[tokio::test]
async fn poll_returns_only_own_accepted_mailbox() {
    let env = build_msg_env().await;
    let sender = bearer("writer-main", "main");
    let recipient_ident = "identity/agent/bob";

    message_send_impl(
        &env.state,
        &sender,
        MessageSendRequest::new(recipient_ident.into(), Some("ping".into()), "hello".into()),
        "rid-poll-1",
    )
    .await
    .expect("send accepté");

    // Bob (destinataire) poll — 1 message accepted.
    let resp = message_poll_impl(&env.state, &bearer("bob", "main"), None, 50, None)
        .await
        .expect("poll bob");
    assert_eq!(resp.messages.len(), 1, "1 message pour bob");
    let msg = &resp.messages[0];
    assert_eq!(msg.recipient_identity, recipient_ident);
    assert_eq!(msg.sender_identity, "identity/agent/writer-main");
    assert_eq!(msg.subject.as_deref(), Some("ping"));
    assert_eq!(msg.body, "hello");
    assert_eq!(msg.vault_id.as_deref(), Some("main"), "provenance (C2)");
    assert!(
        resp.next_cursor.is_none(),
        "1 seul message → pas de page suivante"
    );

    // Alice (autre agent) ne voit RIEN — single-target.
    let alice_poll = message_poll_impl(&env.state, &bearer("alice", "main"), None, 50, None)
        .await
        .expect("poll alice");
    assert!(
        alice_poll.messages.is_empty(),
        "alice ne doit pas voir le message de bob"
    );
}

/// Après `ack`, le poll par défaut (`accepted`) ne remonte plus le message (T7) ;
/// le filtre `status=delivered` le montre (archive).
#[tokio::test]
async fn poll_after_ack_hides_from_default_and_delivered_filter_shows() {
    let env = build_msg_env().await;
    let sender = bearer("writer-main", "main");
    let recipient_ident = "identity/agent/bob";

    let send = message_send_impl(
        &env.state,
        &sender,
        MessageSendRequest::new(recipient_ident.into(), None, "hello".into()),
        "rid-poll-2",
    )
    .await
    .expect("send accepté");

    // Avant ack : visible en default.
    let before = message_poll_impl(&env.state, &bearer("bob", "main"), None, 50, None)
        .await
        .expect("poll avant ack");
    assert_eq!(before.messages.len(), 1, "visible avant ack");

    gradatum_server::api_v1::messaging::message_ack_impl(
        &env.state,
        &bearer("bob", "main"),
        &send.message_id,
    )
    .await
    .expect("ack bob");

    // Default (accepted) : ne remonte plus.
    let after = message_poll_impl(&env.state, &bearer("bob", "main"), None, 50, None)
        .await
        .expect("poll après ack");
    assert!(
        after.messages.is_empty(),
        "message deliveré hors du défaut (accepted)"
    );

    // Archive (status=delivered) : visible.
    let archive = message_poll_impl(
        &env.state,
        &bearer("bob", "main"),
        Some("delivered"),
        50,
        None,
    )
    .await
    .expect("poll delivered");
    assert_eq!(archive.messages.len(), 1, "archive delivered");
    assert_eq!(archive.messages[0].message_id, send.message_id);
}

/// `status` invalide → InvalidInput.
#[tokio::test]
async fn poll_rejects_invalid_status() {
    let env = build_msg_env().await;
    let err = message_poll_impl(&env.state, &bearer("bob", "main"), Some("bogus"), 50, None)
        .await
        .expect_err("status invalide");
    assert!(
        matches!(err, gradatum_core::error::GradatumError::InvalidInput(_)),
        "InvalidInput attendu, obtenu {err}"
    );
}

// ── T9 — C6 rate-limit warden devant POST /messages ───────────────────────────

/// Le routeur /api/v1 est posé SOUS le warden (même topologie que main.rs) : un
/// burst dépassé sur POST /api/v1/messages → 429. Deux requêtes passent (401 sans
/// auth), la 3e est bloquée (burst=2).
#[tokio::test]
async fn message_post_is_rate_limited_by_warden() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use gradatum_server::config::RateLimitConfig;
    use tower::ServiceExt;

    let env = build_msg_env().await;
    let rl = RateLimitConfig {
        enabled: true,
        per_minute: 600,
        burst: 2,
        exempt_localhost: false,
    };
    let warden = gradatum_server::middleware::build_warden_layer(&rl).expect("warden layer");

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(axum::middleware::from_fn_with_state(
            env.state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .layer(warden)
        .with_state(env.state);

    let external: std::net::SocketAddr = "192.0.2.1:12345".parse().expect("IP test");
    let req = |method: &str| {
        let mut r = Request::builder()
            .method(method)
            .uri("/api/v1/messages")
            .body(Body::empty())
            .expect("req");
        r.extensions_mut().insert(ConnectInfo(external));
        r
    };

    // 1e et 2e requêtes : passent le warden (réponse auth 401, non bloquée).
    for i in 1..=2 {
        let resp = app
            .clone()
            .oneshot(req("POST"))
            .await
            .unwrap_or_else(|e| panic!("requête {i} échouée : {e}"));
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "requête {i} dans le burst ne doit pas être 429"
        );
    }
    // 3e requête : burst épuisé → 429.
    let third = app.clone().oneshot(req("POST")).await.expect("3e requête");
    assert_eq!(
        third.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "POST /api/v1/messages au-delà du burst → 429 (C6)"
    );
}

// ── Garde section Messaging réservée (F-144) ──────────────────────────────────

/// `vault_write` générique avec `section_hint="messaging"` → 400 (InvalidInput).
///
/// La section Messaging est écrite EXCLUSIVEMENT via `message_send` (canal C4 :
/// `locus = messaging/<recipient>` évalué par la write-policy). Le chemin générique
/// `vault_write` n'évalue, lui, que le locus racine du vault (`{tenant}/main`) — si un
/// appelant pouvait poser `section_hint="messaging"` dessus, il écrirait dans la section
/// Messaging SANS passer la politique d'émission C4 (même garde que les sections
/// réservées `identity` et `project-map`).
#[tokio::test]
async fn vault_write_generic_section_hint_messaging_rejected() {
    let env = build_msg_env().await;
    let trust = bearer("writer-main", "main");

    let req: VaultWriteRequest = serde_json::from_value(serde_json::json!({
        "title": "note générique",
        "body": "corps",
        "section_hint": "messaging",
    }))
    .expect("VaultWriteRequest — invariant test");

    let err = match vault_write_impl(
        &env.state,
        &trust,
        req,
        "req-messaging-guard",
        FeatureWriteAuthority::External,
    )
    .await
    {
        Err(e) => e,
        Ok(_) => panic!("vault_write générique avec section_hint=messaging doit être rejeté (400)"),
    };

    assert!(
        matches!(&err, gradatum_core::error::GradatumError::InvalidInput(m) if m.contains("reserved section")),
        "rejet attendu InvalidInput(...reserved section...), obtenu {err}"
    );
}

/// Anti-régression : le chemin `vault_write` générique ne porte jamais de locus
/// `messaging/<x>` — il enfile un job Curate avec `section_hint=None`, la section
/// étant attribuée par le curateur (qui n'assigne jamais Messaging), jamais calée
/// sur un canal messagerie.
///
/// Verrouille l'invariant qui rend le contournement inerte : si la garde du
/// `section_hint=messaging` sautait un jour, une écriture générique resterait sans
/// moyen de forcer un canal Messaging (seul `message_send` construit ce locus).
#[tokio::test]
async fn vault_write_generic_never_carries_messaging_locus() {
    let env = build_msg_env().await;
    let trust = bearer("writer-main", "main");

    // Contenu volontairement « en forme de message » — le chemin générique ne doit pas
    // le confondre avec un envoi : pas de section_hint, job Curate à section_hint None.
    let req: VaultWriteRequest = serde_json::from_value(serde_json::json!({
        "title": "bonjour",
        "body": "ceci ressemble à un message inter-agents",
    }))
    .expect("VaultWriteRequest — invariant test");

    let resp = vault_write_impl(
        &env.state,
        &trust,
        req,
        "req-anti-regression",
        FeatureWriteAuthority::External,
    )
    .await
    .expect("vault_write générique sans section_hint doit rester accepté (202 queued)");

    let job_ulid = Ulid::from_string(&resp.job_id).expect("job_id est un ULID — invariant test");
    let record = env
        .state
        .job_store
        .get(job_ulid, None)
        .await
        .expect("lecture job — invariant test")
        .expect("job enfilé — invariant test");

    match &record.spec.kind {
        gradatum_core::job::Job::Curate(spec) => assert!(
            spec.section_hint.is_none(),
            "le chemin générique ne doit jamais porter section_hint (donc jamais un locus \
             messaging/<x>), reçu {:?}",
            spec.section_hint
        ),
        other => panic!("job inattendu — Curate attendu, obtenu {other:?}"),
    }
}
