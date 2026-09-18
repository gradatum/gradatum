//! Client for the out-of-process personal-data detector.
//!
//! The heavy model runs in a process of its own, reached over a unix socket: the
//! workspace carries no in-process foreign-function edge, and the model's memory is
//! not the server's. This module is the client side of that arrangement — the
//! frame format, the handshake, the circuit breaker and the windowed scan. It
//! launches nothing and reads no configuration file: a [`SidecarConfig`] is handed
//! to [`SidecarDetector::connect`] by whoever mounts the detector.
//!
//! # The wire
//!
//! One frame is a big-endian `u32` byte length followed by that many bytes of
//! UTF-8 JSON. The sidecar speaks first: on every accepted connection it announces
//! a handshake naming the build it runs, the model it loaded, a digest over that
//! model's label vocabulary, its own label-renumbering revision, its score
//! aggregation mode and whether it is ready. The digests and the score mode are
//! compared against what the configuration pins; the revision is checked against a
//! floor and the label digest is read without being compared, both because they
//! describe the sidecar's own labelling rather than this client's. A handshake that
//! diverges, or a sidecar that is not ready, leaves the detector **unavailable**: it
//! is not a per-call hiccup, so it is latched once and no request is sent afterwards.
//!
//! Every request is `{"id": …, "body": …}` and every answer
//! `{"id": …, "spans": [{"label", "start", "end", "score"}], "status"}`. The
//! identifier is a string, and it is the guard against cross-talk: an answer
//! carrying an identifier other than the one that was sent is refused. The detected
//! value itself never crosses this boundary in either direction — the sidecar is not
//! asked to return it and the client has no field to hold it — so no error message,
//! and nothing a caller could log, can carry a body's text. An unknown extra field
//! in an answer is ignored rather than refused.
//!
//! # Two versions, two notions
//!
//! The sidecar announces a *renumbering revision* — the protocol's own value, which
//! the deployment that shipped it prints as `0` because it renumbers nothing. This
//! client applies a *label-to-category table*, whose revision is
//! [`ADAPTER_TABLE_V`]. The two are not the same number and neither pins the other:
//! the revision is checked against [`SUPPORTED_MAPPING_V_MIN`], and the table
//! revision is what the detector records in its identity. Comparing them for
//! equality, as an earlier revision of this client did, refuses every sidecar whose
//! own numbering moved for reasons this table absorbs.
//!
//! # One allowance for one call
//!
//! A call cuts its body into windows and sends each one; the allowance it was given
//! covers the call, not each window. The deadline is computed once, from the body's
//! length and the caller's budget, and every window is given whatever remains of it
//! — never a fresh allowance of its own. A per-window allowance would let a body cut
//! into twenty windows spend the caller's budget twenty times over, which is the
//! opposite of what a budget is for. A deadline that has passed between two windows
//! ends the call there, without sending the window.
//!
//! # Failure is not silence
//!
//! `Ok` of an empty span list says a scan ran and found nothing; `Err` says the
//! scan did not complete. The two are never interchangeable, and the four ways a
//! call can fail are kept apart because the caller's policy differs for each:
//!
//! * [`PiiDetectError::Timeout`] — the allowance ran out. Counted by the breaker.
//! * [`PiiDetectError::Unavailable`] — unreachable, unusable, or the breaker is
//!   open. Counted by the breaker.
//! * [`PiiDetectError::Protocol`] — the sidecar answered something the format does
//!   not allow: a frame over the cap, a body that is not JSON, an answer carrying
//!   another request's identifier. Counted by the breaker.
//! * [`PiiDetectError::InputTooLarge`] — a size state, not a fault. **Excluded**
//!   from the breaker: counting it would let three ordinary large writes trip a
//!   breaker that then marks every write unscanned.
//!
//! # One connection, one call at a time
//!
//! The sidecar holds a single model context and its classification is serialized
//! there, so a connection pool would buy nothing. The client keeps one connection
//! behind a mutex and sends one request at a time; a stream that fails is dropped —
//! its position is unknown — and the request is retried once on a fresh connection,
//! which is also where a recycled connection is recovered.
//!
//! The trait this implements is synchronous by contract. A caller on an
//! asynchronous path hands the call to a blocking pool; the detector itself never
//! spawns, so it holds no runtime.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gradatum_core::pii::{
    DetectInput, Detection, DetectorIdentity, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
};
use serde::{Deserialize, Serialize};

use crate::merge::{rebase, validate_and_merge};
use crate::regex_detector::RegexDetector;
use crate::windowing::WindowPlan;

/// Number of bytes of the big-endian length prefix that opens every frame.
const FRAME_PREFIX_BYTES: usize = 4;

/// Largest answer frame this client accepts, in bytes.
///
/// A frame declaring more is refused on its length alone — no body byte is read and
/// none is allocated — so a hostile or broken sidecar cannot make the client
/// reserve memory on demand.
pub const MAX_RESPONSE_FRAME_BYTES: usize = 65_536;

/// Floor of one allowance, in milliseconds.
///
/// The proportional term is an estimate, and an estimate of nearly nothing is still
/// a call that has to open a socket, hand over a body and wait for a model. The
/// floor keeps a short body from being cut off before it is plausibly answered. It
/// is the floor of the call's allowance and of each window's share of it, because
/// the two are the same kind of estimate.
const MIN_CALL_TIMEOUT_MS: u64 = 2_000;

/// Ceiling of one allowance, in milliseconds.
///
/// An allowance is turned into an instant by adding it to the clock, so a caller
/// stating an absurd budget must not be able to reach the arithmetic edge. A day is
/// orders of magnitude above any allowance a scan of one body can need, so this
/// ceiling never shortens a real call — it exists to keep the sum representable.
const MAX_CALL_TIMEOUT_MS: u64 = 86_400_000;

/// Lowest label-mapping revision this client can read.
///
/// The sidecar announces its own renumbering revision, and the protocol types it as
/// a signed integer: a revision below this floor is one this client cannot
/// reconcile, and it is refused. Below the floor is not "older", it is "not the same
/// labelling".
pub const SUPPORTED_MAPPING_V_MIN: i64 = 0;

/// Version of the label-to-category table this client applies to the labels a
/// sidecar reports.
///
/// This is *this* client's revision, not the sidecar's: it moves when the table
/// below moves — a label mapped to a different category, a label added to it, a
/// label moved onto the list of those it deliberately does not map. The sidecar's
/// own revision is a different number, checked against [`SUPPORTED_MAPPING_V_MIN`]
/// and reported by [`SidecarDiagnostics`].
pub const ADAPTER_TABLE_V: u16 = 2;

/// Name this detector records in its identity.
const DETECTOR_NAME: &str = "sidecar-unix";

// Reasons are fixed strings. A reason that interpolated a digest, a path or a body
// would put a value into a message a caller may log; none of these do.

const SIDECAR_UNREACHABLE: &str = "the sidecar socket could not be reached";
const HANDSHAKE_UNREADABLE: &str = "the sidecar handshake is not readable";
const HANDSHAKE_NOT_READY: &str = "the sidecar declares itself not ready";
const HANDSHAKE_BINARY_DIVERGED: &str = "the sidecar build is not the pinned one";
const HANDSHAKE_MODEL_DIVERGED: &str = "the sidecar model is not the pinned one";
const HANDSHAKE_MAPPING_UNREADABLE: &str =
    "the sidecar label mapping revision is below the one this client can read";
