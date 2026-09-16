//! Integration tests for windowing, offset rebasing and span fusion.

use gradatum_core::pii::{
    Budget, DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, Surface,
};
use gradatum_privacy::{MergeStats, RegexDetector, WindowPlan, rebase, validate_and_merge};

/// Builds a detect input for a body scanned under the async surface.
fn input(text: &str) -> DetectInput<'_> {
    DetectInput::new(text, Surface::Async, Budget::new(10_000, 262_144))
}

/// Runs the floor detector over every window of `plan` and rebases each span onto
/// the body, in the normative order (rebase before validation).
fn detect_every_window(plan: &WindowPlan, body: &str, detector: &dyn PiiDetector) -> Vec<PiiSpan> {
    let mut spans: Vec<PiiSpan> = Vec::new();
    for (window_start, window) in plan.windows(body) {
        let detection = detector
            .detect(&DetectInput::new(
                window,
                Surface::Async,
                Budget::new(10_000, plan.max_input_bytes),
            ))
            .expect("a detector that never errors");
        let mut window_spans = detection.spans;
        rebase(&mut window_spans, window_start);
        spans.extend(window_spans);
    }
    spans
}

#[test]
fn entity_straddling_a_window_boundary_yields_one_span_at_body_offset() {
    // An address placed so that a window cut falls strictly inside it: the first
    // window can only see a truncated prefix, the next one sees it whole because of
    // the overlap. What comes out must be a single span at the body offset — not a
    // window-relative one, and not two partial ones.
    let email = "alice@example.com";
    // Padding is blank on purpose: a letter would be a valid local or domain part
    // and the floor detector would legitimately extend the span into it.
    let body = format!("{}{}{}", " ".repeat(21), email, " ".repeat(30));
    let entity_start = 21;
    let entity_end = entity_start + email.len();

    let plan = WindowPlan::new(32, 16);
    let windows = plan.windows(&body);
    assert!(
        windows.len() > 1,
        "the fixture must be cut into several windows, got {windows:?}"
    );
    assert!(
        windows
            .iter()
            .any(|(start, w)| (start + w.len()) > entity_start && (start + w.len()) < entity_end),
        "the fixture must have a cut strictly inside the entity, got {windows:?}"
    );

    let spans = detect_every_window(&plan, &body, &RegexDetector::new());
    let (merged, stats) = validate_and_merge(&body, spans);

    assert_eq!(merged.len(), 1, "expected exactly one span, got {merged:?}");
    assert_eq!(merged[0].category, PiiCategory::Email);
    assert_eq!(
        merged[0].start, entity_start,
        "the span must carry the body offset, not a window-relative one"
    );
    let end = merged[0].start + merged[0].len;
    assert_eq!(&body[merged[0].start..end], email);
    assert_eq!(stats.rejected_out_of_bounds, 0);
    assert_eq!(stats.rejected_utf8_boundary, 0);
}

#[test]
fn span_out_of_bounds_is_rejected_and_counted() {
    let body = "hello world";

    let spans = vec![
        PiiSpan::new(0, 5, PiiCategory::Person, 0.9),
        // Ends past the body.
        PiiSpan::new(6, 100, PiiCategory::Person, 0.9),
        // Offset arithmetic that would overflow: rejected, never a panic.
        PiiSpan::new(usize::MAX, 4, PiiCategory::Person, 0.9),
    ];

    let (merged, stats) = validate_and_merge(body, spans);

    assert_eq!(merged.len(), 1, "the in-bounds span must survive");
    assert_eq!(merged[0], PiiSpan::new(0, 5, PiiCategory::Person, 0.9));
    assert_eq!(stats.rejected_out_of_bounds, 2);
    assert_eq!(stats.rejected_utf8_boundary, 0);
    assert_eq!(stats.merged, 0);
}

