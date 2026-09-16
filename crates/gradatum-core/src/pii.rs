//! Personal-data detection frontier: the [`PiiDetector`] trait, its budgeted
//! input/output types, and the pass-through [`NoopDetector`].
//!
//! This module defines the shared surface every detector implements. The
//! non-trivial implementations (regex floor detector, sidecar client, model
//! adaptor, span fusion) live outside the persistence layer and are injected at
//! mount time — this crate carries only the contract and the do-nothing default.
//!
//! # Contract
//!
//! A detector reports personal-data [`PiiSpan`]s found in a text body, within a
//! [`Budget`]. Two return values must never be conflated:
//!
//! * `Ok(`empty [`Detection`]`)` — a scan ran and found nothing (or, for
//!   [`NoopDetector`], no scan is configured at all).
//! * `Err(`[`PiiDetectError`]`)` — the scan could not complete.
//!
//! A mute detector and a detector that found nothing are different states: the
//! caller marks the note unscanned on `Err` and leaves it untouched on an empty
//! `Ok`. Collapsing the two would be a pass masquerading as a clean result.

use std::str::FromStr;

/// Product-closed taxonomy of personal-data categories.
///
/// The set is deliberately fixed: each detector maps its own labels onto these
/// eight values, and a label with no mapping is dropped and counted rather than
/// mapped by resemblance.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PiiCategory {
    /// Bank or registration numbers (IBAN, account numbers, formal identifiers).
    AccountNumber,
    /// Postal or network addresses.
    Address,
    /// E-mail addresses.
    Email,
    /// Person names.
    Person,
    /// Telephone numbers.
    Phone,
    /// URLs.
    Url,
    /// Dates.
    Date,
    /// Secrets and opaque tokens.
    Secret,
}

impl PiiCategory {
    /// Returns the canonical `snake_case` label of this category.
    ///
    /// This is the inverse of [`PiiCategory::from_str`] and the wire form written
    /// to the frontmatter `counts` map.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AccountNumber => "account_number",
            Self::Address => "address",
            Self::Email => "email",
            Self::Person => "person",
            Self::Phone => "phone",
            Self::Url => "url",
            Self::Date => "date",
            Self::Secret => "secret",
        }
    }
}

/// Error returned when a string does not name a known [`PiiCategory`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error("unknown PII category label: {0}")]
#[non_exhaustive]
pub struct UnknownPiiCategory(pub String);

impl FromStr for PiiCategory {
    type Err = UnknownPiiCategory;

    /// Parses a `snake_case` label into a [`PiiCategory`].
    ///
    /// # Errors
    ///
    /// Returns [`UnknownPiiCategory`] if `s` is not one of the eight canonical
    /// labels returned by [`PiiCategory::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "account_number" => Self::AccountNumber,
            "address" => Self::Address,
            "email" => Self::Email,
            "person" => Self::Person,
            "phone" => Self::Phone,
            "url" => Self::Url,
            "date" => Self::Date,
            "secret" => Self::Secret,
            other => return Err(UnknownPiiCategory(other.to_owned())),
        })
    }
}

/// The write surface a detection runs on.
///
/// The surface governs the budget a caller allocates: synchronous HTTP paths pay
/// the latency on the request, asynchronous worker paths absorb it in the queue.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Asynchronous paths (worker, `curate`, `distill`).
    Async,
    /// Synchronous HTTP paths (`message_send`, `vault_restore`).
    Sync,
}

/// Resource budget for a single detection call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Wall-clock budget in milliseconds.
    pub timeout_ms: u64,
    /// Maximum input size in bytes a detector will accept for a single call.
    pub max_input_bytes: usize,
}

impl Budget {
    /// Builds a [`Budget`] from a timeout and a maximum input size.
    #[must_use]
    pub const fn new(timeout_ms: u64, max_input_bytes: usize) -> Self {
        Self {
            timeout_ms,
            max_input_bytes,
        }
    }
}

/// A single detected span of personal data.
///
/// Spans carry offsets and a category, never the detected value itself. Offsets
/// are UTF-8 byte offsets, rebased onto the note body.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PiiSpan {
    /// UTF-8 byte offset of the span start, rebased onto the body.
    pub start: usize,
    /// Length of the span in bytes.
    pub len: usize,
    /// Detected category.
    pub category: PiiCategory,
    /// Detection score in memory, in `0.0..=1.0`. Written to the frontmatter as
    /// integer thousandths.
    pub score: f32,
}

impl PiiSpan {
    /// Builds a [`PiiSpan`] from its offset, length, category and score.
    #[must_use]
    pub const fn new(start: usize, len: usize, category: PiiCategory, score: f32) -> Self {
        Self {
            start,
            len,
            category,
            score,
        }
    }
}

