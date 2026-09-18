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

use std::collections::BTreeMap;
use std::str::FromStr;

/// Product-closed taxonomy of personal-data categories.
///
/// The set is deliberately fixed: each detector maps its own labels onto these
/// eight values, and a label with no mapping is dropped and counted rather than
/// mapped by resemblance.
///
/// The order is the declaration order of the variants, and it exists for one
/// reason: it makes a category a usable key of an ordered map, so a policy's
/// category set has one canonical iteration order rather than the hash order of a
/// `HashMap`. Nothing reads it as a severity ranking.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The span's score in integer thousandths, as the frontmatter writes it.
    #[must_use]
    pub fn score_m(&self) -> u16 {
        score_m(self.score)
    }
}

/// Converts a score in `0.0..=1.0` into integer thousandths.
///
/// The one conversion between the float a detector reports and the integer the
/// configuration, the frontmatter and every comparison use. It is **the reason no
/// float crosses those boundaries**: a `0.94` widened to `f64` and re-serialised as
/// `0.9399999976158142` would move the content hash of an unchanged note.
///
/// Clamped on both ends, and total on values outside the contract: a detector is a
/// foreign process, and a score of `NaN`, of `-3.0` or of `42.0` must not be able to
/// write a negative count or overflow the cast. An infinity saturates at the bound it
/// points to — it is an extreme, not an absence of answer. `NaN` is answered with `0`,
/// and it is the only value that is: a score that is not a number clears no threshold,
/// and pretending otherwise would let a broken detector mark arbitrary text.
#[must_use]
pub fn score_m(score: f32) -> u16 {
    if score.is_nan() {
        return 0;
    }
    (score.clamp(0.0, 1.0) * 1000.0).round() as u16
}

/// Largest threshold a policy may declare, in integer thousandths.
///
/// The scale is the frontmatter's `score_m` scale. A threshold above its top would
/// leave a category enabled, paying for a detector pass, and unable to ever mark or
/// mask anything — a configuration an operator can only have written by mistake, so
/// it is refused rather than silently accepted.
pub const THRESHOLD_MAX_M: u16 = 1000;

/// What is written over a masked span.
///
/// Deliberately not length-preserving: a filler of the same length as what it
/// replaces publishes the length of the value it removed, and the masked body is the
/// one artefact that must no longer describe the original.
pub const MASK_FILLER: &str = "[redacted]";

/// Rewrites `body`, replacing every span of `spans` with [`MASK_FILLER`].
///
/// The rewrite is **irreversible**: no copy of the replaced text is kept, here or
/// anywhere downstream. `spans` are the spans a policy retained and marked, sorted or
/// not; overlapping and adjacent ranges are merged before the rewrite, so a filler is
/// never written twice over the same range.
///
/// A range that is not a char boundary of `body`, or that lies outside it, is
/// **skipped rather than panicked on**: spans reaching here are validated upstream,
/// and a foreign span must not be able to take a write down. Skipping is the safe
/// direction — it leaves text in place instead of truncating a body at a wrong offset.
#[must_use]
pub fn mask_spans(body: &str, spans: &[PiiSpan]) -> String {
    let mut ranges: Vec<(usize, usize)> = spans
        .iter()
        .filter_map(|span| {
            let start = span.start;
            let end = span.start.checked_add(span.len)?;
            if start >= end
                || end > body.len()
                || !body.is_char_boundary(start)
                || !body.is_char_boundary(end)
            {
                return None;
            }
            Some((start, end))
        })
        .collect();
    if ranges.is_empty() {
        return body.to_owned();
    }
    ranges.sort_unstable();

    // Merge first, write second. A filler per span would leave three marks over a
    // region two overlapping spans describe once, and the number of marks would
    // describe the detector's output rather than the text that was removed.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => merged.push((start, end)),
        }
    }

    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        out.push_str(&body[cursor..start]);
        out.push_str(MASK_FILLER);
        cursor = end;
    }
    out.push_str(&body[cursor..]);
    out
}

/// The activation policy one detection pass runs under.
///
/// Four things the `[privacy]` block declares, resolved into the one shape the pass,
/// the block and the tag decision read:
///
/// - **the mode** — observe (`report_only`) or rewrite (`mask`);
/// - **the enabled category set**, and **the threshold of each**, in integer
///   thousandths. A category absent from the map is not enabled: it is observed, never
///   marked. A threshold of `0` means *not calibrated* and enables nothing — it is not
///   "everything passes", and a category carrying one marks nothing however high the
///   detector scores;
/// - **the exception patterns**, per tenant. A span whose matched text covers one is
///   dropped after fusion, before the block and the tags: an exception is the
///   statement that a literal is *not* personal data, so recording it as a span would
///   contradict the declaration that dropped it.
///
/// The empty policy — [`Default`] — enables nothing, masks nothing and excepts
/// nothing. It is the policy of a deployment that declared no `[privacy]` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivacyPolicy {
    mask: bool,
    thresholds: BTreeMap<PiiCategory, u16>,
    exceptions: BTreeMap<String, Vec<String>>,
}

