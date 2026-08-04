//! Typed audit events for the Gradatum audit trail.
//!
//! ## Design
//!
//! `AuditEvent` is the observability pivot for Gradatum:
//! - Dual storage: SQLite (queries) + JSONL (external SIEM).
//! - `AuditEventType`: typed enum with named variants — no `payload: serde_json::Value`
//!   (avoids type erasure and makes diffs hard to read).
//! - `extra: ExtraFields`: out-of-spec fields preserved verbatim for forward compatibility.
//!   Lazily allocated — `None` when no extra fields are present.
//! - `#[non_exhaustive]` on `AuditEventType`: new variants can be added in SemVer-minor
//!   releases without a breaking change.
//!
//! ## Correlation
//!
//! `correlation_id: Option<Ulid>` — groups multiple `AuditEvent` instances produced by
//! a single atomic operation (e.g. batch import, cascading override resolution).
//! Omitted in serialisation when `None`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::author::AuthorRef;
use crate::frontmatter::ExtraFields;
use crate::identity::{ContentHash, NoteId, NoteVersion};
use crate::scope::OverrideScope;
use crate::status::NoteStatus;

/// Atomic audit event.
///
/// Produced by every significant operation on a note or override.
/// Persisted in SQLite (`audit_events` table) and streamed as JSONL to an external SIEM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Note affected by the event.
    pub note_id: NoteId,

    /// Typed event — no generic `payload: Value`.
    pub event_type: AuditEventType,

    /// Actor responsible for the action.
    pub actor: AuthorRef,

    /// UTC timestamp of the event.
    pub occurred_at: DateTime<Utc>,

    /// Out-of-spec fields preserved verbatim for forward compatibility.
    ///
    /// Omitted in serialisation when empty (reduces JSONL payload size).
    #[serde(default, skip_serializing_if = "ExtraFields::is_empty")]
    pub extra: ExtraFields,

    /// Optional correlation ID grouping events from the same atomic operation.
    ///
    /// Omitted in serialisation when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Ulid>,
}

/// Typed audit event variant.
///
/// Each variant carries exactly the data it needs — no generic `payload` field.
/// The JSON tag is in kebab-case (`#[serde(rename_all = "kebab-case")]`).
///
/// `#[non_exhaustive]` is **required**: new variants may be added in SemVer-minor
/// releases without a breaking change. Downstream `match` expressions must include
/// a catch-all arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AuditEventType {
    /// Note created for the first time.
    Created,

    /// Note frontmatter or body modified.
    Updated {
        /// List of modified fields (e.g. `["status", "tags"]`).
        fields_changed: Vec<String>,
    },

    /// Lifecycle status changed (e.g. `Draft → PendingReview`).
    StatusChanged {
        /// Previous status.
        from: NoteStatus,
        /// New status.
        to: NoteStatus,
        /// Optional reason — supplied by the curator or a human operator.
        reason: Option<String>,
    },

    /// Embedding computed or recomputed.
    Embedded {
        /// Embedder identifier (e.g. `"bge-small-en-v1.5"`, `"bge-m3"`).
        embedder_id: String,
        /// Model version (hash or semantic tag).
        model_version: String,
        /// Number of dimensions in the produced vector.
        dim: u16,
    },

    /// Note indexed for full-text search (FTS).
    Indexed {
        /// Number of FTS tokens generated.
        fts_tokens: u32,
    },

    /// ACL policy modified on the note.
    AclChanged {
        /// Textual diff of the policy (free format).
        policy_diff: String,
    },

    /// Override applied to the note.
    OverrideApplied {
        /// Scope of the override.
        scope: OverrideScope,
        /// Override type discriminant (e.g. `"metadata"`, `"acl"`).
        override_type: String,
    },

    /// Override revoked on the note.
    OverrideRevoked {
        /// Scope of the revoked override.
        scope: OverrideScope,
        /// Override type discriminant.
        override_type: String,
    },

    /// Note score recomputed (decay + pagerank).
    ScoreRecomputed {
        /// New decay score.
        new_decay: f32,
        /// New pagerank score.
        new_pagerank: f32,
    },

    /// Drift detected between the on-disk Markdown file and the SQLite index.
    DriftDetected {
        /// Hash stored in the SQLite index.
        stored_hash: ContentHash,
        /// Hash recomputed from the Markdown file.
        computed_hash: ContentHash,
    },

    /// Note read by a bearer (access traceability).
    Read {
        /// Bearer identifier.
        bearer_id: String,
        /// Fields accessed (e.g. `["body", "frontmatter.tags"]`).
        fields_accessed: Vec<String>,
    },

    /// Note deleted (moved to Garbage or physically removed).
    Deleted {
        /// Optional reason for deletion.
        reason: Option<String>,
    },

    /// Note restored from an earlier version.
    Restored {
        /// Version from which the note was restored.
        from_version: NoteVersion,
    },
}

