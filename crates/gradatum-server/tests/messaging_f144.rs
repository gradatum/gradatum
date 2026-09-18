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
use gradatum_server::api_v1::messaging::{
    message_ack_impl, message_send_impl, reconcile_delivery_jobs,
};
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

// ── F-276 — pagination côté serveur du poll ────────────────────────────────────
//
// Le filtre `accepted`/`delivered` s'appliquait APRÈS la lecture d'une seule page :
// une boîte dominée par des messages déjà acquittés rendait une page vide alors que
// des messages en attente existaient juste derrière. Le poll lit désormais autant de
// pages que nécessaire pour remplir `limit` messages SÉLECTIONNÉS.

/// Envoie `n` messages de `writer-main` vers lui-même et rend leurs ids triés
/// (l'ordre de lecture du locus est `id ASC`, jamais l'ordre d'envoi).
async fn send_n_to_self(env: &MsgEnv, n: usize) -> (TrustContext, String, Vec<String>) {
    let trust = bearer("writer-main", "main");
    let recipient = "identity/agent/writer-main".to_string();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let resp = message_send_impl(
            &env.state,
            &trust,
            MessageSendRequest::new(
                recipient.clone(),
                Some(format!("sujet {i}")),
                format!("corps {i}"),
            ),
            &format!("rid-f276-{i}"),
        )
        .await
        .expect("send accepté");
        ids.push(resp.message_id);
    }
    ids.sort();
    (trust, recipient, ids)
}

/// 15 messages acquittés + 1 en attente : la vue `accepted` traverse la première page
/// entièrement livrée et rend le message en attente (avant F-276 : 0 message).
#[tokio::test]
async fn poll_accepted_traverses_delivered_pages_to_fill_the_page() {
    let env = build_msg_env().await;
    let (trust, _recipient, ids) = send_n_to_self(&env, 16).await;
    for id in ids.iter().take(15) {
        message_ack_impl(&env.state, &trust, id)
            .await
            .expect("ack accepté");
    }

    let page = message_poll_impl(&env.state, &trust, Some("accepted"), 10, None)
        .await
        .expect("poll accepted");

    assert_eq!(
        page.messages.len(),
        1,
        "le seul message non acquitté doit remonter malgré 15 livrés devant"
    );
    assert_eq!(page.messages[0].message_id, ids[15]);
    assert!(
        page.next_cursor.is_none(),
        "locus épuisé ⇒ pas de curseur, obtenu {:?}",
        page.next_cursor
    );
}

/// La vue `delivered` reste paginée : 10 puis 5.
#[tokio::test]
async fn poll_delivered_pages_ten_then_five() {
    let env = build_msg_env().await;
    let (trust, _recipient, ids) = send_n_to_self(&env, 16).await;
    for id in ids.iter().take(15) {
        message_ack_impl(&env.state, &trust, id)
            .await
            .expect("ack accepté");
    }

    let first = message_poll_impl(&env.state, &trust, Some("delivered"), 10, None)
        .await
        .expect("poll delivered page 1");
    assert_eq!(first.messages.len(), 10);
    let cursor = first.next_cursor.clone().expect("page pleine ⇒ curseur");

    let second = message_poll_impl(&env.state, &trust, Some("delivered"), 10, Some(&cursor))
        .await
        .expect("poll delivered page 2");
    assert_eq!(second.messages.len(), 5);
    assert!(second.next_cursor.is_none(), "fin du locus");
}

/// Garde de balayage atteinte : jamais de troncature silencieuse — la page peut être
/// vide, mais elle rend alors un curseur de reprise.
///
/// `limit = 1` ⇒ page de 1 enregistrement : la garde de 20 pages est atteinte après
/// 20 messages livrés, le 21ᵉ (en attente) est hors de portée de ce poll.
#[tokio::test]
async fn poll_returns_a_resume_cursor_when_the_scan_guard_is_reached() {
    let env = build_msg_env().await;
    let (trust, _recipient, ids) = send_n_to_self(&env, 21).await;
    for id in ids.iter().take(20) {
        message_ack_impl(&env.state, &trust, id)
            .await
            .expect("ack accepté");
    }

    let page = message_poll_impl(&env.state, &trust, Some("accepted"), 1, None)
        .await
        .expect("poll accepted");

    assert!(
        page.messages.is_empty(),
        "20 pages de livrés consommées par la garde"
    );
    let cursor = page
        .next_cursor
        .clone()
        .expect("garde atteinte ⇒ curseur de reprise, jamais None (pas de fin de liste feinte)");

    let resumed = message_poll_impl(&env.state, &trust, Some("accepted"), 1, Some(&cursor))
        .await
        .expect("poll accepted reprise");
    assert_eq!(
        resumed.messages.len(),
        1,
        "la reprise atteint le 21ᵉ message"
    );
    assert_eq!(resumed.messages[0].message_id, ids[20]);
}

