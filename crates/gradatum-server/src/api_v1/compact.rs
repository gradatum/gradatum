//! Compact rendering of read-endpoint responses — opt-in `compact: true`.
//!
//! The four vault read tools (`vault_search`, `vault_read`, `vault_timeline`,
//! `vault_lessons_recall`) can return a **compact** projection instead of their full
//! JSON response, tailored to LLM consumers whose only cost is the number of tokens they
//! read. The compaction is **product value**, not transport: it used to live in per-skill
//! bash wrappers (removed) and is now first-class in the service, reachable by any
//! consumer — including the public MCP tool — that sets `compact: true`.
//!
//! ## Envelope (coherent across the four)
//!
//! When `compact: true`, the handler returns a single object [`CompactBody`]:
//!
//! ```json
//! { "compact": "<rendered text>" }
//! ```
//!
//! When `compact: false` (default / absent), the handler returns its historical response
//! **byte-for-byte unchanged** — this module is never on that path.
//!
//! ## What each renderer preserves (faithful to the removed L0 wrappers)
//!
//! - **search**: `<ulid> [<score>]` per hit, plus the `corpus_match_count` hint with a
//!   three-state distinction (see `corpus_hint`): `count > 0`, a proven *lexical*
//!   absence (count `0` on a matchable query form — which never disproves the returned
//!   results), or a count *not applicable* to the query form (count `0` on a
//!   punctuation-only query whose tokens all normalise to empty).
//! - **read**: `path`, optional `section`, optional `title`, `sha256` (needed for a later
//!   in-place update) and the note `content`. Drops only `metadata` and `size_bytes`.
//! - **timeline**: `<anchor_ms> | <doc_kind> | <note_id> — <title>` per entry. Drops
//!   `anchor_src` and the pagination `next_cursor`.
//! - **recall**: `<ulid> — <title> :: <snippet>` (snippet clamped to 120 chars, newlines
//!   flattened). Drops `tags` and `anchor_ms`.

use std::fmt::Write as _;

use serde::Serialize;

use gradatum_dto::LessonsRecallResponse;

use crate::api_v1::dto::{VaultReadResponse, VaultSearchResponse};
use crate::api_v1::timeline::VaultTimelineResponse;

/// Maximum snippet length (chars) kept in the compact recall rendering.
const RECALL_SNIPPET_MAX_CHARS: usize = 120;

/// Compact envelope returned when a read endpoint is called with `compact: true`.
///
/// Single shared shape for the four read tools — a coherent envelope rather than four
/// ad-hoc bodies. Serialises to `{ "compact": "<text>" }`.
#[derive(Debug, Serialize)]
pub struct CompactBody {
    /// The rendered, token-optimised text for LLM consumption.
    pub compact: String,
}

/// Extracts the ULID (last path segment) from a `"<section>/<ulid>"` path.
///
/// A path without `/` is returned as-is (whole string), matching the removed wrapper.
fn ulid_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Returns `true` when the query's *form* could produce a lexical (FTS5) match.
///
/// A query made only of punctuation, symbols or whitespace (`"-"`, `"..."`, `"- -"`)
/// tokenises to nothing under the `unicode61` tokenizer: after `build_fts_query`
/// every token normalises to empty, so the FTS5 query — while syntactically valid —
/// can match no document, whatever the corpus. A zero count on such a query is an
/// artefact of the query form, not a property of the corpus.
///
/// The predicate is `char::is_alphanumeric`, which mirrors the Unicode letter/number
/// classes `unicode61` keeps as token characters. It is deliberately conservative:
/// it never labels a punctuation-only query as matchable.
fn query_form_can_match(query: &str) -> bool {
    query.chars().any(char::is_alphanumeric)
}

