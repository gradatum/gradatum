//! Rebasing, validation and fusion of detected spans.

use gradatum_core::pii::PiiSpan;

/// Bookkeeping of a single validation-and-fusion pass.
///
/// Every count is an observation, never a silent drop: a span that cannot be used
/// is rejected *and* counted, so a caller can tell "nothing was found" from "what
/// was found could not be applied".
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeStats {
    /// Spans falling outside the body, including offset arithmetic that overflows.
    pub rejected_out_of_bounds: usize,
    /// Spans whose start or end is not a code-point boundary of the body.
    pub rejected_utf8_boundary: usize,
    /// Number of spans within one byte of an interior window boundary. Left at
    /// zero by `validate_and_merge`, which does not know the window layout; the
    /// windowing orchestrator fills it.
    pub touching_window_edge: usize,
    /// Spans absorbed by a fusion; equal to the number of union operations.
    pub merged: usize,
}

impl MergeStats {
    /// Builds [`MergeStats`] from its four counters.
    #[must_use]
    pub const fn new(
        rejected_out_of_bounds: usize,
        rejected_utf8_boundary: usize,
        touching_window_edge: usize,
        merged: usize,
    ) -> Self {
        Self {
            rejected_out_of_bounds,
            rejected_utf8_boundary,
            touching_window_edge,
            merged,
        }
    }
}

/// Rebases window-relative spans onto the body.
///
/// A detector answers in the coordinates of the window it was handed. Every offset
/// must be shifted by that window's own start *before* validation: without it, a
/// window-relative offset that happens to fall inside the window is a wrong — yet
/// in-bounds — body offset, which validation cannot tell from a right one.
///
/// The shift saturates. A corrupt offset at the top of the range is left saturated
/// and rejected downstream, never a panic on hostile input.
pub fn rebase(spans: &mut [PiiSpan], window_start: usize) {
    for span in spans {
        span.start = span.start.saturating_add(window_start);
    }
}

/// Validates spans against `body` and fuses the survivors.
///
/// The order is normative: rebase (by the caller), then validate, then sort, then
/// fuse. Validation rejects a span that falls outside `body` or that starts or ends
/// inside a code point. Fusion runs per category, so a span of another category
/// never splits an overlap: the byte ranges of overlapping spans of one category
/// are unioned — the union of the bounds, across windows as well as within one
/// window — and the fused span inherits the category and score of the regex span
/// (1.0) when the fused group holds one, which keeps the result out of the model
/// threshold. A union can widen a span beyond what a model reported alone, and
/// over-masking is the safe direction for privacy.
///
/// The output is ordered by body offset, not by category.
///
/// Nothing here fails as a whole: a hostile span is a counted rejection, never a
/// panic and never a lost batch.
#[must_use]
pub fn validate_and_merge(body: &str, spans: Vec<PiiSpan>) -> (Vec<PiiSpan>, MergeStats) {
    let mut stats = MergeStats::new(0, 0, 0, 0);
    let body_len = body.len();

    let mut accepted: Vec<PiiSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        // `checked_add` first: on hostile input `start + len` overflows, and an
        // offset arithmetic that cannot be represented is out of the body.
        let Some(end) = span.start.checked_add(span.len) else {
            stats.rejected_out_of_bounds += 1;
            continue;
        };
        if end > body_len {
            stats.rejected_out_of_bounds += 1;
            continue;
        }
        // Both ends, not just the start: a span ending inside a code point would
        // slice a non-UTF-8 string downstream.
        if !body.is_char_boundary(span.start) || !body.is_char_boundary(end) {
            stats.rejected_utf8_boundary += 1;
            continue;
        }
        accepted.push(span);
    }

    // Deterministic order, grouped by category: the fusion below only ever
    // compares a span to the last one it produced, so every span of a category
    // must be contiguous — otherwise a span of another category intercalated
    // between two overlapping ones would keep them apart and count one entity
    // twice. Within a category: earliest start first, longest first on a tie.
    accepted.sort_by(|a, b| {
        a.category
            .as_str()
            .cmp(b.category.as_str())
            .then(a.start.cmp(&b.start))
            .then(b.len.cmp(&a.len))
    });

    let mut merged: Vec<PiiSpan> = Vec::with_capacity(accepted.len());
    for span in accepted {
        match merged.last_mut() {
            Some(current)
                if current.category == span.category
                    && span.start < current.start + current.len =>
            {
                // Union of the two byte ranges, keeping the highest score so a
                // regex span (score 1.0) survives the fusion and stays out of the
                // threshold. Every accepted offset is in bounds, so neither sum
                // can overflow.
                let end = (current.start + current.len).max(span.start + span.len);
                current.len = end - current.start;
                current.score = current.score.max(span.score);
                stats.merged += 1;
            }
            _ => merged.push(span),
        }
    }

    // The grouping above is internal: callers get the spans in body order.
    merged.sort_by(|a, b| a.start.cmp(&b.start).then(b.len.cmp(&a.len)));

    (merged, stats)
}
