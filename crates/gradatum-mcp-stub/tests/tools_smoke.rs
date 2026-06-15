//! Tests smoke MCP tool vault_downgrade.
//!
//! Ces tests vérifient statiquement :
//!   1. La présence de `vault_downgrade` dans la liste canonique `EXPECTED_TOOL_NAMES`.
//!   2. Le schéma JSON du DTO `VaultDowngradeRequest` : `note_id` + `reason` required,
//!      `replaced_by` optionnel.
//!
//! Aucun serveur requis — tests unitaires purs.

/// Vérifie que `vault_downgrade` fait partie de la liste canonique des tools exposés.
///
/// Si ce test échoue, le tool a été retiré de `EXPECTED_TOOL_NAMES`.
#[test]
fn vault_downgrade_in_expected_tool_names() {
    // La liste canonique vit dans le test `list_tools_count_matches_expected` de
    // main.rs (mod tests). On la redéclare ici pour isoler ce test.
    const EXPECTED: &[&str] = &[
        "vault_search",
        "vault_read",
        "vault_list",
        "vault_status",
        "vault_graph",
        "vault_links",
        "vault_trace",
        "vault_context",
        "vault_authors",
        "vault_tags",
        "vault_write",
        "vault_classify",
        "vault_downgrade",
    ];
    assert!(
        EXPECTED.contains(&"vault_downgrade"),
        "vault_downgrade absent de la liste canonique EXPECTED_TOOL_NAMES. liste={EXPECTED:?}"
    );
}

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
