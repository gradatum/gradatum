//! Integration tests for the sidecar detector client, driven over a real unix
//! socket against a fake sidecar.
//!
//! The fake is written from the server's own protocol header and server loop: the
//! frame caps, the identifier alphabet, the refusal of an oversized body *before*
//! any span is produced, and the close — not an invented status — on a request the
//! protocol does not admit. A fake that only mimicked the happy path would let a
//! client that speaks a different protocol pass its own tests, which is exactly the
//! failure these tests exist to catch.
//!
//! What the fake answers is scripted per connection and per request, which is what
//! lets these tests drive the paths that matter: a diverging handshake, a truncated
//! frame, a dead stream, a sidecar that never answers.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use gradatum_core::pii::{Budget, DetectInput, PiiCategory, PiiDetectError, PiiDetector, Surface};
use gradatum_privacy::{
    ADAPTER_TABLE_V, MAX_RESPONSE_FRAME_BYTES, SUPPORTED_MAPPING_V_MIN, SidecarConfig,
    SidecarDetector,
};
use serde_json::{Value, json};

// The fake is shared with the server's mount tests, which drive the same protocol:
// a second copy of it would be a second, drifting description of the wire.
#[path = "sidecar_fake/mod.rs"]
mod fake;

use fake::{FakeSidecar, Reply};

/// Digest the fake announces as its own build, and the value the configuration
/// pins. Bare lowercase hex, as the server computes it.
const BINARY_DIGEST: &str = "3f1a5c0b7d9e246813579bdf0246ace813579bdf0246ace813579bdf0246ace8";

/// Digest the fake announces for its weights.
const MODEL_DIGEST: &str = "b7d9e246813579bdf0246ace813579bdf0246ace813579bdf0246ace813579bdf";

/// Digest the fake announces over its label vocabulary: a `sha256:`-tagged
/// string, which is what the server emits and therefore what a client must read.
const LABELS_V: &str = "sha256:fd92891e5a1c0b7d9e246813579bdf0246ace813579bdf0246ace813579bdf02";

/// Aggregation mode the fake announces, and the one the configuration pins.
const SCORE_MODE: &str = "mean_token_prob";

/// The handshake a correctly configured sidecar announces.
fn handshake() -> Value {
    fake::handshake(BINARY_DIGEST, MODEL_DIGEST, LABELS_V, 0)
}

/// A configuration matching [`handshake`], pointed at `path`.
fn config(path: &Path) -> SidecarConfig {
    SidecarConfig::new(path, BINARY_DIGEST, MODEL_DIGEST)
        .with_score_mode(SCORE_MODE)
        .with_timeout_ms_sync_per_kib(800)
        .with_max_input_bytes(262_144)
        .with_window_overlap_bytes(512)
        .with_breaker(3, 30)
}

/// A detect input whose caller budget is short enough to keep a timeout test fast.
fn input(text: &str) -> DetectInput<'_> {
    DetectInput::new(text, Surface::Sync, Budget::new(120, 262_144))
}

/// Answers every request with a scan that found nothing.
fn reply_empty(connection: u64, request: u64, body: Value) -> Reply {
    let _ = (connection, request, body);
    Reply::Spans(Vec::new())
}

fn connect(fake: &FakeSidecar) -> SidecarDetector {
    SidecarDetector::connect(config(fake.path())).expect("a valid configuration")
}

/// Reads the identifier out of a request the fake admitted.
fn request_id(request: &Value) -> &str {
    request["id"]
        .as_str()
        .expect("the protocol admits string ids")
}

// ---------------------------------------------------------------------------
// The three protocol divergences the client used to carry
// ---------------------------------------------------------------------------

#[test]
fn the_handshake_label_digest_is_read_as_a_string_and_reported() {
    // The server announces `labels_v` as `"sha256:<64 hex>"`. A client that typed
    // it as a number could not read a single handshake the server sends, and would
    // latch itself unavailable against every correctly deployed sidecar.
    let fake = FakeSidecar::start(handshake(), reply_empty);
    let detector = connect(&fake);

    let detection = detector
        .detect(&input("a body"))
        .expect("a conformant handshake must be readable");

    assert_eq!(detection.spans.len(), 0);
    assert_eq!(
        detector.diagnostics().labels_v.as_deref(),
        Some(LABELS_V),
        "the label-set digest the sidecar announced must be reported, not dropped"
    );
}