#[test]
fn span_on_invalid_utf8_boundary_is_rejected_and_counted() {
    // 'é' is a two-byte code point at bytes 3..5: 4 is not a char boundary, and a
    // span ending there would produce a non-UTF-8 slice.
    let body = "café moka";
    assert_eq!(body.find('é'), Some(3), "fixture: 'é' occupies bytes 3..5");

    let spans = vec![
        // Start inside 'é'.
        PiiSpan::new(4, 2, PiiCategory::Person, 0.9),
        // End inside 'é'.
        PiiSpan::new(3, 1, PiiCategory::Person, 0.9),
        // The whole code point: valid, must survive untouched.
        PiiSpan::new(3, 2, PiiCategory::Person, 0.9),
    ];

    let (merged, stats) = validate_and_merge(body, spans);

    assert_eq!(merged, vec![PiiSpan::new(3, 2, PiiCategory::Person, 0.9)]);
    assert_eq!(stats.rejected_utf8_boundary, 2);
    assert_eq!(stats.rejected_out_of_bounds, 0);
    assert_eq!(stats.merged, 0);
}

#[test]
fn same_email_seen_by_both_engines_produces_one_span() {
    // The floor detector and the model detector both report the same address. The
    // union on the same category must collapse them into one span carrying the
    // regex score, which is what keeps it out of the threshold.
    let body = "write to alice@example.com before friday";
    let email_start = body.find("alice").expect("fixture holds the address");

    let detection = RegexDetector::new()
        .detect(&input(body))
        .expect("the floor detector never errors");
    let regex_span = detection
        .spans
        .iter()
        .copied()
        .find(|s| s.category == PiiCategory::Email)
        .expect("the floor detector reports the address");
    let model_span = PiiSpan::new(email_start, regex_span.len, PiiCategory::Email, 0.7);

    let (merged, stats) = validate_and_merge(body, vec![regex_span, model_span]);

    assert_eq!(merged.len(), 1, "expected one span, got {merged:?}");
    assert_eq!(merged[0].start, email_start);
    assert_eq!(merged[0].len, regex_span.len);
    assert!(
        (merged[0].score - 1.0).abs() < f32::EPSILON,
        "the regex score must survive the fusion, got {}",
        merged[0].score
    );
    assert_eq!(stats.merged, 1);
}

#[test]
fn regex_span_absorbs_overlapping_model_span() {
    // The model span is nested inside the regex one and is passed first: the
    // outcome must not depend on input order, and the absorbed span must leave no
    // trace in the output.
    let body = "write to alice@example.com before friday";

    let detection = RegexDetector::new()
        .detect(&input(body))
        .expect("the floor detector never errors");
    let regex_span = detection
        .spans
        .iter()
        .copied()
        .find(|s| s.category == PiiCategory::Email)
        .expect("the floor detector reports the address");
    let model_span = PiiSpan::new(
        regex_span.start + 3,
        regex_span.len - 6,
        PiiCategory::Email,
        0.6,
    );

    let (merged, stats) = validate_and_merge(body, vec![model_span, regex_span]);

    assert_eq!(merged, vec![regex_span], "the regex span must absorb it");
    assert_eq!(stats.merged, 1);
    assert_eq!(stats.rejected_out_of_bounds, 0);
    assert_eq!(stats.rejected_utf8_boundary, 0);
}

/// Model detector that reports an entity only when it sees it whole.
struct WholeEntityDetector {
    entity: &'static str,
    category: PiiCategory,
}

impl PiiDetector for WholeEntityDetector {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let spans = input
            .text
            .match_indices(self.entity)
            .map(|(at, _)| PiiSpan::new(at, self.entity.len(), self.category, 0.9))
            .collect();
        Ok(Detection::new(spans, 0))
    }
}

#[test]
fn entity_longer_than_overlap_may_be_missed_is_documented() {
    // Known under-detection, not an error (spec §5.2, window treatment, point 3):
    // an entity longer than the overlap is cut at every boundary it crosses, so no
    // window ever holds it whole. A detector that reports only complete entities
    // therefore reports nothing, and the writing path sees a clean scan while the
    // entity goes undetected. Nothing here counts as a failure: the guards are an
    // `overlap_bytes` sized on the longest entity of the model, and a metric on the
    // spans that touch a window boundary.
    const ENTITY: &str = "alice.super.long.address@example.com";
    let body = format!("{}{}{}", "x".repeat(30), ENTITY, "z".repeat(30));
    let plan = WindowPlan::new(40, 8);
    let detector = WholeEntityDetector {
        entity: ENTITY,
        category: PiiCategory::Email,
    };

    // The entity is detectable in one piece: what follows is not a fixture artefact.
    let whole = detector
        .detect(&DetectInput::new(
            &body,
            Surface::Async,
            Budget::new(10_000, body.len()),
        ))
        .expect("the fixture detector never errors");
    assert_eq!(whole.spans.len(), 1, "the entity is detectable uncut");

    let windows = plan.windows(&body);
    assert!(
        windows.len() > 1,
        "the fixture must be cut, got {windows:?}"
    );
    assert!(
        windows.iter().all(|(_, w)| !w.contains(ENTITY)),
        "the fixture must keep the entity out of every window, got {windows:?}"
    );

    let spans = detect_every_window(&plan, &body, &detector);
    let (merged, stats) = validate_and_merge(&body, spans);

    assert!(
        merged.is_empty(),
        "the entity longer than the overlap is missed, got {merged:?}"
    );
    assert_eq!(stats.merged, 0);
    assert_eq!(stats.rejected_out_of_bounds, 0);
    assert_eq!(stats.rejected_utf8_boundary, 0);
}

