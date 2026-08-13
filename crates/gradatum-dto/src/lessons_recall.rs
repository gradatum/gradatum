//! DTO for the `GET /api/v1/lessons/recall` endpoint.
//!
//! Wire contract: request ([`LessonsRecallRequest`] — `class` + `limit`) and
//! response (list of [`LessonHit`]: ULID, title, snippet, tags, temporal anchor).
//!
//! Server-side, parameters arrive as HTTP query string; on the MCP stub side,
//! [`LessonsRecallRequest`] auto-derives the `inputSchema` for the
//! `vault_lessons_recall` tool.

use serde::{Deserialize, Serialize};

/// Ranking mode for [`LessonsRecallRequest`].
///
/// Controls the final ordering of BM25 results after `recall_lessons`.
/// When absent or `Relevance`, the legacy BM25 order is preserved (backward-compat).
///
/// # Wire encoding
/// `"relevance"` or `"recency-boosted"` (kebab-case, serde `rename_all`).
///
/// # Default behaviour
/// `None` / absent → BM25 order unchanged (LIVE hook `lesson-recall.sh` depends on it).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RankMode {
    /// Legacy BM25 order — default when the field is absent.
    ///
    /// A `rank=relevance` request is a no-op: it produces the same result as omitting
    /// the field entirely. This preserves the backward-compat invariant bit-for-bit.
    Relevance,
    /// Multiply each BM25 rank-proxy score by `recency_factor(anchor_ms, now_ms)`,
    /// then re-sort descending. Fresh lessons surface first.
    ///
    /// `recency_factor` uses λ = 0.01 (half-life ≈ 69 days, same as vault search).
    RecencyBoosted,
}

/// Controlled vocabulary of the 12 lesson classes.
///
/// Single source of truth shared between the server (validates the `class` query param
/// → 400 if not in the list) and the MCP stub (describes the `vault_lessons_recall`
/// tool). All vocabulary changes go through this constant — no duplication elsewhere.
pub const LESSON_CLASSES: [&str; 12] = [
    "deploy",
    "release",
    "migration",
    "crates-io",
    "anti-leak",
    "api-external",
    "archi",
    "git-hygiene",
    "ci-cd",
    "auth-secrets",
    "data-integrity",
    "process-discipline",
];

/// Returns `true` if `class` belongs to the controlled vocabulary [`LESSON_CLASSES`].
///
/// Case-sensitive exact match (no trimming) — the caller must supply a pre-cleaned
/// value if the input may contain surrounding whitespace.
#[must_use]
pub fn is_valid_lesson_class(class: &str) -> bool {
    LESSON_CLASSES.contains(&class)
}

/// Request for lesson recall.
///
/// Used to generate the `inputSchema` for the `vault_lessons_recall` MCP tool.
/// On the HTTP server side, these fields are received as query params (`?class=&limit=`).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LessonsRecallRequest {
    /// Controlled vocabulary class (one of [`LESSON_CLASSES`]):
    /// `deploy`, `release`, `migration`, `crates-io`, `anti-leak`, `api-external`,
    /// `archi`, `git-hygiene`, `ci-cd`, `auth-secrets`, `data-integrity`,
    /// `process-discipline`. Any other value is rejected (400).
    pub class: String,
    /// Maximum number of lessons (server default 5, clamped to `[1, 20]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Optional ranking mode.
    ///
    /// - absent / `"relevance"` → BM25 order (legacy behaviour, LIVE hook depends on it).
    /// - `"recency-boosted"` → rank-proxy × `recency_factor`, then sort descending.
    ///
    /// The field is optional and `deny_unknown_fields` is already set on this struct,
    /// so the addition is backward-compat: existing callers that omit the field continue
    /// to receive BM25-ordered results without any change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<RankMode>,
    /// Opt-in semantic (embedding) recall mode.
    ///
    /// - absent / `false` → BM25 `recall_lessons` path UNCHANGED (absolute backward-compat).
    ///   LIVE callers (hook `lesson-recall.sh`, agents, MCP) that do not send this field
    ///   keep receiving BM25 results without any change.
    /// - `true` → hybrid RRF retrieval, then ULID hydration, then the `codified` and
    ///   `class` filters applied in Rust. If embedding fails, silently falls back to BM25.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<bool>,
    /// Text query for semantic search (opt-in, `semantic = true`).
    ///
    /// If absent when `semantic = true`, the class (`class`) is used as the default
    /// query. Ignored if `semantic = false` / absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// If `true`, the server returns a **compact** rendering instead of the full
    /// [`LessonsRecallResponse`]: an object `{ "compact": "<text>" }` listing each
    /// lesson as `<ulid> — <title> :: <snippet>` (snippet truncated to 120 chars,
    /// newlines flattened), dropping `tags` and `anchor_ms`.
    ///
    /// Optimises token cost for LLM consumers (the compaction is a product feature).
    ///
    /// Opt-in (default `false`): when absent, the response is **byte-for-byte identical**
    /// to the historical [`LessonsRecallResponse`]. Existing callers (LIVE hook
    /// `lesson-recall.sh`, agents, MCP) are unaffected.
    #[serde(default)]
    pub compact: bool,
}

impl LessonsRecallRequest {
    /// Constructs a lessons-recall request for the controlled-vocabulary `class`;
    /// every optional field defaults to its unset (`None` / `false`) value.
    #[must_use]
    pub fn new(class: String) -> Self {
        Self {
            class,
            limit: None,
            rank: None,
            semantic: None,
            query: None,
            compact: false,
        }
    }
}

/// A lesson returned by the recall endpoint.
///
/// All fields are flat `String`/`i64` types (no domain `ULID`), in keeping with
/// the L0 wire-purity of `gradatum-dto`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LessonHit {
    /// ULID identifier of the lesson (26 Crockford base32 characters).
    pub ulid: String,
    /// H1 title of the lesson. Empty string if the note has no extracted title.
    pub title: String,
    /// Native FTS5 snippet localized on the searched class.
    pub snippet: String,
    /// Tags of the lesson (the `codified` tag is guaranteed absent).
    pub tags: Vec<String>,
    /// Temporal anchor: creation timestamp (`created`), Unix epoch ms.
    pub anchor_ms: i64,
}

/// Response for `GET /api/v1/lessons/recall`.
///
/// Ordered by BM25 relevance (best match first), bounded by `limit`.
/// `items` is empty if no lesson matches the class.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LessonsRecallResponse {
    /// Recalled lessons, sorted by descending BM25 score.
    pub items: Vec<LessonHit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_classes_accepted() {
        for c in LESSON_CLASSES {
            assert!(is_valid_lesson_class(c), "{c} doit être valide");
        }
        assert_eq!(
            LESSON_CLASSES.len(),
            12,
            "vocabulaire = 12 classes (spec §2.3)"
        );
    }

    #[test]
    fn invalid_classes_rejected() {
        assert!(!is_valid_lesson_class(""), "vide rejeté");
        assert!(!is_valid_lesson_class("Deploy"), "casse stricte");
        assert!(!is_valid_lesson_class("unknown"), "hors vocabulaire rejeté");
        // Anti-injection FTS : un payload opérateur n'est jamais dans le vocabulaire.
        assert!(
            !is_valid_lesson_class("deploy OR release"),
            "injection rejetée"
        );
        assert!(
            !is_valid_lesson_class("\" OR 1=1 --"),
            "injection SQL rejetée"
        );
    }
}