const HANDSHAKE_SCORE_MODE_DIVERGED: &str = "the sidecar score mode is not the pinned one";
const BREAKER_OPEN: &str = "the detector circuit breaker is open";
const RESPONSE_FRAME_TOO_LARGE: &str = "the answer frame exceeds the accepted size";
const RESPONSE_NOT_READABLE: &str = "the answer is not a readable frame";
const RESPONSE_ID_MISMATCH: &str = "the answer carries another request's identifier";
const RESPONSE_WITHOUT_SPANS: &str = "the answer reports a scan without a span list";
const SPAN_OFFSETS_REVERSED: &str = "a span ends before it starts";
const REQUEST_NOT_ENCODABLE: &str = "the request could not be encoded";
const REQUEST_TOO_LONG: &str = "the request does not fit a frame";
const STREAM_FAILED: &str = "the sidecar stream failed";

/// How a sidecar detector is reached and what it is expected to be.
///
/// Every digest and the score mode are pins, not hints: a sidecar that does not
/// match them is left unused rather than trusted. A configuration that leaves one
/// of them empty describes no verifiable detector, and [`SidecarDetector::connect`]
/// refuses it.
///
/// Three values have no safe default and are the arguments of
/// [`new`](Self::new): the socket, and the two digests an operator reads off the
/// artefacts. The rest are set by chained setters, each of which names what it
/// bounds.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Path of the unix socket the sidecar listens on.
    pub socket_path: PathBuf,
    /// Digest of the sidecar build this deployment expects.
    pub binary_digest: String,
    /// Digest of the model weights this deployment expects.
    pub model_digest: String,
    /// Aggregation mode of the reported scores this deployment expects.
    pub score_mode: String,
    /// Per-kilobyte term of a synchronous call's allowance, in milliseconds.
    ///
    /// Zero means the allowance is its floor alone; the value is calibrated on a
    /// measurement of the deployed model, never guessed from a note's token count.
    pub timeout_ms_sync_per_kib: u64,
    /// Largest body a single call may hand to the sidecar, in bytes.
    ///
    /// A larger body is cut into overlapping windows rather than truncated or
    /// refused, so this is the frame ceiling of one call and not a limit on what
    /// can be scanned.
    pub max_input_bytes: usize,
    /// Overlap between two consecutive windows, in bytes.
    ///
    /// It should be at least the longest entity the model reports. An entity longer
    /// than the overlap is cut at every boundary it crosses, which is a known
    /// under-detection rather than an error — the count of spans found sitting on a
    /// cut is what makes it visible (see [`SidecarDiagnostics::window_edge_spans`]).
    pub window_overlap_bytes: usize,
    /// Consecutive failures attributable to the detector that open the breaker.
    pub breaker_n: u32,
    /// How long the breaker stays open before a probe is allowed, in seconds.
    pub cooldown_s: u64,
}

/// The aggregation mode the protocol defines, and the default of
/// [`SidecarConfig::new`].
const DEFAULT_SCORE_MODE: &str = "mean_token_prob";

/// The per-call ceiling the protocol defines as its own default, in bytes.
const DEFAULT_MAX_INPUT_BYTES: usize = 262_144;

/// The per-kilobyte term [`SidecarConfig::new`] starts from, in milliseconds.
///
/// A starting calibration rather than a measured one for a specific host: a small
/// body is governed by the floor of the call allowance in any case, so an
/// operator calibrates this from a measurement of the deployed model and the
/// floor keeps the uncalibrated value from cutting a short body short.
const DEFAULT_TIMEOUT_MS_SYNC_PER_KIB: u64 = 800;

/// The window overlap [`SidecarConfig::new`] starts from, in bytes.
const DEFAULT_WINDOW_OVERLAP_BYTES: usize = 512;

/// Consecutive failures [`SidecarConfig::new`] lets pass before opening.
const DEFAULT_BREAKER_N: u32 = 3;

/// How long the breaker stays open by default, in seconds.
const DEFAULT_COOLDOWN_S: u64 = 30;

impl SidecarConfig {
    /// Builds a configuration for the sidecar at `socket_path`, pinning
    /// `binary_digest` and `model_digest`.
    ///
    /// The three arguments are the ones a deployment cannot guess: where the
    /// sidecar listens, and the two digests — read off the artefacts it will run —
    /// that decide whether it is trusted at all. Everything else starts from a
    /// value the protocol defines or a starting calibration, and is narrowed by the
    /// setters below.
    ///
    /// # Defaults
    ///
    /// | Field | Default | Why |
    /// |---|---|---|
    /// | `score_mode` | `mean_token_prob` | the mode the protocol defines |
    /// | `timeout_ms_sync_per_kib` | `800` | a starting calibration; the call's floor governs a short body |
    /// | `max_input_bytes` | `262144` | the ceiling the protocol defines as its own default |
    /// | `window_overlap_bytes` | `512` | a starting overlap, to be set to the longest entity the model reports |
    /// | `breaker_n` | `3` | a starting threshold |
    /// | `cooldown_s` | `30` | a starting cooldown |
    #[must_use]
    pub fn new(
        socket_path: impl Into<PathBuf>,
        binary_digest: impl Into<String>,
        model_digest: impl Into<String>,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            binary_digest: binary_digest.into(),
            model_digest: model_digest.into(),
            score_mode: DEFAULT_SCORE_MODE.to_owned(),
            timeout_ms_sync_per_kib: DEFAULT_TIMEOUT_MS_SYNC_PER_KIB,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            window_overlap_bytes: DEFAULT_WINDOW_OVERLAP_BYTES,
            breaker_n: DEFAULT_BREAKER_N,
            cooldown_s: DEFAULT_COOLDOWN_S,
        }
    }

    /// Sets the aggregation mode this deployment pins.
    #[must_use]
    pub fn with_score_mode(mut self, score_mode: impl Into<String>) -> Self {
        self.score_mode = score_mode.into();
        self
    }

    /// Sets the per-kilobyte term of a call's allowance, in milliseconds.
    #[must_use]
    pub fn with_timeout_ms_sync_per_kib(mut self, timeout_ms_sync_per_kib: u64) -> Self {
        self.timeout_ms_sync_per_kib = timeout_ms_sync_per_kib;
        self
    }

    /// Sets the largest body one call may hand to the sidecar, in bytes.
    #[must_use]
    pub fn with_max_input_bytes(mut self, max_input_bytes: usize) -> Self {
        self.max_input_bytes = max_input_bytes;
        self
    }

    /// Sets the overlap between two consecutive windows, in bytes.
    #[must_use]
    pub fn with_window_overlap_bytes(mut self, window_overlap_bytes: usize) -> Self {
        self.window_overlap_bytes = window_overlap_bytes;
        self
    }

    /// Sets the breaker: the consecutive failures that open it, and how long it
    /// stays open before a probe is allowed.
    #[must_use]
    pub fn with_breaker(mut self, breaker_n: u32, cooldown_s: u64) -> Self {
        self.breaker_n = breaker_n;
        self.cooldown_s = cooldown_s;
        self
    }
}

