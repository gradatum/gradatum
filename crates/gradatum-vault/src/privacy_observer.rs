// F-160, L-D.2.
//! What a mounted detection frontier reports to whoever installed it.
//!
//! ## Why a sink rather than a dependency
//!
//! The pass runs inside this crate, which owns no metrics registry: a counter is a
//! property of the server that mounted the frontier, not of the vault that runs it.
//! An observer is the seam — the server installs one through
//! [`Vault::with_privacy_observer`](crate::registry::Vault::with_privacy_observer),
//! exactly as it installs the detector itself, and the vault reports what it saw
//! without ever naming a series.
//!
//! ## What crosses the seam
//!
//! Only **bounded** values, and only what a pass can state about itself: a block
//! status, canonical category labels, and integer counts. Never the body, never a
//! detected value, never a matched fragment — an observer is a reporting channel,
//! and the frontier's own logging rule (`SECURITY.md`: a reason, a length, a code)
//! applies to it unchanged.
//!
//! The payload is [`PrivacyPass`], deliberately **not** the crate-internal
//! `PrivacyScan`: an observer must not be able to read a span, so the shape handed
//! to it carries labels and counts and no offsets.

/// What one detection pass observed about itself.
///
/// Every field is stated **after** validation, fusion and the block's own span
/// ceiling, so the numbers an observer reports are the numbers the note's block
/// records — not the detector's raw answer.
///
/// The type is `#[non_exhaustive]`: a producer-side field may be added without
/// breaking an observer that was compiled against an earlier shape, and no crate
/// outside this one can build a payload to feed itself.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivacyPass {
    /// Block status the pass resolved to: `ok`, `spans_truncated`, `input_too_large`
    /// or `detector_unavailable`. Empty when no pass ran.
    pub status: String,
    /// Canonical category labels the pass reported at least one marker-bearing span
    /// for — the same condition the payload tag rests on, so a count taken here and
    /// a tag posed on the note cannot disagree.
    pub categories: Vec<String>,
    /// Whether this write left the note carrying `personal-classified`.
    ///
    /// The **resolved** decision, not the pass's own reading: a human rejection that
    /// still binds, or a span list past the ceiling, overrules a markable pass, and a
    /// report that counted the pass would claim a marker the note does not carry.
    pub classified_tagged: bool,
    /// Whether this write left the note carrying `privacy-unscanned`.
    pub unscanned_tagged: bool,
    /// Detector labels that matched no category and were dropped, summed over the
    /// body's windows.
    ///
    /// The core contract carries the **count** and not the labels, so this figure is
    /// reportable and the names behind it are not: an observer may alert on a model
    /// change, never name the label that caused it.
    pub unmapped_labels: u64,
    /// Spans the pass rejected for falling outside the body.
    pub rejected_out_of_bounds: u64,
    /// Spans the pass rejected for not sitting on a code-point boundary.
    pub rejected_utf8_boundary: u64,
    /// Spans sitting within one byte of an interior window boundary — the only
    /// observable signal that `window_overlap_bytes` is shorter than the entities
    /// the detector can report.
    pub touching_window_edge: u64,
    /// Spans the exception pass dropped, counted per **pattern identifier**.
    ///
    /// The identifier is a digest of the configured pattern, never the pattern: an
    /// exception list is literal personal data by nature, and this payload ends up in
    /// a metric label. An observer may therefore alert on *which* configured exception
    /// fires and how often, and can never read back the literal it holds.
    ///
    /// Keys are sorted, and a pattern that covered nothing is absent rather than
    /// present at zero: the map states what fired, not what was declared.
    pub exception_hits: std::collections::BTreeMap<String, u64>,
}

impl PrivacyPass {
    /// Builds a [`PrivacyPass`] from its fields, with no exception firing recorded.
    ///
    /// Eight arguments rather than a params struct: this is the seam's **wire shape**,
    /// and the counters it carries are flat by design — one field per observable
    /// quantity, so an installer reads exactly the names the metric labels use. A
    /// struct would move the same fields one level down without bounding anything,
    /// and would hide from a reader that the report is closed: every value here is a
    /// count, a bounded status or an identifier that is a digest — never a body, a
    /// pattern or an identity.
    ///
    /// The exception firings are the one part a caller adds separately, through
    /// [`Self::with_exception_hits`]: they are the only field whose *keys* a
    /// configuration mints, and keeping them out of the constructor is what let the
    /// activation be delivered without moving a signature an installer already compiled
    /// against. A pass built here and never given firings reports none, which is the
    /// ordinary case for a deployment that declares no exception.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "flat counter report — a params struct adds a layer without bounding anything"
    )]
    pub fn new(
        status: impl Into<String>,
        categories: Vec<String>,
        classified_tagged: bool,
        unscanned_tagged: bool,
        unmapped_labels: u64,
        rejected_out_of_bounds: u64,
        rejected_utf8_boundary: u64,
        touching_window_edge: u64,
    ) -> Self {
        Self {
            status: status.into(),
            categories,
            classified_tagged,
            unscanned_tagged,
            unmapped_labels,
            rejected_out_of_bounds,
            rejected_utf8_boundary,
            touching_window_edge,
            exception_hits: std::collections::BTreeMap::new(),
        }
    }

    /// Records the exception firings this pass exercised, keyed by pattern identifier.
    ///
    /// See [`Self::exception_hits`] for why the keys are digests and never patterns.
    #[must_use]
    pub fn with_exception_hits(
        mut self,
        exception_hits: std::collections::BTreeMap<String, u64>,
    ) -> Self {
        self.exception_hits = exception_hits;
        self
    }
}

/// Receives what each detection pass observed.
///
/// One method, called at most once per pass, from the write path. Implementations
/// must be cheap and non-blocking: they run inline, on the writing task, before the
/// note is persisted. A counter is the intended implementation.
///
/// The frontier never fails because an observer failed, and never retries one: an
/// observer that panics or blocks is the installer's defect, and the write is not
/// conditioned on it. That is why observation happens **after** the verdict is
/// resolved and **before** the write, on already-computed values: an observer can
/// lose a sample, never change a verdict.
pub trait PrivacyObserver: Send + Sync {
    /// Records one completed pass.
    ///
    /// Not called when no pass ran — a `code-*` or `identity` write, or a detector
    /// that declares no identity. "No pass" is not a pass that found nothing, and a
    /// seam that conflated them would report the idle installation as a clean one.
    fn pass_observed(&self, pass: &PrivacyPass);
}
