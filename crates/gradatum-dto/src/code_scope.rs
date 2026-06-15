//! DTO for the `POST /api/v1/code_scope` endpoint.
//!
//! Frozen wire contract. Request
//! ([`CodeScopeRequest`] — `vault` + `selector` + `budget_tokens`) and response
//! ([`CodeScopeResponse`] — entries bounded by budget + `truncated`/`stale` flags).
//!
//! `code_scope` is a **dedicated** endpoint that bypasses by design the 403 guard
//! `vault_id ≠ main` — it MUST therefore reject (400) any `vault` not starting with
//! `code-` (security invariant). This validation is enforced server-side in the handler;
//! the DTO only carries the contract.
//!
//! On the MCP stub side, [`CodeScopeRequest`] auto-derives the `inputSchema` for the
//! `code_scope` tool.

use serde::{Deserialize, Serialize};

/// Default token budget when not provided.
pub const DEFAULT_BUDGET_TOKENS: u32 = 800;

/// Valid selector kinds (explicit discriminant).
pub const SELECTOR_KINDS: [&str; 3] = ["query", "path", "symbol"];

/// Returns `true` if `kind` is a valid selector kind.
#[must_use]
pub fn is_valid_selector_kind(kind: &str) -> bool {
    SELECTOR_KINDS.contains(&kind)
}

/// Selector for a `code_scope` request — explicit discriminant.
///
/// `kind` ∈ {`query`, `path`, `symbol`}:
/// - `query`: FTS5 full-text search (BM25) over the symbol corpus.
/// - `path`: all symbols in a file or directory (prefix match on `source_path`).
/// - `symbol`: symbols whose `qualified_name` contains `value` (substring match).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeSelectorDto {
    /// Selector type: `query` | `path` | `symbol`. Any other value → 400.
    pub kind: String,
    /// Criterion value (keywords, path, or symbol name).
    pub value: String,
}

/// Default body token budget when not provided.
///
/// ≈ 15–20 bodies of ~30 lines each. Use 8000–12000 for large bodies (50–100 lines) or N > 20.
pub const DEFAULT_BODY_BUDGET_TOKENS: usize = 4_000;

/// Request body for `POST /api/v1/code_scope` (frozen wire contract).
///
/// ## Additive `include_body` fields (fully backward-compatible)
///
/// - `include_body` (default `false`): if `true`, each entry receives a `body` field
///   containing the exact source span of the symbol. `false` = strictly unchanged
///   behavior, byte-for-byte identical JSON.
/// - `body_budget_tokens` (default 4000, clamped to \[1, 32000\]): body budget independent
///   of `budget_tokens` (orthogonal). Cuts at whole-body boundaries; never truncates
///   mid-body. Entries without a body (stale or span absent): `body=null` omitted from
///   JSON (`skip_serializing_if`).
///
/// ## Mandatory two-pass pattern
///
/// **Pass 1**: `include_body=false` (default) → rank the N relevant symbols by signature.
/// **Pass 2**: `include_body=true` on the selected symbols only, with a targeted `selector`.
/// Using `include_body=true` on a broad query negates the token savings.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeScopeRequest {
    /// Target logical vault — MUST start with `code-` (e.g. `code-gradatum`).
    /// Any other vault (`main`, arbitrary vault_id) → 400 (security invariant).
    pub vault: String,
    /// Search criterion (discriminant `kind` + `value`).
    pub selector: CodeSelectorDto,
    /// Response token budget (default [`DEFAULT_BUDGET_TOKENS`], clamped server-side).
    /// Entries are cut to K whole entries such that Σ tokens ≤ budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// If `true`, each entry receives `body` = exact source span of the symbol (tree-sitter).
    ///
    /// Default `false` — strictly unchanged behavior.
    /// See the two-pass pattern in the struct-level documentation.
    #[serde(default)]
    pub include_body: bool,
    /// Token budget reserved for bodies (default [`DEFAULT_BODY_BUDGET_TOKENS`], clamped to \[1, 32000\]).
    ///
    /// Independent of `budget_tokens`. Cuts at whole-body boundaries.
    /// Ignored if `include_body=false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_budget_tokens: Option<usize>,
}