/// What the detector can say about itself right now.
///
/// These are the readings a health surface reports: whether the detector is usable,
/// whether its breaker is open, how many times it has opened, how much of the
/// scanned corpus sat on a window cut, and what the sidecar announced about its own
/// labelling. The last two are the under-detection signals — a span on a cut is one
/// the overlap, not the model, decided the extent of, and a label-set digest that
/// moved is what a model swap looks like before a span list is read under the wrong
/// table.
///
/// The two announced values are `None` until a handshake has been accepted: no
/// connection has been made, so there is nothing to report.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarDiagnostics {
    /// Whether the detector is usable at all. `false` once a handshake has been
    /// refused; a latched detector answers every call with
    /// [`PiiDetectError::Unavailable`].
    pub available: bool,
    /// Whether the circuit breaker is open, so that calls are refused without
    /// paying the detector's delay.
    pub breaker_open: bool,
    /// Number of times the breaker has opened since the detector was mounted.
    pub breaker_openings: u64,
    /// Number of fused spans found sitting on an interior window cut.
    pub window_edge_spans: u64,
    /// Digest the sidecar announced over its model's label vocabulary, read as the
    /// opaque string it is.
    ///
    /// Reported rather than compared: this client maps label *names*, so a label set
    /// that moved is absorbed by the table where it can be and counted by
    /// [`Detection::unmapped_labels`] where it cannot.
    pub labels_v: Option<String>,
    /// Label-renumbering revision the sidecar announced, checked against
    /// [`SUPPORTED_MAPPING_V_MIN`].
    ///
    /// Distinct from [`ADAPTER_TABLE_V`], which is this client's own table revision.
    pub mapping_v: Option<i64>,
}

/// What the sidecar announced about its own labelling, kept from the accepted
/// handshake.
///
/// Held so that a health surface can read it without a second connection: the
/// handshake is announced once per connection, and nothing re-reads it.
#[derive(Debug, Clone)]
struct Announced {
    labels_v: String,
    mapping_v: i64,
}

/// Detects personal data through a sidecar process over a unix socket.
///
/// The detector is `Send + Sync`: one mutex guards the connection, and calls are
/// serialized through it. A connection is opened on the first call and kept; a
/// stream that fails is dropped and re-established on the next one.
pub struct SidecarDetector {
    cfg: SidecarConfig,
    inner: Mutex<Inner>,
}

/// Everything a call mutates, behind the one mutex.
struct Inner {
    /// The live connection, or `None` when one has to be established.
    stream: Option<UnixStream>,
    /// Consecutive failures and the open window, as one value.
    breaker: Breaker,
    /// Set once the sidecar has declared itself unusable: no further call reaches
    /// the socket, whatever the caller does.
    latched: Option<&'static str>,
    /// Running count of fused spans found sitting on an interior window cut.
    window_edge_spans: u64,
    /// What the sidecar announced about its labelling, once a handshake was
    /// accepted.
    announced: Option<Announced>,
    /// Identifier handed to the last request; the next one takes the following
    /// value. See [`SidecarDetector::next_request_id`].
    last_request_id: u64,
}

/// Consecutive-failure counter and the window during which calls are refused.
///
/// A breaker opens on the transition into failure, not on each failing call while
/// it is open: `openings` counts outages, so a health surface reads it as "how many
/// times did this detector fall over", not as a failure count.
#[derive(Debug, Default)]
struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    openings: u64,
}

impl Breaker {
    /// Whether a call is refused right now. Once the cooldown has elapsed this is
    /// `false`, which is what makes the next call a probe.
    fn is_open(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|until| now < until)
    }

    /// The sidecar answered a complete round trip, in a shape this client accepts.
    ///
    /// This is deliberately the weaker of the two "it worked" statements: a body the
    /// sidecar refused as too large is a completed exchange too, and it says nothing
    /// about the detector being broken. The run of consecutive failures ends here,
    /// because a failure that is separated from the next one by an answer is not
    /// consecutive with it.
    ///
    /// It is recorded only for an answer the client accepts. A frame it refuses — no
    /// span list behind a reported scan, a reversed offset pair — is not an answer
    /// that ends a run of failures: recorded there, it would clear the count on every
    /// request of a sidecar stuck in that malformed answer, which would report itself
    /// healthy while scanning nothing.
    fn record_round_trip(&mut self) {
        self.consecutive_failures = 0;
    }

    /// A call completed: the detector is answering, so the outage is over.
    fn record_success(&mut self) {
        self.record_round_trip();
        self.open_until = None;
    }

    /// A call failed on the detector's side, counted against the breaker.
    ///
    /// A failure while the breaker is already open is a failed probe: the window
    /// restarts and the outage stays the same one.
    fn record_failure(&mut self, now: Instant, threshold: u32, cooldown: Duration) {
        if self.open_until.is_some() {
            self.open_until = Some(now + cooldown);
            return;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= threshold {
            self.consecutive_failures = 0;
            self.open_until = Some(now + cooldown);
            self.openings = self.openings.saturating_add(1);
        }
    }
}

impl SidecarDetector {
    /// Builds a detector for `cfg`, without opening the socket.
    ///
    /// The socket is opened on the first call, so mounting a detector against a
    /// sidecar that is not up yet is not an error: the call that finds it down
    /// reports [`PiiDetectError::Unavailable`], which is what the caller's policy
    /// is written against.
    ///
    /// # Errors
    ///
    /// Returns [`PiiDetectError::Unavailable`] when the configuration could not
    /// describe a usable detector — an empty socket path, an empty pin, a zero
    /// per-call ceiling or a zero breaker threshold. These are refused at mount
    /// because each would otherwise surface as a detector that scans nothing and
    /// reports a clean result.
    pub fn connect(cfg: SidecarConfig) -> Result<Self, PiiDetectError> {
        validate(&cfg)?;
        Ok(Self {
            cfg,
            inner: Mutex::new(Inner {
                stream: None,
                breaker: Breaker::default(),
                latched: None,
                window_edge_spans: 0,
                announced: None,
                last_request_id: 0,
            }),
        })
    }

    /// What the detector can say about itself right now.
    #[must_use]
    pub fn diagnostics(&self) -> SidecarDiagnostics {
        let inner = self.lock();
        SidecarDiagnostics {
            available: inner.latched.is_none(),
            breaker_open: inner.breaker.is_open(Instant::now()),
            breaker_openings: inner.breaker.openings,
            window_edge_spans: inner.window_edge_spans,
            labels_v: inner.announced.as_ref().map(|a| a.labels_v.clone()),
            mapping_v: inner.announced.as_ref().map(|a| a.mapping_v),
        }
    }

    /// The identifier of the next request.
    ///
    /// A counter formatted into the alphabet the protocol admits, rather than the
    /// window's index: the identifier is what tells one request from another, so it
    /// must be distinct across the life of the detector and not merely within one
    /// call — an answer left over from an abandoned call must not be readable as
    /// this call's. A counter of this width does not saturate in the life of a
    /// process.
    fn next_request_id(inner: &mut Inner) -> String {
        inner.last_request_id = inner.last_request_id.saturating_add(1);
        format!("c-{}", inner.last_request_id)
    }

