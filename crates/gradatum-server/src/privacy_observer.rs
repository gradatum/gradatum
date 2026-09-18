//! The metrics sink of the personal-data frontier (F-160 L-D.2).
//!
//! The vault runs the pass and owns no registry; the server owns the registry and runs
//! no pass. [`MetricsPrivacyObserver`] is the adapter between the two, installed through
//! [`Vault::with_privacy_observer`](gradatum_vault::Vault::with_privacy_observer) at the
//! same moment the detector is mounted.
//!
//! ## What it may publish
//!
//! Only what the vault's [`PrivacyPass`] carries: canonical category labels, a block
//! status, and integer counts. The seam exists precisely so that the two sides cannot
//! disagree about what is publishable — a detector label, a matched fragment or a body
//! never crosses it, so no series can be fed with one.

use std::sync::Arc;

use gradatum_vault::{PrivacyObserver, PrivacyPass};

use crate::metrics::{
    AppMetrics, PrivacyCategoryLabel, PrivacyLabelLabel, PrivacyPatternLabel, PrivacyReasonLabel,
};

/// The single label value the unmapped-label series is fed under.
///
/// A constant and not a detector's label: see `PrivacyLabelLabel`. It is repeated here
/// rather than read from the registry because the label set is a *type*, and this is the
/// one value 2.4.0 may put in it.
const UNMAPPED_LABEL: &str = "unmapped";

/// The block status of a pass that ran out of budget before reaching the detector.
///
/// Spelled literally: the status crosses the seam as a reported `String`, and the
/// comparison is with the value the pass states, not with a type of ours.
const STATUS_INPUT_TOO_LARGE: &str = "input_too_large";

/// Why a span was rejected: it does not lie inside the body it was reported against.
const REASON_OUT_OF_BOUNDS: &str = "out_of_bounds";

/// Why a span was rejected: it does not sit on a code-point boundary.
const REASON_UTF8_BOUNDARY: &str = "utf8_boundary";

/// Reports every completed pass to the server's metrics registry.
///
/// Cheap and non-blocking by construction: one observation touches a fixed number of
/// counters and allocates at most a label key. It runs inline on the writing task, before
/// the note is persisted, so nothing here may await, lock or read the filesystem.
#[derive(Clone)]
pub struct MetricsPrivacyObserver {
    /// Registry the pass series are incremented in.
    metrics: Arc<AppMetrics>,
}

impl MetricsPrivacyObserver {
    /// Builds an observer publishing into `metrics`.
    #[must_use]
    pub fn new(metrics: AppMetrics) -> Self {
        Self {
            metrics: Arc::new(metrics),
        }
    }
}

impl PrivacyObserver for MetricsPrivacyObserver {
    fn pass_observed(&self, pass: &PrivacyPass) {
        // Category series are keyed by the pass's own canonical labels, which come from
        // `PiiCategory::as_str` — a closed vocabulary, so the family's cardinality is
        // bounded by the product's own type rather than by whatever the mounted model
        // decided to call things.
        for category in &pass.categories {
            self.metrics
                .privacy_notes_tagged
                .get_or_create(&PrivacyCategoryLabel {
                    category: category.clone(),
                })
                .inc();
        }

        // The unmapped labels are summed under one constant. This is the whole reason the
        // core contract carries a *count* and not the labels: a value chosen by the mounted
        // model would mint one series per label it invents.
        if pass.unmapped_labels > 0 {
            self.metrics
                .privacy_unmapped_labels
                .get_or_create(&PrivacyLabelLabel {
                    label: UNMAPPED_LABEL,
                })
                .inc_by(pass.unmapped_labels);
        }

        // Guarded so that a motive no pass earned is **absent** rather than present at
        // zero: an empty family renders nothing, and a rendered `0` would read as "this
        // motive was considered and not found", which is a claim the observation does not
        // make. The two motives are kept apart because they call for different work.
        if pass.rejected_out_of_bounds > 0 {
            self.metrics
                .privacy_spans_rejected
                .get_or_create(&PrivacyReasonLabel {
                    reason: REASON_OUT_OF_BOUNDS,
                })
                .inc_by(pass.rejected_out_of_bounds);
        }
        if pass.rejected_utf8_boundary > 0 {
            self.metrics
                .privacy_spans_rejected
                .get_or_create(&PrivacyReasonLabel {
                    reason: REASON_UTF8_BOUNDARY,
                })
                .inc_by(pass.rejected_utf8_boundary);
        }
        if pass.touching_window_edge > 0 {
            self.metrics
                .privacy_spans_window_edge
                .inc_by(pass.touching_window_edge);
        }

        // The exceptions the configuration declared and this write actually exercised. The
        // key is the **pattern identifier** the pass computed — a digest — and never the
        // configured literal: an exception list names real values by nature, and a label is
        // the one place this process writes text that outlives the request. Cardinality is
        // bounded by the operator's own configuration, so a series per pattern is a
        // deliberate trade: it answers "which exception is doing the work" (a stale literal,
        // a pattern permissive enough to cover legitimate data) without ever restating it.
        // An empty map renders nothing, which is the ordinary case.
        for (pattern_id, hits) in &pass.exception_hits {
            self.metrics
                .privacy_exception_hits
                .get_or_create(&PrivacyPatternLabel {
                    pattern_id: pattern_id.clone(),
                })
                .inc_by(*hits);
        }

        // The completed/unfinished partition. The key is the **resolved** verdict and not
        // the pass's own status: D-11 gives a human rejection and a span list past the
        // ceiling the power to overrule a markable pass, and a partition keyed on the
        // status would then report a pass as judged that the note records as owed. The
        // two are exhaustive by construction — an unfinished pass always poses the rescan
        // marker — so the pair sums to the number of passes observed.
        if pass.unscanned_tagged {
            self.metrics.privacy_unscanned.inc();
        } else {
            self.metrics.privacy_scanned.inc();
        }

        // A body with no window to scan never reached the detector, which is a different
        // fact from a detector that did not answer, and only the first is fixed by
        // calibrating a threshold. The two are counted apart for that reason.
        if pass.status == STATUS_INPUT_TOO_LARGE {
            self.metrics.privacy_input_too_large.inc();
        }
    }
}