/// `limit` borne la PAGE, pas la moisson : le contrat DTO dit « `limit` caps the page ».
/// La boucle de pagination poussait toute la page sélectionnée avant de tester la limite,
/// donc elle pouvait rendre jusqu'à `2 × limit − 1` messages.
///
/// 16 messages, le 1er acquitté ⇒ 15 en attente. Page 1 n'en sélectionne que 9 (le 1er est
/// filtré), la boucle continue, et la page 2 en ajoutait 6 d'un bloc — 15 rendus pour
/// `limit=10`.
#[tokio::test]
async fn poll_limit_caps_the_page_even_when_the_filter_spans_pages() {
    let env = build_msg_env().await;
    let (trust, _recipient, ids) = send_n_to_self(&env, 16).await;
    message_ack_impl(&env.state, &trust, &ids[0])
        .await
        .expect("ack du premier message");

    let first = message_poll_impl(&env.state, &trust, Some("accepted"), 10, None)
        .await
        .expect("poll accepted page 1");

    assert_eq!(
        first.messages.len(),
        10,
        "`limit` borne la page : 10 demandés, {} rendus",
        first.messages.len()
    );
    let cursor = first
        .next_cursor
        .clone()
        .expect("boîte non épuisée ⇒ curseur de reprise");
    assert_eq!(
        cursor, first.messages[9].message_id,
        "le curseur porte le dernier message RENDU, jamais le dernier enregistrement LU"
    );

    let second = message_poll_impl(&env.state, &trust, Some("accepted"), 10, Some(&cursor))
        .await
        .expect("poll accepted page 2");
    assert_eq!(second.messages.len(), 5);
    assert!(second.next_cursor.is_none(), "boîte épuisée");

    // Ni doublon, ni manquant : l'union des deux pages est exactement la boîte en attente.
    let mut rendus: Vec<String> = first
        .messages
        .iter()
        .chain(second.messages.iter())
        .map(|m| m.message_id.clone())
        .collect();
    let total = rendus.len();
    rendus.sort();
    rendus.dedup();
    assert_eq!(total, rendus.len(), "aucun message rendu deux fois");
    assert_eq!(
        rendus,
        ids[1..].to_vec(),
        "les 15 messages en attente, exactement"
    );
}

// ── F-279 — lectures de file bornées par page ──────────────────────────────────
//
// Le poll (et le balayage de réconciliation qui le précède) lisait la ligne de
// livraison de CHAQUE enregistrement de la page, un appel par note. Une page de 200
// notes coûtait donc 200 lectures de file, et le pire cas du poll (20 pages) 4 000.
// La lecture de livraison est désormais groupée : au plus une par page et par site.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Enveloppe de [`QueueStore`] comptant les LECTURES de file du serveur
/// (`get` / `get_many` / `list`) ; écritures et lectures de notes ne sont pas comptées.
///
/// `get_many` DOIT être surchargé et délégué : s'appuyer sur le défaut du trait
/// recompterait N `get` et laisserait le test rouge après le correctif.
struct CountingQueueStore {
    inner: Arc<dyn QueueStore>,
    reads: AtomicUsize,
}

impl CountingQueueStore {
    fn new(inner: Arc<dyn QueueStore>) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn count(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }

