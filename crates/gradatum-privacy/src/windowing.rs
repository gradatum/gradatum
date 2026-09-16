//! Byte-window partitioning of a body too large for a single detector call.
//!
//! A detector takes a body up to a per-call ceiling. A larger body is neither
//! refused nor truncated in silence: it is cut into overlapping windows, each
//! submitted on its own, and the offsets a detector returns are rebased onto the
//! body before they are validated (see `merge`). The overlap is what keeps an
//! entity sitting across a cut visible to at least one window.

/// Plan for cutting a body into overlapping windows.
///
/// `overlap_bytes` should be at least the length of the longest entity the
/// detector is expected to report: an entity longer than the overlap is cut at
/// every boundary it crosses, so no window ever sees it whole. That is a known
/// under-detection rather than an error, which is why the number of spans sitting
/// on a boundary is counted by `MergeStats` — a `validate_and_merge` pass sees none
/// of it, so the count is the orchestrator's to fill.
#[non_exhaustive]
pub struct WindowPlan {
    /// Largest window this plan will hand to a detector, in bytes.
    pub max_input_bytes: usize,
    /// Overlap between two consecutive windows, in bytes.
    pub overlap_bytes: usize,
}

impl WindowPlan {
    /// Builds a [`WindowPlan`] from a per-window ceiling and an overlap.
    #[must_use]
    pub const fn new(max_input_bytes: usize, overlap_bytes: usize) -> Self {
        Self {
            max_input_bytes,
            overlap_bytes,
        }
    }

    /// Cuts `body` into overlapping windows, each with its own byte offset.
    ///
    /// A ceiling below four bytes is clamped up to four, the widest UTF-8 code
    /// point, so no cut can fall inside a code point and none is skipped.
    ///
    /// Every cut falls on a code-point boundary, so no window splits a UTF-8
    /// character: each returned slice is a valid `&str` and its offset is a
    /// `char_boundary` of `body`. The windows cover `body` from its first byte to
    /// its last, and none is longer than `max_input_bytes` unless that ceiling was
    /// clamped up.
    ///
    /// A degenerate plan still terminates: a zero ceiling yields no window at all,
    /// and an overlap not smaller than the window size still advances the cursor
    /// rather than looping.
    #[must_use]
    pub fn windows<'a>(&self, body: &'a str) -> Vec<(usize, &'a str)> {
        let mut out: Vec<(usize, &'a str)> = Vec::new();

        // A zero ceiling admits no window at all — not an empty one, which would
        // send an empty body to the detector and report a clean scan of nothing.
        if self.max_input_bytes == 0 {
            return out;
        }
        if body.is_empty() {
            out.push((0, body));
            return out;
        }

        // A ceiling narrower than the widest code point cannot hold the character
        // at the cursor: cutting at it would fall inside that character and skip
        // it. Clamped up, the cut always reaches at least one whole code point, so
        // no window is empty and the windows cover every byte.
        let ceiling = self.max_input_bytes.max(4);

        let mut start = 0usize;
        while start < body.len() {
            let mut end = start + (body.len() - start).min(ceiling);
            // Cut back onto a code-point boundary. `start` is one and at most three
            // continuation bytes precede the next boundary, so `end > start`.
            while end > start && !body.is_char_boundary(end) {
                end -= 1;
            }

            out.push((start, &body[start..end]));
            if end == body.len() {
                break;
            }

            // Step back by the overlap to carry a boundary-straddling entity into
            // the next window, then forward onto a code-point boundary. Clamping to
            // `start + 1` keeps a plan whose overlap is not smaller than its window
            // from stalling on the same offset.
            let mut next = end.saturating_sub(self.overlap_bytes);
            if next <= start {
                next = start + 1;
            }
            while next < end && !body.is_char_boundary(next) {
                next += 1;
            }
            start = next;
        }

        out
    }
}