    /// Takes the one lock, tolerating a poisoned one.
    ///
    /// Nothing here panics while the lock is held, so poisoning is not reachable
    /// from this module; tolerating it anyway keeps a panic elsewhere from turning
    /// every later call into a panic. The stream is dropped with it, because a
    /// panic could have interrupted a frame half-written or half-read.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.stream = None;
                guard
            }
        }
    }

    /// The allowance of one call, in milliseconds.
    ///
    /// The detector knows what a body of this size costs it; the caller knows what
    /// the request can afford to wait. The shorter of the two governs, and the
    /// proportional estimate sits on a floor so that a small body is not cut off
    /// before it is plausibly answered. A caller budget of zero states no allowance
    /// and leaves the detector's own estimate in force.
    ///
    /// This is the allowance of the *call*, and every window is served out of it: see
    /// [`SidecarDetector::window_allowance_ms`].
    fn call_allowance_ms(&self, input_bytes: usize, budget_ms: u64) -> u64 {
        let kib = (input_bytes as u64).div_ceil(1024);
        let size_derived = self
            .cfg
            .timeout_ms_sync_per_kib
            .saturating_mul(kib)
            .saturating_add(MIN_CALL_TIMEOUT_MS);
        let allowance = if budget_ms == 0 {
            size_derived
        } else {
            size_derived.min(budget_ms)
        };
        allowance.min(MAX_CALL_TIMEOUT_MS)
    }

    /// What one window may spend, in milliseconds.
    ///
    /// A window may spend what is left of the call, and no more: the estimate for a
    /// window of this size sits on the same floor as the call's, and the two are
    /// capped by what remains of the call's deadline. A window therefore cannot buy
    /// time the call does not have, which is the whole point of the deadline being
    /// computed once.
    ///
    /// Zero means the call has nothing left; no window is sent.
    fn window_allowance_ms(&self, window_bytes: usize, remaining: Duration) -> u64 {
        let left = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        let kib = (window_bytes as u64).div_ceil(1024);
        let size_derived = self
            .cfg
            .timeout_ms_sync_per_kib
            .saturating_mul(kib)
            .saturating_add(MIN_CALL_TIMEOUT_MS);
        size_derived.min(left)
    }

    /// The largest body one window may carry, in bytes.
    ///
    /// The configured ceiling and the caller's, whichever is narrower. A caller that
    /// states a ceiling of its own is narrowing the cut, not merely hoping: the
    /// windows are built to it, so what a refusal reports is the ceiling the window
    /// was actually submitted under.
    fn effective_ceiling(&self, caller_ceiling: usize) -> usize {
        if caller_ceiling == 0 {
            self.cfg.max_input_bytes
        } else {
            self.cfg.max_input_bytes.min(caller_ceiling)
        }
    }

    /// Cuts the body, submits every window, rebases, then validates and fuses.
    ///
    /// The order is normative: a detector answers in the coordinates of the window
    /// it was handed, so offsets are rebased before they are validated — an
    /// unshifted window-relative offset that happens to fall inside the body is a
    /// wrong offset that validation cannot tell from a right one.
    ///
    /// `deadline` bounds the whole call and is the only thing that does: each window
    /// is given what remains of it, and a call that reaches the deadline stops
    /// without sending another window. `call_ms` is the allowance the deadline was
    /// built from, and is what a timeout reports.
    fn scan(
        &self,
        inner: &mut Inner,
        input: &DetectInput<'_>,
        deadline: Instant,
        call_ms: u64,
    ) -> Result<Detection, PiiDetectError> {
        let ceiling = self.effective_ceiling(input.budget.max_input_bytes);
        let plan = WindowPlan::new(ceiling, self.cfg.window_overlap_bytes);
        let windows = plan.windows(input.text);

        // A degenerate plan cuts the body into nothing: no window would be
        // submitted, so none of the body would have been scanned. Reporting that as
        // a clean scan is the one answer that must not be given. `connect` refuses
        // the zero ceiling that produces it, so this is a second line rather than a
        // path — the two are far enough apart to be worth stating twice.
        if windows.is_empty() {
            return Err(PiiDetectError::InputTooLarge {
                len: input.text.len(),
                budget: ceiling,
            });
        }

        // The deterministic floor runs under the model on the same windows: neither
        // engine sees the body whole, and fusing the two per window would fuse
        // half-entities. The floor is a unit type and costs nothing to hold here.
        let floor = RegexDetector::new();
        let mut collected: Vec<PiiSpan> = Vec::new();
        let mut unmapped_labels: u64 = 0;

        for (window_start, window) in windows.iter().copied() {
            // The deadline is checked between windows, not only inside a read: a
            // window the call has no time left to receive an answer for must not be
            // sent, or the sidecar would be paying for a request nobody is listening
            // to.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let window_ms = self.window_allowance_ms(window.len(), remaining);
            if window_ms == 0 {
                return Err(PiiDetectError::Timeout {
                    timeout_ms: call_ms,
                });
            }
            let window_deadline = deadline_after(window_ms);

            let request_id = Self::next_request_id(inner);
            let response = self.classify(inner, &request_id, window, window_deadline, call_ms)?;
            let reported = match (response.status, response.spans) {
                (WireStatus::InputTooLarge, _) => {
                    // The sidecar answered a whole round trip — the connection
                    // worked, the frame was exchanged, the process is alive — so the
                    // run of consecutive failures ends here: a failure recorded
                    // before an answer is not consecutive with one recorded after
                    // it, whatever that answer says about the window itself.
                    inner.breaker.record_round_trip();
                    // The window is what was refused, so the window is what is
                    // reported: the body's length would describe a body the sidecar
                    // never saw, and send an operator looking for a cut that is not
                    // the one that was refused.
                    return Err(PiiDetectError::InputTooLarge {
                        len: window.len(),
                        budget: ceiling,
                    });
                }
                // A window reported as scanned, with no span list behind the
                // report, is refused: an absent list and an empty one read the same
                // way afterwards, and only one of them is a scan that found nothing.
                (WireStatus::Ok, None) => {
                    return Err(PiiDetectError::Protocol {
                        detail: RESPONSE_WITHOUT_SPANS.to_owned(),
                    });
                }
                (WireStatus::Ok, Some(reported)) => {
                    // The offsets are checked before the round trip is recorded, not
                    // after: the record means "the detector answered something this
                    // client accepted", and a frame it refuses wholesale is not
                    // that. Record first and a sidecar emitting the same malformed
                    // frame on every request clears the count each time — it would
                    // report itself healthy forever while scanning nothing.
                    if reported.iter().any(|wire| wire.end < wire.start) {
                        return Err(PiiDetectError::Protocol {
                            detail: SPAN_OFFSETS_REVERSED.to_owned(),
                        });
                    }
                    inner.breaker.record_round_trip();
                    reported
                }
            };

            let mut spans = floor
                .detect(&DetectInput::new(window, input.surface, input.budget))?
                .spans;
            for wire in reported {
                match category_for_label(&wire.label) {
                    Some(category) => spans.push(PiiSpan::new(
                        wire.start,
                        wire.end - wire.start,
                        category,
                        wire.score,
                    )),
                    // A label the table does not carry is dropped, never mapped by
                    // resemblance: the count is the earliest signal that the model
                    // the sidecar loaded is not the one this mapping was written for.
                    None => unmapped_labels = unmapped_labels.saturating_add(1),
                }
            }

            rebase(&mut spans, window_start);
            collected.extend(spans);
        }

        let (spans, _merge_stats) = validate_and_merge(input.text, collected);

        // `validate_and_merge` knows no window layout, so the interior-cut count is
        // filled here, once, by the only holder of the plan — on the fused spans, so
        // that a span an overlapping window completed is not counted as truncated.
        let cuts: Vec<usize> = windows
            .iter()
            .take(windows.len() - 1)
            .map(|(start, window)| start + window.len())
            .collect();
        let on_a_cut = spans
            .iter()
            .filter(|span| cuts.iter().any(|cut| touches_cut(span, *cut)))
            .count();
        inner.window_edge_spans = inner.window_edge_spans.saturating_add(on_a_cut as u64);

        Ok(Detection::new(spans, unmapped_labels))
    }

    /// Sends one window and reads its answer, retrying once on a dead stream.
    fn classify(
        &self,
        inner: &mut Inner,
        id: &str,
        body: &str,
        deadline: Instant,
        call_ms: u64,
    ) -> Result<ClassifyResponse, PiiDetectError> {
        let payload = serde_json::to_vec(&ClassifyRequest { id, body }).map_err(|_| {
            PiiDetectError::Protocol {
                detail: REQUEST_NOT_ENCODABLE.to_owned(),
            }
        })?;

        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.exchange(inner, id, &payload, deadline) {
                Ok(response) => return Ok(response),
                // A stream that died mid-conversation is retried once on a fresh
                // connection: the sidecar may simply have recycled it. A failure
                // that survives the retry is reported, not retried again.
                Err(ExchangeError::Broken) if attempts == 1 => continue,
                Err(error) => return Err(error.into_pii(call_ms)),
            }
        }
    }

    /// One round trip, dropping the connection whatever its outcome.
    ///
    /// The stream's position is unknown after any failure — a frame may be half
    /// written or half read — so reusing it would answer the next request with this
    /// one's reply. It is dropped here, at the single point that knows.
    fn exchange(
        &self,
        inner: &mut Inner,
        id: &str,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<ClassifyResponse, ExchangeError> {
        let outcome = self.exchange_inner(inner, id, payload, deadline);
        if outcome.is_err() {
            inner.stream = None;
        }
        outcome
    }

    fn exchange_inner(
        &self,
        inner: &mut Inner,
        id: &str,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<ClassifyResponse, ExchangeError> {
        if inner.stream.is_none() {
            let mut stream = UnixStream::connect(&self.cfg.socket_path)
                .map_err(|_| ExchangeError::Unavailable(SIDECAR_UNREACHABLE))?;

            let handshake = match read_handshake(&mut stream, deadline) {
                Ok(handshake) => handshake,
                Err(error) => {
                    // A handshake that cannot be read is a statement about the
                    // sidecar, not a transient state: it will say the same thing on
                    // the next call, so it is latched rather than re-read.
                    if matches!(error, ExchangeError::HandshakeRejected(_)) {
                        inner.latched = Some(HANDSHAKE_UNREADABLE);
                    }
                    return Err(error);
                }
            };
            if let Err(reason) = self.check_handshake(&handshake) {
                inner.latched = Some(reason);
                return Err(ExchangeError::HandshakeRejected(reason));
            }
            // Kept from the accepted handshake so that a health surface can report
            // what the sidecar announced without opening a second connection.
            inner.announced = Some(Announced {
                labels_v: handshake.labels_v.clone(),
                mapping_v: handshake.mapping_v,
            });

            inner.stream = Some(stream);
        }

        let stream = inner
            .stream
            .as_mut()
            .ok_or(ExchangeError::Unavailable(SIDECAR_UNREACHABLE))?;

        let length =
            u32::try_from(payload.len()).map_err(|_| ExchangeError::Protocol(REQUEST_TOO_LONG))?;
        write_all(stream, &length.to_be_bytes(), deadline)?;
        write_all(stream, payload, deadline)?;

        let body = read_frame(stream, deadline)?;
        let response: ClassifyResponse = serde_json::from_slice(&body)
            .map_err(|_| ExchangeError::Protocol(RESPONSE_NOT_READABLE))?;
        if response.id != id {
            return Err(ExchangeError::Protocol(RESPONSE_ID_MISMATCH));
        }
        Ok(response)
    }

    /// Compares what the sidecar announced against what this deployment pinned.
    ///
    /// Two things are checked against something other than equality, and both for
    /// the same reason — they describe the sidecar's labelling, not this client's.
    /// The label-set digest is an opaque string and is not compared at all: a model
    /// whose label set moved is visible in [`Detection::unmapped_labels`], which
    /// counts what the table could no longer map. The renumbering revision is
    /// checked against [`SUPPORTED_MAPPING_V_MIN`] rather than against this client's
    /// own table revision, because the two numbers answer different questions and
    /// equality between them is a coincidence, not a contract.
    fn check_handshake(&self, handshake: &Handshake) -> Result<(), &'static str> {
        if !handshake.ready {
            return Err(HANDSHAKE_NOT_READY);
        }
        if handshake.binary_sha256 != self.cfg.binary_digest {
            return Err(HANDSHAKE_BINARY_DIVERGED);
        }
        if handshake.model_sha256 != self.cfg.model_digest {
            return Err(HANDSHAKE_MODEL_DIVERGED);
        }
        if handshake.mapping_v < SUPPORTED_MAPPING_V_MIN {
            return Err(HANDSHAKE_MAPPING_UNREADABLE);
        }
        if handshake.score_mode != self.cfg.score_mode {
            return Err(HANDSHAKE_SCORE_MODE_DIVERGED);
        }
        Ok(())
    }
}

