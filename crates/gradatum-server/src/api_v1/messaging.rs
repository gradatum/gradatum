//! Inter-agent messaging logic (F-144) — `message_send` / `message_ack` /
//! `message_poll`.
//!
//! Pure business logic in the same style as [`crate::api_v1::logic`]: each `*_impl`
//! receives `&AppState` + `&TrustContext` + the request and returns
//! `Result<_, GradatumError>` so both the REST handlers (T9) and the MCP tools (T9)
//! share the same security by construction.
//!
//! ## Model (council 2026-09-07, plan F-144 §SPEC)
//!
//! A message is **two disjoint durable artefacts**:
//!
//! 1. the mailbox content = a note in [`gradatum_core::section::Section::Messaging`]
//!    (vault of the sender's tenant — the shared homelab `main`), whose
//!    `locus = "messaging/<recipient_identity>"` is the C4 channel, `author` is the
//!    credited sender identity and `extra["subject"]` the optional subject. The
//!    content is read back through the standard vault registry (`read_note_by_id`,
//!    same ACL point as `vault_read`, C5);
//! 2. the delivery = a **state-carrying** `Job::Message` row (Pending until ack,
//!    never dequeued — see [`gradatum_core::job::JOB_KIND_MESSAGE`]). The job row id
//!    IS the message id, so `message_ack` completes the delivery by id without any
//!    worker lease ([`gradatum_core::QueueStore::complete_message_delivery`]).
//!
//! ## Guards
//!
//! - `sender_identity` is **derived** from the credential (bearer agent →
//!   `identity/agent/<sub>`); a client-supplied field is rejected at
//!   deserialisation ([`gradatum_dto::MessageSendRequest`] is
//!   `deny_unknown_fields`).
//! - C4 emit policy: `state.acl.evaluate(Write, "messaging/<recipient>")` must be
//!   `Allow`, otherwise a **silent** refusal (C2) — no detail leaks.
//! - Note-first, then enqueue (réserve 5): if the enqueue fails the note is already
//!   written and stays pollable; a reconciliation pass (`reconcile_delivery_jobs`)
//!   recreates the missing job row at the next poll.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use chrono::Utc;
use gradatum_acl_policy::{AclDecision, AclOp, messaging_channel_locus};
use gradatum_core::QueueError;
use gradatum_core::author::AuthorRef;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::job::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobResult, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, MessageDeliverySpec, TriggerSource,
};
use gradatum_core::scope::{AclCheckedVaultId, LocusId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{MessageSendRequest, MessageValidationError, ValidatedMessageSend};
use serde::Serialize;
use ulid::Ulid;

use crate::api_v1::tenant_guard::effective_write_vault;
use crate::state::AppState;

/// Credited agent identity (`identity/<kind>/<id>`, F-125) of a bearer agent.
///
/// Only a `BearerToken` principal is a credited agent in F-144's addressing: its
/// `sub` is the agent id. Studio (`TrustContext::Studio`) and mTLS
/// (`TrustContext::Mtls`) are not agent identities and return `None` — the caller
/// must refuse the send (the message envelope never carries an uncredited sender).
#[must_use]
pub fn credited_agent_identity(trust: &TrustContext) -> Option<String> {
    match trust {
        TrustContext::BearerToken { sub, .. } => Some(format!("identity/agent/{}", sub.as_str())),
        _ => None,
    }
}

/// Builds the state-carrying delivery row for a message (F-144).
///
/// The row id **is** the message id (the mailbox note id), so `message_ack` can
/// complete the delivery by message id. Scope = the message vault (the sender's
/// tenant vault); class `System` (a delivery, not a user job).
fn build_message_job_record(message_id: Ulid, vault: &str, recipient: &str) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: message_id,
        spec: JobSpec {
            kind: Job::Message(MessageDeliverySpec {
                message_id,
                recipient: recipient.to_owned(),
            }),
            class: JobClass::System,
            mode: JobMode::Batch,
            scope: JobScope::Vault(vault.to_string()),
            priority: JobPriority::Low,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
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

/// 202 response of `message_send`.
#[derive(Debug, Serialize)]
pub struct MessageSendResponse {
    /// Server-allocated message identifier (ULID — also the mailbox note id).
    pub message_id: String,
    /// Delivery state, always `"accepted"` right after send.
    pub status: &'static str,
    /// Id of the state-carrying delivery row (equal to `message_id`).
    pub job_id: String,
    /// `poll_url` of the delivery job (`/api/v1/jobs/{id}/v2`).
    pub poll_url: String,
}

impl MessageSendResponse {
    fn new(message_id: Ulid) -> Self {
        let id = message_id.to_string();
        Self {
            message_id: id.clone(),
            status: "accepted",
            job_id: id.clone(),
            poll_url: format!("/api/v1/jobs/{id}/v2"),
        }
    }
}

/// Sends an inter-agent message (F-144) — `message_send`.
///
/// Order (réserve 5): **note first**, then delivery-row enqueue. If the enqueue
/// fails the note is already written and remains pollable as `accepted`; the
/// reconciliation sweep (`reconcile_delivery_jobs`) recreates the missing row.
///
/// # Errors
///
/// - `Unauthorized` — unauthenticated caller.
/// - `Forbidden` — the caller is not a credited agent (Studio/mTLS), or its
///   `write_patterns` do not cover the recipient channel (C4, silent refusal C2).
/// - `InvalidInput` — oversized subject/body (boundary validation, ADN 5).
/// - `Storage` — note write or enqueue failure.
pub async fn message_send_impl(
    state: &AppState,
    trust: &TrustContext,
    req: MessageSendRequest,
    _request_id: &str,
) -> Result<MessageSendResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    // Boundary validation (parse-don't-validate) — defense in depth even when the
    // REST/MCP wrapper already validated: the impl is the shared security seam.
    let validated = ValidatedMessageSend::try_from(req)
        .map_err(|e: MessageValidationError| GradatumError::InvalidInput(e.to_string()))?;

    // Sender derived from the credential (F-125). A non-bearer principal has no
    // credited agent identity → silent refusal.
    let sender = credited_agent_identity(trust).ok_or(GradatumError::Forbidden(
        "message_send requires a credited agent credential".into(),
    ))?;

    // C4 emit policy — the sender must have write coverage over the recipient
    // channel. Refus muet (C2) : aucun détail dans l'erreur.
    let channel = messaging_channel_locus(&validated.recipient_identity);
    if state.acl.evaluate(trust, AclOp::Write, &channel) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("denied".into()));
    }

    // Target vault = the sender's effective (tenant) vault — the shared mailbox
    // vault of the homelab (`main`). Cross-tenant addressing is out of F-144 scope.
    let vault = effective_write_vault(state, trust, None)
        .await
        .map_err(|r| r.into_forbidden("message_send vault resolution"))?;

    // ── 1. Note d'abord (boîte aux lettres) — le contenu + ACL (C5) ───────────
    let message_id = Ulid::generate();
    let now = Utc::now();

    let mut extra = ExtraFields::empty();
    if let Some(subject) = &validated.subject {
        extra.insert("subject".into(), toml::Value::String(subject.clone()));
    }

    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(&vault),
        locus: Some(LocusId::new(&channel)),
        section: Section::Messaging,
        status: gradatum_core::status::NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: Some(AuthorRef::system(sender)),
        created: now,
        updated: None,
        extra,
        provenance: Some("messaging".to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };

    let checked_write = AclCheckedVaultId::for_system_task(VaultId::new(&vault));
    state
        .vault
        .write_note_with_id_internal(
            &checked_write,
            frontmatter,
            validated.body.clone(),
            NoteId(message_id),
        )
        .await
        .map_err(|e| GradatumError::Storage(format!("message note write: {e}")))?;

    // ── 2. Enqueue de la livraison (ligne état-porteuse) ─────────────────────
    //
    // Point d'émission SSE voie2 (F-144) : si le destinataire avait une session MCP
    // connectée, une notification `notifications/message` portant {message_id} seul
    // (pointeur sans état, jamais le corps) serait émise ici. Le transport actuel
    // (Streamable HTTP stateless, LocalSessionManager inerte) n'a aucune session à
    // notifier → no-op par construction ; la capacité `logging` reste annoncée
    // (server_capabilities, T10). Aucune régression si non connecté.
    let record = build_message_job_record(message_id, &vault, &validated.recipient_identity);
    match state.job_store.enqueue(record).await {
        Ok(_) => {}
        Err(e) => {
            // Réserve 5 : la note est déjà écrite — elle reste pollable en
            // `accepted` ; le sweep de réconciliation recréera la ligne.
            tracing::warn!(
                message_id = %message_id,
                error = %e,
                "message_send: note written but delivery enqueue failed — reconciled at next poll"
            );
            return Err(GradatumError::Storage(format!("delivery enqueue: {e}")));
        }
    }

    Ok(MessageSendResponse::new(message_id))
}