/// A scope map entry (one code symbol).
///
/// ## `body` field (additive)
///
/// Present only if `include_body=true` in the request AND the entry is not stale
/// AND the span is available. In all other cases the field is ABSENT from the JSON
/// (`skip_serializing_if = "Option::is_none"` — guarantees byte-for-byte parity
/// with `include_body=false`).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeScopeEntry {
    /// Deterministic ULID identifier of the derived note.
    pub note_id: String,
    /// Source file path (relative to the repo root).
    pub source_path: String,
    /// Entity kind (`fn`, `struct`, `enum`, `trait`, `impl`, `const`, `mod`, `method`).
    pub kind: String,
    /// Qualified name (e.g. `"Parser::parse"`).
    pub qualified_name: String,
    /// Textual signature (params + return type). Empty string if not extractable.
    pub signature: String,
    /// Outgoing intra-repo dependencies (qualified_name, best-effort).
    pub deps: Vec<String>,
    /// `true` if the source file has changed since ingest (drift detected).
    /// A `stale=true` entry MUST NOT be used as ground truth — a regeneration
    /// has been enqueued. Never returned silently stale.
    pub stale: bool,
    /// Exact body of the symbol (lines `[start_line..=end_line]` of the source file).
    ///
    /// Present only if `include_body=true` ∧ `!stale` ∧ span available ∧
    /// body budget not exceeded. `None` (omitted from JSON) otherwise.
    ///
    /// Sliced only when `!stale`, because `stale=false` proves byte-identity of the
    /// file (whole-file hash) ⟹ lines `[start..=end]` are accurate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Response for `POST /api/v1/code_scope` (frozen wire contract).
///
/// ## Orthogonal flags `truncated` ⟂ `body_truncated`
///
/// - `truncated`: **entries** were omitted because `budget_tokens` was reached
///   (signatures). Omitted entries do not appear in `entries[]`.
/// - `body_truncated`: **bodies** were omitted because `body_budget_tokens` was reached
///   among the retained entries. The entry remains in `entries[]` with `body=null`
///   (omitted from JSON).
///
/// Both flags are INDEPENDENT. `body_truncated` is omitted from JSON when `false`
/// (backward-compatible).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeScopeResponse {
    /// Relevant symbols, ranked (BM25 + structural cohesion bonus), bounded by budget.
    pub entries: Vec<CodeScopeEntry>,
    /// `true` if `budget_tokens` was reached → some entries were omitted.
    pub truncated: bool,
    /// Total number of symbols matching the selector (before budget truncation).
    pub total_matched: u32,
    /// `true` if bodies were omitted because `body_budget_tokens` was reached.
    ///
    /// Independent of `truncated`. Omitted from JSON when `false` (backward-compatible).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_kinds_valid() {
        assert!(is_valid_selector_kind("query"));
        assert!(is_valid_selector_kind("path"));
        assert!(is_valid_selector_kind("symbol"));
        assert!(!is_valid_selector_kind("Query"), "casse stricte");
        assert!(!is_valid_selector_kind(""), "vide rejeté");
        assert!(
            !is_valid_selector_kind("unknown"),
            "hors vocabulaire rejeté"
        );
    }

    #[test]
    fn request_roundtrip() {
        let json = r#"{"vault":"code-gradatum","selector":{"kind":"query","value":"parse"},"budget_tokens":500}"#;
        let req: CodeScopeRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(req.vault, "code-gradatum");
        assert_eq!(req.selector.kind, "query");
        assert_eq!(req.budget_tokens, Some(500));
    }

    #[test]
    fn request_budget_default_omitted() {
        let json = r#"{"vault":"code-x","selector":{"kind":"path","value":"src/a.rs"}}"#;
        let req: CodeScopeRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(
            req.budget_tokens, None,
            "budget absent → None (défaut serveur)"
        );
    }

    #[test]
    fn request_rejects_unknown_field() {
        let json = r#"{"vault":"code-x","selector":{"kind":"query","value":"y"},"extra":1}"#;
        assert!(
            serde_json::from_str::<CodeScopeRequest>(json).is_err(),
            "deny_unknown_fields"
        );
    }
}