impl PiiDetector for SidecarDetector {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let mut inner = self.lock();

        if let Some(reason) = inner.latched {
            return Err(PiiDetectError::Unavailable {
                reason: reason.to_owned(),
            });
        }
        if inner.breaker.is_open(Instant::now()) {
            return Err(PiiDetectError::Unavailable {
                reason: BREAKER_OPEN.to_owned(),
            });
        }

        // One allowance for the whole call, computed once. Every window is served
        // out of it, so the caller's budget bounds the call and not each of the
        // windows it happens to be cut into.
        let call_ms = self.call_allowance_ms(input.text.len(), input.budget.timeout_ms);
        let deadline = deadline_after(call_ms);
        match self.scan(&mut inner, input, deadline, call_ms) {
            Ok(detection) => {
                inner.breaker.record_success();
                Ok(detection)
            }
            Err(error) => {
                // A body over the per-call ceiling is a size state. Counting it as a
                // failure would let three ordinary large writes open a breaker that
                // then marks every write unscanned — nominal traffic degrading the
                // whole surface is the one outcome the exclusion exists to prevent.
                if !matches!(error, PiiDetectError::InputTooLarge { .. }) {
                    inner.breaker.record_failure(
                        Instant::now(),
                        self.cfg.breaker_n,
                        Duration::from_secs(self.cfg.cooldown_s),
                    );
                }
                Err(error)
            }
        }
    }

    /// The identity this detector records, whether or not the sidecar is reachable.
    ///
    /// It is always present rather than `None`: `None` is the pass-through marker of
    /// a write that runs no pass at all, and a configured detector is not that. The
    /// model's origin is left empty because the socket path is a host path and this
    /// identity is persisted with the spans it produced — the two digests pin the
    /// provenance without naming the deployment.
    ///
    /// The version it records is [`ADAPTER_TABLE_V`], this client's table revision:
    /// the identity names the mapping the spans below it were read under, which is
    /// this client's business. The sidecar's own revision is a different notion and
    /// is reported by [`SidecarDetector::diagnostics`].
    fn identity(&self) -> Option<DetectorIdentity> {
        Some(DetectorIdentity::new(
            DETECTOR_NAME.to_owned(),
            self.cfg.model_digest.clone(),
            String::new(),
            ADAPTER_TABLE_V,
            self.cfg.binary_digest.clone(),
            self.cfg.score_mode.clone(),
        ))
    }
}

/// A request for the spans one window holds.
#[derive(Serialize)]
struct ClassifyRequest<'a> {
    /// Identifier the answer must carry back.
    ///
    /// A string, because that is what the protocol admits: the server validates it
    /// against an alphabet and a length, and drops the connection on anything else.
    id: &'a str,
    /// The window, in its own coordinates.
    body: &'a str,
}

/// What the sidecar answers for one request.
#[derive(Deserialize)]
struct ClassifyResponse {
    /// Identifier of the request this answers.
    ///
    /// Read as the same string that was sent, and compared as such: a numeric field
    /// here would fail to deserialize every answer a conformant sidecar sends.
    id: String,
    /// Spans in the coordinates of the window that was sent.
    ///
    /// Absent is accepted for a window that was not scanned — there is nothing to
    /// report — and refused for a window that was, where an absent list would be
    /// read as a clean scan it never claimed.
    spans: Option<Vec<WireSpan>>,
    /// Whether the window was scanned at all.
    status: WireStatus,
}