/// Reconciles the delivery rows of a recipient mailbox (réserve 5).
///
/// Scans the `accepted` (live, non-downgraded) messages of `recipient` in
/// `vault_ids` and re-enqueues the state-carrying `Job::Message` row for any whose
/// delivery row is missing (note written, enqueue previously failed). Idempotent:
/// messages whose row already exists are untouched.
///
/// Invoked at `message_poll` (T8); not part of the send critical path.
///
/// # Errors
///
/// `GradatumError::Storage` on listing or enqueue failures.
pub async fn reconcile_delivery_jobs(
    state: &AppState,
    vault_ids: &[String],
    recipient: &str,
) -> Result<usize, GradatumError> {
    let (records, _) = state
        .search
        .list_messaging_notes(vault_ids, recipient, 200, None)
        .await?;

    let mut recreated = 0usize;
    for rec in &records {
        // Un id de note est un ULID valide (les messages sont créés avec un ULID).
        let Ok(message_id) = Ulid::from_string(&rec.id) else {
            continue;
        };
        let existing = state.job_store.get(message_id, None).await.map_err(|e| {
            GradatumError::Storage(format!("reconcile get delivery {message_id}: {e}"))
        })?;
        if existing.is_some() {
            continue;
        }
        // La ligne manque → recréer la livraison (vault = celui de la note).
        let record = build_message_job_record(message_id, &rec.vault_id, recipient);
        match state.job_store.enqueue(record).await {
            Ok(_) => recreated += 1,
            Err(e) => {
                tracing::warn!(
                    message_id = %message_id,
                    error = %e,
                    "reconcile_delivery_jobs: re-enqueue failed — will retry next poll"
                );
            }
        }
    }
    Ok(recreated)
}

