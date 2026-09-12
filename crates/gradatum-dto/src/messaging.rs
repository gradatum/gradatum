//! Shared wire contract for inter-agent messaging.
//!
//! A message is a durable, single-target piece of mail between two credited agent
//! identities (`identity/<kind>/<id>`). Defined **once** here so the producer
//! (`gradatum-server`, `message_send` / `message_poll` / `message_ack`) and the
//! consumers (MCP clients, REST agents, `gradatum-mcp-stub` input schemas) agree on
//! a single shape.
//!
//! The mailbox content is a note in [`gradatum_core::section::Section::Messaging`];
//! the delivery is a state-carrying job row (`Job::Message`), never the body. This
//! DTO only carries the envelope and the client request — the two persistent stores
//! stay out of the wire contract.
//!
//! ## Forward/backward compatibility
//!
//! `#[non_exhaustive]` on the public structs lets later phases add fields without a
//! SemVer break; the constructors below exist because a `#[non_exhaustive]` struct
//! cannot be built with literal syntax from another crate. Additive fields carry
//! `#[serde(default)]`, so an older or partial payload deserializes cleanly
//! (missing field → default). [`MessageEnvelope::vault_id`] is the canonical
//! example: it is a read-side provenance (C2), filled by `message_poll`, absent on
//! the send path, and `None` on any payload that predates it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Maximum accepted message body size, in bytes (DoS safety cap, ADN 5).
///
/// The message body becomes a note body in `Section::Messaging`; the cap keeps a
/// single message far below any note-size ceiling while leaving room for
/// structured payloads.
pub const MAX_MESSAGE_BODY_BYTES: usize = 64 * 1024;

/// Maximum accepted message subject size, in bytes (DoS safety cap, ADN 5).
///
/// A subject is a short human-readable headline; the cap is deliberately small.
pub const MAX_MESSAGE_SUBJECT_BYTES: usize = 512;

/// Maximum accepted recipient identity size, in bytes (DoS safety cap, ADN 5).
///
/// A recipient identity is a short `identity/<kind>/<id>` string; the cap keeps
/// the routing key bounded well below any note-key ceiling.
pub const MAX_MESSAGE_RECIPIENT_BYTES: usize = 256;

/// One inter-agent message, as read from a recipient mailbox (`message_poll`).
///
/// This is the **read projection**: the server fills every field from the stored
/// note + delivery job. `vault_id` states the origin vault of the content (C2
/// provenance) and is only present on the read path — the send path answers with a
/// bare [`MessageStatus`], never an envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageEnvelope {
    /// Server-allocated message identifier (ULID).
    pub message_id: String,
    /// Credited sender identity (`identity/<kind>/<id>`), derived by the
    /// server from the request credential. A client-supplied sender is rejected.
    pub sender_identity: String,
    /// Credited recipient identity (`identity/<kind>/<id>`), single-target.
    pub recipient_identity: String,
    /// Optional short subject, size-bounded (absent → `null`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Message content, size-bounded.
    pub body: String,
    /// Instant the message was accepted by the server (RFC 3339 on the wire).
    pub created_at: DateTime<Utc>,
    /// Provenance vault of the stored content (C2), filled on the read path.
    /// Absent (or `None`) on payloads that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<String>,
}

impl MessageEnvelope {
    /// Builds an envelope without provenance.
    ///
    /// The send path constructs the envelope before the note exists; `vault_id` is
    /// attached later by [`MessageEnvelope::with_vault_id`] once the read path knows
    /// the origin vault.
    #[must_use]
    pub fn new(
        message_id: String,
        sender_identity: String,
        recipient_identity: String,
        subject: Option<String>,
        body: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            message_id,
            sender_identity,
            recipient_identity,
            subject,
            body,
            created_at,
            vault_id: None,
        }
    }

    /// Attaches the provenance vault (C2) to a previously built envelope.
    #[must_use]
    pub fn with_vault_id(mut self, vault_id: String) -> Self {
        self.vault_id = Some(vault_id);
        self
    }
}