/// One span as the sidecar reports it.
///
/// There is deliberately no field for the detected value: the client has nowhere to
/// put it, so it cannot be logged, returned or persisted by accident.
#[derive(Deserialize)]
struct WireSpan {
    /// Model label, uppercase; mapped onto a product category by the table below.
    label: String,
    /// Start offset in bytes, in the window's coordinates.
    start: usize,
    /// End offset in bytes, exclusive, in the window's coordinates.
    end: usize,
    /// Detection confidence in `0.0..=1.0`.
    score: f32,
}

/// Whether a window was scanned.
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WireStatus {
    /// The window was scanned; `spans` holds whatever it found.
    Ok,
    /// The window was not scanned because it exceeds what the sidecar accepts.
    InputTooLarge,
}

/// What a sidecar announces about itself when a connection is accepted.
///
/// The field types are the protocol's, not this client's preferences: the two
/// digests are bare lowercase hex, `labels_v` is a `sha256:`-tagged string, and
/// `mapping_v` is a signed integer. A field typed against the wrong shape does not
/// fail loudly — it fails by refusing every conformant sidecar, which is what an
/// earlier revision of this client did to all three at once.
#[derive(Deserialize)]
struct Handshake {
    binary_sha256: String,
    model_sha256: String,
    /// The sidecar's own label-renumbering revision: signed, and checked against
    /// [`SUPPORTED_MAPPING_V_MIN`] rather than against this client's table.
    mapping_v: i64,
    /// Digest over the model's label vocabulary. Read and reported, never compared
    /// and never parsed: the protocol types it as a string, and its content is the
    /// sidecar's business.
    labels_v: String,
    score_mode: String,
    ready: bool,
}

/// How a round trip failed, before it is turned into a caller-facing error.
///
/// [`HandshakeRejected`](Self::HandshakeRejected) is a separate variant because its
/// lifecycle differs, not its meaning: it is the one failure the detector latches,
/// since a sidecar that announces itself wrongly will do so again.
enum ExchangeError {
    /// The detector cannot be used for this call.
    Unavailable(&'static str),
    /// The sidecar announced a handshake this client will not trust again.
    HandshakeRejected(&'static str),
    /// The allowance ran out.
    Timeout,
    /// The sidecar answered outside the format.
    Protocol(&'static str),
    /// The stream failed while a request was in flight.
    Broken,
}

impl ExchangeError {
    fn into_pii(self, timeout_ms: u64) -> PiiDetectError {
        match self {
            Self::Unavailable(reason) | Self::HandshakeRejected(reason) => {
                PiiDetectError::Unavailable {
                    reason: reason.to_owned(),
                }
            }
            Self::Timeout => PiiDetectError::Timeout { timeout_ms },
            Self::Protocol(detail) => PiiDetectError::Protocol {
                detail: detail.to_owned(),
            },
            // One retry has already been spent on a fresh connection.
            Self::Broken => PiiDetectError::Unavailable {
                reason: STREAM_FAILED.to_owned(),
            },
        }
    }
}

/// Reads the handshake the sidecar announces on a freshly accepted connection.
fn read_handshake(stream: &mut UnixStream, deadline: Instant) -> Result<Handshake, ExchangeError> {
    let body = read_frame(stream, deadline)?;
    serde_json::from_slice(&body)
        .map_err(|_| ExchangeError::HandshakeRejected(HANDSHAKE_UNREADABLE))
}

/// Reads one frame, refusing an over-sized one on its declared length alone.
fn read_frame(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, ExchangeError> {
    let mut prefix = [0u8; FRAME_PREFIX_BYTES];
    read_exact(stream, &mut prefix, deadline)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_RESPONSE_FRAME_BYTES {
        return Err(ExchangeError::Protocol(RESPONSE_FRAME_TOO_LARGE));
    }
    let mut body = vec![0u8; length];
    read_exact(stream, &mut body, deadline)?;
    Ok(body)
}

/// Fills `buffer`, rearming the read timeout against the call's deadline.
///
/// A socket timeout applies to one system call, so arming it once per call would
/// let a sidecar that answers one byte at a time take the allowance over and over.
fn read_exact(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), ExchangeError> {
    let mut filled = 0;
    while filled < buffer.len() {
        arm(stream, deadline, false)?;
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Err(ExchangeError::Broken),
            Ok(read) => filled += read,
            Err(error) => return Err(classify_io(&error)),
        }
    }
    Ok(())
}

/// Writes all of `buffer`, rearming the write timeout against the deadline.
fn write_all(
    stream: &mut UnixStream,
    buffer: &[u8],
    deadline: Instant,
) -> Result<(), ExchangeError> {
    let mut written = 0;
    while written < buffer.len() {
        arm(stream, deadline, true)?;
        match stream.write(&buffer[written..]) {
            Ok(0) => return Err(ExchangeError::Broken),
            Ok(count) => written += count,
            Err(error) => return Err(classify_io(&error)),
        }
    }
    Ok(())
}

/// Arms the timeout of the next system call, or reports the deadline as passed.
fn arm(stream: &UnixStream, deadline: Instant, writing: bool) -> Result<(), ExchangeError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ExchangeError::Timeout);
    }
    let armed = if writing {
        stream.set_write_timeout(Some(remaining))
    } else {
        stream.set_read_timeout(Some(remaining))
    };
    armed.map_err(|_| ExchangeError::Broken)
}

/// Tells a socket timeout apart from a stream that failed.
fn classify_io(error: &io::Error) -> ExchangeError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ExchangeError::Timeout,
        _ => ExchangeError::Broken,
    }
}

/// The instant `allowance_ms` from now.
///
/// A deadline has to be an instant, and an instant is reached by adding a duration
/// to the clock — an operation that has an edge. The allowance is capped well below
/// that edge ([`MAX_CALL_TIMEOUT_MS`]), so the saturating branch is not reachable
/// from any allowance this module computes; it exists so that no arithmetic edge can
/// panic inside a call.
fn deadline_after(allowance_ms: u64) -> Instant {
    let now = Instant::now();
    now.checked_add(Duration::from_millis(allowance_ms))
        .unwrap_or(now)
}

/// Does `span` come within one byte of the cut at `cut`?
///
/// Either end landing on the cut, or one byte away from it, counts: the span is
/// close enough that the overlap, not the detector, decided how much of it was
/// visible. Saturating arithmetic keeps a span that ran off the body from
/// overflowing.
fn touches_cut(span: &PiiSpan, cut: usize) -> bool {
    let end = span.start.saturating_add(span.len);
    span.start <= cut.saturating_add(1) && end.saturating_add(1) >= cut
}

