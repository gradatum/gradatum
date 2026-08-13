use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Request body for `vault_downgrade` — downgrades a note.
///
/// Serialised via `serde_json`. Consumed synchronously by `notes::vault_downgrade`.
///
/// POST `/api/v1/vault_downgrade`. Idempotent: downgrading an already-downgraded
/// note returns 200 with the current status.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VaultDowngradeRequest {
    /// ULID identifier of the note to downgrade.
    pub note_id: String,
    /// Reason for the downgrade (e.g. `"obsolete"`, `"duplicate"`, `"revised"`).
    /// Maximum 500 characters.
    pub reason: String,
    /// Replacement note (ULID, optional).
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// Target tenant (principal) — optional; when omitted the server resolves it
    /// from the credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
}

impl VaultDowngradeRequest {
    /// Constructs a downgrade request with the mandatory `note_id` and `reason`;
    /// `replaced_by` and `tenant_id` default to `None`.
    #[must_use]
    pub fn new(note_id: String, reason: String) -> Self {
        Self {
            note_id,
            reason,
            replaced_by: None,
            tenant_id: None,
        }
    }
}

/// Response for `vault_downgrade`.
///
/// Returned by POST `/api/v1/vault_downgrade` after a successful operation (200).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultDowngradeResponse {
    /// ULID of the modified note.
    pub note_id: String,
    /// Status after the operation: `"downgraded"`.
    pub status: String,
    /// Unix epoch milliseconds of the status change.
    pub status_changed: i64,
    /// Recorded reason.
    pub reason: String,
}

/// Body for PATCH `/api/v1/notes/{id}`.
///
/// Partial body: all fields are optional in serialization.
/// The handler requires at least one field to be present (application-level validation).
///
/// Supports: downgrade, revert downgrade → live, reclassify staging → live,
/// additive tag addition (`add_tags`), etc.
///
/// ## Backward compatibility
///
/// No `deny_unknown_fields`: the DTO tolerates schema evolution (adding fields without
/// breaking existing clients). Absent fields → `None` via `#[serde(default)]`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoteStatusPatch {
    /// New status: `"live"` | `"staging"` | `"downgraded"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Reason for the status change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// Replacement note (ULID, only relevant when `status = "downgraded"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<String>,
    /// Tags to ADD to the note (additive only — never replaces or removes existing tags).
    ///
    /// Semantics: case-insensitive UNION with existing tags. Idempotent
    /// (re-adding an already-present tag is a no-op). No `remove_tags`.
    ///
    /// Handler validation: each tag non-empty, lowercase-with-dash format
    /// (`[a-z0-9][a-z0-9-]*`, max 64 chars), max 20 tags per call → 400 otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_tags: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rétrocompat : un body sans `add_tags` désérialise avec `None` (clients legacy).
    #[test]
    fn note_status_patch_without_add_tags_is_none() {
        let json = r#"{"status":"live"}"#;
        let p: NoteStatusPatch = serde_json::from_str(json).expect("désérialisation legacy");
        assert_eq!(p.status.as_deref(), Some("live"));
        assert!(p.add_tags.is_none(), "add_tags absent → None (rétrocompat)");
    }

    /// `add_tags` est désérialisé quand présent.
    #[test]
    fn note_status_patch_with_add_tags() {
        let json = r#"{"add_tags":["deploy","release"]}"#;
        let p: NoteStatusPatch = serde_json::from_str(json).expect("désérialisation add_tags");
        assert_eq!(
            p.add_tags.as_deref(),
            Some(&["deploy".to_string(), "release".to_string()][..])
        );
        assert!(p.status.is_none());
    }

    /// PATCH combiné status + add_tags désérialise les deux champs.
    #[test]
    fn note_status_patch_combined_status_and_add_tags() {
        let json = r#"{"status":"live","add_tags":["migration"]}"#;
        let p: NoteStatusPatch = serde_json::from_str(json).expect("désérialisation combinée");
        assert_eq!(p.status.as_deref(), Some("live"));
        assert_eq!(p.add_tags.as_deref(), Some(&["migration".to_string()][..]));
    }

    /// `add_tags: None` n'est pas sérialisé (skip_serializing_if).
    #[test]
    fn note_status_patch_omits_none_add_tags() {
        let p = NoteStatusPatch {
            status: Some("live".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&p).expect("sérialisation");
        assert!(
            !json.contains("add_tags"),
            "add_tags None ne doit pas apparaître : {json}"
        );
    }

    /// Champ inconnu toléré (pas de deny_unknown_fields → rétrocompat évolutive).
    #[test]
    fn note_status_patch_tolerates_unknown_field() {
        let json = r#"{"add_tags":["x"],"future_field":42}"#;
        let p: NoteStatusPatch =
            serde_json::from_str(json).expect("champ inconnu toléré (pas de deny_unknown_fields)");
        assert_eq!(p.add_tags.as_deref(), Some(&["x".to_string()][..]));
    }

    /// Schéma schemars : le champ `add_tags` est présent dans le schéma JSON exposé au MCP.
    #[cfg(feature = "schemars")]
    #[test]
    fn note_status_patch_schema_exposes_add_tags() {
        let schema = serde_json::to_value(schemars::schema_for!(NoteStatusPatch))
            .expect("schema_for produit du JSON valide");
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("schéma a un objet properties");
        assert!(
            props.contains_key("add_tags"),
            "le schéma MCP doit exposer add_tags : {props:?}"
        );
    }
}
