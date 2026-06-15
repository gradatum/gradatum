//! Injects the `slot_id` field into the upstream JSON body.
//!
//! llama.cpp KV-cache slot pinning (native `slot_id` parameter) allows pinning
//! a slot per session and reusing the KV cache across turns without recompute.
//! This module provides the extraction and injection logic.
//!
//! Contract:
//! - `extract_slot_id()`: extracts and parses the `X-Slot-Id` header from HTTP headers
//! - `inject_slot_id_if_needed()`: injects `slot_id` into a `serde_json::Value` when applicable
//!
//! Design: pure functions with no side effects — easy to unit-test.

use axum::http::HeaderMap;

/// Extracts and parses the `X-Slot-Id` header value as a `u32`.
///
/// Returns `None` when:
/// - the header is absent
/// - the value is not a valid non-negative integer (`u32`)
/// - the value contains non-ASCII characters
///
/// The parsed value is a `u32` — llama.cpp slot identifier (typically 0–127).
/// An unparseable value is silently ignored (non-breaking).
pub fn extract_slot_id(headers: &HeaderMap) -> Option<u32> {
    headers
        .get("x-slot-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Injects `slot_id` into the upstream JSON body when conditions are met.
///
/// Injection conditions:
/// 1. `enable_slot_passthrough` == `true` (config kill-switch)
/// 2. `slot_id` == `Some(n)` (header present and parseable)
///
/// Effects:
/// - When injecting: inserts `"slot_id": n` into the root JSON object.
///   If the field already exists, it is overwritten (the header takes precedence).
/// - When not injecting: the body is returned unchanged.
///
/// Panics: never — `body` is always a `Value::Object` (deserialized from a Rust struct).
pub fn inject_slot_id_if_needed(
    mut body: serde_json::Value,
    slot_id: Option<u32>,
    enable_slot_passthrough: bool,
) -> serde_json::Value {
    if enable_slot_passthrough {
        if let Some(id) = slot_id {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("slot_id".to_string(), serde_json::Value::Number(id.into()));
            }
            tracing::debug!(
                slot_id = id,
                "X-Slot-Id passthrough injecté dans body upstream"
            );
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn extract_slot_id_header_present_valid() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static("42"));
        assert_eq!(extract_slot_id(&headers), Some(42));
    }

    #[test]
    fn extract_slot_id_zero_is_valid() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static("0"));
        assert_eq!(extract_slot_id(&headers), Some(0));
    }

    #[test]
    fn extract_slot_id_header_absent_returns_none() {
        let headers = HeaderMap::new();
        assert_eq!(extract_slot_id(&headers), None);
    }

    #[test]
    fn extract_slot_id_invalid_string_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static("not-a-number"));
        assert_eq!(extract_slot_id(&headers), None);
    }

    #[test]
    fn extract_slot_id_negative_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static("-1"));
        assert_eq!(extract_slot_id(&headers), None);
    }

    #[test]
    fn extract_slot_id_float_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static("3.14"));
        assert_eq!(extract_slot_id(&headers), None);
    }

    #[test]
    fn extract_slot_id_with_whitespace_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slot-id", HeaderValue::from_static(" 7 "));
        assert_eq!(extract_slot_id(&headers), Some(7));
    }

    #[test]
    fn inject_slot_id_inserts_field_when_enabled() {
        let body = serde_json::json!({
            "model": "qwen3",
            "messages": []
        });
        let result = inject_slot_id_if_needed(body, Some(42), true);
        assert_eq!(result["slot_id"], serde_json::json!(42));
        assert_eq!(result["model"], "qwen3");
    }

    #[test]
    fn inject_slot_id_no_field_when_no_slot_id() {
        let body = serde_json::json!({
            "model": "qwen3",
            "messages": []
        });
        let result = inject_slot_id_if_needed(body, None, true);
        assert!(
            !result.as_object().unwrap().contains_key("slot_id"),
            "slot_id ne doit PAS être présent si header absent"
        );
    }

    #[test]
    fn inject_slot_id_no_field_when_disabled_via_config() {
        let body = serde_json::json!({
            "model": "qwen3",
            "messages": []
        });
        let result = inject_slot_id_if_needed(body, Some(5), false);
        assert!(
            !result.as_object().unwrap().contains_key("slot_id"),
            "slot_id ne doit PAS être injecté quand enable_slot_passthrough=false"
        );
    }

    #[test]
    fn inject_slot_id_overwrites_existing_field() {
        let body = serde_json::json!({
            "model": "qwen3",
            "slot_id": 99
        });
        let result = inject_slot_id_if_needed(body, Some(42), true);
        assert_eq!(
            result["slot_id"],
            serde_json::json!(42),
            "le header X-Slot-Id doit écraser un slot_id déjà présent"
        );
    }

    #[test]
    fn inject_slot_id_slot_zero_is_valid() {
        let body = serde_json::json!({"model": "m", "messages": []});
        let result = inject_slot_id_if_needed(body, Some(0), true);
        assert_eq!(result["slot_id"], serde_json::json!(0));
    }

    #[test]
    fn inject_slot_id_preserves_other_fields() {
        let body = serde_json::json!({
            "model": "qwen3",
            "messages": [{"role": "user", "content": "bonjour"}],
            "temperature": 0.7,
            "stream": false
        });
        let result = inject_slot_id_if_needed(body, Some(3), true);
        assert_eq!(result["slot_id"], serde_json::json!(3));
        assert_eq!(result["model"], "qwen3");
        assert_eq!(result["temperature"], serde_json::json!(0.7));
        assert_eq!(result["stream"], serde_json::json!(false));
        assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    }
}