/// A detection request: the text, the surface it is written on, and the budget.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct DetectInput<'a> {
    /// Body to scan.
    pub text: &'a str,
    /// Write surface the detection runs on.
    pub surface: Surface,
    /// Resource budget for this call.
    pub budget: Budget,
}

impl<'a> DetectInput<'a> {
    /// Builds a [`DetectInput`] from a text body, a surface and a budget.
    #[must_use]
    pub const fn new(text: &'a str, surface: Surface, budget: Budget) -> Self {
        Self {
            text,
            surface,
            budget,
        }
    }
}

/// The result of a completed detection.
///
/// An empty `spans` vector with `unmapped_labels == 0` is a clean scan that found
/// nothing — never the same value as a [`PiiDetectError`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// Detected spans, sorted by start offset.
    pub spans: Vec<PiiSpan>,
    /// Count of detector labels that had no mapping onto [`PiiCategory`] and were
    /// dropped. A non-zero value is the early signal of a model change.
    pub unmapped_labels: u64,
}

impl Detection {
    /// Builds a [`Detection`] from its spans and its count of unmapped labels.
    #[must_use]
    pub fn new(spans: Vec<PiiSpan>, unmapped_labels: u64) -> Self {
        Self {
            spans,
            unmapped_labels,
        }
    }
}

/// Error raised when a detection cannot complete.
///
/// Each variant is distinguishable from an empty [`Detection`]: an `Err` marks
/// the note `privacy-unscanned`, an empty `Ok` marks nothing.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum PiiDetectError {
    /// The detector did not answer within its budget.
    #[error("detector timed out after {timeout_ms} ms")]
    Timeout {
        /// Budget that was exceeded, in milliseconds.
        timeout_ms: u64,
    },
    /// The detector is not reachable (down, misconfigured, circuit open).
    #[error("detector unavailable: {reason}")]
    Unavailable {
        /// Human-readable reason.
        reason: String,
    },
    /// The input is larger than the detector's per-call byte budget.
    #[error("input of {len} bytes exceeds the {budget} byte budget")]
    InputTooLarge {
        /// Actual input size in bytes.
        len: usize,
        /// Configured maximum in bytes.
        budget: usize,
    },
    /// The sidecar transport returned a malformed or unexpected frame.
    #[error("sidecar protocol error: {detail}")]
    Protocol {
        /// Detail of the protocol violation.
        detail: String,
    },
}

/// What a detector declares about itself, for the record.
///
/// A span list with no provenance cannot be re-read later: the same offsets mean
/// different things under a different model, a different label mapping or a
/// different aggregation mode. These are the fields a persisted block records so
/// that a pass stays interpretable after the fact — the two digests the pass itself
/// computes (its policy fingerprint and its span digest) are deliberately not here.
///
/// The fields are descriptive, never instructional: nothing reads them back to
/// decide anything.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorIdentity {
    /// Detector identifier, as it should be recorded.
    pub detector: String,
    /// Digest of the model file (`sha256:…`); empty when the detector has no model.
    pub model_digest: String,
    /// Where the model came from (a hub reference, a build identity); empty when
    /// the detector has no model.
    pub model_source: String,
    /// Version of the detector's label mapping onto [`PiiCategory`].
    pub mapping_v: u16,
    /// Build identity of the process hosting the detector; empty for an in-process
    /// detector.
    pub sidecar_v: String,
    /// Aggregation mode of the reported scores.
    pub score_mode: String,
}

impl DetectorIdentity {
    /// Builds a [`DetectorIdentity`] from its six descriptive fields.
    #[must_use]
    pub fn new(
        detector: String,
        model_digest: String,
        model_source: String,
        mapping_v: u16,
        sidecar_v: String,
        score_mode: String,
    ) -> Self {
        Self {
            detector,
            model_digest,
            model_source,
            mapping_v,
            sidecar_v,
            score_mode,
        }
    }

    /// The identity of a detector that declares none, and the identity any
    /// detector that does not override [`PiiDetector::identity`] carries.
    ///
    /// It is unattributed rather than absent: a pass that ran is on the record even
    /// when the detector cannot name itself. Only [`NoopDetector`] reports no
    /// identity at all, which is a different statement — no detection is configured.
    #[must_use]
    pub fn unattributed() -> Self {
        Self {
            detector: "unattributed".to_owned(),
            model_digest: String::new(),
            model_source: String::new(),
            mapping_v: 1,
            sidecar_v: String::new(),
            score_mode: "unattributed".to_owned(),
        }
    }
}

