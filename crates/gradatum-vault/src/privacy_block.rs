//! The `extra.privacy` block: what a detection pass records about the note it
//! scanned.
//!
//! ## Why it is written at all
//!
//! The block is the only durable evidence that a pass ran, and the only input a
//! later review or re-scan reads back. Four states have to stay distinguishable,
//! and none of them may be deduced from a silence: no block at all means no pass;
//! `counts: {}` means a pass that found nothing; a `status` other than `ok` means a
//! pass that was attempted and did not complete; a non-empty `counts` with no tag
//! means spans were seen under the activation thresholds.
//!
//! ## What it never carries
//!
//! Offsets, lengths, categories and integer scores — never a detected value. The
//! span list is not a copy of the note: it says where to look, not what is there.
//! This is also why `spans_digest` covers the `(start, end, category)` triplets and
//! nothing else — covering the matched text would persist a low-entropy fingerprint
//! of personal data in the `.md`, in `.history/`, in `notes.extra_json` and in the
//! object store, and covering `score_m` would let a re-pass of the same model on the
//! same body reset a human review for no reason.
//!
//! ## Integers only
//!
//! `score_m` is a `u16` in thousandths (`round(x * 1000.0) as u16`). `extra` carries
//! no float today, and a block is hashed into the note's `ContentHash`: a `0.94`
//! widened to `f64` and re-serialised as `0.9399999976158142` would change the
//! content hash of an unchanged note. The tolerance is zero.
//!
//! ## The ceiling
//!
//! Past [`MAX_SPANS`] the span list is no longer the whole picture, so the block
//! says so (`status: spans_truncated`) and the note is marked unscanned; `counts` is
//! then the authority, and it is computed over every span the pass reported, not
//! over the truncated list.
//!
//! ## The marker is monotone, and a verdict binds
//!
//! The block carries a human verdict, and the block is what decides the tags. Two
//! rules make that decision safe:
//!
//! - **Only a completed pass, or a rejection, retires a reserved tag.** An
//!   incomplete pass adds the rescan marker and has no authority over the payload
//!   one; a write that recomputes no verdict changes nothing at all.
//! - **A rejection binds the spans it was made about.** While `spans_digest` is
//!   unchanged, a `rejected` verdict keeps the payload marker off, so that the pass
//!   which re-detects the very same spans cannot undo the human judgement that
//!   discarded them. The moment the digest moves, the verdict is spent: `review`
//!   returns to `pending` and the marker is posed normally again.
//!
//! The two interactions are deliberate, and they are the reason the tag decisions
//! are resolved here rather than at the call site: a caller re-posting a note's
//! tags has no way to express either of them.

use gradatum_core::identity::ContentHash;
use gradatum_core::pii::{DetectorIdentity, PiiSpan, PrivacyBudgets};
use gradatum_core::tag::{ReservedTag, Tag};
use sha2::{Digest, Sha256};

/// Key the block is written under in `frontmatter.extra`.
pub(crate) const BLOCK_KEY: &str = "privacy";

/// Tag marking a note whose pass did not complete, or whose span list is no longer
/// the whole picture.
///
/// Held as a [`ReservedTag`] rather than a `&str`: this frontier is one of the two
/// authorities sanctioned to pose it, and naming the capability is what says so. The
/// type is what a caller-supplied string can never produce, since the `Tag`
/// constructors refuse every spelling that coerces onto it.
pub(crate) const UNSCANNED_TAG: ReservedTag = ReservedTag::PRIVACY_UNSCANNED;

/// Tag marking a note a completed pass judged to hold personal data of an activated
/// category.
pub(crate) const CLASSIFIED_TAG: ReservedTag = ReservedTag::PERSONAL_CLASSIFIED;

/// The tags this frontier owns.
///
/// No other authority poses or retires them. A caller re-posting a note's tags as
/// strings cannot retire one — the insertion point re-reports it from the note on
/// disk — and no pass short of a completed one can either.
pub(crate) const RESERVED_TAGS: [ReservedTag; 2] = [CLASSIFIED_TAG, UNSCANNED_TAG];

/// Is `tag` one of the tags this frontier owns?
///
/// The test is on the literal, so a tag read back from a note is recognised whatever
/// produced it — including a note written before this frontier existed. It has to be:
/// [`Tag`]'s `Deserialize` does not validate, deliberately, so a note read back from
/// disk may well hold a `Tag` no constructor would have produced, and this is the
/// frontier that must recognise it.
pub(crate) fn is_reserved(tag: &Tag) -> bool {
    RESERVED_TAGS
        .iter()
        .any(|reserved| reserved.as_ref() == tag.as_str())
}

/// Product ceiling on the spans a block carries.
pub(crate) const MAX_SPANS: usize = 256;

/// Version of the block layout a reader must expect.
const BLOCK_VERSION: i64 = 1;