// ─── HTTP service audit types ──────────────────────────────────────────────

/// HTTP audit types for the `gradatum-server` service.
///
/// `AuditEvent` and `AuditEventType` are reserved for the internal SQLite+SIEM trail.
/// The types below are flat (no typed enum) to match the JSONL contract of the HTTP service.
pub mod http {
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};

    /// Actor that triggered the audited HTTP operation.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HttpAuditActor {
        /// JWT token key ID (`kid` claim).
        pub kid: String,
        /// JWT token subject (`sub` claim).
        pub sub: String,
        /// JWT token audience (`aud` claim).
        pub aud: String,
        /// Token instance identifier (`jti` claim).
        ///
        /// `Some` for a JWT bearer (a revocable token instance), `None` for the other
        /// contexts — on the direct api-key path the stable key ULID already lives in
        /// `kid`. Omitted from the JSONL line when `None`, so that lines written before
        /// the field existed stay readable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub jti: Option<String>,
    }

    /// Flat audit event for the gradatum-server HTTP service.
    ///
    /// Persisted to JSONL with daily rotation (mode 0640). Distinct from
    /// `AuditEvent` (typed enum for SQLite + internal SIEM).
    ///
    /// ## Fields
    ///
    /// - `ts`: UTC timestamp of the operation.
    /// - `event`: free-form operation name (e.g. `"vault_write"`, `"vault_read"`).
    /// - `actor`: JWT bearer extracted by middleware.
    /// - `tenant_id`: affected tenant identifier.
    /// - `locus`: section/note path (e.g. `"decisions/my-note"`).
    /// - `note_id`: note ULID when applicable.
    /// - `content_hash`: JCS RFC 8785 SHA-256 of the canonical content (`sha256:<hex>`).
    /// - `outcome`: operation result (e.g. `"admitted"`, `"rejected"`, `"error"`).
    /// - `curator`: optional curator metadata (score, labels, etc.).
    /// - `request_id`: HTTP request correlation identifier.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HttpAuditEvent {
        /// UTC timestamp of the operation.
        pub ts: DateTime<Utc>,
        /// Audited operation name.
        pub event: String,
        /// Actor that triggered the operation.
        pub actor: HttpAuditActor,
        /// Affected tenant identifier.
        pub tenant_id: String,
        /// Section/note path (e.g. `"decisions/my-note"`).
        pub locus: String,
        /// Note ULID when applicable — omitted if absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub note_id: Option<String>,
        /// JCS RFC 8785 + SHA-256 of the canonical content — omitted if absent.
        ///
        /// Format: `"sha256:<hex64>"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub content_hash: Option<String>,
        /// Operation result.
        pub outcome: String,
        /// Optional curator metadata — omitted if absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub curator: Option<serde_json::Value>,
        /// HTTP request correlation identifier.
        pub request_id: String,
    }

    /// Sink trait for recording HTTP audit events durably.
    ///
    /// The production implementation is `JsonlFileSink` (in `gradatum-server`).
    /// A `NoopAuditSink` implementation is available for tests.
    #[async_trait]
    pub trait AuditSink: Send + Sync + 'static {
        /// Records an audit event durably.
        ///
        /// ## Side effects
        ///
        /// Production implementations write to disk and flush.
        /// I/O errors are returned without panicking.
        async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error>;
    }

    /// Computes the JCS RFC 8785 + SHA-256 hash of a JSON value.
    ///
    /// Returns `"sha256:<hex>"`. Two semantically equivalent JSON values (fields
    /// in different order) produce the same hash thanks to JCS canonicalisation.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if the value cannot be canonicalised
    /// (e.g. NaN number, floating-point map key — `serde_jcs::to_string` delegates
    /// to `serde_json` infrastructure for such errors).
    pub fn content_hash_jcs(value: &serde_json::Value) -> Result<String, serde_json::Error> {
        use sha2::{Digest, Sha256};
        let canonical = serde_jcs::to_string(value)?;
        let mut h = Sha256::new();
        h.update(canonical.as_bytes());
        // sha2 ≥0.11 : Output<Sha256> est un Array<u8,32> — plus de LowerHex natif.
        let digest: [u8; 32] = h.finalize().into();
        Ok(format!(
            "sha256:{}",
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
}