/// Delivery-state of a message in a mailbox.
///
/// Two-state acknowledgement, never merged (council 2026-09-07):
/// - [`MessageStatus::Accepted`] — the message note is written and its delivery job
///   is queued (`send` returned `202`).
/// - [`MessageStatus::Delivered`] — the delivery job reached the terminal `Done`
///   state via `QueueStore::complete` (the recipient acknowledged).
///
/// Serialised as lowercase on the wire (`"accepted"` / `"delivered"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MessageStatus {
    /// Note written + delivery job queued; the recipient has not acknowledged yet.
    Accepted,
    /// Delivery job terminal `Done`; the recipient acknowledged the message.
    Delivered,
}

/// Client request body for `message_send` / `POST /api/v1/messages`.
///
/// The sender identity is intentionally **absent**: it is derived by the server
/// from the request credential. `recipient_identity` is the single target; the
/// server enforces the emit policy (C4) before anything is persisted.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MessageSendRequest {
    /// Recipient identity (`identity/<kind>/<id>`).
    pub recipient_identity: String,
    /// Optional short subject (absent → `null`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Message content.
    pub body: String,
}

impl MessageSendRequest {
    /// Builds a send request.
    #[must_use]
    pub fn new(recipient_identity: String, subject: Option<String>, body: String) -> Self {
        Self {
            recipient_identity,
            subject,
            body,
        }
    }
}

/// Input parameters of `message_poll` (MCP tool + REST query mirror).
///
/// Every field is optional: `status` filters the mailbox (`accepted` (default) |
/// `delivered`), `limit` caps the page, `cursor` paginates.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MessagePollParams {
    /// Delivery-state filter (`accepted` | `delivered`); absent → `accepted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Page size cap (server default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque pagination cursor returned by a previous poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl MessagePollParams {
    /// Builds poll parameters.
    #[must_use]
    pub fn new(status: Option<String>, limit: Option<u32>, cursor: Option<String>) -> Self {
        Self {
            status,
            limit,
            cursor,
        }
    }
}

/// Input of `message_ack` — the message id to acknowledge.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MessageAckRequest {
    /// Server-allocated message identifier (ULID).
    pub message_id: String,
}

impl MessageAckRequest {
    /// Builds an ack request.
    #[must_use]
    pub fn new(message_id: String) -> Self {
        Self { message_id }
    }
}

/// Size-validated send payload, obtained from a [`MessageSendRequest`].
///
/// Parse-don't-validate at the boundary: [`TryFrom<MessageSendRequest>`] rejects
/// any request whose `subject` or `body` exceeds the declared caps. Once built, the
/// content is within bounds and safe to persist into a `Messaging` note — the
/// server never re-checks sizes downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMessageSend {
    /// Recipient identity (`identity/<kind>/<id>`), unchanged by validation.
    pub recipient_identity: String,
    /// Optional short subject, `None` or within [`MAX_MESSAGE_SUBJECT_BYTES`].
    pub subject: Option<String>,
    /// Message content, within [`MAX_MESSAGE_BODY_BYTES`].
    pub body: String,
}

/// Boundary rejection of an oversized message payload.
///
/// Carries the offending size and the cap that was exceeded, so the caller can
/// produce a precise `400` reason.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageValidationError {
    /// `body` exceeds [`MAX_MESSAGE_BODY_BYTES`].
    BodyTooLarge {
        /// Actual body size in bytes.
        size: usize,
        /// The cap that was exceeded.
        max: usize,
    },
    /// `subject` exceeds [`MAX_MESSAGE_SUBJECT_BYTES`].
    SubjectTooLarge {
        /// Actual subject size in bytes.
        size: usize,
        /// The cap that was exceeded.
        max: usize,
    },
    /// `recipient_identity` is not a well-formed `identity/<kind>/<id>` string
    /// (empty, missing the `identity/` prefix, or containing an empty segment).
    RecipientIdentityMalformed,
    /// `recipient_identity` exceeds [`MAX_MESSAGE_RECIPIENT_BYTES`].
    RecipientIdentityTooLarge {
        /// Actual recipient identity size in bytes.
        size: usize,
        /// The cap that was exceeded.
        max: usize,
    },
}