#[test]
fn a_label_digest_that_is_not_a_number_is_still_a_handshake() {
    // The positive form of the same claim: nothing about the digest is parsed,
    // compared or validated as a number.
    let mut announced = handshake();
    announced["labels_v"] =
        json!("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    let fake = FakeSidecar::start(announced, reply_empty);
    let detector = connect(&fake);

    assert!(
        detector.detect(&input("a body")).is_ok(),
        "the label digest is an opaque string and must be read as one"
    );
    assert_eq!(
        detector.diagnostics().labels_v.as_deref(),
        Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
    );
}

#[test]
fn a_mapping_revision_above_the_floor_is_accepted_and_reported() {
    // `mapping_v` is the sidecar's own renumbering revision, not this client's
    // table version. Pinning it to equality would refuse every sidecar that
    // renumbers its labels — a change the label-name mapping absorbs by design.
    for revision in [SUPPORTED_MAPPING_V_MIN, SUPPORTED_MAPPING_V_MIN + 7] {
        let mut announced = handshake();
        announced["mapping_v"] = json!(revision);
        let fake = FakeSidecar::start(announced, reply_empty);
        let detector = connect(&fake);

        assert!(
            detector.detect(&input("a body")).is_ok(),
            "mapping_v={revision} is at or above the floor and must be accepted"
        );
        assert_eq!(
            detector.diagnostics().mapping_v,
            Some(revision),
            "the revision the sidecar announced must be reported"
        );
    }
}

#[test]
fn a_mapping_revision_below_the_floor_is_refused() {
    // The floor is not vacuous: the protocol carries a signed integer, and a
    // revision below it is a sidecar this client cannot read.
    let mut announced = handshake();
    announced["mapping_v"] = json!(SUPPORTED_MAPPING_V_MIN - 1);
    let fake = FakeSidecar::start(announced, reply_empty);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body nobody should scan"));

    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "a revision below the floor must leave the detector unavailable, got {result:?}"
    );
    assert_eq!(fake.requests(), 0, "no body may be sent to such a sidecar");
}

#[test]
fn the_client_table_version_is_its_own_and_is_recorded_in_the_identity() {
    let fake = FakeSidecar::start(handshake(), reply_empty);
    let detector = connect(&fake);

    let identity = detector.identity().expect("a configured detector");

    assert_eq!(
        identity.mapping_v, ADAPTER_TABLE_V,
        "the identity records the version of the table this client applied"
    );
    assert_ne!(
        i64::from(ADAPTER_TABLE_V),
        SUPPORTED_MAPPING_V_MIN,
        "the client's table version and the sidecar's revision floor are distinct notions"
    );
}

#[test]
fn every_request_identifier_is_one_the_protocol_admits() {
    let fake = FakeSidecar::start(handshake(), reply_empty);
    let detector = connect(&fake);

    for body in ["first body", "second body", "a third body"] {
        detector.detect(&input(body)).expect("a scan");
    }

    assert_eq!(
        fake.refused(),
        0,
        "the server drops a request whose identifier breaks its alphabet, its \
         length or its JSON shape; the client must never send one"
    );
    let ids: Vec<String> = fake
        .seen()
        .iter()
        .map(|request| request_id(request).to_owned())
        .collect();
    assert_eq!(ids.len(), 3);
    for id in &ids {
        assert!(
            fake::is_valid_id(id),
            "identifier {id:?} is outside [A-Za-z0-9._:-]{{1,64}}"
        );
    }
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        distinct.len(),
        ids.len(),
        "identifiers must tell one request from another: {ids:?}"
    );
}

#[test]
fn the_identifier_is_a_string_and_another_requests_identifier_is_refused() {
    // The guard against cross-talk compares identifiers, so it is exercised with
    // an identifier of the same shape as the request's, differing by its digits.
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        let id = body["id"].as_str().unwrap_or("c-0").to_owned();
        Reply::Json(json!({"id": format!("{id}-elsewhere"), "status": "ok", "spans": []}))
    });
    let detector = connect(&fake);

    let result = detector.detect(&input("a body"));

    assert!(
        matches!(result, Err(PiiDetectError::Protocol { .. })),
        "an answer carrying another request's identifier must be refused, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Handshake gating
// ---------------------------------------------------------------------------

#[test]
fn a_matching_handshake_lets_spans_through() {
    // The body carries nothing the deterministic floor recognises, so every span
    // found below came from the model: the assertion is about the model path alone.
    const BODY: &str = "call alice about the contract";
    let at = BODY
        .find("alice")
        .expect("the fixture body carries the name");
    let fake = FakeSidecar::start(handshake(), move |_, _, _| {
        Reply::Spans(vec![json!({
            "label": "FIRSTNAME",
            "start": at,
            "end": at + 5,
            "score": 0.75,
        })])
    });
    let detector = connect(&fake);

    let detection = detector
        .detect(&input(BODY))
        .expect("a detector that answered");

    assert_eq!(detection.spans.len(), 1);
    assert_eq!(detection.spans[0].start, at);
    assert_eq!(detection.spans[0].len, 5);
    assert_eq!(detection.spans[0].category, PiiCategory::Person);
    assert_eq!(detection.spans[0].score, 0.75);
    assert_eq!(detection.unmapped_labels, 0);
    assert_eq!(fake.connections(), 1);
}

#[test]
fn a_diverging_handshake_digest_latches_the_detector_unavailable() {
    let mut announced = handshake();
    announced["binary_sha256"] = json!("sha256:some-other-build");
    let fake = FakeSidecar::start(announced, reply_empty);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body nobody should scan"));

    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "a diverging binary digest must leave the detector unavailable, got {result:?}"
    );
    // The latch is not a per-call hiccup: the request was never sent at all, and a
    // second call must not reach the socket either.
    assert_eq!(fake.requests(), 0);
    assert!(detector.detect(&input("a second body")).is_err());
    assert_eq!(fake.requests(), 0);
}

