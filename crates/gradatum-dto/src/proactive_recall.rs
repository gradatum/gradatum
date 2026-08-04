//! DTOs for the proactive memory surface endpoints.
//!
//! Wire contract for:
//! - [`ProactiveRecallRequest`] — pull the pre-computed surface or trigger contextual recall.
//! - [`ProactiveRecallResponse`] — surfaced notes + recall session id.
//! - [`ProactiveHit`] — a single surfaced note.
//! - [`ProactiveRecallFeedbackRequest`] — user feedback (accepted ULIDs).
//!
//! Server-side, [`ProactiveRecallRequest`] is deserialized from a JSON POST body.
//! On the MCP stub side, both request structs auto-derive the `inputSchema` via the
//! `schemars` feature.

use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Request to pull the proactive memory surface or trigger contextual recall.
///
/// When `context` is absent, the server returns the pre-computed proactive surface
/// (`mode = "proactive"`). When `context` is present, the server runs an on-demand
/// contextual RRF retrieval (`mode = "contextual"`).
///
/// # Wire notes
/// `#[serde(deny_unknown_fields)]` rejects unrecognised fields — forward-compat
/// discipline: consumers must stay within the declared contract.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProactiveRecallRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Optional context text for contextual recall.
    ///
    /// When present, the server computes an on-demand RRF retrieval using this text
    /// as the query and returns `mode = "contextual"`. When absent, the server returns
    /// the pre-computed proactive surface (`mode = "proactive"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Optional section filter — restricts surfaced notes to this set of sections.
    ///
    /// Absent or `null`: no section restriction (all sections surfaced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<String>>,
    /// Maximum number of items to return (server default 10, clamped to `[1, 20]`).
    ///
    /// Absent or `null`: server-default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A single note surfaced by the proactive recall pipeline.
///
/// All fields are flat `String`/`f64` types (L0 wire-purity — no domain `ULID`/`TenantId`).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProactiveHit {
    /// ULID of the note (26 Crockford base32 characters).
    pub ulid: String,
    /// H1 title of the note. Empty string if the note has no extracted title.
    pub title: String,
    /// Vault section of the note (e.g. `"lessons-learned"`, `"decisions"`).
    pub section: String,
    /// FTS5 snippet or semantic excerpt from the note body.
    pub snippet: String,
    /// Composite relevance score (RRF-based, higher = more relevant).
    ///
    /// `f64` — not `Eq` (NaN semantics); use `PartialEq` for test comparisons only.
    pub score: f64,
}

/// Response for the proactive recall endpoints.
///
/// `recall_id` is a server-generated ULID identifying this recall session. Pass it
/// back in [`ProactiveRecallFeedbackRequest`] to correlate accepted notes with this
/// surface computation.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProactiveRecallResponse {
    /// Server-generated recall session identifier (ULID).
    ///
    /// Passed back in [`ProactiveRecallFeedbackRequest::recall_id`] to correlate
    /// user feedback with this surface.
    pub recall_id: String,
    /// Recall mode: `"proactive"` (pre-computed surface) or `"contextual"` (on-demand RRF).
    pub mode: String,
    /// Surfaced notes, ordered by descending composite score.
    pub items: Vec<ProactiveHit>,
}

