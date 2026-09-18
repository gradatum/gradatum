//! Integration tests for the regex floor detector.

use gradatum_core::pii::{Budget, DetectInput, PiiCategory, PiiDetector, PiiSpan, Surface};
use gradatum_privacy::{RegexDetector, validate_and_merge};

/// Builds a default detect input for a body under the async surface.
fn input(text: &str) -> DetectInput<'_> {
    DetectInput::new(text, Surface::Async, Budget::new(10_000, 262_144))
}

/// Runs the floor detector over `text` and returns the spans it reported.
fn spans(text: &str) -> Vec<PiiSpan> {
    RegexDetector::new()
        .detect(&input(text))
        .expect("detector never errors")
        .spans
}

/// Asserts that `text` yields exactly one span of `category`, covering exactly
/// the `needle` occurrence: the assertion is on the offsets, not just on a
/// category being present somewhere in the body.
fn assert_one_span(text: &str, category: PiiCategory, needle: &str) {
    let found: Vec<PiiSpan> = spans(text)
        .into_iter()
        .filter(|s| s.category == category)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one {category:?} span in {text:?}, got {found:?}",
    );
    let start = text
        .find(needle)
        .expect("the fixture contains the expected match");
    assert_eq!(
        (found[0].start, found[0].len),
        (start, needle.len()),
        "expected the {category:?} span to cover {needle:?} in {text:?}, got {found:?}",
    );
}

/// Asserts that `text` yields no span at all of `category`.
fn assert_no_span(text: &str, category: PiiCategory) {
    let found: Vec<PiiSpan> = spans(text)
        .into_iter()
        .filter(|s| s.category == category)
        .collect();
    assert!(
        found.is_empty(),
        "expected no {category:?} span in {text:?}, got {found:?}",
    );
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

// --- the phone pattern carries an anchor, a separator set and a ceiling ---

#[test]
fn phone_matches_domestic_and_international_spellings() {
    for spelling in [
        "06 12 34 56 78",
        "0612345678",
        "06.12.34.56.78",
        "06-12-34-56-78",
        "+33 6 12 34 56 78",
        "+33612345678",
        "+33.6.12.34.56.78",
    ] {
        let text = format!("call {spelling} today");
        assert_one_span(&text, PiiCategory::Phone, spelling);
    }
}

#[test]
fn phone_span_offsets_land_on_the_number() {
    assert_one_span(
        "joignable au 06 12 34 56 78, merci",
        PiiCategory::Phone,
        "06 12 34 56 78",
    );
}

#[test]
fn phone_is_not_found_in_iso_timestamps_or_long_identifiers() {
    for text in [
        "2026-09-16T14:01:04.341719759Z",
        "written at T14:01:04.341719759Z sharp",
        "fragment 14:01:04.341719759 alone",
        "migration 20260506000001 applied",
        "release 2.3.1 is out",
        "listening on :19090",
        "the pair 14:01 or 15:02",
    ] {
        assert_no_span(text, PiiCategory::Phone);
    }
}

#[test]
fn phone_requires_the_whole_ten_digit_run() {
    // A longer digit run must not surrender a ten-digit prefix: the ceiling and
    // the digit boundary are what keep the pattern off identifiers.
    assert_no_span("06123456789012", PiiCategory::Phone);
    assert_no_span("prefixed 900612345678", PiiCategory::Phone);
}

// --- the floor gains an address category ---

#[test]
fn address_matches_ipv4_bare_with_port_and_as_a_cidr() {
    assert_one_span(
        "host 192.0.2.10 answers",
        PiiCategory::Address,
        "192.0.2.10",
    );
    assert_one_span(
        "bound to 203.0.113.42:8080 daily",
        PiiCategory::Address,
        "203.0.113.42:8080",
    );
    assert_one_span(
        "route 198.51.100.0/24 via the tunnel",
        PiiCategory::Address,
        "198.51.100.0/24",
    );
}

#[test]
fn address_in_a_cidr_is_a_single_span() {
    let text = "route 198.51.100.0/24 via the tunnel";
    let found: Vec<PiiSpan> = spans(text)
        .into_iter()
        .filter(|s| s.category == PiiCategory::Address)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "the bare address nested in the CIDR must not survive pruning, got {found:?}",
    );

    // The same fusion, stated at the geometry layer: two overlapping spans of one
    // category are unioned into the wider one, whatever the detector reported.
    let (merged, stats) = validate_and_merge(
        "198.51.100.0/24",
        vec![
            PiiSpan::new(0, 14, PiiCategory::Address, 1.0),
            PiiSpan::new(0, 11, PiiCategory::Address, 1.0),
        ],
    );
    assert_eq!((merged.len(), stats.merged), (1, 1), "got {merged:?}");
    assert_eq!((merged[0].start, merged[0].len), (0, 14));
}

#[test]
fn address_matches_ipv4_with_a_port_spanning_the_whole_pair() {
    // Documented choice: the port belongs to the span. A host and the port it
    // answers on are one observed network location, and splitting them would
    // report the port as an address of its own.
    assert_one_span(
        "peer 192.0.2.10:8443 up",
        PiiCategory::Address,
        "192.0.2.10:8443",
    );
}

#[test]
fn address_matches_ipv6_in_full_and_compressed_forms() {
    assert_one_span(
        "peer 2001:0db8:85a3:0000:0000:8a2e:0370:7334 replied",
        PiiCategory::Address,
        "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
    );
    assert_one_span(
        "gateway 2001:db8::1 here",
        PiiCategory::Address,
        "2001:db8::1",
    );
    assert_one_span(
        "peer 2001:db8::8a2e:370:7334 replied",
        PiiCategory::Address,
        "2001:db8::8a2e:370:7334",
    );
}

#[test]
fn address_matches_a_mount_path() {
    assert_one_span(
        "mounted at /mnt/data/notes.md today",
        PiiCategory::Address,
        "/mnt/data/notes.md",
    );
}

#[test]
fn address_ignores_versions_loopback_identifiers_and_colon_runs() {
    for text in [
        "release 2.3.1 is out",
        "build 1.2.3.4.5 shipped",
        "loopback 127.0.0.1 is up",
        "unspecified 0.0.0.0 route",
        "self ::1 address",
        "link-local fe80::1 seen",
        "mac aa:bb:cc:dd:ee:ff seen",
        "module std::io used",
        "the /mnt directory",
        "clock 12:34:56 sharp",
    ] {
        assert_no_span(text, PiiCategory::Address);
    }
}

#[test]
fn address_reports_a_four_group_version_run_as_an_address() {
    // Documented limit, not a defect being asserted away: a four-group dotted run
    // is an address by shape, and a version of exactly four groups is not
    // distinguishable from one. A fifth group is (see the negative cases above),
    // and a three-group version never reaches this pattern.
    assert_one_span("build 1.2.3.4 shipped", PiiCategory::Address, "1.2.3.4");
}

#[test]
fn a_dotted_phone_number_is_not_also_read_as_an_address() {
    // The two patterns overlap on this spelling, and only the phone reading is
    // right: `6.12.34.56` would be four quads. The numeric boundary is what keeps
    // the address pattern off it — a leading zero is not an octet.
    let text = "call 06.12.34.56.78 today";
    assert_no_span(text, PiiCategory::Address);
    assert_one_span(text, PiiCategory::Phone, "06.12.34.56.78");
}