#[test]
fn a_sidecar_that_is_not_ready_latches_the_detector_unavailable() {
    let mut announced = handshake();
    announced["ready"] = json!(false);
    let fake = FakeSidecar::start(announced, reply_empty);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body nobody should scan"));

    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "an unready sidecar must leave the detector unavailable, got {result:?}"
    );
    assert_eq!(fake.requests(), 0);
}

#[test]
fn a_handshake_that_is_not_json_latches_the_detector_unavailable() {
    // A handshake frame whose body is not a JSON object at all: the fake announces
    // `null`, which parses but carries none of the declared fields.
    let fake = FakeSidecar::start(json!(null), |_, _, _| Reply::Close);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body nobody should scan"));

    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "an unreadable handshake must leave the detector unavailable, got {result:?}"
    );
}

#[test]
fn a_diverging_score_mode_latches_the_detector_unavailable() {
    let mut announced = handshake();
    announced["score_mode"] = json!("max_token_prob");
    let fake = FakeSidecar::start(announced, reply_empty);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body nobody should scan"));

    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "a score mode this deployment did not pin must not be trusted, got {result:?}"
    );
    assert_eq!(fake.requests(), 0);
}

// ---------------------------------------------------------------------------
// Answers outside the format
// ---------------------------------------------------------------------------

#[test]
fn an_input_too_large_status_is_a_size_error_outside_the_breaker() {
    // The fake refuses the body the way the server does: a length comparison,
    // before the model is consulted. One failure of any kind is enough to open the
    // breaker below, so a size state that were counted would make the third call
    // come back unavailable.
    const BODY: &str = "a body too large for the synchronous path";
    let fake = FakeSidecar::start_limited(handshake(), 4, reply_empty);
    assert!(
        BODY.len() > fake.max_input_bytes(),
        "the fixture must exceed the fake's own threshold, or the refusal below \
         would prove nothing about a size state"
    );
    let mut cfg = config(fake.path());
    cfg = cfg.with_breaker(1, 30);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    for _ in 0..3 {
        let result = detector.detect(&input(BODY));
        assert!(
            matches!(result, Err(PiiDetectError::InputTooLarge { .. })),
            "a size state must stay a size state, got {result:?}"
        );
    }

    assert_eq!(
        fake.requests(),
        3,
        "every call must have reached the sidecar: a size state opens no breaker"
    );
    let diagnostics = detector.diagnostics();
    assert!(!diagnostics.breaker_open);
    assert_eq!(diagnostics.breaker_openings, 0);
}

#[test]
fn an_oversized_window_reports_the_window_length_and_the_ceiling() {
    // A window is refused for *its* size, not the body's: reporting the body's
    // length would describe a body the sidecar never saw, and send an operator
    // looking for a cut that is not the one that was refused. The ceiling reported
    // is the one the window was submitted under — the caller's, where the caller
    // stated one — not the configured maximum the caller narrowed away from.
    const CALLER_CEILING: usize = 16;
    let fake = FakeSidecar::start_limited(handshake(), 4, reply_empty);
    let detector = connect(&fake);

    let body = "y".repeat(200);
    let result = detector.detect(&DetectInput::new(
        &body,
        Surface::Sync,
        Budget::new(120, CALLER_CEILING),
    ));

    let Err(PiiDetectError::InputTooLarge { len, budget }) = result else {
        panic!("an oversized window must be a size error, got {result:?}");
    };
    assert_eq!(
        len, CALLER_CEILING,
        "the reported length is the refused window's, not the body's"
    );
    assert!(
        len < body.len(),
        "the fixture must be cut, or the assertion above proves nothing"
    );
    assert_eq!(
        budget, CALLER_CEILING,
        "the reported ceiling is the one the window was submitted under"
    );
}

