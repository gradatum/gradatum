use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_list` — legacy vault v1.6.2 `VaultListArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultListRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Section to list (optional — if absent, lists all sections).
    pub section: Option<String>,
    /// Maximum number of entries. Default 20, clamped to the range 1..=200 — a larger
    /// value is silently lowered to 200, not rejected.
    pub limit: Option<u32>,
    /// Pagination cursor (opaque token).
    pub cursor: Option<String>,
    /// Filter on the `kind` role of a project-map card (`FIX`, `FEATURE`,
    /// `ENHANCEMENT`, `TASK`). Absent or `null`: no filter — the routing to
    /// `list_notes` is unchanged. The value is the canonical SCREAMING_SNAKE wire form.
    pub role_kind: Option<String>,
    /// Filter on the `status` role of a project-map card (`OPEN`, `IN_PROGRESS`, `BLOCKED`,
    /// `DONE`, `OBSOLETE`, `BRAINSTORMING`). Not to be confused with the note's lifecycle
    /// status (`live`, `downgraded`): two distinct notions, two distinct columns.
    pub role_status: Option<String>,
    /// Filter on an **exact tag**: a note is listed when it carries this name as a whole
    /// token of its tags, never as a substring (`personal-classified-2026` is a different
    /// tag). Absent or `null`: no filter — the routing to `list_notes` is unchanged.
    ///
    /// The two reserved names (`personal-classified`, `privacy-unscanned`) are accepted
    /// **here** and refused everywhere a tag is written: a caller may ask which notes carry
    /// a marker without being able to pose one. The answer still passes the read guard, so
    /// asking is not a way to see what the caller may not read.
    ///
    /// A value outside the tag form (uppercase, `%`, `_`, spaces, a leading `-`) is rejected
    /// with `400` rather than answered with an empty page.
    #[serde(default)]
    pub tag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removed `pattern` field is rejected by `deny_unknown_fields`. The error is a
    /// serde *data* error (the JSON is syntactically valid, the field is unknown), which
    /// axum maps to HTTP 422 — never 400 — and whose message names the field.
    #[test]
    fn rejects_removed_pattern_field() {
        let err = serde_json::from_str::<VaultListRequest>(r#"{"pattern":"x"}"#)
            .expect_err("the removed `pattern` field must be rejected (deny_unknown_fields)");
        assert_eq!(
            err.classify(),
            serde_json::error::Category::Data,
            "unknown field is a data error, not a syntax error"
        );
        assert!(
            err.to_string().contains("pattern"),
            "the rejection message names the unknown field: {err}"
        );
    }

    /// The tag filter is additive: a body that does not carry it keeps deserialising, and
    /// the field defaults to `None` rather than to a filter that would match nothing.
    #[test]
    fn tag_filter_defaults_to_none_and_is_read_when_present() {
        let absent = serde_json::from_str::<VaultListRequest>(r#"{"limit":5}"#)
            .expect("a body without `tag` still deserialises");
        assert_eq!(absent.tag, None, "absent means 'no filter'");

        let null = serde_json::from_str::<VaultListRequest>(r#"{"tag":null}"#)
            .expect("an explicit null deserialises");
        assert_eq!(null.tag, None, "null means 'no filter' too");

        let valued = serde_json::from_str::<VaultListRequest>(r#"{"tag":"privacy-unscanned"}"#)
            .expect("a reserved name is a legitimate query value");
        assert_eq!(valued.tag.as_deref(), Some("privacy-unscanned"));
    }
}