/// Detects personal-data spans in a text body, within a budget.
///
/// One detection entry point — [`detect`](Self::detect) — and one descriptive
/// method. Any extension of this surface is a deliberate decision: the identity is
/// here because a persisted span list must be re-readable, not as a second way in.
pub trait PiiDetector: Send + Sync {
    /// Scans `input.text` and returns the detected spans.
    ///
    /// # Errors
    ///
    /// Returns a [`PiiDetectError`] when the scan cannot complete (timeout,
    /// unavailable detector, oversized input, protocol error). An `Err` is never
    /// interchangeable with an empty [`Detection`]: the caller marks the note
    /// `privacy-unscanned` on `Err` and leaves it untouched on an empty `Ok`.
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError>;

    /// What this detector declares about itself, or `None` when no detection is
    /// configured at all.
    ///
    /// `None` is the pass-through marker: a write whose mounted detector reports it
    /// runs **no pass** — no block is recorded, no tag is applied, and a block the
    /// note already carried transits unchanged. [`NoopDetector`] reports it. An
    /// unconfigured detector is not a failed one, so it is never recorded as one.
    ///
    /// The default is unattributed rather than absent, which is the safe direction:
    /// a detector that does not override this method is recorded as an unnamed pass
    /// rather than silently treated as no detector at all. Implementations that can
    /// name themselves should override it.
    #[must_use]
    fn identity(&self) -> Option<DetectorIdentity> {
        Some(DetectorIdentity::unattributed())
    }
}

/// Strict pass-through detector: always returns an empty [`Detection`].
///
/// The default mounted when no detector is configured. It ignores `budget`
/// entirely — it never returns [`PiiDetectError::InputTooLarge`] — so a note
/// written under it is neither blocked nor tagged.
#[derive(Debug, Clone, Copy)]
pub struct NoopDetector;

impl PiiDetector for NoopDetector {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(Vec::new(), 0))
    }

    /// No detection is configured, so there is no identity to record and no pass to
    /// run: the write stays byte-identical to a write made before the frontier
    /// existed.
    fn identity(&self) -> Option<DetectorIdentity> {
        None
    }
}

/// Resource budgets governing a whole detection pass.
///
/// A pass reads its per-call [`Budget`] from here and never from its caller: these
/// values belong to the vault that was configured, so no request body can narrow
/// one. The window layout is part of the same contract, and lives here rather than
/// beside the detector because the plan that cuts a body is built from this struct.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivacyBudgets {
    /// Largest body, in bytes, a single detector call may be handed. A larger body
    /// is cut into windows, each within this ceiling.
    pub max_input_bytes: usize,
    /// Overlap, in bytes, between two consecutive windows of a body larger than
    /// [`max_input_bytes`](Self::max_input_bytes). An entity longer than the overlap
    /// can be cut at a window boundary and missed — a known under-detection, not an
    /// error, which is why a detector's spans near a boundary are counted.
    pub window_overlap_bytes: usize,
    /// Wall-clock allowance of one detector call on an asynchronous path.
    pub async_timeout_ms: u64,
    /// Constant term of a synchronous path's allowance, in milliseconds.
    pub sync_base_ms: u64,
    /// Per-kilobyte term of a synchronous path's allowance, in milliseconds. Zero
    /// means the synchronous allowance is the base term alone, capped.
    pub sync_per_kib_ms: u64,
    /// Upper bound of a synchronous path's allowance: a body whose proportional
    /// allowance reaches it is not scanned synchronously at all.
    pub sync_cap_ms: u64,
}

impl PrivacyBudgets {
    /// Builds [`PrivacyBudgets`] from its window layout and its per-surface delays.
    #[must_use]
    pub const fn new(
        max_input_bytes: usize,
        window_overlap_bytes: usize,
        async_timeout_ms: u64,
        sync_base_ms: u64,
        sync_per_kib_ms: u64,
        sync_cap_ms: u64,
    ) -> Self {
        Self {
            max_input_bytes,
            window_overlap_bytes,
            async_timeout_ms,
            sync_base_ms,
            sync_per_kib_ms,
            sync_cap_ms,
        }
    }

    /// Per-call budget for a detection running on `surface` over `input_bytes` bytes.
    ///
    /// An asynchronous path is allowed the whole per-call allowance. A synchronous
    /// path pays its detection on the request, so its allowance grows with the input
    /// — `sync_base_ms + sync_per_kib_ms × KiB` — and stops at `sync_cap_ms`: a body
    /// that would need more than the cap is a size state, never a detector failure.
    ///
    /// The byte ceiling is the same on both surfaces. Only the delay varies.
    #[must_use]
    pub fn for_surface(&self, surface: Surface, input_bytes: usize) -> Budget {
        let timeout_ms = match surface {
            Surface::Async => self.async_timeout_ms,
            Surface::Sync => {
                let kib = (input_bytes as u64).div_ceil(1024);
                let proportional = self
                    .sync_base_ms
                    .saturating_add(self.sync_per_kib_ms.saturating_mul(kib));
                proportional.min(self.sync_cap_ms)
            }
        };
        Budget::new(timeout_ms, self.max_input_bytes)
    }
}