#[test]
fn a_response_frame_over_the_cap_is_a_protocol_error() {
    let fake = FakeSidecar::start(handshake(), |_, _, _| {
        Reply::DeclaredLength(MAX_RESPONSE_FRAME_BYTES + 1)
    });
    let detector = connect(&fake);

    let result = detector.detect(&input("a body"));

    assert!(
        matches!(result, Err(PiiDetectError::Protocol { .. })),
        "a frame over the cap must be refused, got {result:?}"
    );
}

#[test]
fn a_response_that_is_not_json_is_a_protocol_error() {
    let fake = FakeSidecar::start(handshake(), |_, _, _| {
        Reply::RawBody(b"{ this is not json".to_vec())
    });
    let detector = connect(&fake);

    let result = detector.detect(&input("a body"));

    assert!(
        matches!(result, Err(PiiDetectError::Protocol { .. })),
        "a malformed body must be refused, got {result:?}"
    );
}

#[test]
fn a_scan_reported_without_a_span_list_is_a_protocol_error() {
    // An absent span list and an empty one are indistinguishable to the caller, and
    // only one of them is a scan that found nothing. A sidecar that omits the list
    // while reporting a scan is refused rather than read as reporting no finding.
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        Reply::Json(json!({"id": body["id"], "status": "ok"}))
    });
    let detector = connect(&fake);

    let result = detector.detect(&input("a body"));

    assert!(
        matches!(result, Err(PiiDetectError::Protocol { .. })),
        "a scan without a span list must be refused, got {result:?}"
    );
}

#[test]
fn no_error_message_carries_the_scanned_body() {
    /// A value that cannot reach an error message by accident.
    const CANARY: &str = "C4N4RY-DO-NOT-LEAK-7f3a";
    let body = format!("prefix {CANARY} suffix");

    let mut diverging = handshake();
    diverging["model_sha256"] = json!("sha256:another-model");

    let cases: Vec<(&str, FakeSidecar)> = vec![
        (
            "a frame over the cap",
            FakeSidecar::start(handshake(), |_, _, _| {
                Reply::DeclaredLength(MAX_RESPONSE_FRAME_BYTES + 1)
            }),
        ),
        (
            "a malformed response body",
            FakeSidecar::start(handshake(), |_, _, _| {
                Reply::RawBody(b"{\"spans\": [{\"label\": ".to_vec())
            }),
        ),
        (
            "a diverging handshake",
            FakeSidecar::start(diverging, reply_empty),
        ),
        (
            "a sidecar that never answers",
            FakeSidecar::start(handshake(), |_, _, _| Reply::Silent),
        ),
    ];

    for (label, fake) in cases {
        let detector = connect(&fake);
        let error = detector
            .detect(&input(&body))
            .err()
            .unwrap_or_else(|| panic!("{label} must be an error"));

        let rendered = format!("{error} | {error:?}");
        assert!(
            !rendered.contains(CANARY),
            "{label}: the scanned body reached the error message: {rendered}"
        );
    }

    // Not vacuous: the body really is what the client transmits, so the tests above
    // exercised a request that carried the canary.
    let fake = FakeSidecar::start(handshake(), |_, _, _| {
        Reply::DeclaredLength(MAX_RESPONSE_FRAME_BYTES + 1)
    });
    let detector = connect(&fake);
    let _ = detector.detect(&input(&body));
    assert!(
        fake.seen()
            .iter()
            .any(|request| request["body"].as_str().is_some_and(|b| b.contains(CANARY))),
        "the fixture must actually send the canary, or the assertions above prove nothing"
    );
}

// ---------------------------------------------------------------------------
// Connections, retries and the breaker
// ---------------------------------------------------------------------------

#[test]
fn sequential_requests_reuse_one_connection() {
    let fake = FakeSidecar::start(handshake(), reply_empty);
    let detector = connect(&fake);

    detector.detect(&input("first body")).expect("a first scan");
    detector
        .detect(&input("second body"))
        .expect("a second scan");

    assert_eq!(fake.connections(), 1, "the connection must be persistent");
    assert_eq!(fake.requests(), 2);
}

#[test]
fn a_broken_pipe_is_reconnected_and_the_request_retried() {
    let fake = FakeSidecar::start(handshake(), |connection, _, _| {
        if connection == 1 {
            // The first connection dies mid-conversation, without answering.
            Reply::Close
        } else {
            Reply::Spans(Vec::new())
        }
    });
    let detector = connect(&fake);

    let result = detector.detect(&input("a body"));

    assert!(
        result.is_ok(),
        "a broken stream must be reconnected and the request retried, got {result:?}"
    );
    assert_eq!(fake.connections(), 2);
    assert_eq!(fake.requests(), 2);
}