    fn reset(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl QueueStore for CountingQueueStore {
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError> {
        self.inner.enqueue(job).await
    }

    async fn dequeue(&self, tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        self.inner.dequeue(tenant_filter).await
    }

    async fn dequeue_by_kind(
        &self,
        kind: &str,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        self.inner.dequeue_by_kind(kind, tenant_filter).await
    }

    async fn get(
        &self,
        id: Ulid,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        self.count();
        self.inner.get(id, tenant_filter).await
    }

    async fn get_many(
        &self,
        ids: &[Ulid],
        tenant_filter: Option<&str>,
    ) -> Result<Vec<JobRecord>, QueueError> {
        self.count();
        self.inner.get_many(ids, tenant_filter).await
    }

    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError> {
        self.inner.complete(id, result).await
    }

    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError> {
        self.inner.fail(id, err, attempt).await
    }

    async fn cancel(&self, id: Ulid, tenant_filter: Option<&str>) -> Result<(), QueueError> {
        self.inner.cancel(id, tenant_filter).await
    }

    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError> {
        self.inner.fail_dlq(id, err).await
    }

    async fn find_awaiting(&self, job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        self.inner.find_awaiting(job_id).await
    }

    async fn set_pending(&self, id: Ulid) -> Result<(), QueueError> {
        self.inner.set_pending(id).await
    }

    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        self.inner.recover_stale_leases(ttl).await
    }

    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        self.inner.cancel_expired_deadlines(now).await
    }

    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        self.inner.promote_retries(now).await
    }

    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError> {
        self.inner.schedule_retry(id, at).await
    }

    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        self.count();
        self.inner.list(filter).await
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        self.inner.subscribe()
    }
}

/// Une page pleine de 200 notes coûte AU PLUS deux lectures de file : une pour le
/// balayage de réconciliation, une pour le poll. Avant correctif : une par note.
#[tokio::test]
async fn poll_page_issues_at_most_two_queue_reads_per_page() {
    let mut env = build_msg_env().await;
    // Le compteur enveloppe le vrai store SQLite : aucune duplication de fixture.
    let counting = Arc::new(CountingQueueStore::new(Arc::clone(&env.state.job_store)));
    env.state.job_store = Arc::clone(&counting) as Arc<dyn QueueStore>;

    let (trust, _recipient, _ids) = send_n_to_self(&env, 200).await;
    // Les envois (« écritures ») sont hors mesure : on remet le compteur à zéro.
    counting.reset();

    let page = message_poll_impl(&env.state, &trust, Some("accepted"), 200, None)
        .await
        .expect("poll accepted");

    assert_eq!(
        page.messages.len(),
        200,
        "la page est pleine : le coût mesuré porte bien sur 200 enregistrements"
    );
    // Plancher ET plafond, pas seulement un plafond : un `<= 2` seul reste vert si le
    // balayage cesse de lire la page (0 lecture), c'est-à-dire si la fonctionnalité
    // disparaît. La valeur observée est déterministe : 1 réconciliation + 1 poll = 2.
    let reads = counting.reads();
    assert!(
        (1..=2).contains(&reads),
        "une page de 200 notes coûte 1 ou 2 lectures de file — \
         la borne basse (≥ 1) prouve que la page est bien lue : 0 lecture signifierait \
         que le balayage ne lit plus rien, et ne doit pas passer ; \
         la borne haute (≤ 2) prouve que la lecture est groupée par page \
         (réconciliation + poll), pas une par enregistrement (200 avant correctif) — \
         obtenu {reads}"
    );
}

// ── L-C (F-160) — une livraison acquittée puis purgée ─────────────────────────
//
// Une livraison acquittée est terminale (`Done` via `complete_message_delivery`),
// donc purgeable comme n'importe quel job terminal. La purge remplace son payload —
// `spec.kind`, où vivait le destinataire — par la sentinelle `Job::PayloadPurged` :
// l'appartenance n'est plus ré-établissable depuis la ligne. L'acquittement doit
// donc répondre EXACTEMENT ce qu'il répond pour une livraison inconnue — le 404
// muet (C2) —, jamais un statut distinct ni un message nommant l'état purgé : ce
// serait un oracle d'existence pour un non-destinataire, et la réponse dépendrait
// d'une purge de maintenance interne plutôt que de l'état propre de l'appelant.

use gradatum_core::error::GradatumError;

use gradatum_core::job::{
    CurateSpec, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRetry, JobScheduling,
    JobScope, JobSpec, TriggerSource,
};

