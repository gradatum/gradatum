//! Integration tests for the regex floor detector.

use gradatum_core::pii::{Budget, DetectInput, PiiCategory, PiiDetector, Surface};
use gradatum_privacy::RegexDetector;

/// Builds a default detect input for a body under the async surface.
fn input(text: &str) -> DetectInput<'_> {
    DetectInput::new(text, Surface::Async, Budget::new(10_000, 262_144))
}

#[test]
fn regex_detects_email_iban_phone_fr_nir() {
    let detector = RegexDetector::new();

    let cases: [(&str, PiiCategory); 4] = [
        (
            "write to alice.martin@example.com please",
            PiiCategory::Email,
        ),
        (
            "IBAN FR7630006000011234567890189 for the transfer",
            PiiCategory::AccountNumber,
        ),
        ("call me on 06 12 34 56 78 tomorrow", PiiCategory::Phone),
        (
            "NIR 1 85 12 75 116 001 42 on file",
            PiiCategory::AccountNumber,
        ),
    ];

    for (body, expected) in cases {
        let detection = detector
            .detect(&input(body))
            .expect("detector never errors");
        assert!(
            detection.spans.iter().any(|s| s.category == expected),
            "expected a {expected:?} span in {body:?}, got {:?}",
            detection.spans,
        );
    }
}

#[test]
fn regex_scores_are_maximal() {
    let detector = RegexDetector::new();
    let detection = detector
        .detect(&input(
            "mail bob@example.org and IBAN FR7630006000011234567890189",
        ))
        .expect("detector never errors");

    assert!(!detection.spans.is_empty(), "expected at least one span");
    for span in &detection.spans {
        assert!(
            (span.score - 1.0).abs() < f32::EPSILON,
            "regex spans must carry the maximal score, got {}",
            span.score,
        );
    }
}