#[test]
fn a_timeout_is_a_typed_error_and_never_an_empty_detection() {
    let fake = FakeSidecar::start(handshake(), |_, _, _| Reply::Silent);
    let detector = connect(&fake);

    let result = detector.detect(&input("a body the sidecar never answers"));

    assert!(
        matches!(result, Err(PiiDetectError::Timeout { timeout_ms: 120 })),
        "a silent sidecar must time out on the caller's allowance, got {result:?}"
    );
    // The distinction the caller's policy rests on: a timeout is not a clean scan.
    assert!(
        result.is_err(),
        "an unanswered scan is never a successful empty detection"
    );
}

#[test]
fn the_breaker_opens_after_consecutive_failures_and_recovers_after_the_cooldown() {
    let answered = Arc::new(AtomicU64::new(0));
    let scripted = Arc::clone(&answered);
    let fake = FakeSidecar::start(handshake(), move |_, _, _| {
        // The first two requests are swallowed; from the third on the sidecar
        // answers, which is what a probe after the cooldown must observe.
        if scripted.fetch_add(1, Ordering::SeqCst) < 2 {
            Reply::Silent
        } else {
            Reply::Spans(Vec::new())
        }
    });
    let mut cfg = config(fake.path());
    cfg = cfg.with_breaker(2, 1);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    for attempt in 1..=2 {
        let result = detector.detect(&input("a body the sidecar swallows"));
        assert!(
            matches!(result, Err(PiiDetectError::Timeout { .. })),
            "attempt {attempt} must time out, got {result:?}"
        );
    }
    assert!(
        detector.diagnostics().breaker_open,
        "two failures open a breaker of two"
    );

    let before = fake.requests();
    let result = detector.detect(&input("a body while the breaker is open"));
    assert!(
        matches!(result, Err(PiiDetectError::Unavailable { .. })),
        "an open breaker must report unavailable, got {result:?}"
    );
    assert_eq!(
        fake.requests(),
        before,
        "an open breaker must not pay the detector's delay again"
    );

    sleep(Duration::from_millis(1_200));

    let probe = detector.detect(&input("the half-open probe"));
    assert!(
        probe.is_ok(),
        "the probe after the cooldown must reach the recovered sidecar, got {probe:?}"
    );
    let diagnostics = detector.diagnostics();
    assert!(
        !diagnostics.breaker_open,
        "a successful probe closes the breaker"
    );
    assert_eq!(diagnostics.breaker_openings, 1);

    assert!(
        detector.detect(&input("a body after recovery")).is_ok(),
        "the breaker must stay closed after a successful probe"
    );
}

#[test]
fn a_completed_round_trip_clears_the_consecutive_failure_count() {
    // A size refusal is the sidecar *answering*: the connection worked, the frame
    // was exchanged, the process is alive. Counting it as a failure would make
    // three ordinary large writes open the breaker; leaving the count untouched
    // would let a completed round trip sit between two failures that then add up
    // to an outage which never happened.
    const CEILING: usize = 32;
    let fake = FakeSidecar::start_limited(handshake(), CEILING, |_, _, _| Reply::Close);
    let mut cfg = config(fake.path());
    cfg = cfg.with_breaker(3, 30);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    // The failing body must fit the fake's ceiling, or the fake would answer it
    // instead of closing on it — a size refusal is a completed round trip, which is
    // the very thing this test is about.
    let failing = "a body the script closes on";
    assert!(
        failing.len() <= CEILING,
        "the fixture must be admitted by the fake, not refused by it"
    );
    let oversized = "a body over the ceiling of this fake sidecar";
    assert!(
        oversized.len() > CEILING,
        "the fixture must be the one the fake refuses"
    );
    let failing = DetectInput::new(failing, Surface::Sync, Budget::new(120, 262_144));
    let oversized = DetectInput::new(oversized, Surface::Sync, Budget::new(120, 262_144));

    for attempt in 1..=2 {
        let result = detector.detect(&failing);
        assert!(
            matches!(result, Err(PiiDetectError::Unavailable { .. })),
            "failure {attempt} must be a stream that failed, got {result:?}"
        );
    }
    assert!(
        matches!(
            detector.detect(&oversized),
            Err(PiiDetectError::InputTooLarge { .. })
        ),
        "the fixture must produce a completed round trip"
    );

    // Two failures, a completed round trip, then two more failures. Three
    // consecutive failures would have opened the breaker; these are not three.
    for attempt in 1..=2 {
        let result = detector.detect(&failing);
        assert!(
            matches!(result, Err(PiiDetectError::Unavailable { .. })),
            "failure after the round trip {attempt} must be a stream that failed, \
             got {result:?}"
        );
        assert!(
            !detector.diagnostics().breaker_open,
            "the round trip must have restarted the count: {attempt} failure(s) since it is not an outage of three"
        );
    }

    // Not vacuous: the third consecutive failure does open it.
    let _ = detector.detect(&failing);
    assert!(
        detector.diagnostics().breaker_open,
        "three consecutive failures must still open a breaker of three"
    );
}