/// `POST /api/v1/messages` — thin axum wrapper around [`message_send_impl`].
///
/// Thin wrapper only (même pattern que `write::vault_write`) : auth, ACL C4,
/// validation et écriture vivent dans l'impl, partagée avec le chemin MCP (T9).
///
/// # Returns
///
/// - **202 Accepted** + [`MessageSendResponse`] (`status: "accepted"`).
/// - **400 Bad Request** — body invalide (champ inconnu `sender_identity` inclus,
///   bornes subject/body).
/// - **401/403/500** — mappés par `err_to_status` (403 = refus muet C2).
pub async fn message_send(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<MessageSendRequest>,
) -> Result<(StatusCode, Json<MessageSendResponse>), StatusCode> {
    message_send_impl(&state, &trust, req, "http")
        .await
        .map(|resp| (StatusCode::ACCEPTED, Json(resp)))
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}

/// 200 response of `message_ack`.
#[derive(Debug, Serialize)]
pub struct MessageAckResponse {
    /// Delivery state — always `"delivered"` after a successful ack.
    pub status: &'static str,
}

/// Acknowledges a message delivery (F-144) — `message_ack`.
///
/// Authority = the **recipient** of the delivery (credited identity derived from the
/// credential). A non-recipient (or an unknown/already-terminal delivery) receives a
/// **mute 404** (C2) — nothing reveals the existence of another actor's message.
/// Success transitions the state-carrying row to terminal `Done` via
/// [`gradatum_core::QueueStore::complete_message_delivery`] — never a worker lease.
///
/// # Errors
///
/// - `Unauthorized` — unauthenticated caller.
/// - `Forbidden` — the caller is not a credited agent (Studio/mTLS).
/// - `InvalidInput` — `message_id` is not a valid ULID.
/// - `Storage` ("…not found…", → 404 mute) — unknown delivery, already terminal, or
///   addressed to another recipient.
pub async fn message_ack_impl(
    state: &AppState,
    trust: &TrustContext,
    message_id: &str,
) -> Result<MessageAckResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let recipient = credited_agent_identity(trust).ok_or(GradatumError::Forbidden(
        "message_ack requires a credited agent credential".into(),
    ))?;
    let id = Ulid::from_string(message_id)
        .map_err(|_| GradatumError::InvalidInput(format!("invalid message id: {message_id}")))?;

    // Ownership: la livraison doit exister ET être adressée à ce destinataire. Toute
    // autre issue → 404 muet (le message n'existe pas « pour vous »).
    let job = state
        .job_store
        .get(id, None)
        .await
        .map_err(|e| GradatumError::Storage(format!("message delivery get: {e}")))?;
    let Some(job) = job else {
        return Err(GradatumError::Storage("message delivery not found".into()));
    };
    let owned = match &job.spec.kind {
        Job::Message(spec) => spec.recipient == recipient,
        _ => false,
    };
    if !owned {
        return Err(GradatumError::Storage("message delivery not found".into()));
    }

    let result = JobResult {
        success: true,
        duration_ms: 1,
        cost_usd: None,
        result_note: None,
        conflict_payload: None,
    };
    state
        .job_store
        .complete_message_delivery(id, result)
        .await
        .map_err(|e| match e {
            QueueError::NotFound(_) => GradatumError::Storage("message delivery not found".into()),
            other => GradatumError::Storage(format!("message delivery complete: {other}")),
        })?;

    Ok(MessageAckResponse {
        status: "delivered",
    })
}

