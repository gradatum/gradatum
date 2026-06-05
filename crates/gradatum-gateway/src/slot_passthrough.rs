//! Injection du champ `slot_id` dans le body JSON upstream.
//!
//! Le slot pinning KV cache llama.cpp (paramètre natif `slot_id`) permet
//! de pincer un slot par session et de bénéficier du cache KV sans recompute
//! inter-tours. Ce module fournit la logique d'extraction et d'injection.
//!
//! Contrat :
//! - `extract_slot_id()` : extrait et parse le header `X-Slot-Id` depuis les headers HTTP
//! - `inject_slot_id_if_needed()` : injecte `slot_id` dans un `serde_json::Value` si applicable
//!
//! Design : fonctions pures sans side effects — facilitant les tests unitaires.

use axum::http::HeaderMap;

/// Extrait la valeur du header `X-Slot-Id` et la parse en `u32`.
///
/// Retourne `None` si :
/// - le header est absent
/// - la valeur n'est pas un entier positif valide (u32)
/// - la valeur contient des caractères non-ASCII
///
/// La valeur parsée est un `u32` — identifiant de slot llama.cpp (0 à 127 typiquement).
/// Une valeur non parseable est ignorée silencieusement (non-breaking).
pub fn extract_slot_id(headers: &HeaderMap) -> Option<u32> {
    headers
        .get("x-slot-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Injecte `slot_id` dans le body JSON upstream si les conditions sont remplies.
///
/// Conditions d'injection :
/// 1. `enable_slot_passthrough` == `true` (kill-switch config)
/// 2. `slot_id` == `Some(n)` (header présent et parseable)
///
/// Effets :
/// - Si injection : insère `"slot_id": n` dans l'objet JSON racine.
///   Si le champ existe déjà, il est écrasé (le header fait foi).
/// - Si pas d'injection : le body est retourné inchangé.
///
/// Panics : jamais — `body` est toujours un `Value::Object` (serde depuis struct Rust).
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