/// Builds the `corpus_match_count` hint appended to the compact search header.
///
/// Three states, not two:
///
/// 1. **count > 0** — some lexical matches exist (rendered plainly, `+` if capped).
/// 2. **count == 0 on a matchable query form** — the *lexical* absence of the term in
///    the filtered surface is proven. This is scoped to the lexical arm only: it does
///    **not** disprove the semantic relevance of the results that were returned. The
///    older wording ("absence proven, semantic neighbours only") made two correct
///    results — one answering the exact question asked — be discarded; hence this
///    branch never invites dropping the returned hits.
/// 3. **count == 0 on an unmatchable query form** — a punctuation/operators-only query
///    whose tokens all normalise to empty (see [`query_form_can_match`]). The zero is
///    an artefact of the query form, never a corpus property, so the count is reported
///    as *not applicable*. This branch never claims any absence is proven.
///
/// Empty string when the count is absent (the request did not opt into it).
fn corpus_hint(resp: &VaultSearchResponse, query: &str) -> String {
    match resp.corpus_match_count {
        None => String::new(),
        Some(0) if query_form_can_match(query) => {
            // State 2 — matchable form, zero lexical hits: proven lexical absence.
            " (corpus_match_count=0 -> 0 lexical match: lexical absence of the term in the \
             filtered surface is proven; the semantic relevance of the returned results is \
             NOT disproven)"
                .to_string()
        }
        Some(0) => {
            // State 3 — unmatchable form: the zero is an artefact of the query, not the corpus.
            " (corpus_match_count=0 -> count not applicable to this query form: every token \
             normalises to empty (punctuation/operators only), so no document can match \
             lexically whatever the corpus; the returned results are semantic-only and this \
             zero says nothing about them)"
                .to_string()
        }
        Some(n) => {
            let capped = if resp.corpus_count_capped { "+" } else { "" };
            format!(" (corpus_match_count={n}{capped} lexical matches)")
        }
    }
}

/// Renders a [`VaultSearchResponse`] to its compact text form.
///
/// `query` is the original search string. It is needed to distinguish a *lexical
/// absence* (count `0` on a query whose form could have matched) from a count that
/// is simply *not applicable* to the query form (a punctuation-only query whose
/// tokens all normalise to empty). See `corpus_hint`.
#[must_use]
pub fn render_search(resp: &VaultSearchResponse, query: &str) -> String {
    let hint = corpus_hint(resp, query);
    if resp.items.is_empty() {
        return format!("0 relevant notes on this topic{hint}");
    }
    let mut out = String::with_capacity(32 * resp.items.len());
    let _ = write!(out, "{} notes{hint}: ", resp.items.len());
    for (i, hit) in resp.items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // `score` is f32 RRF-composite; 4 decimals is enough to order/compare within a
        // result set (upper bound ~0.04).
        let _ = write!(out, "{} [{:.4}]", ulid_of(&hit.path), hit.score);
    }
    out
}

/// Renders a [`VaultReadResponse`] to its compact text form.
///
/// `section` is best-effort from the frontmatter `metadata.section` (the response does not
/// carry a dedicated section field), matching the removed wrapper.
#[must_use]
pub fn render_read(resp: &VaultReadResponse) -> String {
    let mut out = String::with_capacity(resp.content.len() + 128);
    out.push_str(&resp.path);
    if let Some(section) = resp
        .metadata
        .as_ref()
        .and_then(|m| m.get("section"))
        .and_then(serde_json::Value::as_str)
    {
        let _ = write!(out, " (section: {section})");
    }
    if let Some(title) = resp.title.as_deref().filter(|t| !t.is_empty()) {
        let _ = write!(out, "\ntitle: {title}");
    }
    let _ = write!(out, "\nsha256: {}\n---\n{}", resp.sha256, resp.content);
    out
}

/// Renders a [`VaultTimelineResponse`] to its compact text form.
#[must_use]
pub fn render_timeline(resp: &VaultTimelineResponse) -> String {
    if resp.items.is_empty() {
        return "0 entries".to_string();
    }
    let mut out = format!("{} entries:", resp.items.len());
    for item in &resp.items {
        let title = item.title.as_deref().unwrap_or("(untitled)");
        let _ = write!(
            out,
            "\n  {} | {} | {} — {title}",
            item.anchor_ms, item.doc_kind, item.note_id
        );
    }
    out
}

