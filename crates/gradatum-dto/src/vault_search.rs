use serde::Deserialize;

use gradatum_core::scope::{TenantId, VaultId};

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
#[non_exhaustive]
pub struct VaultSearchRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Full-text or semantic search query.
    ///
    /// ## FTS5 operators are NOT interpreted (known, accepted degradation)
    ///
    /// The lexical arm quotes each token individually before handing it to FTS5.
    /// As a result the FTS5 operators `OR`, `NOT` and `NEAR` are **not** parsed as
    /// operators — they are searched as the literal words `or`, `not`, `near`.
    /// Writing `a OR b` therefore matches an implicit AND of the three terms
    /// `a`, `or`, `b`, not a boolean disjunction. No FTS5 query parser is in scope;
    /// this behaviour is deliberate and stable, not a bug.
    ///
    /// ## Punctuation-only queries cannot match lexically
    ///
    /// A query made solely of punctuation, symbols or whitespace (e.g. `-`, `...`,
    /// `- -`) has every token normalise to empty under the `unicode61` tokenizer.
    /// The resulting FTS5 query is syntactically valid but can match no document,
    /// whatever the corpus. For such a query, `corpus_match_count == 0` is reported
    /// as *not applicable to the query form* — never as a proven absence.
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
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub vault_id: Option<VaultId>,
    /// Opt-in multi-vault: read the union of THESE vaults in addition to the
    /// principal's own vault, with global RRF re-scoring; each hit carries its
    /// `vault_id` provenance. `None` (default) → own vault only, response
    /// **byte-identical** to the historical shape.
    ///
    /// A requested vault that is not authorized (no covering read grant, target ACL
    /// deny, or inactive target tenant) is **silently excluded**: never a `403`, no
    /// observable distinction between "vault does not exist" and "vault not authorized"
    /// (no existence oracle).
    ///
    /// Honored only when `multi_tenant.enabled = true`. Mutually exclusive with the
    /// singular `vault_id` field (supplying both → `400 Bad Request`). The resolved set
    /// is capped at `MAX_VAULTS_PER_SEARCH` (overflow → `400`, never silent
    /// truncation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_ids: Option<Vec<String>>,
    /// Opt-in multi-vault: read ALL vaults readable by the principal, with global RRF
    /// re-scoring. Default `false` → own vault only.
    ///
    /// Same silent-exclusion semantics and `MAX_VAULTS_PER_SEARCH` cap as `vault_ids`
    /// (a set beyond the cap → `400`, the caller must enumerate). Honored only when
    /// `multi_tenant.enabled = true`. Mutually exclusive with the singular `vault_id`
    /// field.
    #[serde(default)]
    pub all_readable: bool,
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
    /// The count distinguishes three states (surfaced verbatim in the compact hint):
    ///
    /// 1. `corpus_match_count > 0` — lexical matches exist (some may rank below K).
    /// 2. `corpus_match_count == 0` on a query whose form **could** match — the
    ///    *lexical* absence of the term in the filtered surface is proven. This is
    ///    scoped to the lexical arm only and does **not** disprove the semantic
    ///    relevance of the results that were returned.
    /// 3. `corpus_match_count == 0` on a query whose form **cannot** match (a
    ///    punctuation-only query, see the `query` field) — the count is *not
    ///    applicable* to the query form; the zero is an artefact of the query, not a
    ///    corpus property, and is never a proven absence.
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
    /// If `true`, the server returns a **compact** rendering instead of the full
    /// `VaultSearchResponse`: an object `{ "compact": "<text>" }` where the text lists
    /// each hit as `<ulid> [<score>]` plus the `corpus_match_count` absence-proof hint.
    ///
    /// Optimises token cost for LLM consumers (the compaction is a product feature,
    /// not transport). Preserves the lexical-absence reasoning with the three-state
    /// distinction of the `include_corpus_count` field: a proven *lexical* absence
    /// (which does not disprove the returned results) is rendered differently from a
    /// count that is *not applicable* to a punctuation-only query.
    ///
    /// Opt-in (default `false`): when absent, the response is **byte-for-byte identical**
    /// to the historical `VaultSearchResponse`. Existing clients are unaffected.
    #[serde(default)]
    pub compact: bool,
}

impl VaultSearchRequest {
    /// Constructs a search request with the mandatory `query`; all filters and
    /// flags default to their unset (`None` / `false`) values.
    #[must_use]
    pub fn new(query: String) -> Self {
        Self {
            tenant_id: None,
            query,
            section: None,
            status: None,
            limit: None,
            include_downgraded: false,
            locus: None,
            vault_id: None,
            vault_ids: None,
            all_readable: false,
            include_scores: false,
            include_corpus_count: false,
            from_ms: None,
            to_ms: None,
            compact: false,
        }
    }
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
    use super::{VaultSearchRequest, escape_like};

    /// F-63 : un payload SANS les champs multi-vault → défauts (`None` / `false`),
    /// rétro-compatibilité stricte (`deny_unknown_fields` respecté).
    #[test]
    fn multivault_fields_default_when_absent() {
        let req: VaultSearchRequest =
            serde_json::from_value(serde_json::json!({ "query": "foo" })).expect("payload minimal");
        assert_eq!(req.vault_ids, None, "vault_ids absent → None");
        assert!(!req.all_readable, "all_readable absent → false");
    }

    /// F-63 : un payload AVEC les champs multi-vault → parsés tels quels.
    #[test]
    fn multivault_fields_parsed_when_present() {
        let req: VaultSearchRequest = serde_json::from_value(serde_json::json!({
            "query": "foo",
            "vault_ids": ["a", "b"],
            "all_readable": true,
        }))
        .expect("payload multi-vault");
        assert_eq!(
            req.vault_ids.as_deref(),
            Some(["a".to_owned(), "b".to_owned()].as_slice()),
            "vault_ids parsés"
        );
        assert!(req.all_readable, "all_readable parsé");
    }

    /// F-63 : un champ inconnu reste refusé (`deny_unknown_fields` préservé).
    #[test]
    fn unknown_field_still_rejected() {
        let res: Result<VaultSearchRequest, _> = serde_json::from_value(serde_json::json!({
            "query": "foo",
            "vault_idz": ["typo"],
        }));
        assert!(res.is_err(), "champ inconnu → erreur de désérialisation");
    }

    /// F-63 : `vault_id` (singulier) + `vault_ids` coexistent au parse — le conflit
    /// est détectable (les deux champs sont peuplés) ; la validation 400 est en T4.
    #[test]
    fn singular_and_plural_vault_id_both_parse() {
        let req: VaultSearchRequest = serde_json::from_value(serde_json::json!({
            "query": "foo",
            "vault_id": "main",
            "vault_ids": ["a"],
        }))
        .expect("payload conflit vault_id + vault_ids");
        assert!(req.vault_id.is_some(), "vault_id singulier présent");
        assert!(
            req.vault_ids.is_some(),
            "vault_ids présent → conflit détectable"
        );
    }

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
