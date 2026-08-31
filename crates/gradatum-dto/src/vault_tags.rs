use serde::Deserialize;

/// Request body for `vault_tags`.
///
/// Without a bound, `vault_tags` returns the full tag list of the vault — up to
/// ~135 KB in a single response, enough to saturate the context budget of agent
/// callers. This structure makes the response **bounded by default** and lets the
/// caller lift the bound **explicitly** via [`limit`](Self::limit).
///
/// All fields are optional: an absent body (or `{}`) produces the default bounded
/// response — the addition is therefore strictly additive (no existing caller is
/// broken).
///
/// `vault_tags` is an **own-vault** READ (the effective vault is derived from the
/// credential, never from a parameter): it therefore exposes **no** `tenant_id`,
/// unlike the other read requests (a deliberate parity with the former
/// parameter-less contract — see `authors_tags_tenant_scope`).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultTagsRequest {
    /// Maximum number of tags returned, **most frequent first**.
    ///
    /// `None` (default) → server bound (`DEFAULT_TAGS_LIMIT`). Setting an explicit
    /// value lifts the bound — passing a very large value returns the full list. The
    /// `total` field of the response reports how many tags exist in all, which lets a
    /// caller detect truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl VaultTagsRequest {
    /// Builds a default `vault_tags` request (server bound, no explicit selection).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