/// Job terminal `Done` de remplissage, à ULID **explicitement postérieur** à la
/// livraison : la fenêtre de conservation des 200 derniers en devient déterministe,
/// sans dépendre de la monotonie de l'horloge du générateur d'ULID.
fn filler_terminal_record(seq: u64) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: Ulid::from_parts(4_000_000_000_000 + seq, u128::from(seq) + 1),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec::default()),
            class: JobClass::System,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::System),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Done,
            created_at: now,
            started_at: None,
            completed_at: Some(now),
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Sends a delivery to `identity/agent/bob`, has Bob acknowledge it (terminal
/// `Done`), then pushes it out of the retention window and purges it.
///
/// Returns the environment and the delivery id. The two preconditions a body needs
/// are asserted here — the payload is really purged, and the state-carrying row
/// really survived the purge — so no body can pass by observing a plain unknown id.
async fn purged_acknowledged_delivery() -> (MsgEnv, Ulid) {
    let env = build_msg_env().await;

    let resp = message_send_impl(
        &env.state,
        &bearer("writer-main", "main"),
        MessageSendRequest::new("identity/agent/bob".into(), None, "hello".into()),
        "rid-purge-ack",
    )
    .await
    .expect("send accepté");
    let delivery_id = Ulid::from_string(&resp.message_id).expect("ULID livraison");

    // Bob acquitte : la livraison passe terminale `Done`, donc purgeable.
    message_ack_impl(&env.state, &bearer("bob", "main"), &resp.message_id)
        .await
        .expect("ack destinataire");

    // 200 jobs terminaux plus récents remplissent la fenêtre de conservation : la
    // livraison en sort et son payload est réellement purgé.
    for seq in 0..200u64 {
        env.state
            .job_store
            .enqueue(filler_terminal_record(seq))
            .await
            .expect("enqueue filler");
    }
    let purged = env
        .state
        .job_store
        .purge_terminal_payloads(None)
        .await
        .expect("purge_terminal_payloads");
    assert!(
        purged >= 1,
        "la livraison est hors fenêtre : au moins un payload purgé, obtenu {purged}"
    );

    let delivery = env
        .state
        .job_store
        .get(delivery_id, None)
        .await
        .expect("get livraison")
        .expect("la ligne de livraison survit à la purge");
    assert!(
        matches!(delivery.spec.kind, Job::PayloadPurged),
        "sans purge effective de la livraison, ce test ne prouverait rien"
    );
    assert_eq!(
        delivery.lifecycle.status,
        JobStatus::Done,
        "le statut terminal du payload purgé est conservé"
    );

    (env, delivery_id)
}

/// A delivery id that was never enqueued — the reference answer the purged case
/// must be indistinguishable from.
fn unknown_delivery_id() -> Ulid {
    Ulid::from_parts(9_000_000_000_000, 7)
}

/// Asserts the answer is the mute 404 of an unknown delivery: byte-identical to
/// `reference`, carrying "not found", and naming neither the purged state nor the
/// acknowledgement.
fn assert_mute_not_found(err: &GradatumError, reference: &GradatumError, case: &str) {
    let msg = err.to_string();
    assert_eq!(
        msg,
        reference.to_string(),
        "{case}: la réponse doit être indistinguable de celle d'une livraison inconnue"
    );
    assert!(
        msg.contains("not found"),
        "{case}: 404 muet attendu (`err_to_status` mappe « not found » sur 404), obtenu {msg}"
    );
    let lower = msg.to_lowercase();
    for leaked in ["purge", "acknowledged", "conflict", "owned"] {
        assert!(
            !lower.contains(leaked),
            "{case}: la réponse ne doit pas nommer « {leaked} », obtenu {msg}"
        );
    }
}

/// Re-acknowledging a purged delivery is a mute 404, even for its legitimate
/// recipient: the purged row no longer carries the recipient, so its
/// acknowledgement cannot be told apart from an unknown delivery — the very answer
/// the recipient already gets for a non-purged double ack.
#[tokio::test]
async fn purged_delivery_acknowledged_by_recipient_is_silent_404() {
    let (env, delivery_id) = purged_acknowledged_delivery().await;
    let bob = bearer("bob", "main");

    let reference = message_ack_impl(&env.state, &bob, &unknown_delivery_id().to_string())
        .await
        .expect_err("livraison inconnue");
    let err = message_ack_impl(&env.state, &bob, &delivery_id.to_string())
        .await
        .expect_err("une livraison purgée ne peut plus être acquittée");
    assert_mute_not_found(&err, &reference, "ré-acquittement par le destinataire");
}

/// A non-recipient gets the very same mute 404 on a purged delivery: no status
/// code and no message may reveal that the delivery exists and was acknowledged by
/// someone else (C2).
#[tokio::test]
async fn purged_delivery_acknowledged_by_non_recipient_is_silent_404() {
    let (env, delivery_id) = purged_acknowledged_delivery().await;
    let stranger = bearer("alice", "main");

    let reference = message_ack_impl(&env.state, &stranger, &unknown_delivery_id().to_string())
        .await
        .expect_err("livraison inconnue");
    let err = message_ack_impl(&env.state, &stranger, &delivery_id.to_string())
        .await
        .expect_err("un non-destinataire ne peut pas acquitter une livraison purgée");
    assert_mute_not_found(&err, &reference, "acquittement par un non-destinataire");
}

// ── lectures de notes bornées par page ─────────────────────────────────────────
//
// Le poll lisait la note de CHAQUE message sélectionné, un appel par note : une page
// de 50 messages coûtait 50 lectures de notes, et le pire cas du balayage (20 pages)
// jusqu'à 4 000. La note est désormais lue groupée, une fois par page.

/// Enveloppe de [`gradatum_vault::Registry`] comptant les LECTURES de notes du
/// serveur (`read_notes_many` / `read_note_by_id`) ; écritures et lectures de file ne
/// sont pas comptées.
///
/// `read_notes_many` DOIT être surchargé et délégué : s'appuyer sur le défaut du trait
/// recompterait N `read_note_by_id` et laisserait le test rouge après le correctif.
struct CountingRegistry {
    inner: Arc<dyn gradatum_vault::Registry>,
    many_calls: AtomicUsize,
    unit_calls: AtomicUsize,
}

impl CountingRegistry {
    fn new(inner: Arc<dyn gradatum_vault::Registry>) -> Self {
        Self {
            inner,
            many_calls: AtomicUsize::new(0),
            unit_calls: AtomicUsize::new(0),
        }
    }

    fn many_calls(&self) -> usize {
        self.many_calls.load(Ordering::SeqCst)
    }

    fn unit_calls(&self) -> usize {
        self.unit_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl gradatum_vault::Registry for CountingRegistry {
    async fn tenant_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.inner.tenant_count().await
    }

    async fn locus_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.inner.locus_count().await
    }

    async fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner.ensure_tenant(tenant_id).await
    }

    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        // Toute lecture UNITAIRE de note est un retour au N+1 : comptée pour
        // l'INTERDIRE, pas seulement pour la documenter.
        self.unit_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_note_by_id(note_id).await
    }

    async fn read_notes_many(
        &self,
        note_ids: &[String],
    ) -> Result<Vec<gradatum_core::note::Note>, gradatum_core::error::GradatumError> {
        // LA lecture mesurée : une lecture groupée pour la page entière.
        self.many_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_notes_many(note_ids).await
    }

    async fn note_indexed(
        &self,
        _note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError> {
        self.inner.note_indexed(_note_id).await
    }

    async fn history_versions(
        &self,
        note_id: &str,
    ) -> Result<Vec<i64>, gradatum_core::error::GradatumError> {
        self.inner.history_versions(note_id).await
    }

    async fn history_get(
        &self,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        self.inner.history_get(note_id, ts_ms).await
    }

    async fn history_restore(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<String, gradatum_core::error::GradatumError> {
        self.inner.history_restore(checked, note_id, ts_ms).await
    }

    async fn history_diff(
        &self,
        note_id: &str,
        a: &str,
        b: &str,
    ) -> Result<Vec<String>, gradatum_core::error::GradatumError> {
        self.inner.history_diff(note_id, a, b).await
    }

    async fn update_note_status(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        target: gradatum_core::status::NoteStatus,
        reason: Option<String>,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner
            .update_note_status(checked, note_id, target, reason)
            .await
    }

    async fn add_tags(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        tags: &[String],
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner.add_tags(checked, note_id, tags).await
    }

    async fn move_locus(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner.move_locus(checked, note_id, new_locus).await
    }

    async fn write_note_with_id_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        self.inner
            .write_note_with_id_internal(checked, frontmatter, body, id)
            .await
    }

    async fn write_if_match_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
        expected_sha256: [u8; 32],
    ) -> Result<gradatum_vault::write::WriteResult, gradatum_core::error::GradatumError> {
        self.inner
            .write_if_match_internal(checked, frontmatter, body, id, expected_sha256)
            .await
    }

    async fn rescan_privacy_note(
        &self,
        vault_id: &str,
        note_id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_vault::PrivacyRescanOutcome, gradatum_core::error::GradatumError> {
        self.inner.rescan_privacy_note(vault_id, note_id).await
    }

    async fn delete_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner.delete_note_by_id(id).await
    }

    async fn delete_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.inner.delete_note_by_id_in(vault_id, id).await
    }

    async fn archive_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<gradatum_vault::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError>
    {
        self.inner
            .archive_note_by_id(id, archived_by, gc_due_ms)
            .await
    }

    async fn archive_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<gradatum_vault::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError>
    {
        self.inner
            .archive_note_by_id_in(vault_id, id, archived_by, gc_due_ms)
            .await
    }

    async fn run_archive_gc(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<u64, gradatum_core::error::GradatumError> {
        self.inner.run_archive_gc(now_ms, limit).await
    }

    async fn list_archives(
        &self,
        filter: &gradatum_index::ArchiveListFilter,
    ) -> Result<Vec<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError> {
        self.inner.list_archives(filter).await
    }

    async fn get_active_archive(
        &self,
        note_id: &str,
    ) -> Result<Option<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError> {
        self.inner.get_active_archive(note_id).await
    }

    async fn purge_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError> {
        self.inner.purge_archive_by_id(note_id).await
    }

    async fn restore_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_vault::lifecycle::RestoreOutcome, gradatum_core::error::GradatumError>
    {
        self.inner.restore_archive_by_id(note_id).await
    }
}

