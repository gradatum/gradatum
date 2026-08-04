use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Request body for `create_feature_card` — creates a project-map **feature card** whose
/// `F-XX` number is chosen **by the server**, never by the client.
///
/// The body carries the five other typed roles of a feature card
/// (`[[project:…]]` · `[[status:…]]` · `[[kind:…]]` · `[[release:…]]` · `[[version:…]]`)
/// but **must not** carry a `[[feature:…]]` role: the server allocates the number
/// atomically and injects the `[[feature:F-XX]]` link itself. A body that already contains
/// a `[[feature:…]]` link is rejected — the client cannot pick the number.
///
/// The write is asynchronous (same queue path as `vault_write`): the response returns the
/// allocated number together with a `job_id`, and the card materialises once the worker
/// processes the job. Confirm via `job_status`.
///
/// The **LIVE queue path** serializes the derived `VaultWriteRequest` (not this struct) via
/// `serde_json`; this struct is only the create-operation wire contract.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateFeatureCardRequest {
    /// H1 title of the feature card.
    pub title: String,
    /// Markdown body carrying the five non-feature roles. Must **not** contain a
    /// `[[feature:…]]` link — the server injects it after allocating the number.
    pub body: String,
    /// Author (optional).
    #[serde(default)]
    pub author: Option<String>,
    /// Initial tags (optional).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Target tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Temporal anchor — event date of the card (ISO 8601 UTC or `YYYY-MM-DD`), optional.
    #[serde(default)]
    pub occurred_at: Option<String>,
}

/// Response for `create_feature_card` — the server-assigned number plus the async job handle.
///
/// The card is **not yet written** when this returns: `number` is the allocated feature
/// number, injected into the enqueued write. Poll `job_status` on `job_id` to confirm the
/// card materialised.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFeatureCardResponse {
    // scan-fr-strings: allow-jargon F-135 — valeur d'exemple du format d'identifiant, pas un renvoi à un ticket
    /// Server-assigned identifier, e.g. `"F-135"` (injected into `[[feature:F-135]]`).
    pub feature: String,
    /// Raw allocated number, e.g. `135`.
    pub number: u32,
    /// Async job handle — poll `job_status` to confirm the card was written.
    pub job_id: String,
    /// Pre-allocated ULID of the card being created.
    pub note_id: String,
    /// Relative URL to poll the job state.
    pub poll_url: String,
}