#[test]
fn overlapping_same_category_spans_separated_by_another_category_still_merge() {
    // A span of another category sitting inside the byte range where two spans of
    // the same category overlap must not split that overlap. Fusing on adjacency
    // over the whole list would leave two `Email` spans in the output, and
    // `counts` is a count of entities, not of spans: a duplicated entity in the
    // overlap would be counted twice (spec §5.2 point 2, §5.3).
    let body = " ".repeat(40);
    let spans = vec![
        PiiSpan::new(0, 20, PiiCategory::Email, 1.0),
        PiiSpan::new(5, 3, PiiCategory::Phone, 0.9),
        PiiSpan::new(15, 20, PiiCategory::Email, 0.8),
    ];

    let (merged, stats) = validate_and_merge(&body, spans);

    assert_eq!(
        merged.len(),
        2,
        "expected one Email and one Phone, got {merged:?}"
    );
    assert_eq!(stats.merged, 1, "the two Email spans are one fusion");
    let email = merged
        .iter()
        .find(|s| s.category == PiiCategory::Email)
        .expect("the Email span survives");
    assert_eq!(
        (email.start, email.len),
        (0, 35),
        "same-category fusion unions the bounds, got {email:?}"
    );
    let phone = merged
        .iter()
        .find(|s| s.category == PiiCategory::Phone)
        .expect("the Phone span survives");
    assert_eq!(
        (phone.start, phone.len),
        (5, 3),
        "the foreign span is untouched, got {phone:?}"
    );
    assert_eq!(
        merged
            .iter()
            .filter(|s| s.category == PiiCategory::Email)
            .count(),
        1,
        "one entity of that category, not two, got {merged:?}"
    );
}

#[test]
fn window_size_below_four_bytes_is_clamped_and_skips_no_code_point() {
    // Four bytes is the widest UTF-8 code point: a plan asking for less cannot
    // hold any character whole, so the ceiling is clamped up rather than letting
    // a cut fall inside a code point and skip it. Every byte of the body must
    // therefore be covered by at least one window.
    let body = "éé";
    assert_eq!(body.len(), 4, "fixture: two two-byte code points");
    let plan = WindowPlan::new(1, 0);
    let windows = plan.windows(body);

    assert!(
        !windows.is_empty(),
        "a ceiling clamped to four bytes still yields a window, got {windows:?}"
    );
    assert!(
        windows.iter().all(|(_, w)| !w.is_empty()),
        "no window may be empty, got {windows:?}"
    );

    let mut covered = vec![false; body.len()];
    for (start, window) in &windows {
        let begin = *start;
        let end = begin + window.len();
        assert!(body.is_char_boundary(begin) && body.is_char_boundary(end));
        for byte in covered.iter_mut().take(end).skip(begin) {
            *byte = true;
        }
    }
    assert!(
        covered.iter().all(|byte| *byte),
        "every byte must be covered, got {covered:?} from {windows:?}"
    );
}

/// The pass-through the compiler needs: `MergeStats` is `#[non_exhaustive]`, so a
/// consumer builds it through `new` rather than a struct literal.
#[test]
fn merge_stats_is_constructible_through_new() {
    let stats = MergeStats::new(1, 2, 3, 4);
    assert_eq!(stats.rejected_out_of_bounds, 1);
    assert_eq!(stats.rejected_utf8_boundary, 2);
    assert_eq!(stats.touching_window_edge, 3);
    assert_eq!(stats.merged, 4);
}