/// `POST /api/v1/messages/{id}/ack` — thin axum wrapper around [`message_ack_impl`].
///
/// # Returns
///
/// - **200 OK** + `{ "status": "delivered" }`.
/// - **400/401/403/404/500** — mappés par `err_to_status` (404 = muet, C2).
pub async fn message_ack(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Path(message_id): axum::extract::Path<String>,
) -> Result<Json<MessageAckResponse>, StatusCode> {
    message_ack_impl(&state, &trust, &message_id)
        .await
        .map(Json)
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}

/// 200 response of `message_poll`.
#[derive(Debug, Serialize)]
pub struct MessagePollResponse {
    /// Mailbox messages of the caller, ordered by message id (ULID asc).
    pub messages: Vec<gradatum_dto::MessageEnvelope>,
    /// Opaque pagination cursor — `Some(last_id)` when a further page exists.
    pub next_cursor: Option<String>,
}

/// Delivery-state filter of `message_poll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePollStatus {
    /// Not yet acknowledged — the default inbox view.
    Accepted,
    /// Acknowledged (`delivered`) — the archive view.
    Delivered,
}

impl MessagePollStatus {
    /// Parses the `?status=` query value (lowercase wire form of
    /// [`gradatum_dto::MessageStatus`]); `None`/empty → [`MessagePollStatus::Accepted`].
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.map(str::trim).unwrap_or("") {
            "" | "accepted" => Some(Self::Accepted),
            "delivered" => Some(Self::Delivered),
            _ => None,
        }
    }
}

/// Returns the mailbox vaults the caller may read messaging in (C1 allow-list).
///
/// Mono-vault (default, homelab) → `["main"]`, byte-identical legacy (grants never
/// consulted). `multi_tenant` ON → intersection tenant-grants ∩ agent-grants whose
/// access allows read of the `messaging` section (vault-wide or messaging-scoped).
async fn readable_messaging_vaults(
    state: &AppState,
    trust: &TrustContext,
) -> Result<Vec<String>, GradatumError> {
    use gradatum_core::scope::GrantAccess;
    if !state.server_config.multi_tenant.enabled {
        return Ok(vec!["main".to_string()]);
    }
    let tenant = match trust {
        TrustContext::BearerToken { tenant_id, .. } => tenant_id.as_str(),
        _ => return Ok(Vec::new()),
    };
    let tenant_grants = state
        .search
        .tenant_grants(&gradatum_core::scope::TenantId::new(tenant))
        .await?;
    let mut allowed: std::collections::BTreeSet<String> = tenant_grants
        .iter()
        .filter(|g| {
            matches!(g.access, GrantAccess::Read | GrantAccess::Write)
                && g.covers_section(Some("messaging"))
        })
        .map(|g| g.vault_id.as_str().to_string())
        .collect();

    if let Some(sub) = trust.subject() {
        let agent_grants = state.search.agent_grants(sub).await?;
        let agent: std::collections::BTreeSet<String> = agent_grants
            .iter()
            .filter(|g| {
                matches!(g.access, GrantAccess::Read | GrantAccess::Write)
                    && g.covers_section(Some("messaging"))
            })
            .map(|g| g.vault_id.as_str().to_string())
            .collect();
        allowed = allowed.intersection(&agent).cloned().collect();
    }
    Ok(allowed.into_iter().collect())
}