/// Effective mode recorded by the block. `mask` is delivered disabled: the pass
/// reports, it never rewrites a body.
const MODE_REPORT_ONLY: &str = "report_only";

/// Review state of a freshly written block: a human verdict is a separate act,
/// applied by the review surface, never by a pass.
const REVIEW_PENDING: &str = "pending";

/// Review state of a block a human accepted.
const REVIEW_ACCEPTED: &str = "accepted";

/// Review state of a block a human rejected: the spans are not personal data.
const REVIEW_REJECTED: &str = "rejected";

/// How the block names a pass that completed over the whole of its input.
const STATUS_OK: &str = "ok";

/// How the block names a pass cut short by the span ceiling.
const STATUS_SPANS_TRUNCATED: &str = "spans_truncated";

/// How the block names an input the synchronous path cannot scan within its
/// allowance. A size state, never a detector failure.
const STATUS_INPUT_TOO_LARGE: &str = "input_too_large";

/// How the block names a detector that did not answer. Every incomplete pass that
/// is not a size state lands here: the distinction between a timeout, an
/// unreachable host and a malformed frame is not one a note has to carry.
const STATUS_DETECTOR_UNAVAILABLE: &str = "detector_unavailable";

/// Outcome of a pass, as the block records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// The pass completed over the whole of its input.
    Ok,
    /// The detector did not answer.
    DetectorUnavailable,
    /// The input is larger than the path's per-call byte ceiling.
    InputTooLarge,
    /// The pass completed, and reported more spans than a block carries.
    SpansTruncated,
}

impl Status {
    /// Maps a detector failure onto the state the block records.
    ///
    /// Only the size state is distinguished: `InputTooLarge` is a property of the
    /// body, not a fault of the detector, and conflating the two would make a large
    /// body look like a broken detector.
    pub(crate) fn from_error(err: &gradatum_core::pii::PiiDetectError) -> Self {
        match err {
            gradatum_core::pii::PiiDetectError::InputTooLarge { .. } => Self::InputTooLarge,
            _ => Self::DetectorUnavailable,
        }
    }

    /// The label written to the block.
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => STATUS_OK,
            Self::DetectorUnavailable => STATUS_DETECTOR_UNAVAILABLE,
            Self::InputTooLarge => STATUS_INPUT_TOO_LARGE,
            Self::SpansTruncated => STATUS_SPANS_TRUNCATED,
        }
    }

    /// Does this state mark the note unscanned?
    ///
    /// Every state but a completed pass does: a truncated span list is a pass whose
    /// result is no longer the whole picture, which is the same thing to a reader as
    /// a pass that never finished.
    fn is_unscanned(self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// A built block, and what it implies for the note that carries it.
pub(crate) struct Block {
    /// The block, as it is written into `frontmatter.extra`.
    pub(crate) value: toml::Value,
    /// Whether the note must be marked unscanned.
    pub(crate) unscanned: bool,
}

/// Human verdict a block carries over the spans it records, as the review surface
/// writes it back into `review`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Review {
    /// No verdict yet — the state a pass writes.
    Pending,
    /// A human accepted the marking.
    Accepted,
    /// A human rejected it: the spans are not personal data.
    Rejected,
}

impl Review {
    /// The label written to the block.
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => REVIEW_PENDING,
            Self::Accepted => REVIEW_ACCEPTED,
            Self::Rejected => REVIEW_REJECTED,
        }
    }

    /// Reads the verdict a block carries.
    ///
    /// An absent or unrecognised value reads as [`Review::Pending`]: the field is
    /// data in a note a foreign writer could have produced, and an unknown spelling
    /// must not be able to retire a marker.
    fn of(block: &toml::Value) -> Self {
        match block.get("review").and_then(toml::Value::as_str) {
            Some(REVIEW_ACCEPTED) => Self::Accepted,
            Some(REVIEW_REJECTED) => Self::Rejected,
            _ => Self::Pending,
        }
    }
}

/// What a verdict requires for one reserved tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagAction {
    /// Add the tag when the note does not already carry it.
    Pose,
    /// Remove the tag when the note carries it.
    Retire,
    /// Leave the note's tag exactly as it is.
    Keep,
}

/// What one write decides about the note's block and its two reserved tags.
pub(crate) struct Verdict {
    /// The block to persist.
    pub(crate) block: toml::Value,
    /// What becomes of [`CLASSIFIED_TAG`].
    pub(crate) classified: TagAction,
    /// What becomes of [`UNSCANNED_TAG`].
    pub(crate) unscanned: TagAction,
}

/// The `spans_digest` a block carries, when it carries one.
pub(crate) fn digest_of(block: &toml::Value) -> Option<&str> {
    block.get("spans_digest").and_then(toml::Value::as_str)
}