impl Default for PrivacyBudgets {
    /// The delivered defaults: a 256 KiB per-call ceiling, a 512-byte window
    /// overlap, 10 s on an asynchronous path, and a synchronous allowance that is
    /// the 2 s base term capped at 20 s — no per-kilobyte term until it is
    /// calibrated on a measurement.
    fn default() -> Self {
        Self {
            max_input_bytes: 262_144,
            window_overlap_bytes: 512,
            async_timeout_ms: 10_000,
            sync_base_ms: 2_000,
            sync_per_kib_ms: 0,
            sync_cap_ms: 20_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A detector that always times out — used to prove the typed-error contract.
    struct AlwaysTimeout;

    impl PiiDetector for AlwaysTimeout {
        fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
            Err(PiiDetectError::Timeout {
                timeout_ms: input.budget.timeout_ms,
            })
        }
    }

    #[test]
    fn noop_detector_always_returns_zero_spans() {
        let input = DetectInput::new(
            "contact alice@example.com now",
            Surface::Async,
            Budget::new(10_000, 262_144),
        );
        let detection = NoopDetector.detect(&input).expect("noop never errors");
        assert!(detection.spans.is_empty());
    }

    #[test]
    fn noop_detector_ignores_budget() {
        // A zero budget must not make the noop detector refuse: it ignores the
        // budget and always returns Ok, never InputTooLarge.
        let input = DetectInput::new("some body text", Surface::Sync, Budget::new(0, 0));
        let result = NoopDetector.detect(&input);
        assert!(matches!(result, Ok(d) if d.spans.is_empty() && d.unmapped_labels == 0));
    }

    #[test]
    fn timeout_returns_typed_error_never_empty_detection() {
        let input = DetectInput::new("body", Surface::Sync, Budget::new(2_000, 1_024));
        let result = AlwaysTimeout.detect(&input);
        // A timeout is a typed Err carrying the budget, not an empty Detection.
        assert!(matches!(
            result,
            Err(PiiDetectError::Timeout { timeout_ms: 2_000 })
        ));
        // And an empty Detection is a distinct Ok value.
        let empty: Result<Detection, PiiDetectError> = Ok(Detection::new(Vec::new(), 0));
        assert!(empty.is_ok());
    }

    #[test]
    fn pii_category_labels_round_trip() {
        let all = [
            PiiCategory::AccountNumber,
            PiiCategory::Address,
            PiiCategory::Email,
            PiiCategory::Person,
            PiiCategory::Phone,
            PiiCategory::Url,
            PiiCategory::Date,
            PiiCategory::Secret,
        ];
        for cat in all {
            let parsed: PiiCategory = cat.as_str().parse().expect("canonical label parses");
            assert_eq!(parsed, cat);
        }
    }

    #[test]
    fn pii_category_rejects_unknown_label() {
        let err = "national_id".parse::<PiiCategory>().unwrap_err();
        assert_eq!(err, UnknownPiiCategory("national_id".to_owned()));
    }

    #[test]
    fn privacy_budgets_default_matches_the_delivered_configuration() {
        let b = PrivacyBudgets::default();
        assert_eq!(b.max_input_bytes, 262_144);
        assert_eq!(b.window_overlap_bytes, 512);
        assert_eq!(b.async_timeout_ms, 10_000);
        assert_eq!(b.sync_base_ms, 2_000);
        assert_eq!(b.sync_per_kib_ms, 0);
        assert_eq!(b.sync_cap_ms, 20_000);
    }

    #[test]
    fn async_surface_gets_the_whole_allowance_and_the_same_byte_ceiling() {
        let b = PrivacyBudgets::new(4_096, 64, 12_345, 7, 3, 100);
        let budget = b.for_surface(Surface::Async, 10_000_000);
        assert_eq!(
            budget.timeout_ms, 12_345,
            "size never narrows an async budget"
        );
        assert_eq!(budget.max_input_bytes, 4_096);
    }

    #[test]
    fn sync_allowance_grows_per_kib_and_stops_at_the_cap() {
        let b = PrivacyBudgets::new(4_096, 64, 12_345, 2_000, 10, 20_000);
        // Ceiling division: one byte over a kibibyte already pays the next kibibyte.
        assert_eq!(b.for_surface(Surface::Sync, 1).timeout_ms, 2_010);
        assert_eq!(b.for_surface(Surface::Sync, 1_024).timeout_ms, 2_010);
        assert_eq!(b.for_surface(Surface::Sync, 1_025).timeout_ms, 2_020);
        // The cap bounds the growth instead of the body being refused for size.
        assert_eq!(b.for_surface(Surface::Sync, 100_000_000).timeout_ms, 20_000);
    }
}
