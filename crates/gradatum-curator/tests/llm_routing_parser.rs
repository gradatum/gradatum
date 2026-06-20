//! Tests TDD pour `LlmRoutingResponse` — parseur JSON 4-champs (L-02).
//!
//! Vérifie le parsing du format JSON retourné par le prompt curator-classifier-v1
//! et le mapping vers `gradatum_chat::backend::CuratorDecision`.
//!
//! Step 1 : ces tests doivent échouer avec "type LlmRoutingResponse not found"
//! avant l'implémentation (Step 3).

use gradatum_curator::routing::LlmRoutingResponse;

#[test]
fn parse_valid_classifier_v1_json() {
    let raw =
        r#"{"section":"decisions","tags":["rust","p2.0c"],"wikilinks":[],"duplicate_hint":null}"#;
    let parsed: LlmRoutingResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed.section, "decisions");
    assert_eq!(parsed.tags, vec!["rust".to_string(), "p2.0c".to_string()]);
    assert!(parsed.wikilinks.is_empty());
    assert!(parsed.duplicate_hint.is_none());
}

#[test]
fn parse_with_duplicate_hint() {
    let raw = r#"{"section":"reasoning","tags":["pattern"],"wikilinks":["[[other-note]]"],"duplicate_hint":"01HMXJ2K..."}"#;
    let parsed: LlmRoutingResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed.section, "reasoning");
    assert_eq!(parsed.tags, vec!["pattern".to_string()]);
    assert_eq!(parsed.wikilinks, vec!["[[other-note]]".to_string()]);
    assert_eq!(parsed.duplicate_hint, Some("01HMXJ2K...".into()));
}

#[test]
fn map_to_curator_decision() {
    let raw = r#"{"section":"debug","tags":["bug","fix"],"wikilinks":[],"duplicate_hint":null}"#;
    let parsed: LlmRoutingResponse = serde_json::from_str(raw).unwrap();
    let decision = parsed.into_curator_decision();
    // La section "debug" doit être préservée dans le champ section du CuratorDecision
    assert_eq!(decision.section, "debug");
    assert_eq!(decision.tags, vec!["bug".to_string(), "fix".to_string()]);
    assert!(decision.wikilinks.is_empty());
    assert!(decision.duplicate_hint.is_none());
}
