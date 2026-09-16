use std::sync::LazyLock;

use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
};
use regex::Regex;

/// Score carried by every regex-produced span. Regex matches are exact by
/// construction and are not subject to a probability threshold.
const MAX_SCORE: f32 = 1.0;

// Patterns are compiled once, at first use, and never inside `detect`.
// `expect` is sound here: each pattern is a compile-time string literal, so a
// failure to compile is an authoring bug caught on the first call, never a
// function of runtime input.

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
        .expect("email pattern is a valid compile-time literal")
});

static IBAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{2}[0-9]{2}[A-Z0-9]{11,30}\b")
        .expect("iban pattern is a valid compile-time literal")
});

static PHONE_FR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\+33|0)\s?[1-9](?:[\s.]?\d{2}){4}")
        .expect("french phone pattern is a valid compile-time literal")
});

static NIR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[12]\s?\d{2}\s?(?:0[1-9]|1[0-2])\s?\d{2}\s?\d{3}\s?\d{3}\s?\d{2}\b")
        .expect("french nir pattern is a valid compile-time literal")
});

/// Ordered list of `(pattern, category)` the detector applies.
///
/// IBAN precedes the shorter numeric patterns so that, after overlap pruning, a
/// long account number absorbs any phone- or NIR-shaped substring nested inside
/// it rather than emitting a spurious second span.
fn patterns() -> [(&'static LazyLock<Regex>, PiiCategory); 4] {
    [
        (&EMAIL_RE, PiiCategory::Email),
        (&IBAN_RE, PiiCategory::AccountNumber),
        (&PHONE_FR_RE, PiiCategory::Phone),
        (&NIR_RE, PiiCategory::AccountNumber),
    ]
}

/// Deterministic floor detector backed by a fixed set of compiled patterns.
///
/// Covers e-mail addresses, IBANs, French phone numbers and French
/// social-security numbers (NIR). Every span carries the maximal score.
pub struct RegexDetector;

impl RegexDetector {
    /// Builds a [`RegexDetector`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for RegexDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiDetector for RegexDetector {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let text = input.text;

        let mut spans: Vec<PiiSpan> = Vec::new();
        for (re, category) in patterns() {
            for m in re.find_iter(text) {
                spans.push(PiiSpan::new(m.start(), m.len(), category, MAX_SCORE));
            }
        }

        // Deterministic order, then greedy overlap pruning: earlier start first,
        // longer match first on ties. A span nested in an already-kept, longer
        // match is dropped — full regex ∪ sidecar fusion is a later concern; here
        // we only stop a sub-pattern from double-counting inside another entity.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.len.cmp(&a.len)));

        let mut kept: Vec<PiiSpan> = Vec::with_capacity(spans.len());
        let mut last_end = 0usize;
        for span in spans {
            if kept.is_empty() || span.start >= last_end {
                last_end = span.start + span.len;
                kept.push(span);
            }
        }

        Ok(Detection::new(kept, 0))
    }
}