/// Whether a block carries a rejected verdict that still covers the note's spans.
///
/// A human rejection is about the spans the block records, so it stands until those
/// spans move — and `spans_digest` is the whole of that comparison. A rewrite that
/// recomputes the same spans leaves it unchanged, and the verdict then binds the
/// marker off instead of being undone by the next pass. Without this, path (b) of
/// D-11 would be cancelled by the rewrite that follows it.
pub(crate) fn binds_the_tag(block: &toml::Value, previous: Option<&toml::Value>) -> bool {
    Review::of(block) == Review::Rejected && digest_of(block) == previous.and_then(digest_of)
}

/// Compares two blocks ignoring `scanned_at`.
///
/// The field is the pass's wall clock, so it differs between two passes that decided
/// exactly the same thing. Comparing whole blocks would therefore make every re-scan
/// look like a change, and the sweep's idempotence clause — a re-judgement that
/// decides the same thing writes nothing — would never hold. Every other key is
/// compared, including `spans`, `counts`, `policy` and `review`.
///
/// The exemption is deliberately narrow: it covers one key whose value the frontier
/// re-stamps on every pass, and nothing else. A block that differs anywhere else is a
/// different block, and the write must happen.
pub(crate) fn same_block_modulo_scanned_at(a: &toml::Value, b: &toml::Value) -> bool {
    fn stripped(value: &toml::Value) -> toml::Value {
        let mut clone = value.clone();
        if let Some(root) = clone.as_table_mut() {
            root.remove("scanned_at");
        }
        clone
    }
    stripped(a) == stripped(b)
}

/// Whether a pass reported at least one span a marker can rest on.
///
/// The threshold that decides is the detector's. Activation gates the *mount* — a
/// detector is mounted only once a category is activated — and the spans a mounted
/// detector reports are the ones that cleared its thresholds; the insertion point
/// re-applies no threshold of its own, having none. What it can still assert is the
/// clause's necessary condition: a strictly positive threshold is only ever cleared
/// by a strictly positive score. A span at `score_m == 0` — the shape an observation
/// threshold produces, and the reason `categories = []` tags nothing — poses no
/// marker.
fn markable(spans: &[PiiSpan]) -> bool {
    spans.iter().any(|span| score_m(span.score) > 0)
}

/// The canonical category labels a marker can rest on, sorted and deduplicated.
///
/// Derived from the same predicate as [`markable`] — a strictly positive score — so a
/// count taken here and the marker the note ends up carrying cannot disagree: every
/// label returned is one the pass would tag the note for, and a label the pass found
/// only at score zero is not returned. Sorted and deduplicated because the caller
/// reports these as bounded counter labels, and a label set that varied with the
/// detector's span order would fragment one series into many.
#[must_use]
pub(crate) fn markable_categories(spans: &[PiiSpan]) -> Vec<String> {
    let mut labels: Vec<&'static str> = spans
        .iter()
        .filter(|span| score_m(span.score) > 0)
        .map(|span| span.category.as_str())
        .collect();
    labels.sort_unstable();
    labels.dedup();
    labels.into_iter().map(str::to_owned).collect()
}

/// Applies one tag decision to a note's tags.
///
/// Generic over the tag collection so that no caller has to name its concrete type,
/// and so that no caller can forget one of the three outcomes: [`TagAction::Keep`]
/// is an instruction, not an omission. [`Default`] is what a retirement rebuilds the
/// list through — the collection a note carries is a `SmallVec`, and a slice offers
/// no in-place compaction.
///
/// Takes a [`ReservedTag`], so a caller cannot name an ordinary tag here at all —
/// this function exists to pose and retire the two names the frontier reserves, and its
/// argument type says so.
pub(crate) fn apply_reserved_tag<T>(tags: &mut T, reserved: ReservedTag, action: TagAction)
where
    T: core::ops::DerefMut<Target = [Tag]> + Extend<Tag> + Default,
{
    // Infallible by construction: `Tag::from` on a `ReservedTag` builds the tag
    // directly, and the value is one of two compile-time constants. The fallible
    // `Tag::new` path that used to stand here is gone — and gone with it the
    // `tracing::warn!` that reported a refusal no runtime state could ever produce.
    let tag = Tag::from(reserved);
    match action {
        TagAction::Keep => {}
        TagAction::Pose => {
            if !tags.contains(&tag) {
                tags.extend(core::iter::once(tag));
            }
        }
        TagAction::Retire => {
            if tags.contains(&tag) {
                let kept: Vec<Tag> = tags.iter().filter(|held| **held != tag).cloned().collect();
                *tags = T::default();
                tags.extend(kept);
            }
        }
    }
}