/// Request to submit feedback on a proactive recall session.
///
/// `recall_id` must match a `recall_id` previously returned by the recall endpoint.
/// `accepted_ulids` should be a subset of the ULIDs present in that response.
///
/// # Wire notes
/// `#[serde(deny_unknown_fields)]` enforces contract discipline (same as request).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProactiveRecallFeedbackRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Recall session identifier returned by the recall endpoint.
    pub recall_id: String,
    /// ULIDs of notes accepted (acted on) by the user.
    ///
    /// Must be a subset of the ULIDs present in the corresponding
    /// [`ProactiveRecallResponse::items`]. An empty vec is valid (no note accepted).
    pub accepted_ulids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProactiveRecallRequest — defaults ────────────────────────────────────

    #[test]
    fn request_tenant_id_absent_defaults_to_none() {
        let req: ProactiveRecallRequest = serde_json::from_str("{}").expect("empty object valide");
        assert_eq!(
            req.tenant_id, None,
            "tenant_id absent → None (A1 : plus de défaut \"main\")"
        );
    }

    #[test]
    fn request_context_absent_defaults_to_none() {
        let req: ProactiveRecallRequest =
            serde_json::from_str(r#"{"limit": 5}"#).expect("deserialize OK");
        assert_eq!(req.context, None, "context absent → None");
        assert_eq!(req.sections, None, "sections absent → None");
        assert_eq!(req.limit, Some(5));
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn request_limit_absent_defaults_to_none() {
        let req: ProactiveRecallRequest = serde_json::from_str("{}").expect("empty object valide");
        assert_eq!(req.limit, None, "limit absent → None");
    }

    #[test]
    fn request_sections_present_preserved() {
        let json = r#"{"sections": ["decisions", "lessons-learned"]}"#;
        let req: ProactiveRecallRequest = serde_json::from_str(json).expect("deserialize OK");
        assert_eq!(
            req.sections,
            Some(vec!["decisions".to_string(), "lessons-learned".to_string()])
        );
    }

    // ── ProactiveRecallRequest — deny_unknown_fields ─────────────────────────

    #[test]
    fn request_deny_unknown_field_returns_error() {
        let json = r#"{"unknown_field": "valeur_inconnue"}"#;
        let result: Result<ProactiveRecallRequest, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields doit rejeter un champ non déclaré"
        );
    }

    // ── ProactiveRecallFeedbackRequest — defaults ────────────────────────────

    #[test]
    fn feedback_tenant_id_absent_defaults_to_none() {
        let json = r#"{"recall_id": "01JXYZTEST", "accepted_ulids": []}"#;
        let req: ProactiveRecallFeedbackRequest =
            serde_json::from_str(json).expect("deserialize OK");
        assert_eq!(
            req.tenant_id, None,
            "tenant_id absent → None (A1 : plus de défaut \"main\")"
        );
        assert_eq!(req.recall_id, "01JXYZTEST");
        assert!(req.accepted_ulids.is_empty());
    }

    #[test]
    fn feedback_deny_unknown_field_returns_error() {
        let json = r#"{"recall_id": "01JXYZTEST", "accepted_ulids": [], "extra_champ": true}"#;
        let result: Result<ProactiveRecallFeedbackRequest, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields doit rejeter un champ non déclaré"
        );
    }

    // ── Roundtrip serialize → deserialize ───────────────────────────────────

    #[test]
    fn request_roundtrip() {
        let original = ProactiveRecallRequest {
            tenant_id: Some(TenantId::new("main")),
            context: Some("contexte de test".to_string()),
            sections: Some(vec!["decisions".to_string(), "lessons-learned".to_string()]),
            limit: Some(10),
        };
        let json = serde_json::to_string(&original).expect("serialize OK");
        let restored: ProactiveRecallRequest = serde_json::from_str(&json).expect("deserialize OK");
        assert_eq!(original, restored);
    }

    #[test]
    fn response_roundtrip() {
        let original = ProactiveRecallResponse {
            recall_id: "01JXYZ000000000000000000".to_string(),
            mode: "proactive".to_string(),
            items: vec![ProactiveHit {
                ulid: "01JABC000000000000000000".to_string(),
                title: "Titre de test".to_string(),
                section: "lessons-learned".to_string(),
                snippet: "Un snippet de test extrait du corps.".to_string(),
                score: 0.85,
            }],
        };
        let json = serde_json::to_string(&original).expect("serialize OK");
        let restored: ProactiveRecallResponse =
            serde_json::from_str(&json).expect("deserialize OK");
        assert_eq!(original, restored);
    }

    #[test]
    fn feedback_roundtrip() {
        let original = ProactiveRecallFeedbackRequest {
            tenant_id: Some(TenantId::new("test-tenant")),
            recall_id: "01JXYZ000000000000000000".to_string(),
            accepted_ulids: vec![
                "01JABC000000000000000000".to_string(),
                "01JDEF000000000000000000".to_string(),
            ],
        };
        let json = serde_json::to_string(&original).expect("serialize OK");
        let restored: ProactiveRecallFeedbackRequest =
            serde_json::from_str(&json).expect("deserialize OK");
        assert_eq!(original, restored);
    }
}