/// Maps a model label onto a product category, or `None` when it has no mapping.
///
/// The table is the versioned contract named by [`ADAPTER_TABLE_V`]. Every label of
/// the model this client was built against is either in it or on the list of the
/// labels the revision deliberately does not map; the test over the model's own
/// vocabulary is what holds that, so a model swap cannot drop a label in silence.
/// A label in neither place is never mapped by resemblance: the caller counts it,
/// which is how a label set that moved becomes visible before its spans are
/// misread.
///
/// It is crate-private because it is the detector's own reading of the wire, not an
/// interface: a caller handed the mapping would be handed the decision of which
/// categories a body's spans carry, and the detector would no longer be the one
/// place where that reading is versioned.
#[must_use]
pub(crate) fn category_for_label(label: &str) -> Option<PiiCategory> {
    Some(match label {
        "EMAIL" => PiiCategory::Email,
        "PHONE" => PiiCategory::Phone,
        "URL" => PiiCategory::Url,
        "DATE" | "DATEOFBIRTH" | "TIME" => PiiCategory::Date,
        "FIRSTNAME" | "LASTNAME" | "MIDDLENAME" | "USERNAME" | "PREFIX" => PiiCategory::Person,
        "IBAN" | "BANKACCOUNT" | "ACCOUNTNAME" | "BIC" | "CREDITCARD" | "SSN" | "VIN" | "IMEI"
        | "BITCOINADDRESS" | "ETHEREUMADDRESS" | "LITECOINADDRESS" => PiiCategory::AccountNumber,
        "PASSWORD" | "PIN" | "CVV" | "MASKEDNUMBER" => PiiCategory::Secret,
        "STREET" | "CITY" | "COUNTY" | "STATE" | "IPADDRESS" | "MACADDRESS" | "GPSCOORDINATES"
        | "SECONDARYADDRESS" | "BUILDINGNUMBER" | "ZIPCODE" => PiiCategory::Address,
        _ => return None,
    })
}