/// Renders a [`LessonsRecallResponse`] to its compact text form.
///
/// `class` is the recall class from the request (not carried in the response body).
#[must_use]
pub fn render_recall(resp: &LessonsRecallResponse, class: &str) -> String {
    if resp.items.is_empty() {
        return format!("0 lessons [class={class}]");
    }
    let mut out = format!("{} lessons [class={class}]:", resp.items.len());
    for hit in &resp.items {
        // Flatten newlines and clamp to a char boundary (never mid-codepoint).
        let flat: String = hit
            .snippet
            .chars()
            .map(|c| if c == '\n' { ' ' } else { c })
            .collect();
        let snippet: String = flat.chars().take(RECALL_SNIPPET_MAX_CHARS).collect();
        let title = if hit.title.is_empty() {
            "(untitled)"
        } else {
            &hit.title
        };
        let _ = write!(out, "\n  {} — {title} :: {snippet}", hit.ulid);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_v1::dto::SearchHit;
    use crate::api_v1::timeline::TimelineItem;
    use gradatum_core::scope::VaultId;
    use gradatum_dto::LessonHit;

    #[allow(deprecated)] // `SearchHit.trust` is a deprecated wire field we must still set.
    fn hit(path: &str, score: f32) -> SearchHit {
        SearchHit {
            vault_id: VaultId::new("main"),
            path: path.to_string(),
            score,
            title: Some("t".to_string()),
            snippet: Some("s".to_string()),
            trust: 0.5,
            status: "live".to_string(),
            scores: None,
            anchor_ms: None,
        }
    }

    #[test]
    fn search_empty_no_corpus() {
        let resp = VaultSearchResponse {
            items: vec![],
            corpus_match_count: None,
            corpus_count_capped: false,
        };
        // No count opted in → no hint regardless of query form.
        assert_eq!(
            render_search(&resp, "gradatum"),
            "0 relevant notes on this topic"
        );
    }

    // ── Three states of the corpus-count hint ─────────────────────────────────
    // State 1: count > 0  → unchanged.
    // State 2: count == 0 on a query whose FORM could match → proven *lexical* absence.
    // State 3: count == 0 on a query whose FORM cannot match → count not applicable.

    #[test]
    fn query_form_can_match_alnum_true_punctuation_false() {
        // Token characters (letters/digits, incl. CJK, and base of a combining pair).
        assert!(query_form_can_match("cargo-semver-checks"));
        assert!(query_form_can_match("2.0.7"));
        assert!(query_form_can_match("中文"));
        assert!(query_form_can_match("e\u{0301}"));
        // Pure punctuation / symbols / whitespace → every token normalises to empty.
        assert!(!query_form_can_match("-"));
        assert!(!query_form_can_match("..."));
        assert!(!query_form_can_match("- -"));
        assert!(!query_form_can_match(""));
        assert!(!query_form_can_match("🙂"));
    }

    #[test]
    fn search_state1_positive_count_never_mentions_absence() {
        let resp = VaultSearchResponse {
            items: vec![hit("s/01A", 0.02)],
            corpus_match_count: Some(3),
            corpus_count_capped: false,
        };
        let out = render_search(&resp, "cargo-semver-checks");
        assert!(
            out.contains("corpus_match_count=3 lexical matches"),
            "positive count rendered plainly: {out}"
        );
        assert!(
            !out.contains("absence"),
            "a positive count never speaks of absence: {out}"
        );
    }

    #[test]
    fn search_state2_lexical_absence_when_form_matchable() {
        let resp = VaultSearchResponse {
            items: vec![hit("decisions/01ABC", 0.0167)],
            corpus_match_count: Some(0),
            corpus_count_capped: false,
        };
        // Matchable form (has token characters) → proven *lexical* absence.
        let out = render_search(&resp, "cargo-semver-checks");
        assert!(
            out.contains("lexical absence"),
            "absence must be named as lexical, not generic: {out}"
        );
        assert!(
            out.contains("NOT disproven"),
            "the semantic relevance of returned results must not be presented as disproven: {out}"
        );
        assert!(
            out.contains("01ABC [0.0167]"),
            "the returned result is still projected, never dropped: {out}"
        );
    }

    #[test]
    fn search_state3_unmatchable_form_is_not_applicable_never_absence() {
        let resp = VaultSearchResponse {
            items: vec![hit("decisions/01ABC", 0.0167)],
            corpus_match_count: Some(0),
            corpus_count_capped: false,
        };
        // Punctuation-only form → the zero is an artefact of the query, not the corpus.
        let out = render_search(&resp, "- ...");
        assert!(
            out.contains("not applicable to this query form"),
            "must state the count is not applicable to this form: {out}"
        );
        assert!(
            !out.contains("absence"),
            "an unmatchable form must never claim any absence: {out}"
        );
        assert!(
            !out.contains("proven"),
            "an unmatchable form must never claim anything is proven: {out}"
        );
        assert!(
            out.contains("01ABC [0.0167]"),
            "the semantic result is still projected: {out}"
        );
    }

    #[test]
    fn search_corpus_capped_marks_plus() {
        let resp = VaultSearchResponse {
            items: vec![hit("s/01A", 0.01)],
            corpus_match_count: Some(10_000),
            corpus_count_capped: true,
        };
        assert!(render_search(&resp, "gradatum").contains("corpus_match_count=10000+ lexical"));
    }

    #[test]
    fn read_keeps_sha_and_content_drops_meta() {
        let resp = VaultReadResponse {
            path: "decisions/01A".to_string(),
            title: Some("My note".to_string()),
            content: "body here".to_string(),
            metadata: Some(serde_json::json!({ "section": "decisions", "extra": "dropped" })),
            size_bytes: 9,
            sha256: "abcd".to_string(),
        };
        let out = render_read(&resp);
        assert!(out.contains("decisions/01A"));
        assert!(out.contains("(section: decisions)"));
        assert!(out.contains("title: My note"));
        assert!(out.contains("sha256: abcd"));
        assert!(out.ends_with("---\nbody here"));
        assert!(
            !out.contains("dropped"),
            "metadata beyond section is dropped: {out}"
        );
        assert!(!out.contains("size_bytes"));
    }

    #[test]
    fn timeline_empty_and_rows() {
        let empty = VaultTimelineResponse {
            items: vec![],
            next_cursor: None,
        };
        assert_eq!(render_timeline(&empty), "0 entries");
        let resp = VaultTimelineResponse {
            items: vec![TimelineItem {
                note_id: "01A".to_string(),
                anchor_ms: 1700,
                anchor_src: "created".to_string(),
                doc_kind: "Event".to_string(),
                title: Some("hello".to_string()),
            }],
            next_cursor: Some("cur".to_string()),
        };
        let out = render_timeline(&resp);
        assert_eq!(out, "1 entries:\n  1700 | Event | 01A — hello");
        assert!(!out.contains("created"), "anchor_src dropped: {out}");
        assert!(!out.contains("cur"), "next_cursor dropped: {out}");
    }

    #[test]
    fn recall_truncates_snippet_and_drops_tags() {
        let long = "x".repeat(200);
        let resp = LessonsRecallResponse {
            items: vec![LessonHit {
                ulid: "01A".to_string(),
                title: "Lesson".to_string(),
                snippet: format!("line1\n{long}"),
                tags: vec!["secret-tag".to_string()],
                anchor_ms: 1700,
            }],
        };
        let out = render_recall(&resp, "deploy");
        assert!(out.starts_with("1 lessons [class=deploy]:"));
        assert!(out.contains("01A — Lesson :: "));
        assert!(!out.contains("secret-tag"), "tags dropped: {out}");
        // snippet clamped to 120 chars (newline flattened to space counts as 1 char).
        let rendered_snippet = out.split(":: ").nth(1).unwrap();
        assert_eq!(
            rendered_snippet.chars().count(),
            120,
            "snippet clamped: {out}"
        );
    }
}