#[test]
fn a_scan_reported_without_a_span_list_never_ends_the_run_of_failures() {
    // A round trip the client *refuses* is not the detector working. A sidecar that
    // answers a well-framed body that reports a scan with no span list behind it is
    // as broken as one that never answers, and it must open the breaker on the same
    // count: if the refusal ends the run of failures, a sidecar stuck in that answer
    // reports itself healthy forever while scanning nothing.
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        Reply::Json(json!({"id": body["id"], "status": "ok"}))
    });
    let mut cfg = config(fake.path());
    cfg = cfg.with_breaker(3, 30);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    for attempt in 1..=2 {
        let result = detector.detect(&input("a body"));
        assert!(
            matches!(result, Err(PiiDetectError::Protocol { .. })),
            "refusal {attempt} must be a protocol error, got {result:?}"
        );
        assert!(
            !detector.diagnostics().breaker_open,
            "two refusals do not open a breaker of three"
        );
    }

    let third = detector.detect(&input("a body"));
    assert!(
        matches!(third, Err(PiiDetectError::Protocol { .. })),
        "the refusal before the threshold is still a protocol error, got {third:?}"
    );
    let diagnostics = detector.diagnostics();
    assert!(
        diagnostics.breaker_open,
        "three consecutive refusals must open a breaker of three"
    );
    assert_eq!(
        diagnostics.breaker_openings, 1,
        "the outage is counted once"
    );
    // The refusal is not silently read as a scan that found nothing.
    assert_eq!(
        fake.requests(),
        3,
        "every call must have reached the sidecar and been refused"
    );
}

#[test]
fn a_span_list_with_reversed_offsets_never_ends_the_run_of_failures() {
    // The same argument one layer further in: the frame is well formed and the span
    // list is present, so the answer looks complete — but an offset pair this client
    // refuses is not an answer it may read as the detector working. A sidecar
    // emitting a reversed pair on every request must open the breaker, not clear it.
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        Reply::Json(json!({
            "id": body["id"],
            "spans": [{"label": "EMAIL", "start": 10, "end": 4, "score": 0.9}],
            "status": "ok",
        }))
    });
    let mut cfg = config(fake.path());
    cfg = cfg.with_breaker(3, 30);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    for attempt in 1..=2 {
        let result = detector.detect(&input("a body"));
        assert!(
            matches!(result, Err(PiiDetectError::Protocol { .. })),
            "refusal {attempt} must be a protocol error, got {result:?}"
        );
        assert!(
            !detector.diagnostics().breaker_open,
            "two refusals do not open a breaker of three"
        );
    }

    let third = detector.detect(&input("a body"));
    assert!(
        matches!(third, Err(PiiDetectError::Protocol { .. })),
        "the refusal before the threshold is still a protocol error, got {third:?}"
    );
    assert!(
        detector.diagnostics().breaker_open,
        "three consecutive refusals must open a breaker of three"
    );
}

// ---------------------------------------------------------------------------
// Windows, labels and the floor
// ---------------------------------------------------------------------------

#[test]
fn an_entity_across_two_windows_yields_one_span() {
    const TOKEN: &str = "across-a-window-boundary";
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        let spans = match body["body"].as_str().and_then(|text| text.find(TOKEN)) {
            Some(start) => vec![json!({
                "label": "IBAN",
                "start": start,
                "end": start + TOKEN.len(),
                "score": 0.9,
            })],
            // A window that only holds part of the entity reports nothing: the
            // overlap is what puts the whole entity in another window.
            None => Vec::new(),
        };
        Reply::Spans(spans)
    });
    let mut cfg = config(fake.path());
    cfg = cfg.with_max_input_bytes(32).with_window_overlap_bytes(16);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    let body = format!("{}{}{}", " ".repeat(21), TOKEN, " ".repeat(30));
    let detection = detector.detect(&input(&body)).expect("a scan");

    assert!(
        fake.requests() > 1,
        "the fixture must be cut into several windows, got {} request(s)",
        fake.requests()
    );
    assert_eq!(
        detection.spans.len(),
        1,
        "an entity seen by two windows is one entity, got {:?}",
        detection.spans
    );
    assert_eq!(detection.spans[0].start, 21);
    assert_eq!(detection.spans[0].len, TOKEN.len());
    assert_eq!(detection.spans[0].category, PiiCategory::AccountNumber);
}