impl std::fmt::Display for MessageValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BodyTooLarge { size, max } => {
                write!(f, "message body too large ({size} bytes > {max} cap)")
            }
            Self::SubjectTooLarge { size, max } => {
                write!(f, "message subject too large ({size} bytes > {max} cap)")
            }
            Self::RecipientIdentityMalformed => {
                write!(
                    f,
                    "recipient identity malformed (expected `identity/<kind>/<id>`, non-empty)"
                )
            }
            Self::RecipientIdentityTooLarge { size, max } => {
                write!(f, "recipient identity too large ({size} bytes > {max} cap)")
            }
        }
    }
}

impl std::error::Error for MessageValidationError {}

impl TryFrom<MessageSendRequest> for ValidatedMessageSend {
    type Error = MessageValidationError;

    fn try_from(req: MessageSendRequest) -> Result<Self, Self::Error> {
        if req.body.len() > MAX_MESSAGE_BODY_BYTES {
            return Err(MessageValidationError::BodyTooLarge {
                size: req.body.len(),
                max: MAX_MESSAGE_BODY_BYTES,
            });
        }
        if let Some(subject) = &req.subject
            && subject.len() > MAX_MESSAGE_SUBJECT_BYTES
        {
            return Err(MessageValidationError::SubjectTooLarge {
                size: subject.len(),
                max: MAX_MESSAGE_SUBJECT_BYTES,
            });
        }
        // Recipient routing key: bound the size before inspecting the shape, so an
        // oversized string is rejected with a precise cap rather than parsed.
        if req.recipient_identity.len() > MAX_MESSAGE_RECIPIENT_BYTES {
            return Err(MessageValidationError::RecipientIdentityTooLarge {
                size: req.recipient_identity.len(),
                max: MAX_MESSAGE_RECIPIENT_BYTES,
            });
        }
        // Shape: `identity/<kind>/<id>` with a non-empty tail and no empty segment.
        let well_formed = req
            .recipient_identity
            .strip_prefix("identity/")
            .is_some_and(|rest| !rest.is_empty() && rest.split('/').all(|s| !s.is_empty()));
        if !well_formed {
            return Err(MessageValidationError::RecipientIdentityMalformed);
        }
        Ok(Self {
            recipient_identity: req.recipient_identity,
            subject: req.subject,
            body: req.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> DateTime<Utc> {
        "2026-09-07T10:00:00Z"
            .parse()
            .expect("static RFC 3339 instant parses")
    }

    #[test]
    fn envelope_roundtrips_and_omits_none_vault_id() {
        let env = MessageEnvelope::new(
            "01J00000000000000000000000".into(),
            "identity/agent/sender".into(),
            "identity/agent/recipient".into(),
            Some("ping".into()),
            "hello".into(),
            ts(),
        );
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            !json.contains("vault_id"),
            "None vault_id must be omitted: {json}"
        );
        let back: MessageEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, env);
    }

    #[test]
    fn envelope_without_vault_id_defaults_to_none() {
        // A payload that predates the provenance field must deserialize cleanly
        // (additive-field guard): vault_id → None.
        let json = r#"{
            "message_id": "01J00000000000000000000000",
            "sender_identity": "identity/agent/sender",
            "recipient_identity": "identity/agent/recipient",
            "subject": "ping",
            "body": "hello",
            "created_at": "2026-09-07T10:00:00Z"
        }"#;
        let env: MessageEnvelope = serde_json::from_str(json).expect("deserialize");
        assert_eq!(env.vault_id, None);
        assert_eq!(env.subject.as_deref(), Some("ping"));
    }

