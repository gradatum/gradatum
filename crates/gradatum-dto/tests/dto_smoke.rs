//! Phase 2.1.2 alpha.9 — Tests DTO vault_downgrade.
//!
//! Couvre : serde round-trip VaultDowngradeRequest/Response + NoteStatusPatch,
//! et (sous feature `schemars`) validation JSON Schema required fields.

use gradatum_dto::{NoteStatusPatch, VaultDowngradeRequest, VaultDowngradeResponse};

#[test]
fn vault_downgrade_request_minimal_serde() {
    let json = r#"{"note_id":"01KR2XXXXXXXXXXXXXXXXXXX","reason":"superseded by 01KR3..."}"#;
    let req: VaultDowngradeRequest = serde_json::from_str(json).expect("parse");
    assert_eq!(req.note_id, "01KR2XXXXXXXXXXXXXXXXXXX");
    assert_eq!(req.reason, "superseded by 01KR3...");
    assert!(req.replaced_by.is_none());
}

#[test]
fn vault_downgrade_request_with_replaced_by() {
    let json = r#"{"note_id":"01KR2","reason":"r","replaced_by":"01KR3"}"#;
    let req: VaultDowngradeRequest = serde_json::from_str(json).expect("parse");
    assert_eq!(req.replaced_by.as_deref(), Some("01KR3"));
}

#[test]
fn vault_downgrade_response_serialize() {
    let resp = VaultDowngradeResponse {
        note_id: "01KR2".to_string(),
        status: "downgraded".to_string(),
        status_changed: 1715000000000,
        reason: "test".to_string(),
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"status\":\"downgraded\""));
    assert!(json.contains("\"status_changed\":1715000000000"));
}

#[test]
fn note_status_patch_partial_serde() {
    let json = r#"{"status":"live"}"#;
    let patch: NoteStatusPatch = serde_json::from_str(json).expect("parse");
    assert_eq!(patch.status.as_deref(), Some("live"));
    assert!(patch.status_reason.is_none());
    assert!(patch.replaced_by.is_none());
}

#[test]
fn note_status_patch_full_serde() {
    let json = r#"{"status":"downgraded","status_reason":"manual review","replaced_by":"01KR3"}"#;
    let patch: NoteStatusPatch = serde_json::from_str(json).expect("parse");
    assert_eq!(patch.status.as_deref(), Some("downgraded"));
    assert_eq!(patch.status_reason.as_deref(), Some("manual review"));
    assert_eq!(patch.replaced_by.as_deref(), Some("01KR3"));
}

#[test]
fn vault_search_request_include_downgraded_default_false() {
    let json = r#"{"query":"test"}"#;
    let req: gradatum_dto::VaultSearchRequest = serde_json::from_str(json).expect("parse");
    assert!(
        !req.include_downgraded,
        "include_downgraded default = false"
    );
}

#[test]
fn vault_search_request_include_downgraded_explicit_true() {
    let json = r#"{"query":"test","include_downgraded":true}"#;
    let req: gradatum_dto::VaultSearchRequest = serde_json::from_str(json).expect("parse");
    assert!(req.include_downgraded);
}

#[cfg(feature = "schemars")]
#[test]
fn vault_downgrade_request_schema_required_fields() {
    let schema = schemars::schema_for!(VaultDowngradeRequest);
    let json = serde_json::to_value(&schema).unwrap();
    let required = json
        .pointer("/required")
        .and_then(|v| v.as_array())
        .expect("required array");
    let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(names.contains(&"note_id"), "note_id required");
    assert!(names.contains(&"reason"), "reason required");
    assert!(
        !names.contains(&"replaced_by"),
        "replaced_by must be optional"
    );
}
