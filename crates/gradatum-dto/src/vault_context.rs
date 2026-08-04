use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Assembly mode for the context produced by `vault_context`.
///
/// - `Assembled` (default): the pipeline produces a pre-formatted LLM Markdown block.
/// - `Raw`: returns raw hits without narrative assembly.
/// - `Compact`: folded view — the most relevant notes stay inline, the rest are
///   folded into dereferenceable stubs. Requires a `session_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ContextMode {
    /// Assembles hits into a ready-to-use LLM context block (default).
    #[default]
    Assembled,
    /// Returns raw hits without narrative assembly.
    Raw,
    /// Folded view: the most relevant notes stay inline; the rest are folded into
    /// dereferenceable stubs. Replaces the context block on the client side (one cache reset).
    /// Requires a `session_id`.
    Compact,
}

/// Optional scoring weights for context-hit ranking.
///
/// All fields are optional — only the provided weights override pipeline defaults.
/// Absent fields leave the pipeline's internal values unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ScoringWeights {
    /// Recency weight in the composite ranking score.
    pub recency: Option<f64>,
    /// PageRank weight in the composite ranking score.
    pub pagerank: Option<f64>,
    /// Trust weight in the composite ranking score.
    pub trust: Option<f64>,
}

/// Request body for `vault_context` — legacy vault v1.6.2 `VaultContextArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultContextRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Query for which to build the LLM context.
    pub query: String,
    /// Maximum number of context tokens (legacy — see `budget_tokens` for v0.7.0+).
    pub max_tokens: Option<u32>,
    /// Token budget for context assembly (v0.7.0+).
    ///
    /// Takes precedence over `max_tokens` when both are present.
    pub budget_tokens: Option<u32>,
    /// Section filter (optional).
    pub section: Option<String>,
    /// Context assembly mode (default: `Assembled`).
    #[serde(default)]
    pub mode: ContextMode,
    /// Optional scoring weights for hit ranking.
    pub scoring: Option<ScoringWeights>,
    /// Inject available skills into the context (default: `false`).
    #[serde(default)]
    pub inject_skills: bool,
    /// Skill selection query (active when `inject_skills = true`).
    pub skill_query: Option<String>,
    /// Enable reference mode: candidates that do not fit the inline budget are returned
    /// as dereferenceable stubs in the response's `references` field.
    ///
    /// `false` (default): `references` is always empty (backward-compatible behaviour).
    /// `true`: the inline/stub split is exposed in the response.
    ///
    /// **Additive field**: existing clients that omit it receive `false`
    /// despite `deny_unknown_fields` (absent field → default value, no error).
    #[serde(default)]
    pub reference_mode: bool,
    /// Session identifier for incremental context filtering.
    ///
    /// Expected format: ULID Crockford base32, 26 ASCII alphanumeric characters —
    /// aligned with the `POST /api/v1/session-log/trace` handler validation.
    ///
    /// `None` (default): no session filter — all matching notes are candidates.
    /// `Some(id)`: notes already sent inline in this session are demoted to stubs
    /// (frozen snippet from `session_trace`); new inline notes are marked via
    /// `mark_sent` for subsequent turns (no re-promotion).
    ///
    /// **Additive field**: existing clients that omit it receive `None`.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_defaults_are_assembled_and_no_skills() {
        // Un payload legacy (sans les nouveaux champs) doit désérialiser sans erreur
        let json = r#"{"query":"hello"}"#;
        let req: VaultContextRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mode, ContextMode::Assembled);
        assert!(!req.inject_skills);
        assert!(req.budget_tokens.is_none());
        assert!(req.scoring.is_none());
        assert!(req.skill_query.is_none());
        assert_eq!(req.tenant_id, None);
    }

    /// An absent `reference_mode` field defaults to `false` (backward-compatible).
    ///
    /// Ensures that existing clients which do not send `reference_mode` do not break
    /// despite `deny_unknown_fields` (the field is additive on the request side).
    #[test]
    fn request_reference_mode_defaults_to_false() {
        let req: VaultContextRequest = serde_json::from_str(r#"{"query":"hello"}"#).unwrap();
        assert!(
            !req.reference_mode,
            "reference_mode absent → false (rétro-compat F-35)"
        );
    }

    /// `reference_mode=true` se désérialise correctement.
    #[test]
    fn request_reference_mode_true_deserializes() {
        let req: VaultContextRequest =
            serde_json::from_str(r#"{"query":"hello","reference_mode":true}"#).unwrap();
        assert!(
            req.reference_mode,
            "reference_mode=true doit se désérialiser"
        );
    }

    #[test]
    fn context_mode_roundtrips_lowercase() {
        assert_eq!(serde_json::to_string(&ContextMode::Raw).unwrap(), "\"raw\"");
        assert_eq!(
            serde_json::to_string(&ContextMode::Compact).unwrap(),
            "\"compact\""
        );
        let m: ContextMode = serde_json::from_str("\"assembled\"").unwrap();
        assert_eq!(m, ContextMode::Assembled);
        let c: ContextMode = serde_json::from_str("\"compact\"").unwrap();
        assert_eq!(c, ContextMode::Compact);
    }

    #[test]
    fn scoring_weights_all_optional() {
        let json = r#"{"recency":0.5}"#;
        let w: ScoringWeights = serde_json::from_str(json).unwrap();
        assert_eq!(w.recency, Some(0.5));
        assert!(w.pagerank.is_none());
    }
}