#[test]
fn a_span_sitting_on_a_window_cut_is_counted() {
    let fake = FakeSidecar::start(handshake(), |_, request, body| {
        // A detector that reports the whole window it was handed. The category
        // alternates so that two consecutive windows — which overlap — do not fuse
        // into one long span: what survives the merge still sits on the cuts.
        let len = body["body"].as_str().map_or(0, str::len);
        let label = if request % 2 == 1 { "STREET" } else { "PHONE" };
        Reply::Spans(vec![json!({
            "label": label,
            "start": 0,
            "end": len,
            "score": 0.6,
        })])
    });
    let mut cfg = config(fake.path());
    cfg = cfg.with_max_input_bytes(32).with_window_overlap_bytes(16);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    let body = "x".repeat(200);
    let detection = detector.detect(&input(&body)).expect("a scan");

    assert!(fake.requests() > 1, "the fixture must be cut into windows");
    assert!(
        !detection.spans.is_empty(),
        "the scripted spans must survive the merge"
    );
    assert!(
        detector.diagnostics().window_edge_spans > 0,
        "a span ending on a cut is the under-detection signal this counter carries"
    );
}

#[test]
fn an_unknown_label_is_counted_and_never_mapped() {
    let fake = FakeSidecar::start(handshake(), |_, _, _| {
        Reply::Spans(vec![
            json!({"label": "PASSPORT", "start": 0, "end": 3, "score": 0.9}),
            json!({"label": "PASSPORT", "start": 5, "end": 8, "score": 0.9}),
        ])
    });
    let detector = connect(&fake);

    let detection = detector
        .detect(&input("abc de fgh"))
        .expect("a scan that completed");

    assert!(
        detection.spans.is_empty(),
        "an unmapped label must not be mapped by resemblance, got {:?}",
        detection.spans
    );
    assert_eq!(
        detection.unmapped_labels, 2,
        "each dropped span is counted, so a model change is visible"
    );
}

#[test]
fn a_regex_and_a_model_span_on_one_email_fuse_into_a_single_span() {
    let fake = FakeSidecar::start(handshake(), |_, _, body| {
        let start = body["body"]
            .as_str()
            .and_then(|text| text.find("alice@example.com"))
            .unwrap_or(0);
        Reply::Spans(vec![json!({
            "label": "EMAIL",
            "start": start,
            "end": start + "alice@example.com".len(),
            "score": 0.7,
        })])
    });
    let detector = connect(&fake);

    let detection = detector
        .detect(&input("contact alice@example.com now"))
        .expect("a scan");

    assert_eq!(
        detection.spans.len(),
        1,
        "the floor and the model agree on one entity, got {:?}",
        detection.spans
    );
    assert_eq!(detection.spans[0].category, PiiCategory::Email);
    assert_eq!(detection.spans[0].start, 8);
    assert_eq!(detection.spans[0].len, "alice@example.com".len());
    assert_eq!(
        detection.spans[0].score, 1.0,
        "an exact floor match keeps the fused span out of the model threshold"
    );
}

// ---------------------------------------------------------------------------
// The call-wide deadline
// ---------------------------------------------------------------------------

