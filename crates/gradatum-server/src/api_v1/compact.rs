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
//! - **search**: `<ulid> [<score>]` per hit, plus the `corpus_match_count` absence-proof
//!   hint — when `corpus_match_count == 0`, the results are semantic neighbours only, so
//!   the absence of a lexical match is *proven* (reasoning surfaced, not dropped).
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

/// Builds the `corpus_match_count` hint appended to the compact search header.
///
/// Empty string when the count is absent (the request did not opt into it). When present,
/// distinguishes a proven lexical absence (`0`) from matches beyond the K limit.
fn corpus_hint(resp: &VaultSearchResponse) -> String {
    match resp.corpus_match_count {
        None => String::new(),
        Some(0) => {
            " (corpus_match_count=0 -> 0 lexical match: absence proven, semantic neighbours only)"
                .to_string()
        }
        Some(n) => {
            let capped = if resp.corpus_count_capped { "+" } else { "" };
            format!(" (corpus_match_count={n}{capped} lexical matches)")
        }
    }
}

/// Renders a [`VaultSearchResponse`] to its compact text form.
#[must_use]
pub fn render_search(resp: &VaultSearchResponse) -> String {
    let hint = corpus_hint(resp);
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
        assert_eq!(render_search(&resp), "0 relevant notes on this topic");
    }

    #[test]
    fn search_absence_proof_when_corpus_zero() {
        let resp = VaultSearchResponse {
            items: vec![hit("decisions/01ABC", 0.0167)],
            corpus_match_count: Some(0),
            corpus_count_capped: false,
        };
        let out = render_search(&resp);
        assert!(
            out.contains("absence proven"),
            "must surface the absence reasoning: {out}"
        );
        assert!(
            out.contains("01ABC [0.0167]"),
            "ulid+score projected: {out}"
        );
        assert!(!out.contains('/'), "path reduced to ulid: {out}");
    }

    #[test]
    fn search_corpus_capped_marks_plus() {
        let resp = VaultSearchResponse {
            items: vec![hit("s/01A", 0.01)],
            corpus_match_count: Some(10_000),
            corpus_count_capped: true,
        };
        assert!(render_search(&resp).contains("corpus_match_count=10000+ lexical"));
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