impl PrivacyPolicy {
    /// Builds a policy from a mode flag, an enabled-category threshold map and
    /// per-tenant exception patterns.
    #[must_use]
    pub fn new(
        mask: bool,
        thresholds: BTreeMap<PiiCategory, u16>,
        exceptions: BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            mask,
            thresholds,
            exceptions,
        }
    }

    /// Does this policy rewrite the bodies it scans?
    #[must_use]
    pub const fn masks(&self) -> bool {
        self.mask
    }

    /// Threshold of an enabled category, in thousandths.
    ///
    /// `None` means the category is not enabled. A threshold of `0` is returned as
    /// it stands — it is declared, it is not calibrated, and [`marks`](Self::marks)
    /// is where that difference is spent.
    #[must_use]
    pub fn threshold_m(&self, category: PiiCategory) -> Option<u16> {
        self.thresholds.get(&category).copied()
    }

    /// Enabled categories, in taxonomy order.
    #[must_use]
    pub fn enabled_categories(&self) -> Vec<PiiCategory> {
        self.thresholds.keys().copied().collect()
    }

    /// The exception patterns of `tenant`, in declaration order.
    #[must_use]
    pub fn exceptions_for(&self, tenant: &str) -> &[String] {
        self.exceptions.get(tenant).map_or(&[], Vec::as_slice)
    }

    /// Every declared exception list, keyed by tenant.
    ///
    /// Exposed for the one caller that has to fold the patterns into a digest without
    /// ever rendering them: the policy fingerprint a block carries.
    #[must_use]
    pub fn exceptions(&self) -> &BTreeMap<String, Vec<String>> {
        &self.exceptions
    }

    /// Does this policy mark a span?
    ///
    /// Two conditions, and both are needed: the span's category must be enabled, and
    /// its threshold must be **strictly positive** and cleared by the span's
    /// `score_m`. A threshold of `0` observes the distribution and poses nothing —
    /// the separation between an observation point and a marking point, which is what
    /// keeps `categories = []` from tagging a whole corpus.
    ///
    /// A span reported at the top of the scale clears any threshold a policy can
    /// declare, which is exactly the position of a regex span: it is not subject to a
    /// threshold, and needs no provenance flag to be treated as such.
    #[must_use]
    pub fn marks(&self, span: &PiiSpan) -> bool {
        self.threshold_m(span.category)
            .is_some_and(|threshold| threshold > 0 && span.score_m() >= threshold)
    }

    /// Drops the spans a declared exception covers.
    ///
    /// Returns the retained spans, in their incoming order, and the number of spans
    /// dropped **per pattern identifier** — the identifier being a digest of the
    /// pattern, never the pattern itself: an exception list is a deposit of personal
    /// data by nature, and a counter that carried one would publish what the
    /// configuration was written to withhold.
    ///
    /// A span is covered when its matched text **contains** a pattern of the write's
    /// tenant. Containment, not equality: a pattern names a benign fragment inside a
    /// larger match, and requiring the whole match to equal the pattern would make
    /// the exception fire on nothing a detector actually reports.
    #[must_use]
    pub fn retain(
        &self,
        tenant: &str,
        body: &str,
        spans: Vec<PiiSpan>,
    ) -> (Vec<PiiSpan>, BTreeMap<String, u64>) {
        let patterns = self.exceptions_for(tenant);
        if patterns.is_empty() {
            return (spans, BTreeMap::new());
        }
        let mut hits: BTreeMap<String, u64> = BTreeMap::new();
        let mut retained = Vec::with_capacity(spans.len());
        for span in spans {
            let end = span.start.saturating_add(span.len);
            let matched = body.get(span.start..end);
            let covered = matched.is_some_and(|text| {
                patterns
                    .iter()
                    .any(|pattern| !pattern.is_empty() && text.contains(pattern.as_str()))
            });
            if covered {
                let Some(text) = matched else {
                    // Unreachable for a validated span, and the safe direction if it
                    // ever is not: an unreadable match is kept, never dropped.
                    retained.push(span);
                    continue;
                };
                let id = pattern_id(
                    patterns
                        .iter()
                        .find(|pattern| !pattern.is_empty() && text.contains(pattern.as_str()))
                        .map_or("", String::as_str),
                );
                *hits.entry(id).or_insert(0) += 1;
            } else {
                retained.push(span);
            }
        }
        (retained, hits)
    }
}