    #[test]
    fn envelope_carries_vault_id_when_attached() {
        let env = MessageEnvelope::new(
            "01J00000000000000000000000".into(),
            "identity/agent/sender".into(),
            "identity/agent/recipient".into(),
            None,
            "hello".into(),
            ts(),
        )
        .with_vault_id("main".into());
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("\"vault_id\":\"main\""), "json: {json}");
        let back: MessageEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.vault_id.as_deref(), Some("main"));
    }

    #[test]
    fn message_status_wire_form_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&MessageStatus::Accepted).expect("serialize"),
            "\"accepted\""
        );
        assert_eq!(
            serde_json::to_string(&MessageStatus::Delivered).expect("serialize"),
            "\"delivered\""
        );
        let back: MessageStatus = serde_json::from_str("\"delivered\"").expect("deserialize");
        assert_eq!(back, MessageStatus::Delivered);
    }

    #[test]
    fn send_bounds_reject_oversized_body() {
        let oversized = "x".repeat(MAX_MESSAGE_BODY_BYTES + 1);
        let req =
            MessageSendRequest::new("identity/agent/recipient".into(), None, oversized.clone());
        let err = ValidatedMessageSend::try_from(req).expect_err("must reject oversized body");
        assert_eq!(
            err,
            MessageValidationError::BodyTooLarge {
                size: oversized.len(),
                max: MAX_MESSAGE_BODY_BYTES,
            }
        );
    }

    #[test]
    fn send_bounds_reject_oversized_subject() {
        let oversized = "s".repeat(MAX_MESSAGE_SUBJECT_BYTES + 1);
        let req = MessageSendRequest::new(
            "identity/agent/recipient".into(),
            Some(oversized.clone()),
            "ok".into(),
        );
        let err = ValidatedMessageSend::try_from(req).expect_err("must reject oversized subject");
        assert_eq!(
            err,
            MessageValidationError::SubjectTooLarge {
                size: oversized.len(),
                max: MAX_MESSAGE_SUBJECT_BYTES,
            }
        );
    }

    #[test]
    fn send_bounds_accept_valid_payload() {
        let req = MessageSendRequest::new(
            "identity/agent/recipient".into(),
            Some("ping".into()),
            "hello".into(),
        );
        let validated = ValidatedMessageSend::try_from(req).expect("within caps");
        assert_eq!(validated.recipient_identity, "identity/agent/recipient");
        assert_eq!(validated.subject.as_deref(), Some("ping"));
        assert_eq!(validated.body, "hello");
    }

    #[test]
    fn send_bounds_reject_empty_recipient() {
        let req = MessageSendRequest::new(String::new(), None, "hello".into());
        let err = ValidatedMessageSend::try_from(req).expect_err("must reject empty recipient");
        assert_eq!(err, MessageValidationError::RecipientIdentityMalformed);
    }

    #[test]
    fn send_bounds_reject_recipient_without_identity_prefix() {
        let req = MessageSendRequest::new("agent/recipient".into(), None, "hello".into());
        let err = ValidatedMessageSend::try_from(req)
            .expect_err("must reject recipient without identity/ prefix");
        assert_eq!(err, MessageValidationError::RecipientIdentityMalformed);
    }

    #[test]
    fn send_bounds_accept_valid_recipient() {
        let req = MessageSendRequest::new("identity/agent/recipient".into(), None, "hello".into());
        let validated = ValidatedMessageSend::try_from(req).expect("valid recipient identity");
        assert_eq!(validated.recipient_identity, "identity/agent/recipient");
    }

    /// `sender_identity` est DÉRIVÉ du credential serveur (jamais du client) : une
    /// requête qui tente de le fournir est rejetée à la désérialisation (400).
    #[test]
    fn send_request_rejects_client_supplied_sender_identity() {
        let json = r#"{
            "sender_identity": "identity/agent/forged",
            "recipient_identity": "identity/agent/recipient",
            "subject": "ping",
            "body": "hello"
        }"#;
        let err = serde_json::from_str::<MessageSendRequest>(json)
            .expect_err("un champ sender_identity client doit être rejeté (dérivé credential, C4)");
        assert!(err.to_string().contains("sender_identity"), "err: {err}");
    }
}