/// Builds the block of one pass.
///
/// `spans` are the pass's validated, fused spans, in body order — empty for a pass
/// that completed and found nothing, and empty for a pass that failed. The ceiling
/// is applied here rather than at the caller: a pass reports what it saw, and the
/// block decides how much of it fits.
///
/// `review` is the human verdict this block carries: [`Review::Pending`] for a
/// freshly judged pass, and the verdict the note already carried when the pass
/// re-reports the very same spans — see [`verdict`], which is what decides it.
pub(crate) fn block(
    identity: &DetectorIdentity,
    status: Status,
    spans: &[PiiSpan],
    budgets: &PrivacyBudgets,
    review: Review,
) -> Block {
    // The ceiling turns a completed pass into a truncated one: it is a property of
    // the record, not of the pass.
    let status = if status == Status::Ok && spans.len() > MAX_SPANS {
        Status::SpansTruncated
    } else {
        status
    };
    let carried = &spans[..spans.len().min(MAX_SPANS)];

    let mut root = toml::value::Table::new();
    root.insert("v".to_owned(), toml::Value::Integer(BLOCK_VERSION));
    root.insert(
        "mode".to_owned(),
        toml::Value::String(MODE_REPORT_ONLY.to_owned()),
    );
    root.insert(
        "status".to_owned(),
        toml::Value::String(status.as_str().to_owned()),
    );
    root.insert(
        "review".to_owned(),
        toml::Value::String(review.as_str().to_owned()),
    );
    // A string, never a TOML datetime: the private representation of a `toml`
    // datetime does not survive the YAML round trip.
    root.insert(
        "scanned_at".to_owned(),
        toml::Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
    root.insert(
        "detector".to_owned(),
        toml::Value::String(identity.detector.clone()),
    );
    root.insert(
        "model_digest".to_owned(),
        toml::Value::String(identity.model_digest.clone()),
    );
    root.insert(
        "model_source".to_owned(),
        toml::Value::String(identity.model_source.clone()),
    );
    root.insert(
        "mapping_v".to_owned(),
        toml::Value::Integer(i64::from(identity.mapping_v)),
    );
    root.insert(
        "sidecar_v".to_owned(),
        toml::Value::String(identity.sidecar_v.clone()),
    );
    root.insert(
        "score_mode".to_owned(),
        toml::Value::String(identity.score_mode.clone()),
    );
    root.insert(
        "policy".to_owned(),
        toml::Value::String(policy_digest(identity, budgets)),
    );
    root.insert(
        "spans_digest".to_owned(),
        toml::Value::String(spans_digest(carried)),
    );
    // Present even when empty: a pass that found nothing and a pass that never ran
    // are different states, and a missing key would collapse them.
    root.insert("counts".to_owned(), toml::Value::Table(counts(spans)));
    root.insert(
        "spans".to_owned(),
        toml::Value::Array(carried.iter().map(span_value).collect()),
    );

    Block {
        value: toml::Value::Table(root),
        unscanned: status.is_unscanned(),
    }
}

/// Resolves what one pass decides for the note that ran it.
///
/// `previous` is the block the note carried before this write, when it carried one.
/// It is the whole of the state this resolution reads: the verdict it holds, and the
/// digest that tells whether that verdict still covers the spans being re-reported.
///
/// The ordering is the one D-11 fixes: the caller re-reports the note's tags before
/// this runs, and the pass recomputes after — so a completed pass is the only thing
/// that can retire what the re-report restored, and it retires only with cause.
pub(crate) fn verdict(
    identity: &DetectorIdentity,
    status: Status,
    spans: &[PiiSpan],
    budgets: &PrivacyBudgets,
    previous: Option<&toml::Value>,
) -> Verdict {
    // The digest covers the spans the block will carry, ceiling included, so a pass
    // that merely re-reports the same thing does not spend a verdict.
    let carried = &spans[..spans.len().min(MAX_SPANS)];
    let digest = spans_digest(carried);
    let review = match previous {
        Some(prev) if digest_of(prev) == Some(digest.as_str()) => Review::of(prev),
        _ => Review::Pending,
    };

    // Built before the tags are decided, because the ceiling is one of the inputs:
    // a pass that reported more spans than a block carries is no longer the whole
    // picture, and the block is what says so.
    let built = block(identity, status, spans, budgets, review);

    let classified = if review == Review::Rejected {
        // The verdict was made about these spans and they have not moved: it binds
        // the marker off rather than being undone by the pass that re-detected them.
        // It outranks the pass, whatever the pass's outcome.
        TagAction::Retire
    } else if built.unscanned {
        // An incomplete pass has no authority over what is personal data: it may add
        // the rescan marker, never retire the payload one.
        TagAction::Keep
    } else if markable(spans) {
        TagAction::Pose
    } else {
        // A completed pass that reports nothing a marker can rest on is the first of
        // the two legitimate removal paths.
        TagAction::Retire
    };

    // The rescan marker states whether the pass completed, and only a completed pass
    // can say it did.
    let unscanned = if built.unscanned {
        TagAction::Pose
    } else {
        TagAction::Retire
    };

    Verdict {
        block: built.value,
        classified,
        unscanned,
    }
}

/// One span, as the block writes it.
fn span_value(span: &PiiSpan) -> toml::Value {
    let mut table = toml::value::Table::new();
    table.insert("start".to_owned(), toml::Value::Integer(span.start as i64));
    table.insert("len".to_owned(), toml::Value::Integer(span.len as i64));
    table.insert(
        "category".to_owned(),
        toml::Value::String(span.category.as_str().to_owned()),
    );
    table.insert(
        "score_m".to_owned(),
        toml::Value::Integer(i64::from(score_m(span.score))),
    );
    toml::Value::Table(table)
}

/// Converts a score in `0.0..=1.0` into integer thousandths.
///
/// Clamped on both ends: a detector is a foreign process, and a score outside the
/// contract must not be able to write a negative count or overflow the cast.
#[must_use]
pub(crate) fn score_m(score: f32) -> u16 {
    if !score.is_finite() {
        return 0;
    }
    (score.clamp(0.0, 1.0) * 1000.0).round() as u16
}

/// Counts of spans per category, over **every** span the pass reported.
///
/// Above the ceiling, the counts are the authority: the span list is cut, the
/// counts are not.
fn counts(spans: &[PiiSpan]) -> toml::value::Table {
    let mut table = toml::value::Table::new();
    for span in spans {
        let key = span.category.as_str().to_owned();
        let seen = table
            .get(&key)
            .and_then(toml::Value::as_integer)
            .unwrap_or(0);
        table.insert(key, toml::Value::Integer(seen + 1));
    }
    table
}

/// `spans_digest` — SHA-256 over the sorted `(start, end, category)` triplets.
///
/// `end` is `start + len`. Domains are `usize` offsets and closed category labels,
/// so the encoding is unambiguous: one triplet per line, colon-separated, in
/// ascending `(start, end, category)` order — independent of the order the pass
/// happened to report spans in, and of the scores it gave them.
#[must_use]
pub(crate) fn spans_digest(spans: &[PiiSpan]) -> String {
    let mut triplets: Vec<(usize, usize, &'static str)> = spans
        .iter()
        .map(|span| {
            (
                span.start,
                span.start.saturating_add(span.len),
                span.category.as_str(),
            )
        })
        .collect();
    triplets.sort_unstable();

    let mut hasher = Sha256::new();
    for (start, end, category) in triplets {
        hasher.update(start.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(end.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(category.as_bytes());
        hasher.update(b"\n");
    }
    format!("sha256:{}", ContentHash(hasher.finalize().into()).hex())
}

/// `policy` — the fingerprint of what this pass ran under.
///
/// Projected over the two things the insertion point knows that change what a pass
/// reports: the detector's declared identity, and the window/ceiling budget it was
/// given. The activation thresholds, the enabled category set and the exceptions are
/// server configuration and do not reach this point yet; when they do, they extend
/// this projection and the digest moves with them — which is the intent, since a
/// note scanned under a different policy should not claim to have been scanned
/// under this one.
#[must_use]
pub(crate) fn policy_digest(identity: &DetectorIdentity, budgets: &PrivacyBudgets) -> String {
    let mut canon = String::new();
    for (key, value) in [
        ("detector", identity.detector.as_str()),
        ("model_digest", identity.model_digest.as_str()),
        ("model_source", identity.model_source.as_str()),
        ("sidecar_v", identity.sidecar_v.as_str()),
        ("score_mode", identity.score_mode.as_str()),
    ] {
        canon.push_str(key);
        canon.push('=');
        canon.push_str(value);
        canon.push('\n');
    }
    canon.push_str("mapping_v=");
    canon.push_str(&identity.mapping_v.to_string());
    canon.push('\n');
    canon.push_str("max_input_bytes=");
    canon.push_str(&budgets.max_input_bytes.to_string());
    canon.push('\n');
    canon.push_str("window_overlap_bytes=");
    canon.push_str(&budgets.window_overlap_bytes.to_string());
    canon.push('\n');

    let mut hasher = Sha256::new();
    hasher.update(canon.as_bytes());
    format!("sha256:{}", ContentHash(hasher.finalize().into()).hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::pii::PiiCategory;

    fn identity() -> DetectorIdentity {
        DetectorIdentity::new(
            "a-detector".to_owned(),
            "sha256:00".to_owned(),
            "builtin".to_owned(),
            1,
            String::new(),
            "calibrated-span".to_owned(),
        )
    }

    fn span(start: usize, len: usize) -> PiiSpan {
        PiiSpan::new(start, len, PiiCategory::Email, 0.5)
    }

    #[test]
    fn a_tag_read_back_from_a_note_is_recognised_as_reserved() {
        for reserved in [CLASSIFIED_TAG, UNSCANNED_TAG] {
            // Reached through `Deserialize` and not through `Tag::from`: this is the
            // shape a note read back from disk arrives in, and the whole reason the
            // frontier tests on the literal instead of on provenance. Neither
            // constructor would hand back these tags any more — that is the point of
            // the guard this test sits under.
            let tag: Tag =
                serde_json::from_value(serde_json::Value::String(reserved.as_ref().to_owned()))
                    .expect("a tag deserialises without validation");
            assert!(
                is_reserved(&tag),
                "the test is on the literal, so a tag a note already carries is \
                 recognised without being rebuilt"
            );
        }
        assert!(!is_reserved(&Tag::new("a-note-tag").expect("a note tag")));
    }

    #[test]
    fn a_score_is_converted_to_integer_thousandths() {
        assert_eq!(score_m(0.0), 0);
        assert_eq!(score_m(1.0), 1000);
        assert_eq!(score_m(0.94), 940);
        assert_eq!(score_m(0.9399), 940);
        assert_eq!(score_m(0.0004), 0);
    }

    #[test]
    fn a_score_outside_the_contract_is_clamped_never_wrapped() {
        assert_eq!(score_m(-3.0), 0, "a negative score is not a negative count");
        assert_eq!(score_m(42.0), 1000);
        assert_eq!(score_m(f32::NAN), 0);
        assert_eq!(score_m(f32::INFINITY), 0, "an infinite score is no score");
    }

    #[test]
    fn the_digest_ignores_order_and_scores() {
        let ascending = vec![span(0, 3), span(9, 2)];
        let descending = vec![span(9, 2), span(0, 3)];
        assert_eq!(spans_digest(&ascending), spans_digest(&descending));

        let scored = vec![
            PiiSpan::new(0, 3, PiiCategory::Email, 0.99),
            PiiSpan::new(9, 2, PiiCategory::Email, 0.01),
        ];
        assert_eq!(spans_digest(&ascending), spans_digest(&scored));
    }

    #[test]
    fn the_digest_moves_with_a_triplet() {
        let base = spans_digest(&[span(0, 3)]);
        assert_ne!(base, spans_digest(&[span(1, 3)]), "start is covered");
        assert_ne!(base, spans_digest(&[span(0, 4)]), "length is covered");
        assert_ne!(
            base,
            spans_digest(&[PiiSpan::new(0, 3, PiiCategory::Phone, 0.5)]),
            "category is covered"
        );
        let empty = spans_digest(&[]);
        assert_ne!(
            base, empty,
            "a pass that found nothing is not a pass that saw one"
        );
        assert!(
            empty.starts_with("sha256:") && empty.len() == 71,
            "an empty pass has a digest of its own, not an absent one"
        );
    }

    #[test]
    fn the_policy_moves_with_the_identity_and_the_budget() {
        let budgets = PrivacyBudgets::default();
        let base = policy_digest(&identity(), &budgets);
        assert!(base.starts_with("sha256:") && base.len() == 71);

        let mut renamed = identity();
        renamed.detector = "another-detector".to_owned();
        assert_ne!(base, policy_digest(&renamed, &budgets));

        let other_window = PrivacyBudgets::new(1_024, 512, 10_000, 2_000, 0, 20_000);
        assert_ne!(base, policy_digest(&identity(), &other_window));
        assert_eq!(
            base,
            policy_digest(&identity(), &PrivacyBudgets::default()),
            "the same identity under the same budget is the same policy"
        );
    }

    #[test]
    fn a_truncated_pass_reports_the_whole_count_over_the_cut_list() {
        let spans: Vec<PiiSpan> = (0..MAX_SPANS + 1).map(|i| span(i, 1)).collect();
        let built = block(
            &identity(),
            Status::Ok,
            &spans,
            &PrivacyBudgets::default(),
            Review::Pending,
        );
        let root = built.value.as_table().expect("a table");

        assert_eq!(
            root["status"].as_str(),
            Some(STATUS_SPANS_TRUNCATED),
            "the ceiling is not a completed pass"
        );
        assert!(built.unscanned, "a truncated list marks the note unscanned");
        assert_eq!(
            root["spans"].as_array().map(Vec::len),
            Some(MAX_SPANS),
            "the list is cut at the ceiling"
        );
        assert_eq!(
            root["counts"]["email"].as_integer(),
            Some(MAX_SPANS as i64 + 1),
            "the counts cover the whole pass, not the cut list"
        );
    }

    #[test]
    fn a_failed_pass_still_carries_a_block_and_an_empty_count() {
        let built = block(
            &identity(),
            Status::DetectorUnavailable,
            &[],
            &PrivacyBudgets::default(),
            Review::Pending,
        );
        let root = built.value.as_table().expect("a table");
        assert_eq!(root["status"].as_str(), Some(STATUS_DETECTOR_UNAVAILABLE));
        assert!(built.unscanned);
        assert_eq!(
            root["counts"].as_table().map(toml::value::Table::is_empty),
            Some(true),
            "a failed pass counts nothing, and says so"
        );
        assert_eq!(
            root.keys().count(),
            15,
            "the layout is the same whatever the outcome"
        );
    }

    #[test]
    fn a_size_state_is_not_a_detector_failure() {
        let too_large = gradatum_core::pii::PiiDetectError::InputTooLarge { len: 10, budget: 4 };
        assert_eq!(Status::from_error(&too_large), Status::InputTooLarge);

        let timed_out = gradatum_core::pii::PiiDetectError::Timeout { timeout_ms: 20 };
        assert_eq!(Status::from_error(&timed_out), Status::DetectorUnavailable);
    }

    #[test]
    fn no_float_reaches_the_block() {
        fn find_float(value: &toml::Value) -> bool {
            match value {
                toml::Value::Float(_) => true,
                toml::Value::Array(items) => items.iter().any(find_float),
                toml::Value::Table(map) => map.values().any(find_float),
                _ => false,
            }
        }
        let built = block(
            &identity(),
            Status::Ok,
            &[PiiSpan::new(3, 5, PiiCategory::Email, 0.9399)],
            &PrivacyBudgets::default(),
            Review::Pending,
        );
        assert!(
            !find_float(&built.value),
            "a float in `extra` would move the content hash on the first re-serialiser \
             that renders it differently"
        );
    }

    /// A block as a note would carry it, with its review replaced by `review`.
    fn block_with_review(spans: &[PiiSpan], status: Status, review: &str) -> toml::Value {
        let mut value = block(
            &identity(),
            status,
            spans,
            &PrivacyBudgets::default(),
            Review::Pending,
        )
        .value;
        value
            .as_table_mut()
            .expect("a table")
            .insert("review".to_owned(), toml::Value::String(review.to_owned()));
        value
    }

    /// The ordinary tags of a note, in order.
    ///
    /// Panics on a literal the constructor refuses rather than dropping it: a reserved
    /// name cannot be posed through `Tag::new` any more, and a silently shortened list
    /// would turn a test that means to carry one into a test that carries none.
    fn tags(literals: &[&str]) -> Vec<Tag> {
        literals
            .iter()
            .map(|literal| Tag::new(*literal).expect("an ordinary tag literal"))
            .collect()
    }

    #[test]
    fn a_completed_pass_with_a_markable_span_poses_the_marker() {
        let decided = verdict(
            &identity(),
            Status::Ok,
            &[span(0, 3)],
            &PrivacyBudgets::default(),
            None,
        );
        assert_eq!(decided.classified, TagAction::Pose);
        assert_eq!(decided.unscanned, TagAction::Retire);
        assert_eq!(
            decided.block.get("review").and_then(toml::Value::as_str),
            Some(REVIEW_PENDING)
        );
    }

    #[test]
    fn a_completed_pass_without_a_markable_span_retires_the_marker() {
        let decided = verdict(
            &identity(),
            Status::Ok,
            &[],
            &PrivacyBudgets::default(),
            None,
        );
        assert_eq!(
            decided.classified,
            TagAction::Retire,
            "path (a) of the marker's removal is a completed pass with nothing to mark"
        );
        assert_eq!(decided.unscanned, TagAction::Retire);
    }

    #[test]
    fn a_span_at_zero_score_does_not_mark_the_note() {
        let observation = [PiiSpan::new(0, 3, PiiCategory::Email, 0.0)];
        assert!(!markable(&observation), "score_m == 0 clears no threshold");
        assert_eq!(
            verdict(
                &identity(),
                Status::Ok,
                &observation,
                &PrivacyBudgets::default(),
                None
            )
            .classified,
            TagAction::Retire,
            "a zero-score observation is the shape `categories = []` produces, and it \
             poses nothing"
        );
    }

    #[test]
    fn an_incomplete_pass_never_retires_the_marker() {
        for status in [Status::DetectorUnavailable, Status::InputTooLarge] {
            let decided = verdict(
                &identity(),
                status,
                &[span(0, 3)],
                &PrivacyBudgets::default(),
                None,
            );
            assert_eq!(
                decided.classified,
                TagAction::Keep,
                "an incomplete pass is not an authority on what is personal data"
            );
            assert_eq!(decided.unscanned, TagAction::Pose);
        }
    }

    #[test]
    fn the_review_survives_a_pass_over_the_same_spans() {
        let spans = [span(0, 3)];
        let accepted = block_with_review(&spans, Status::Ok, REVIEW_ACCEPTED);

        let decided = verdict(
            &identity(),
            Status::Ok,
            &spans,
            &PrivacyBudgets::default(),
            Some(&accepted),
        );
        assert_eq!(
            decided.block.get("review").and_then(toml::Value::as_str),
            Some(REVIEW_ACCEPTED),
            "a re-pass over the same spans is not a reason to undo a human acceptance"
        );
        assert_eq!(decided.classified, TagAction::Pose);

        let moved = verdict(
            &identity(),
            Status::Ok,
            &[span(9, 3)],
            &PrivacyBudgets::default(),
            Some(&accepted),
        );
        assert_eq!(
            moved.block.get("review").and_then(toml::Value::as_str),
            Some(REVIEW_PENDING),
            "the verdict was about the spans that moved, and it is spent with them"
        );
    }

    #[test]
    fn a_rejected_verdict_binds_the_pass_that_redetects_the_same_spans() {
        let spans = [span(0, 3)];
        let rejected = block_with_review(&spans, Status::Ok, REVIEW_REJECTED);

        let decided = verdict(
            &identity(),
            Status::Ok,
            &spans,
            &PrivacyBudgets::default(),
            Some(&rejected),
        );
        assert_eq!(
            decided.classified,
            TagAction::Retire,
            "path (b) of the marker's removal stands until the spans it judged move"
        );

        let moved = verdict(
            &identity(),
            Status::Ok,
            &[span(9, 3)],
            &PrivacyBudgets::default(),
            Some(&rejected),
        );
        assert_eq!(
            moved.classified,
            TagAction::Pose,
            "a rejection is about its spans; other spans are judged afresh"
        );
    }

    #[test]
    fn the_digest_decides_whether_a_verdict_still_covers_the_note() {
        let spans = [span(0, 3)];
        let rejected = block_with_review(&spans, Status::Ok, REVIEW_REJECTED);
        assert!(binds_the_tag(&rejected, Some(&rejected)));

        assert!(
            !binds_the_tag(&rejected, None),
            "a verdict with nothing to have been made about binds nothing"
        );

        let other = block_with_review(&[span(9, 3)], Status::Ok, REVIEW_REJECTED);
        assert!(!binds_the_tag(&other, Some(&rejected)));

        let pending = block_with_review(&spans, Status::Ok, REVIEW_PENDING);
        assert!(!binds_the_tag(&pending, Some(&rejected)));

        let unknown = block_with_review(&spans, Status::Ok, "a-foreign-spelling");
        assert!(
            !binds_the_tag(&unknown, Some(&unknown)),
            "an unrecognised verdict reads as no verdict, never as a rejection"
        );
        assert_eq!(Review::of(&unknown), Review::Pending);
    }

    #[test]
    fn a_pass_over_the_ceiling_is_an_incomplete_pass() {
        // Every span at score 0.0, so nothing can be posed; the count is what puts
        // the pass past the ceiling. A list that is not the whole picture is the
        // same thing to a reader as a pass that never finished.
        let spans: Vec<PiiSpan> = (0..MAX_SPANS + 1)
            .map(|i| PiiSpan::new(i, 1, PiiCategory::Email, 0.0))
            .collect();
        let decided = verdict(
            &identity(),
            Status::Ok,
            &spans,
            &PrivacyBudgets::default(),
            None,
        );
        assert_eq!(
            decided.classified,
            TagAction::Keep,
            "the ceiling turns a completed pass into an incomplete one, and an \
             incomplete pass retires nothing"
        );
        assert_eq!(
            decided.unscanned,
            TagAction::Pose,
            "a truncated list is a note to rescan"
        );
    }

    #[test]
    fn a_tag_action_is_applied_exactly_once() {
        let mut held = vec![Tag::from(CLASSIFIED_TAG)];
        apply_reserved_tag(&mut held, CLASSIFIED_TAG, TagAction::Pose);
        assert_eq!(
            held.len(),
            1,
            "posing a tag the note carries changes nothing"
        );

        let mut empty = tags(&["a-note-tag"]);
        apply_reserved_tag(&mut empty, CLASSIFIED_TAG, TagAction::Pose);
        assert_eq!(
            empty.iter().map(Tag::as_str).collect::<Vec<_>>(),
            vec!["a-note-tag", CLASSIFIED_TAG.as_ref()],
            "posing appends, and leaves the note's own tags alone"
        );

        apply_reserved_tag(&mut empty, CLASSIFIED_TAG, TagAction::Retire);
        assert_eq!(
            empty.iter().map(Tag::as_str).collect::<Vec<_>>(),
            vec!["a-note-tag"]
        );

        apply_reserved_tag(&mut empty, UNSCANNED_TAG, TagAction::Keep);
        assert_eq!(
            empty.iter().map(Tag::as_str).collect::<Vec<_>>(),
            vec!["a-note-tag"],
            "keeping is an instruction, not an omission"
        );
    }
}
