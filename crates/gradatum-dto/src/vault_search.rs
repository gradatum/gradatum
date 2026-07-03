use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_search`.
///
/// ## Optional fields (backward-compatible)
///
/// - `locus`: filter by physical path prefix (e.g. `"council/"` restricts to notes
///   whose `locus` starts with `council/`). Applied to both FTS and semantic paths.
///   The prefix is escaped against LIKE injection (metacharacters `%`, `_`, `\`).
/// - `vault_id`: cross-vault read — allows querying a vault other than `tenant_id`.
///   Writes remain unchanged (single-tenant). Validation: non-empty, max 128 chars.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultSearchRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Full-text or semantic search query.
    pub query: String,
    /// Section filter (optional).
    pub section: Option<String>,
    /// Lifecycle status filter (optional).
    ///
    /// Accepted values: the 6 kebab-case `NoteStatus` variants (`draft`, `staging`,
    /// `pending-review`, `live`, `deprecated`, `garbage`) plus the legacy SQL value
    /// `downgraded` (tolerated for backward-compatible filtering). Any other value
    /// → `400 Bad Request`. Applied at the SQL level (FTS + semantic), consistent
    /// with the `section` filter.
    ///
    /// Absent or `null`: no status filter (unchanged behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Maximum number of results (default 10, max 50).
    pub limit: Option<u32>,
    /// If `true`, includes notes with `status = 'downgraded'`
    /// in the results with a penalized BM25 score (×0.1). Default `false`.
    #[serde(default)]
    pub include_downgraded: bool,
    /// locus prefix filter (optional).
    ///
    /// E.g. `"council/"` → restricts to notes whose `locus` starts with
    /// `council/`. LIKE metacharacters (`%`, `_`, `\`) are automatically
    /// escaped — no SQL injection risk.
    ///
    /// Absent or `null`: no locus filter (unchanged behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locus: Option<String>,
    /// vault to query in read mode (optional).
    ///
    /// Lifts the single-tenant constraint **for reads only** — allows querying
    /// a vault other than `tenant_id`. Writes remain unchanged.
    /// Validation: non-empty, max 128 chars.
    ///
    /// Absent or `null`: uses `tenant_id` (unchanged behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<String>,
    /// If `true`, enriches each result with a `scores` object detailing
    /// the composite score breakdown.
    ///
    /// Opt-in (default `false`): only consumers that need it pay the payload overhead.
    /// Existing clients are unaffected — the `scores` field is omitted from the
    /// response when `false`.
    #[serde(default)]
    pub include_scores: bool,
    /// If `true`, includes in the response the total count of notes matching
    /// the **FTS5/BM25 lexical** query within the filtered scope, unbounded by K.
    ///
    /// Allows distinguishing "topic absent from the corpus" (`corpus_match_count == 0`)
    /// from "notes present but ranked below K" (`corpus_match_count > len(results)`).
    ///
    /// Does NOT indicate semantic ANN hit count.
    /// Opt-in (default `false`): zero overhead when absent — no COUNT is executed.
    #[serde(default)]
    pub include_corpus_count: bool,
    /// Earliest temporal anchor (inclusive), Unix epoch milliseconds.
    ///
    /// If provided, restricts results to notes whose `temporal_index.anchor_ms >= from_ms`.
    /// Applied as an AND filter after all other filters (ACL, section, locus, status).
    /// Notes without a `temporal_index` entry are excluded when this bound is set.
    ///
    /// Absent or `null`: no lower bound (unchanged behavior).
    #[serde(default)]
    pub from_ms: Option<i64>,
    /// Latest temporal anchor (inclusive), Unix epoch milliseconds.
    ///
    /// If provided, restricts results to notes whose `temporal_index.anchor_ms <= to_ms`.
    /// Applied as an AND filter after all other filters (ACL, section, locus, status).
    /// Notes without a `temporal_index` entry are excluded when this bound is set.
    ///
    /// Absent or `null`: no upper bound (unchanged behavior).
    ///
    /// Validation: `from_ms` and `to_ms` may be provided independently; if both are
    /// present, `from_ms` must be ≤ `to_ms` (otherwise `400 Bad Request`).
    #[serde(default)]
    pub to_ms: Option<i64>,
}

/// Escapes SQLite LIKE metacharacters (`%`, `_`, `\`) in a value
/// intended for use in a `LIKE ? ESCAPE '\'` clause.
///
/// Ensures that a user-supplied value is treated literally:
/// `%` does not become a wildcard, `_` does not match any single character.
///
/// ## Example
///
/// ```
/// use gradatum_dto::escape_like;
/// assert_eq!(escape_like("foo%bar"), "foo\\%bar");
/// assert_eq!(escape_like("a_b"),     "a\\_b");
/// assert_eq!(escape_like("c\\d"),    "c\\\\d");
/// ```
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '%' => out.push_str(r"\%"),
            '_' => out.push_str(r"\_"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    /// escape_like encode correctement les métacaractères LIKE SQLite.
    ///
    /// Couvre les 3 métacaractères (`%`, `_`, `\`) seuls et combinés.
    /// Complète le doc-test (qui vérifie la signature publique).
    #[test]
    fn escape_like_roundtrip() {
        assert_eq!(escape_like("foo"), "foo", "valeur simple inchangée");
        assert_eq!(escape_like("foo%bar"), r"foo\%bar", "% échappé");
        assert_eq!(escape_like("a_b"), r"a\_b", "_ échappé");
        assert_eq!(escape_like(r"c\d"), r"c\\d", r"\ échappé");
        assert_eq!(
            escape_like(r"pre%fix_path\test"),
            r"pre\%fix\_path\\test",
            "combinaison des 3 métacaractères"
        );
        assert_eq!(
            escape_like("normal/path/section"),
            "normal/path/section",
            "locus normal sans métacaractères"
        );
        assert_eq!(escape_like(""), "", "chaîne vide");
        assert_eq!(escape_like("%%%"), r"\%\%\%", "3 jokers consécutifs");
    }
}
