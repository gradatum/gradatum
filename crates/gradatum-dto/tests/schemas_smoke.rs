//! Smoke tests : `schemars::schema_for!` génère un JSON Schema non vide
//! pour chacune des 10 structs Request avec feature `schemars` activée.

#![cfg(feature = "schemars")]

use gradatum_dto::*;
use schemars::schema_for;

fn assert_non_empty_schema<T: schemars::JsonSchema>(name: &str) {
    let schema = schema_for!(T);
    let json = serde_json::to_value(&schema).expect("schema serializable");
    let obj = json.as_object().expect("schema root is object");

    // JSON Schema canonique : root has "$schema", "title", "type", "properties"
    assert_eq!(
        obj.get("type").and_then(|v| v.as_str()),
        Some("object"),
        "{}: schema.type expected 'object'",
        name
    );
    let properties = obj
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("{}: schema.properties missing", name));
    assert!(
        !properties.is_empty(),
        "{}: schema.properties is empty",
        name
    );
}

#[test]
fn vault_search_schema_non_empty() {
    assert_non_empty_schema::<VaultSearchRequest>("VaultSearchRequest");
}

#[test]
fn vault_read_schema_non_empty() {
    assert_non_empty_schema::<VaultReadRequest>("VaultReadRequest");
}

#[test]
fn vault_list_schema_non_empty() {
    assert_non_empty_schema::<VaultListRequest>("VaultListRequest");
}

#[test]
fn vault_graph_schema_non_empty() {
    assert_non_empty_schema::<VaultGraphRequest>("VaultGraphRequest");
}

#[test]
fn vault_links_schema_non_empty() {
    assert_non_empty_schema::<VaultLinksRequest>("VaultLinksRequest");
}

#[test]
fn vault_trace_schema_non_empty() {
    assert_non_empty_schema::<VaultTraceRequest>("VaultTraceRequest");
}

#[test]
fn vault_context_schema_non_empty() {
    assert_non_empty_schema::<VaultContextRequest>("VaultContextRequest");
}

#[test]
fn vault_write_schema_non_empty() {
    assert_non_empty_schema::<VaultWriteRequest>("VaultWriteRequest");
}

#[test]
fn vault_classify_schema_non_empty() {
    assert_non_empty_schema::<VaultClassifyRequest>("VaultClassifyRequest");
}

#[test]
fn vault_downgrade_schema_non_empty() {
    assert_non_empty_schema::<VaultDowngradeRequest>("VaultDowngradeRequest");
}

#[test]
fn vault_search_required_fields() {
    let schema = serde_json::to_value(schema_for!(VaultSearchRequest)).unwrap();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .expect("VaultSearchRequest.required missing");
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    // `query` is required (no default), `tenant_id` has default and may or may not be in required
    assert!(
        required_strs.contains(&"query"),
        "query expected in required"
    );
}