/// Une page de 50 messages coûte **une** lecture de notes, quel que soit le nombre de
/// messages sélectionnés. Avant correctif : une lecture par message (50).
///
/// Le compteur porte sur le VRAI registre de vault (aucune duplication de fixture) :
/// c'est le chemin réellement emprunté par le serveur qui est mesuré.
#[tokio::test]
async fn poll_page_reads_note_content_in_one_batched_call() {
    let mut env = build_msg_env().await;

    // Les envois sont hors mesure : le compteur n'est installé qu'ensuite.
    let (trust, _recipient, ids) = send_n_to_self(&env, 50).await;
    let counting = Arc::new(CountingRegistry::new(Arc::clone(&env.state.vault)));
    env.state.vault = Arc::clone(&counting) as Arc<dyn gradatum_vault::Registry>;

    let page = message_poll_impl(&env.state, &trust, Some("accepted"), 50, None)
        .await
        .expect("poll accepted");

    assert_eq!(
        page.messages.len(),
        50,
        "la page est pleine : le coût mesuré porte bien sur 50 notes"
    );
    assert!(
        page.next_cursor.is_some(),
        "page pleine → un curseur de reprise, jamais une fin de liste feinte"
    );

    // L'exactitude AVANT le coût : une lecture groupée qui perd du contenu reste
    // « une seule lecture » — le vert par vacuité est ici explicitement écarté.
    let rendus: Vec<String> = page.messages.iter().map(|m| m.message_id.clone()).collect();
    assert_eq!(rendus, ids, "les 50 messages, dans l'ordre du locus");
    for (msg, i) in page.messages.iter().zip(0..50) {
        assert_eq!(
            msg.subject.as_deref(),
            Some(format!("sujet {i}").as_str()),
            "le sujet vient de la note, pas d'une lecture perdue"
        );
        assert_eq!(msg.body, format!("corps {i}"));
    }

    // Plancher ET plafond, pas seulement un plafond : `many_calls == 1` seul resterait
    // vert si le poll cessait de lire les notes (0 lecture) et rendait 0 message — mais
    // l'assertion de page pleine ci-dessus l'interdit déjà. Le couple est néanmoins
    // asserté tel quel, pour que la mesure soit lisible : 1 lecture groupée, 0 unitaire.
    assert_eq!(
        (counting.many_calls(), counting.unit_calls()),
        (1, 0),
        "une page de 50 messages = UNE lecture groupée de notes et AUCUNE lecture \
         unitaire (avant correctif : 50 lectures unitaires) — obtenu {} groupée(s), \
         {} unitaire(s)",
        counting.many_calls(),
        counting.unit_calls()
    );
}