#[test]
fn one_deadline_bounds_the_whole_call_and_not_each_window() {
    // Three windows or more, a sidecar that answers each one slowly, and a caller
    // budget that a single window can meet. A per-window allowance would let the
    // call spend the budget once per window; the deadline is call-wide, so the
    // first window that cannot be answered inside the remaining time ends the call
    // — and the windows after it are never sent.
    const DELAY: Duration = Duration::from_millis(250);
    const BUDGET_MS: u64 = 400;

    let fake = FakeSidecar::start(handshake(), |_, _, _| Reply::Slow(DELAY));
    let mut cfg = config(fake.path());
    // The proportional term is zeroed so a window's own allowance is its floor
    // alone: what bounds this call is the caller's budget, and only it.
    cfg = cfg
        .with_max_input_bytes(32)
        .with_window_overlap_bytes(16)
        .with_timeout_ms_sync_per_kib(0);
    let detector = SidecarDetector::connect(cfg).expect("a valid configuration");

    let body = "z".repeat(200);
    let windows = {
        let plan = gradatum_privacy::WindowPlan::new(32, 16);
        plan.windows(&body).len()
    };
    assert!(
        windows >= 3,
        "the fixture must be cut into at least three windows, got {windows}"
    );

    let started = Instant::now();
    let result = detector.detect(&DetectInput::new(
        &body,
        Surface::Sync,
        Budget::new(BUDGET_MS, 262_144),
    ));
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(PiiDetectError::Timeout { .. })),
        "a sidecar slower than the call's allowance must time out, got {result:?}"
    );
    assert!(
        fake.requests() < windows as u64,
        "the call must be abandoned at the deadline, not paid once per window: \
         {} request(s) for {windows} window(s)",
        fake.requests()
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "the whole call must stay inside one allowance plus one window's delay, \
         took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Configuration and identity
// ---------------------------------------------------------------------------

#[test]
fn the_configuration_defaults_are_documented_and_usable() {
    let fake = FakeSidecar::start(handshake(), reply_empty);

    // The only three values a deployment must state; everything else has a
    // documented default that a sidecar speaking this protocol accepts.
    let cfg = SidecarConfig::new(fake.path(), BINARY_DIGEST, MODEL_DIGEST);
    let detector = SidecarDetector::connect(cfg).expect("the defaults must mount");
    detector.detect(&input("a body")).expect("a scan");

    assert_eq!(fake.refused(), 0);
    assert_eq!(
        detector
            .identity()
            .expect("a configured detector")
            .score_mode,
        SCORE_MODE,
        "the default score mode is the one the protocol defines"
    );
}

#[test]
fn each_setter_takes_effect() {
    // The ceiling and the overlap decide how a body is cut: a long body under a
    // narrow ceiling reaches the sidecar more than once, and under a wide one it
    // does not. The setter is not decorative.
    let fake = FakeSidecar::start(handshake(), |_, _, _| Reply::Spans(Vec::new()));
    let narrow = SidecarDetector::connect(
        SidecarConfig::new(fake.path(), BINARY_DIGEST, MODEL_DIGEST)
            .with_score_mode(SCORE_MODE)
            .with_max_input_bytes(32)
            .with_window_overlap_bytes(16),
    )
    .expect("a valid configuration");
    narrow
        .detect(&input(&"q".repeat(100)))
        .expect("a scan of a cut body");
    assert!(
        fake.requests() > 1,
        "the ceiling setter must decide the cut, got {} request(s)",
        fake.requests()
    );

    // The score-mode setter is recorded in the identity the pass persists.
    let wide = SidecarDetector::connect(
        SidecarConfig::new(fake.path(), BINARY_DIGEST, MODEL_DIGEST)
            .with_score_mode("max_token_prob")
            .with_max_input_bytes(262_144),
    )
    .expect("a valid configuration");
    assert_eq!(
        wide.identity().expect("a configured detector").score_mode,
        "max_token_prob"
    );

    // The breaker setter: a breaker of one opens on the first failure, which the
    // default of three would not.
    let silent = FakeSidecar::start(handshake(), |_, _, _| Reply::Silent);
    let impatient = SidecarDetector::connect(
        SidecarConfig::new(silent.path(), BINARY_DIGEST, MODEL_DIGEST)
            .with_score_mode(SCORE_MODE)
            .with_breaker(1, 30),
    )
    .expect("a valid configuration");
    let _ = impatient.detect(&input("a body"));
    assert!(
        impatient.diagnostics().breaker_open,
        "a breaker of one must open on the first failure"
    );
}

#[test]
fn identity_reports_the_configured_digests_and_the_table_version() {
    let fake = FakeSidecar::start(handshake(), reply_empty);
    let detector = connect(&fake);

    let identity = detector
        .identity()
        .expect("a configured detector names itself");

    assert_eq!(identity.model_digest, MODEL_DIGEST);
    assert_eq!(identity.sidecar_v, BINARY_DIGEST);
    assert_eq!(identity.score_mode, SCORE_MODE);
    assert_eq!(identity.mapping_v, ADAPTER_TABLE_V);
    assert!(!identity.detector.is_empty());
}

#[test]
fn a_configuration_that_could_scan_nothing_is_refused() {
    let fake = FakeSidecar::start(handshake(), reply_empty);

    let empty_path = SidecarConfig::new(std::path::PathBuf::new(), BINARY_DIGEST, MODEL_DIGEST);
    assert!(
        matches!(
            SidecarDetector::connect(empty_path),
            Err(PiiDetectError::Unavailable { .. })
        ),
        "an empty socket path must be refused rather than silently scanning nothing"
    );

    let no_ceiling = SidecarConfig::new(fake.path(), BINARY_DIGEST, MODEL_DIGEST)
        .with_max_input_bytes(0)
        .with_breaker(3, 30);
    assert!(
        matches!(
            SidecarDetector::connect(no_ceiling),
            Err(PiiDetectError::Unavailable { .. })
        ),
        "a zero per-call ceiling admits no window at all"
    );

    let no_breaker = SidecarConfig::new(fake.path(), BINARY_DIGEST, MODEL_DIGEST)
        .with_max_input_bytes(1_024)
        .with_breaker(0, 30);
    assert!(
        matches!(
            SidecarDetector::connect(no_breaker),
            Err(PiiDetectError::Unavailable { .. })
        ),
        "a breaker that opens on zero failures would never let a scan through"
    );

    let unpinned = SidecarConfig::new(fake.path(), String::new(), MODEL_DIGEST);
    assert!(
        matches!(
            SidecarDetector::connect(unpinned),
            Err(PiiDetectError::Unavailable { .. })
        ),
        "a detector that cannot verify the sidecar's build must not be mounted"
    );
}