/// Refuses a configuration that could not describe a usable detector.
///
/// Each refusal names a field, never its value: a message carrying the socket path
/// would put a host path where a caller may log it.
fn validate(cfg: &SidecarConfig) -> Result<(), PiiDetectError> {
    let missing = if cfg.socket_path.as_os_str().is_empty() {
        Some("socket path")
    } else if cfg.binary_digest.is_empty() {
        Some("binary digest")
    } else if cfg.model_digest.is_empty() {
        Some("model digest")
    } else if cfg.score_mode.is_empty() {
        Some("score mode")
    } else if cfg.max_input_bytes == 0 {
        Some("per-call ceiling")
    } else if cfg.breaker_n == 0 {
        Some("breaker threshold")
    } else {
        None
    };
    match missing {
        Some(field) => Err(PiiDetectError::Unavailable {
            reason: format!("sidecar configuration is missing its {field}"),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SidecarConfig {
        SidecarConfig::new(
            "/a/socket/that/is/never/opened",
            "sha256:binary",
            "sha256:model",
        )
        .with_score_mode("entity-group")
        .with_timeout_ms_sync_per_kib(800)
        .with_max_input_bytes(262_144)
        .with_window_overlap_bytes(512)
        .with_breaker(3, 30)
    }

    #[test]
    fn the_call_allowance_is_built_from_the_floor_and_bounded_by_the_caller() {
        let detector = SidecarDetector::connect(config()).expect("a valid configuration");
        // The call's own estimate: a floor, plus a per-kilobyte term for the whole
        // body.
        assert_eq!(detector.call_allowance_ms(0, 0), MIN_CALL_TIMEOUT_MS);
        assert_eq!(detector.call_allowance_ms(1_024, 0), 2_000 + 800);
        assert_eq!(detector.call_allowance_ms(3_000, 0), 2_000 + 3 * 800);
        // The caller's allowance is the shorter of the two when it is stated.
        assert_eq!(detector.call_allowance_ms(3_000, 300), 300);
        assert_eq!(detector.call_allowance_ms(3_000, 5_000), 2_000 + 3 * 800);
        // And an absurd one is capped rather than allowed to reach the clock's edge.
        assert_eq!(detector.call_allowance_ms(0, u64::MAX), MIN_CALL_TIMEOUT_MS);
        let huge = 110 * 1024 * 1024;
        assert_eq!(detector.call_allowance_ms(huge, 0), MAX_CALL_TIMEOUT_MS);
        assert_eq!(
            detector.call_allowance_ms(huge, u64::MAX),
            MAX_CALL_TIMEOUT_MS
        );
    }

    #[test]
    fn a_window_may_spend_the_remainder_of_the_call_and_no_more() {
        let detector = SidecarDetector::connect(config()).expect("a valid configuration");
        let generous = Duration::from_secs(60);
        // Its own estimate, when the call has more time left than the window needs.
        assert_eq!(detector.window_allowance_ms(1_024, generous), 2_800);
        // The remainder, when the call has less: a window never buys time the call
        // does not have.
        assert_eq!(
            detector.window_allowance_ms(1_024, Duration::from_millis(40)),
            40
        );
        assert_eq!(detector.window_allowance_ms(1_024, Duration::ZERO), 0);
    }

    #[test]
    fn the_window_ceiling_is_the_narrower_of_the_two() {
        let detector = SidecarDetector::connect(config()).expect("a valid configuration");
        // A caller that states no ceiling of its own leaves the configured one.
        assert_eq!(detector.effective_ceiling(0), 262_144);
        assert_eq!(detector.effective_ceiling(262_144), 262_144);
        // A narrower one narrows the cut, which is what makes a refusal's reported
        // ceiling truthful.
        assert_eq!(detector.effective_ceiling(1_024), 1_024);

        let narrow = SidecarDetector::connect(
            SidecarConfig::new("/a/socket", "sha256:binary", "sha256:model")
                .with_score_mode("entity-group")
                .with_max_input_bytes(64),
        )
        .expect("a valid configuration");
        assert_eq!(narrow.effective_ceiling(0), 64);
        assert_eq!(narrow.effective_ceiling(4_096), 64);
    }

    #[test]
    fn the_breaker_opens_once_on_the_threshold_and_restarts_on_a_failed_probe() {
        let now = Instant::now();
        let cooldown = Duration::from_secs(30);
        let mut breaker = Breaker::default();

        breaker.record_failure(now, 3, cooldown);
        breaker.record_failure(now, 3, cooldown);
        assert!(
            !breaker.is_open(now),
            "two failures do not open a breaker of three"
        );

        breaker.record_failure(now, 3, cooldown);
        assert!(breaker.is_open(now));
        assert_eq!(breaker.openings, 1);

        // The cooldown elapses: the next call is a probe, and it is allowed.
        let after = now + Duration::from_secs(31);
        assert!(
            !breaker.is_open(after),
            "an elapsed cooldown admits a probe"
        );

        // The probe fails: the window restarts, and it is the same outage.
        breaker.record_failure(after, 3, cooldown);
        assert!(breaker.is_open(after));
        assert_eq!(breaker.openings, 1);

        breaker.record_success();
        assert!(!breaker.is_open(after), "a success closes the breaker");
    }

    #[test]
    fn a_completed_round_trip_ends_the_run_of_failures_without_closing_the_breaker() {
        let now = Instant::now();
        let cooldown = Duration::from_secs(30);
        let mut breaker = Breaker::default();

        breaker.record_failure(now, 3, cooldown);
        breaker.record_failure(now, 3, cooldown);
        // A round trip the sidecar answered — including one it answered with a
        // refusal of its own — ends the run, so the next two failures do not add up
        // to an outage of three.
        breaker.record_round_trip();
        breaker.record_failure(now, 3, cooldown);
        breaker.record_failure(now, 3, cooldown);
        assert!(
            !breaker.is_open(now),
            "a round trip between failures makes them no longer consecutive"
        );
        breaker.record_failure(now, 3, cooldown);
        assert!(breaker.is_open(now));
    }

    #[test]
    fn a_span_on_a_cut_is_recognised_on_both_sides() {
        let span = PiiSpan::new(10, 20, PiiCategory::Email, 1.0);
        assert!(touches_cut(&span, 30), "the end lands on the cut");
        assert!(touches_cut(&span, 31), "one byte past the end");
        assert!(touches_cut(&span, 9), "one byte before the start");
        assert!(!touches_cut(&span, 32), "two bytes past the end");
        assert!(!touches_cut(&span, 8), "two bytes before the start");
        // A hostile span that ran off the body does not overflow.
        assert!(touches_cut(
            &PiiSpan::new(usize::MAX - 1, 8, PiiCategory::Email, 1.0),
            usize::MAX
        ));
    }

    // -----------------------------------------------------------------------
    // The label adaptation table
    // -----------------------------------------------------------------------

    /// The model's entity groups, one per line.
    ///
    /// The fixture is the model's own vocabulary — the entity-group names, one per
    /// line, with the BIOES tag prefixes removed. It carries no corpus text: the
    /// names are the model's class labels, and they are what the table is written
    /// against. It sits under `tests/` because it is a fixture rather than source;
    /// the test that reads it lives here because the table it checks is crate-private,
    /// and an integration test is a crate of its own and could not see it.
    const MODEL_GROUPS: &str = include_str!("../tests/fixtures/model-labels.txt");

    /// Groups this revision of the table deliberately does not map.
    ///
    /// These are the groups whose meaning the product's category set does not carry:
    /// quantities, currencies, demographics, job descriptions, the user agent and the
    /// vehicle registration mark. They are *dropped and counted*, which is a decision
    /// written down here — the opposite of a group nobody thought about.
    ///
    /// Adding a category to the product is what would move a line from this list into
    /// the table, and that is a table revision, not an edit.
    const UNMAPPED_GROUPS: &[&str] = &[
        "AGE",
        "AMOUNT",
        "CREDITCARDISSUER",
        "CURRENCY",
        "CURRENCYCODE",
        "CURRENCYNAME",
        "CURRENCYSYMBOL",
        "EYECOLOR",
        "GENDER",
        "HEIGHT",
        "JOBDEPARTMENT",
        "JOBTITLE",
        "OCCUPATION",
        "ORDINALDIRECTION",
        "ORGANIZATION",
        "SEX",
        "USERAGENT",
        "VRM",
    ];

    /// The fixture's groups, in file order.
    fn fixture_groups() -> Vec<&'static str> {
        MODEL_GROUPS
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn every_group_of_the_model_vocabulary_is_mapped_or_listed_as_unmapped() {
        let groups = fixture_groups();
        assert_eq!(
            groups.len(),
            54,
            "the deployed model carries 54 entity groups; the fixture must carry them"
        );

        let mut mapped = 0usize;
        let mut forgotten: Vec<&str> = Vec::new();
        for group in &groups {
            let is_mapped = category_for_label(group).is_some();
            let listed = UNMAPPED_GROUPS.contains(group);
            match (is_mapped, listed) {
                (true, false) => mapped += 1,
                (false, true) => {}
                (false, false) => forgotten.push(group),
                (true, true) => panic!("{group} is both mapped and listed as unmapped"),
            }
        }
        assert!(
            forgotten.is_empty(),
            "these groups are silently dropped: {forgotten:?} — map them or list them as unmapped"
        );
        // The partition, counted rather than asserted in prose: 36 groups reach a
        // category and 18 are deliberately dropped, and the two lists are the whole
        // vocabulary. A group moved from one list to the other without the other
        // moving would break this line before it broke a caller.
        assert_eq!(mapped, 36, "the table maps 36 of the model's groups");
        assert_eq!(
            UNMAPPED_GROUPS.len(),
            18,
            "18 groups are deliberately left unmapped"
        );
        assert_eq!(
            mapped + UNMAPPED_GROUPS.len(),
            groups.len(),
            "mapped and unmapped groups are the whole vocabulary, in two lists"
        );
    }

    #[test]
    fn the_unmapped_list_carries_no_group_the_table_maps_and_no_stranger() {
        use std::collections::BTreeSet;

        let groups: BTreeSet<&str> = fixture_groups().into_iter().collect();
        for listed in UNMAPPED_GROUPS {
            assert!(
                groups.contains(listed),
                "{listed} is listed as unmapped but is not a group of this model"
            );
            assert_eq!(
                category_for_label(listed),
                None,
                "{listed} is listed as unmapped and is mapped anyway"
            );
        }
    }

    #[test]
    fn the_mapped_groups_reach_the_categories_the_table_promises() {
        // Written out rather than derived from the table: a test that read the table
        // it checks would agree with any table at all.
        const EXPECTED: &[(&str, PiiCategory)] = &[
            ("EMAIL", PiiCategory::Email),
            ("PHONE", PiiCategory::Phone),
            ("URL", PiiCategory::Url),
            ("DATE", PiiCategory::Date),
            ("DATEOFBIRTH", PiiCategory::Date),
            ("TIME", PiiCategory::Date),
            ("FIRSTNAME", PiiCategory::Person),
            ("LASTNAME", PiiCategory::Person),
            ("MIDDLENAME", PiiCategory::Person),
            ("USERNAME", PiiCategory::Person),
            ("PREFIX", PiiCategory::Person),
            ("IBAN", PiiCategory::AccountNumber),
            ("BANKACCOUNT", PiiCategory::AccountNumber),
            ("ACCOUNTNAME", PiiCategory::AccountNumber),
            ("BIC", PiiCategory::AccountNumber),
            ("CREDITCARD", PiiCategory::AccountNumber),
            ("SSN", PiiCategory::AccountNumber),
            ("VIN", PiiCategory::AccountNumber),
            ("IMEI", PiiCategory::AccountNumber),
            ("BITCOINADDRESS", PiiCategory::AccountNumber),
            ("ETHEREUMADDRESS", PiiCategory::AccountNumber),
            ("LITECOINADDRESS", PiiCategory::AccountNumber),
            ("PASSWORD", PiiCategory::Secret),
            ("PIN", PiiCategory::Secret),
            ("CVV", PiiCategory::Secret),
            ("MASKEDNUMBER", PiiCategory::Secret),
            ("STREET", PiiCategory::Address),
            ("SECONDARYADDRESS", PiiCategory::Address),
            ("BUILDINGNUMBER", PiiCategory::Address),
            ("CITY", PiiCategory::Address),
            ("COUNTY", PiiCategory::Address),
            ("STATE", PiiCategory::Address),
            ("ZIPCODE", PiiCategory::Address),
            ("IPADDRESS", PiiCategory::Address),
            ("MACADDRESS", PiiCategory::Address),
            ("GPSCOORDINATES", PiiCategory::Address),
        ];

        for (label, expected) in EXPECTED {
            assert_eq!(
                category_for_label(label),
                Some(*expected),
                "{label} must map onto {expected:?}"
            );
        }

        // The mapped set is exactly these: a group the table maps but this list omits
        // would be a mapping nobody reviewed.
        let groups = fixture_groups();
        for group in &groups {
            let mapped = category_for_label(group).is_some();
            let reviewed = EXPECTED.iter().any(|(label, _)| label == group);
            assert_eq!(
                mapped, reviewed,
                "{group}: mapped={mapped} but reviewed={reviewed}"
            );
        }
    }

    #[test]
    fn the_table_revision_is_the_one_this_mapping_was_written_for() {
        assert_eq!(
            ADAPTER_TABLE_V, 2,
            "the table was rebuilt on the model's real vocabulary; that is a new revision"
        );
    }

    #[test]
    fn a_label_outside_the_vocabulary_maps_nowhere() {
        for label in ["PASSPORT", "email", "Email", "", "DRIVERSLICENCE"] {
            assert_eq!(
                category_for_label(label),
                None,
                "{label} must not be mapped"
            );
        }
    }
}