/// Stable identifier of one exception pattern, as a counter label may carry it.
///
/// A digest rather than the pattern: see [`PrivacyPolicy::retain`]. Truncated to 16
/// hexadecimal characters, which is ample for the handful of patterns a tenant
/// declares and keeps a metric label readable.
#[must_use]
pub fn pattern_id(pattern: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pattern.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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

    // ---- score_m: the one float-to-integer conversion ----

    #[test]
    fn score_m_rounds_to_thousandths() {
        assert_eq!(score_m(0.0), 0);
        assert_eq!(score_m(0.5), 500);
        assert_eq!(score_m(1.0), 1000);
        // 0.94 * 1000 is 939.9999... in binary floating point; the rounding, not the
        // truncation, is what keeps a threshold of 940 from rejecting a span the
        // configuration plainly admits.
        assert_eq!(score_m(0.94), 940);
        assert_eq!(score_m(0.0004), 0);
        assert_eq!(score_m(0.9999), 1000);
    }

    #[test]
    fn score_m_saturates_outside_the_contract_instead_of_overflowing() {
        // A detector is a foreign process: its score is validated here or not at all.
        assert_eq!(score_m(-3.0), 0);
        assert_eq!(score_m(42.0), 1000);
        assert_eq!(score_m(f32::NAN), 0);
        assert_eq!(score_m(f32::INFINITY), 1000);
        assert_eq!(score_m(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn span_score_m_is_the_shared_conversion() {
        let span = PiiSpan::new(0, 5, PiiCategory::Email, 0.94);
        assert_eq!(span.score_m(), 940);
    }

    // ---- mask_spans: the in-place rewrite ----

    #[test]
    fn mask_replaces_every_span_and_keeps_the_rest_verbatim() {
        let body = "mail alice@example.com, tel 0612";
        let spans = [
            PiiSpan::new(5, 17, PiiCategory::Email, 1.0),
            PiiSpan::new(28, 4, PiiCategory::Phone, 1.0),
        ];
        assert_eq!(mask_spans(body, &spans), "mail [redacted], tel [redacted]");
    }

    #[test]
    fn mask_does_not_publish_the_length_of_what_it_removed() {
        let short = mask_spans("ab xy", &[PiiSpan::new(3, 2, PiiCategory::Person, 1.0)]);
        let long = mask_spans(
            "ab xxxxxxxxxxxxxxxx",
            &[PiiSpan::new(3, 16, PiiCategory::Person, 1.0)],
        );
        assert_eq!(short, long, "the filler is not length-preserving");
    }

    #[test]
    fn mask_merges_overlapping_and_adjacent_spans() {
        let body = "aaaabbbbcc";
        let spans = [
            PiiSpan::new(0, 4, PiiCategory::Person, 1.0),
            PiiSpan::new(2, 4, PiiCategory::Person, 1.0), // overlaps the first
            PiiSpan::new(4, 4, PiiCategory::Person, 1.0), // adjacent to both
        ];
        assert_eq!(mask_spans(body, &spans), "[redacted]cc");
    }

    #[test]
    fn mask_skips_an_unusable_span_rather_than_truncating_the_body() {
        let body = "héllo wörld";
        let spans = [
            PiiSpan::new(1, 1, PiiCategory::Person, 1.0), // inside 'é' — not a boundary
            PiiSpan::new(500, 10, PiiCategory::Person, 1.0), // past the end
        ];
        assert_eq!(
            mask_spans(body, &spans),
            body,
            "an unusable range must leave the body whole"
        );
    }

    #[test]
    fn mask_of_nothing_is_the_identity() {
        assert_eq!(mask_spans("unchanged", &[]), "unchanged");
        assert_eq!(
            mask_spans("", &[PiiSpan::new(0, 4, PiiCategory::Email, 1.0)]),
            ""
        );
    }

    // ---- PrivacyPolicy: the activation decision ----

    fn policy_with(mask: bool, category: PiiCategory, threshold_m: u16) -> PrivacyPolicy {
        let mut thresholds = BTreeMap::new();
        thresholds.insert(category, threshold_m);
        PrivacyPolicy::new(mask, thresholds, BTreeMap::new())
    }

    #[test]
    fn the_empty_policy_marks_nothing_masks_nothing_and_excepts_nothing() {
        let policy = PrivacyPolicy::default();
        assert!(!policy.masks());
        assert!(policy.enabled_categories().is_empty());
        assert!(!policy.marks(&PiiSpan::new(0, 4, PiiCategory::Email, 1.0)));
        assert!(policy.exceptions().is_empty());
        assert!(policy.exceptions_for("main").is_empty());
    }

    #[test]
    fn a_span_is_marked_only_above_its_category_threshold() {
        let policy = policy_with(false, PiiCategory::Email, 900);
        assert!(policy.marks(&PiiSpan::new(0, 4, PiiCategory::Email, 0.95)));
        assert!(
            policy.marks(&PiiSpan::new(0, 4, PiiCategory::Email, 0.9)),
            "the threshold is inclusive"
        );
        assert!(!policy.marks(&PiiSpan::new(0, 4, PiiCategory::Email, 0.89)));
    }

    #[test]
    fn an_unlisted_category_is_observed_but_never_marked() {
        let policy = policy_with(true, PiiCategory::Email, 900);
        assert!(
            !policy.marks(&PiiSpan::new(0, 4, PiiCategory::Phone, 1.0)),
            "a category the configuration does not enable never marks, however it scores"
        );
    }

    #[test]
    fn a_zero_threshold_is_not_calibrated_rather_than_permissive() {
        // The distinction the whole activation rests on: an observation point is not a
        // marking point, so a category the operator declared at 0 marks nothing at all.
        let policy = policy_with(true, PiiCategory::Email, 0);
        assert!(!policy.marks(&PiiSpan::new(0, 4, PiiCategory::Email, 1.0)));
        assert_eq!(policy.enabled_categories(), vec![PiiCategory::Email]);
    }

    #[test]
    fn a_regex_span_at_the_top_of_the_scale_clears_any_threshold() {
        let policy = policy_with(false, PiiCategory::Secret, THRESHOLD_MAX_M);
        assert!(policy.marks(&PiiSpan::new(0, 4, PiiCategory::Secret, 1.0)));
    }

    // ---- exceptions: applied last, identified by digest ----

    #[test]
    fn an_exception_drops_the_span_it_covers_and_counts_the_pattern() {
        let mut exceptions = BTreeMap::new();
        exceptions.insert("main".to_owned(), vec!["example.com".to_owned()]);
        let policy = PrivacyPolicy::new(false, BTreeMap::new(), exceptions);
        let body = "mail alice@example.com and bob@other.org";
        let spans = vec![
            PiiSpan::new(5, 17, PiiCategory::Email, 1.0),
            PiiSpan::new(27, 13, PiiCategory::Email, 1.0),
        ];
        let (retained, hits) = policy.retain("main", body, spans);
        assert_eq!(retained.len(), 1, "only the covered span is dropped");
        assert_eq!(retained[0].start, 27);
        assert_eq!(hits.get(&pattern_id("example.com")), Some(&1));
        assert!(
            !hits.contains_key("example.com"),
            "the counter is keyed by a digest, never by the pattern"
        );
    }

    #[test]
    fn an_exception_declared_for_another_tenant_does_not_apply() {
        let mut exceptions = BTreeMap::new();
        exceptions.insert("main".to_owned(), vec!["example.com".to_owned()]);
        let policy = PrivacyPolicy::new(false, BTreeMap::new(), exceptions);
        let body = "alice@example.com";
        let spans = vec![PiiSpan::new(0, 17, PiiCategory::Email, 1.0)];
        let (retained, hits) = policy.retain("other", body, spans);
        assert_eq!(retained.len(), 1);
        assert!(hits.is_empty());
    }

    #[test]
    fn no_exception_means_no_allocation_and_no_rewrite_of_the_span_list() {
        let policy = PrivacyPolicy::default();
        let spans = vec![PiiSpan::new(0, 17, PiiCategory::Email, 1.0)];
        let (retained, hits) = policy.retain("main", "alice@example.com", spans);
        assert_eq!(retained.len(), 1);
        assert!(hits.is_empty());
    }

    #[test]
    fn a_pattern_that_covers_nothing_keeps_every_span() {
        let mut exceptions = BTreeMap::new();
        exceptions.insert("main".to_owned(), vec!["noreply.example".to_owned()]);
        let policy = PrivacyPolicy::new(false, BTreeMap::new(), exceptions);
        let (retained, hits) = policy.retain(
            "main",
            "alice@example.com",
            vec![PiiSpan::new(0, 17, PiiCategory::Email, 1.0)],
        );
        assert_eq!(retained.len(), 1);
        assert!(hits.is_empty());
    }

    #[test]
    fn the_pattern_digest_is_stable_and_does_not_carry_the_pattern() {
        let id = pattern_id("example.com");
        assert_eq!(id, pattern_id("example.com"), "stable across calls");
        assert_ne!(id, pattern_id("example.org"));
        assert!(
            !id.contains("example"),
            "the digest never spells the pattern"
        );
        assert_eq!(id.len(), 16);
    }
}
