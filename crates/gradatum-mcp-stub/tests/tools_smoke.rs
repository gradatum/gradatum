//! Test smoke du schéma MCP de `vault_downgrade`.
//!
//! Vérifie statiquement le schéma JSON du DTO `VaultDowngradeRequest` : `note_id` +
//! `reason` required, `replaced_by` optionnel.
//!
//! Aucun serveur requis — test unitaire pur.
//!
//! ## Ce que ce fichier ne teste plus, et pourquoi
//!
//! Il portait un `vault_downgrade_in_expected_tool_names` qui déclarait une liste locale
//! de 13 noms puis assertait que cette liste contenait `vault_downgrade` : une constante
//! confrontée à elle-même, verte par construction, sans jamais toucher le catalogue servi.
//! `gradatum-mcp-stub` étant un crate binaire, un test d'intégration ne peut pas atteindre
//! `tool_catalogue()` — la propriété visée n'est donc pas réparable ici. Elle est désormais
//! réellement vérifiée par `catalogue_expose_exactement_les_outils_canoniques`, dans le
//! `mod tests` de `src/main.rs`.

/// Vérifie le schéma JSON de `VaultDowngradeRequest` :
///   - `note_id` → required
///   - `reason`  → required
///   - `replaced_by` → optionnel (absent de `required`)
///
/// Parité avec la spec MCP legacy vault `vault_downgrade`.
#[test]
fn vault_downgrade_input_schema_required_fields() {
    use gradatum_dto::VaultDowngradeRequest;
    use schemars::schema_for;

    let schema = schema_for!(VaultDowngradeRequest);
    let json = serde_json::to_value(&schema).expect("schema VaultDowngradeRequest → json");

    let required = json
        .pointer("/required")
        .and_then(|v| v.as_array())
        .expect("champ `required` absent du schéma JSON — note_id et reason doivent être required");

    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

    assert!(
        required_names.contains(&"note_id"),
        "note_id doit être required dans VaultDowngradeRequest. required={required_names:?}"
    );
    assert!(
        required_names.contains(&"reason"),
        "reason doit être required dans VaultDowngradeRequest. required={required_names:?}"
    );
    assert!(
        !required_names.contains(&"replaced_by"),
        "replaced_by doit être optionnel (Option<String>) — ne doit pas figurer dans required. required={required_names:?}"
    );
}