/// Resolves the read-back registry for a vault (mono → `state.vault`).
fn read_registry(
    state: &AppState,
    vault_id: &str,
) -> Result<std::sync::Arc<dyn gradatum_vault::Registry>, GradatumError> {
    if state.server_config.multi_tenant.enabled {
        state.vaults.resolve(&VaultId::new(vault_id))
    } else {
        Ok(std::sync::Arc::clone(&state.vault))
    }
}

/// Polls the caller's mailbox (F-144) — `message_poll`.
///
/// Recipient = the caller's credited identity (never a request parameter).
/// Default view = [`MessagePollStatus::Accepted`] (the pending inbox): after an ack
/// the message no longer surfaces (T7 "re-poll ne remonte plus le message"); pass
/// `status=delivered` to read the archive. Reconciliation of missing delivery rows
/// runs first (réserve 5).
///
/// # Errors
///
/// - `Unauthorized` — unauthenticated.
/// - `Forbidden` — uncredited principal.
/// - `InvalidInput` — invalid `status`.
/// - `Storage` — index/queue/read failures.
pub async fn message_poll_impl(
    state: &AppState,
    trust: &TrustContext,
    status: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<MessagePollResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let recipient = credited_agent_identity(trust).ok_or(GradatumError::Forbidden(
        "message_poll requires a credited agent credential".into(),
    ))?;
    let filter = MessagePollStatus::parse(status).ok_or_else(|| {
        GradatumError::InvalidInput("status must be 'accepted' or 'delivered'".into())
    })?;

    let vaults = readable_messaging_vaults(state, trust).await?;
    if vaults.is_empty() {
        return Ok(MessagePollResponse {
            messages: vec![],
            next_cursor: None,
        });
    }

    // Sweep de réconciliation (réserve 5) : toute note sans ligne de livraison est
    // re-enfilée avant la lecture → aucun `accepted` muet.
    let _reconciled = reconcile_delivery_jobs(state, &vaults, &recipient).await?;

    let (records, _total) = state
        .search
        .list_messaging_notes(&vaults, &recipient, limit, cursor)
        .await?;

    let mut messages = Vec::with_capacity(records.len());
    for rec in &records {
        // Statut de livraison (ligne état-porteuse) : Done → delivered, sinon accepted.
        let Ok(id) = Ulid::from_string(&rec.id) else {
            continue;
        };
        let job = state
            .job_store
            .get(id, None)
            .await
            .map_err(|e| GradatumError::Storage(format!("delivery get {id}: {e}")))?;
        let delivered = matches!(
            job.as_ref().map(|j| &j.lifecycle.status),
            Some(JobStatus::Done)
        );
        let is_selected = match filter {
            MessagePollStatus::Accepted => !delivered,
            MessagePollStatus::Delivered => delivered,
        };
        if !is_selected {
            continue;
        }

        // Sujet porté dans le frontmatter extra de la note (lu via le registre vault,
        // même point d'ACL que vault_read — C5). Une note ghost (impossible ici) est
        // ignorée silencieusement.
        let Ok(note) = read_registry(state, &rec.vault_id)?
            .read_note_by_id(&rec.id)
            .await
        else {
            continue;
        };
        let subject = note
            .frontmatter
            .extra
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let created_at = match chrono::DateTime::<Utc>::from_timestamp_millis(rec.created) {
            Some(ts) => ts,
            None => continue,
        };

        messages.push(
            gradatum_dto::MessageEnvelope::new(
                rec.id.clone(),
                rec.author.clone().unwrap_or_default(),
                recipient.clone(),
                subject,
                rec.body_text.clone(),
                created_at,
            )
            .with_vault_id(rec.vault_id.clone()),
        );
    }

    // Pagination : page pleine → cursor = dernier id ; sinon fin de liste.
    let next_cursor = if records.len() == limit.clamp(1, 200) {
        records.last().map(|r| r.id.clone())
    } else {
        None
    };

    Ok(MessagePollResponse {
        messages,
        next_cursor,
    })
}

/// `GET /api/v1/messages` — thin axum wrapper around [`message_poll_impl`].
///
/// Query params: `status` (`accepted` (default) | `delivered`), `limit`, `cursor`.
pub async fn message_poll(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<MessagePollResponse>, StatusCode> {
    let status = params.get("status").map(String::as_str);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let cursor = params.get("cursor").map(String::as_str);
    message_poll_impl(&state, &trust, status, limit, cursor)
        .await
        .map(Json)
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}
