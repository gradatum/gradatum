//! Single source of truth for the JSON Schema helpers used by the gradatum MCP tools.
//!
//! Enabled only with the `schemars` feature. It replaces what used to be four duplicated
//! copies of these two helpers across the server and the MCP stub.
//!
//! # Why this module exists
//!
//! A regression once made the no-parameter tool definition emit an empty map `{}` instead
//! of `{"type":"object","properties":{}}`. Strict MCP clients reject an empty map, and the
//! rejection cascades: the **entire** tool list is discarded and the MCP server becomes
//! unusable. Centralising the two helpers here means such a fix only ever has to be made
//! in one place.

use serde_json::{Map, Value};

/// MCP schema for a tool that takes no parameters.
///
/// Returns `{"type":"object","properties":{}}`, as required by the MCP specification.
///
/// An empty map `{}` is rejected by strict client-side validators, which require
/// `type: "object"` — and that rejection invalidates the whole tool list in cascade.
///
/// # Examples
///
/// ```rust
/// # #[cfg(feature = "schemars")]
/// # {
/// use gradatum_dto::mcp_empty_params_schema;
/// let schema = mcp_empty_params_schema();
/// assert_eq!(schema["type"], "object");
/// assert_eq!(schema["properties"], serde_json::json!({}));
/// # }
/// ```
pub fn mcp_empty_params_schema() -> Map<String, Value> {
    let mut m = Map::with_capacity(2);
    m.insert("type".to_owned(), Value::String("object".to_owned()));
    m.insert("properties".to_owned(), Value::Object(Map::new()));
    m
}

/// MCP schema derived from type `T` via schemars.
///
/// Returns a `serde_json::Map` holding the JSON schema of `T`, in the shape MCP consumers
/// expect for the HTTP wire contract.
///
/// # Panics
///
/// Panics if schemars produces a non-object root schema. In practice `schema_for!(T)`
/// **always** produces a JSON object, so this panic is unreachable. Failing loudly is
/// deliberate: a crash at build or test time is far preferable to silently emitting an
/// empty map, which invalidates the whole tool list in production.
///
/// **Never substitute `unwrap_or_default()` here**: an empty map is wrong in a way that
/// no caller can detect, and it takes the entire tool list down with it.
///
/// # Examples
///
/// ```rust
/// # #[cfg(feature = "schemars")]
/// # {
/// use gradatum_dto::{mcp_tool_schema, VaultSearchRequest};
/// let schema = mcp_tool_schema::<VaultSearchRequest>();
/// assert_eq!(schema["type"], "object");
/// assert!(schema.contains_key("properties"));
/// # }
/// ```
pub fn mcp_tool_schema<T: schemars::JsonSchema>() -> Map<String, Value> {
    let schema = schemars::schema_for!(T);
    // SAFETY (sémantique) : schema_for!(T) produit toujours un objet JSON valide.
    // La sérialisation d'un RootSchema schemars ne peut pas échouer (types internes
    // sont tous sérialisables) et le résultat est toujours Value::Object.
    // Fail-loud intentionnel — jamais de dégradé silencieux (anti-34e70eb).
    let value = serde_json::to_value(&schema)
        .expect("schemars::schema_for!(T) always produces a valid JSON Value — failure impossible");
    match value {
        Value::Object(m) => m,
        other => panic!(
            "schemars::schema_for!(T) must return a JSON object but returned: {other:?} \
             — this indicates an internal bug in schemars or a non-standard type T"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mcp_empty_params_schema` sérialise exactement en `{"type":"object","properties":{}}`.
    ///
    /// Ce test est la garde anti-34e70eb pour les outils sans paramètres :
    /// un Map vide `{}` ou l'absence de `type:"object"` invalide la liste d'outils entière.
    #[test]
    fn empty_params_schema_exact_serialization() {
        let schema = mcp_empty_params_schema();
        let serialized = serde_json::to_value(&schema)
            .expect("sérialisation du schéma vide ne peut pas échouer");

        let expected = serde_json::json!({
            "type": "object",
            "properties": {}
        });

        assert_eq!(
            serialized, expected,
            "mcp_empty_params_schema() doit sérialiser exactement en \
             {{\"type\":\"object\",\"properties\":{{}}}} — anti-34e70eb"
        );
    }

    /// `mcp_tool_schema::<VaultWriteRequest>` a `"type":"object"` ET `"properties"` non-vide.
    ///
    /// Vérifie que la SSOT produit un schéma complet pour un type DTO réel.
    #[test]
    fn tool_schema_has_type_object_and_nonempty_properties() {
        use crate::VaultWriteRequest;

        let schema = mcp_tool_schema::<VaultWriteRequest>();
        let value = serde_json::Value::Object(schema);

        assert_eq!(
            value["type"], "object",
            "mcp_tool_schema doit avoir \"type\":\"object\""
        );

        let properties = value["properties"]
            .as_object()
            .expect("mcp_tool_schema doit avoir une clé \"properties\" de type objet");

        assert!(
            !properties.is_empty(),
            "mcp_tool_schema pour VaultWriteRequest doit avoir des propriétés non-vides \
             (title, body, author, tags, section_hint, tenant_id, note_id, sha256)"
        );

        // Vérifie la présence des champs obligatoires du DTO.
        assert!(
            properties.contains_key("title"),
            "propriété 'title' attendue dans le schéma VaultWriteRequest"
        );
        assert!(
            properties.contains_key("body"),
            "propriété 'body' attendue dans le schéma VaultWriteRequest"
        );
    }
}
