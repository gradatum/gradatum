//! SSOT des helpers de schéma JSON pour les outils MCP gradatum.
//!
//! Ce module est activé uniquement avec la feature `schemars`. Il remplace les
//! 4 copies dupliquées dans `gradatum-server/src/api_v1/mcp.rs` et
//! `gradatum-mcp-stub/src/main.rs`.
//!
//! # Rationale — régression 34e70eb
//!
//! Lors du cutover MCP (commit `34e70eb`), `tool_def_no_params` émettait `{}` (Map vide)
//! au lieu de `{"type":"object","properties":{}}`. Le validateur zod de Claude Code rejette
//! un Map vide → la liste entière des 21 outils était rejetée en cascade, rendant le serveur
//! MCP inutilisable. Ce module est la SSOT anti-34e70eb : toute correction future n'a
//! besoin d'être faite qu'ici.

use serde_json::{Map, Value};

/// Schéma MCP d'un outil sans paramètres.
///
/// Retourne `{"type":"object","properties":{}}`, conforme à la spec MCP.
///
/// Un Map vide `{}` est rejeté par les validateurs clients (ex : zod de Claude Code)
/// qui exigent `type: "object"` — ce qui invalide toute la liste d'outils en cascade.
/// Régression historique `34e70eb` (serveur émettait `{}` → 21 outils rejetés).
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

/// Schéma MCP dérivé du type `T` via schemars.
///
/// Retourne un `serde_json::Map` représentant le schéma JSON de `T` tel qu'attendu
/// par les consommateurs MCP (wire contract HTTP).
///
/// # Fail-loud
///
/// Panique si schemars produit un schéma racine non-objet. En pratique, `schema_for!(T)`
/// produit **toujours** un objet JSON → cette panique est impossible. Le fail-loud est
/// intentionnel (anti-34e70eb) : on préfère un crash à la compilation/test plutôt qu'un
/// Map vide silencieux qui invalide la liste d'outils en prod.
///
/// **Ne jamais substituer par `unwrap_or_default()`** : un Map vide est silencieusement
/// erroné (zod rejet en cascade) — c'est exactement ce que `34e70eb` a introduit.
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
    let value = serde_json::to_value(&schema).expect(
        "schemars::schema_for!(T) produit toujours un Value JSON valide — échec impossible",
    );
    match value {
        Value::Object(m) => m,
        other => panic!(
            "schemars::schema_for!(T) doit retourner un objet JSON mais a retourné : {other:?} \
             — cela indique un bug interne dans schemars ou un type T non standard"
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
