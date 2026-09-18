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
use gradatum_core::pii::{DetectorIdentity, PiiSpan, PrivacyBudgets, PrivacyPolicy, score_m};
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

/// Effective mode recorded by the block when the policy reports and never rewrites.
const MODE_REPORT_ONLY: &str = "report_only";

/// Effective mode recorded by the block when the policy rewrites the bodies it
/// scans.
///
/// Read from the policy, never from a constant: the mode a note carries is the mode
/// its pass actually ran under, and a note that claims `report_only` while its body
/// was rewritten is a false record of what happened to it.
const MODE_MASK: &str = "mask";

/// Review state of a freshly written block: a human verdict is a separate act,
/// applied by the review surface, never by a pass.
/// Key the human verdict is written under inside the block.
const REVIEW_KEY: &str = "review";

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
        match block.get(REVIEW_KEY).and_then(toml::Value::as_str) {
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

/// Whether an incoming block is a verdict stated over a block the note already carries:
/// the recorded block with [`REVIEW_KEY`] moved, and nothing else.
///
/// A block that enters a write is caller-supplied data on every path that has one, and
/// the pass resolves a verdict out of it. What a caller may *state* is therefore bounded
/// here rather than assumed: a block may only move the human verdict over a block the
/// note already carries. A block that differs anywhere else is one the caller composed
/// rather than one the note is shown to have received, and it reads as no verdict at all
/// — the pass then re-judges from what is on disk, which is what it does when no block
/// enters. Both directions of the bound matter: it is what lets the review surface record
/// a verdict instead of having it recomputed, and it is what keeps a composed block from
/// outranking a pass over spans no one judged.
pub(crate) fn moves_only_the_review(incoming: &toml::Value, recorded: &toml::Value) -> bool {
    let (Some(incoming), Some(recorded)) = (incoming.as_table(), recorded.as_table()) else {
        return false;
    };
    incoming.len() == recorded.len()
        && incoming
            .iter()
            .all(|(key, value)| key == REVIEW_KEY || recorded.get(key) == Some(value))
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

/// Whether a pass reported at least one span the activation policy marks.
///
/// The decision is the policy's, and only the policy's: a span marks when its
/// category is enabled **and** its threshold is strictly positive **and** its
/// `score_m` clears that threshold. Three ways to fail, and each is a real state:
///
/// - the category is not in the enabled set — the detector may well have reported it
///   (it reports what it sees), but a category the operator did not activate poses no
///   marker;
/// - the threshold is `0` — the observation point, which measures a distribution and
///   never tags. This is what keeps `categories = []`, or a threshold set the campaign
///   has not calibrated yet, from tagging a whole corpus;
/// - the score is below a positive threshold.
///
/// A regex span is not a special case here: it carries the top of the scale, and
/// clearing every threshold a policy can declare is exactly what that means.
pub(crate) fn markable(spans: &[PiiSpan], policy: &PrivacyPolicy) -> bool {
    spans.iter().any(|span| policy.marks(span))
}

/// The canonical category labels a marker can rest on, sorted and deduplicated.
///
/// Derived from the same predicate as [`markable`] — the policy's marking decision —
/// so a count taken here and the marker the note ends up carrying cannot disagree:
/// every label returned is one the pass would tag the note for, and a label the pass
/// found only under its threshold, or for a category it did not activate, is not
/// returned. Sorted and deduplicated because the caller reports these as bounded
/// counter labels, and a label set that varied with the detector's span order would
/// fragment one series into many.
#[must_use]
pub(crate) fn markable_categories(spans: &[PiiSpan], policy: &PrivacyPolicy) -> Vec<String> {
    let mut labels: Vec<&'static str> = spans
        .iter()
        .filter(|span| policy.marks(span))
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
/// The **whole** span list is recorded, scores included, whatever the policy marks:
/// the block is the matter a later calibration reads, and a span the activation
/// thresholds left unmarked is precisely the observation the campaign is made of.
/// What the policy decides is the tags — see [`verdict`] — never what is written
/// down here. One exception, and it is the mode's: under `mask` the list is emptied,
/// because the text those offsets describe was replaced by the time this block is
/// persisted.
///
/// `review` is the human verdict this block carries: [`Review::Pending`] for a
/// freshly judged pass, and the verdict the note already carried when the pass
/// re-reports the very same spans — see [`verdict`], which is what decides it.
///
/// `policy` is recorded, not applied: the mode it ran under is written verbatim, and
/// its fingerprint goes into `policy`, so a note scanned under one activation is
/// distinguishable from the same note scanned under another.
pub(crate) fn block(
    identity: &DetectorIdentity,
    status: Status,
    spans: &[PiiSpan],
    budgets: &PrivacyBudgets,
    policy: &PrivacyPolicy,
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
        toml::Value::String(
            if policy.masks() {
                MODE_MASK
            } else {
                MODE_REPORT_ONLY
            }
            .to_owned(),
        ),
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
        toml::Value::String(policy_digest(identity, budgets, policy)),
    );
    root.insert(
        "spans_digest".to_owned(),
        toml::Value::String(spans_digest(carried)),
    );
    // Present even when empty: a pass that found nothing and a pass that never ran
    // are different states, and a missing key would collapse them.
    root.insert("counts".to_owned(), toml::Value::Table(counts(spans)));
    // Emptied by the mode that destroyed what the list pointed at: under `mask` the
    // offsets describe text the note no longer holds, and a block that kept them would
    // be aiming a later reader — a review surface, a calibration export — at the filler.
    // The counts survive, because a per-category tally names no offset, and so does the
    // digest, which is what the next pass compares against.
    root.insert(
        "spans".to_owned(),
        toml::Value::Array(if policy.masks() {
            Vec::new()
        } else {
            carried.iter().map(span_value).collect()
        }),
    );

    Block {
        value: toml::Value::Table(root),
        unscanned: status.is_unscanned(),
    }
}

/// Resolves what one pass decides for the note that ran it.
///
/// `previous` is the block this write states, when it states one — the block the note
/// carried before this write for every ordinary path, and the block the caller posts
/// when it is the review surface moving `review` (D-13). It is the whole of the state
/// this resolution reads: the verdict it holds, and the digest that tells whether that
/// verdict still covers the spans being re-reported. The digest is the guard — a block
/// that declares itself `rejected` over spans the pass no longer reports has its verdict
/// spent like any other.
///
/// The ordering is the one D-11 fixes: the caller re-reports the note's tags before
/// this runs, and the pass recomputes after — so a completed pass is the only thing
/// that can retire what the re-report restored, and it retires only with cause.
pub(crate) fn verdict(
    identity: &DetectorIdentity,
    status: Status,
    spans: &[PiiSpan],
    budgets: &PrivacyBudgets,
    policy: &PrivacyPolicy,
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
    let built = block(identity, status, spans, budgets, policy, review);

    let classified = if review == Review::Rejected {
        // The verdict was made about these spans and they have not moved: it binds
        // the marker off rather than being undone by the pass that re-detected them.
        // It outranks the pass, whatever the pass's outcome.
        TagAction::Retire
    } else if built.unscanned {
        // An incomplete pass has no authority over what is personal data: it may add
        // the rescan marker, never retire the payload one.
        TagAction::Keep
    } else if markable(spans, policy) {
        TagAction::Pose
    } else {
        // A completed pass that reports nothing the activation marks is the first of
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
/// Projected over everything that changes what a pass reports or what it marks: the
/// detector's declared identity, the window/ceiling budget it was given, and the
/// activation the pass applied — the enabled category set, each threshold in
/// thousandths, and the exception patterns per tenant.
///
/// **Why the exceptions are covered without being published.** The exception lists
/// are literal personal data by nature, and the digest is the one place they could
/// leak: a fingerprint that rendered their text would put them in the `.md`, in
/// `.history/`, in the index and in the object store. They go into the pre-image and
/// never out of the result — the block carries the hash and nothing else, which is
/// what makes "the same note scanned under a different activation" a visible
/// difference and the activation itself unreadable.
///
/// The iteration order is canonical by construction: `BTreeMap` for the categories
/// and for the tenants, declaration order for the patterns inside one tenant. Two
/// policies that differ only in the order their configuration listed them are the
/// same policy, and must fingerprint alike — otherwise every reload of an unchanged
/// `server.toml` would look like a policy change.
#[must_use]
pub(crate) fn policy_digest(
    identity: &DetectorIdentity,
    budgets: &PrivacyBudgets,
    policy: &PrivacyPolicy,
) -> String {
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
    canon.push_str("mode=");
    canon.push_str(if policy.masks() {
        MODE_MASK
    } else {
        MODE_REPORT_ONLY
    });
    canon.push('\n');
    for category in policy.enabled_categories() {
        canon.push_str("category=");
        canon.push_str(category.as_str());
        canon.push(':');
        // Unreachable for a category the policy enabled — every enabled category has
        // a threshold by construction — and rendered as the uncalibrated point rather
        // than skipped, so the pre-image's shape never depends on the map's contents.
        canon.push_str(&policy.threshold_m(category).unwrap_or_default().to_string());
        canon.push('\n');
    }
    for (tenant, patterns) in policy.exceptions() {
        canon.push_str("exception_tenant=");
        canon.push_str(tenant);
        canon.push('\n');
        for pattern in patterns {
            canon.push_str("exception=");
            canon.push_str(pattern);
            canon.push('\n');
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(canon.as_bytes());
    format!("sha256:{}", ContentHash(hasher.finalize().into()).hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::pii::{PiiCategory, THRESHOLD_MAX_M};

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

    /// The activation the tests below run under.
    ///
    /// Marking is the policy's decision, so a test that stated none would run against
    /// the empty policy — which marks nothing at all. This one enables the four
    /// categories this suite exercises at a threshold of one thousandth: a span at
    /// `0.5` clears it, and a span at `0.0` does not, which is the distinction the
    /// `observation threshold ≠ marking threshold` rule rests on.
    fn active_policy() -> PrivacyPolicy {
        let mut thresholds = std::collections::BTreeMap::new();
        for category in [
            PiiCategory::Email,
            PiiCategory::Phone,
            PiiCategory::Secret,
            PiiCategory::AccountNumber,
        ] {
            thresholds.insert(category, 1);
        }
        PrivacyPolicy::new(false, thresholds, std::collections::BTreeMap::new())
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
    fn a_threshold_above_the_span_score_leaves_the_note_unmarked() {
        // A configuration that carries `thresholds` has to produce its effect: the
        // same pass, the same span, and a different verdict on either side of the
        // threshold. The comparison is on integer thousandths — 0.5 is 500 — and it is
        // inclusive, so a threshold set exactly on the score still marks.
        let marked = [PiiSpan::new(0, 3, PiiCategory::Email, 0.5)];
        for (threshold, expected) in [
            (0, TagAction::Retire), // not calibrated: observed, never marked
            (1, TagAction::Pose),
            (500, TagAction::Pose), // inclusive
            (501, TagAction::Retire),
            (THRESHOLD_MAX_M, TagAction::Retire),
        ] {
            let mut thresholds = std::collections::BTreeMap::new();
            thresholds.insert(PiiCategory::Email, threshold);
            let policy = PrivacyPolicy::new(false, thresholds, std::collections::BTreeMap::new());

            let decided = verdict(
                &identity(),
                Status::Ok,
                &marked,
                &PrivacyBudgets::default(),
                &policy,
                None,
            );
            assert_eq!(
                decided.classified, expected,
                "a threshold of {threshold} thousandths against a span at 500"
            );
        }
    }

    #[test]
    fn a_category_the_configuration_does_not_enable_is_observed_but_never_marked() {
        // The regex floor reports four categories whatever the configuration says.
        // An operator whose activation list omits a category must not see the note
        // tagged for a span of that category the detector happened to find in the
        // same body — however unambiguous the pattern, and however high its score.
        let spans = [PiiSpan::new(0, 3, PiiCategory::Address, 1.0)];
        assert!(
            active_policy().threshold_m(PiiCategory::Address).is_none(),
            "the activation these tests run under does not enable this category"
        );
        let decided = verdict(
            &identity(),
            Status::Ok,
            &spans,
            &PrivacyBudgets::default(),
            &active_policy(),
            None,
        );
        assert_eq!(decided.classified, TagAction::Retire);
        assert!(
            markable_categories(&spans, &active_policy()).is_empty(),
            "the reporter counts the categories the activation marks, not the ones the \
             detector reported"
        );

        let enabled = [PiiSpan::new(0, 3, PiiCategory::Phone, 1.0)];
        let mut thresholds = std::collections::BTreeMap::new();
        thresholds.insert(PiiCategory::Phone, 900);
        let policy = PrivacyPolicy::new(false, thresholds, std::collections::BTreeMap::new());
        assert_eq!(
            verdict(
                &identity(),
                Status::Ok,
                &enabled,
                &PrivacyBudgets::default(),
                &policy,
                None,
            )
            .classified,
            TagAction::Pose,
            "a regex span carries the top of the scale and is not subject to the threshold"
        );
    }

    #[test]
    fn the_block_records_the_mode_the_policy_ran_under() {
        let report = block(
            &identity(),
            Status::Ok,
            &[span(0, 3)],
            &PrivacyBudgets::default(),
            &active_policy(),
            Review::Pending,
        );
        assert_eq!(
            report.value.get("mode").and_then(toml::Value::as_str),
            Some(MODE_REPORT_ONLY)
        );

        let masking = {
            let mut thresholds = std::collections::BTreeMap::new();
            thresholds.insert(PiiCategory::Email, 1);
            PrivacyPolicy::new(true, thresholds, std::collections::BTreeMap::new())
        };
        let masked = block(
            &identity(),
            Status::Ok,
            &[span(0, 3)],
            &PrivacyBudgets::default(),
            &masking,
            Review::Pending,
        );
        assert_eq!(
            masked.value.get("mode").and_then(toml::Value::as_str),
            Some(MODE_MASK),
            "the mode a note carries is the mode its pass actually ran under"
        );
    }

    #[test]
    fn the_policy_digest_moves_with_the_activation_but_never_spells_it() {
        let budgets = PrivacyBudgets::default();
        let base = policy_digest(&identity(), &budgets, &active_policy());

        let mut other_threshold = std::collections::BTreeMap::new();
        other_threshold.insert(PiiCategory::Email, 900);
        assert_ne!(
            base,
            policy_digest(
                &identity(),
                &budgets,
                &PrivacyPolicy::new(false, other_threshold, std::collections::BTreeMap::new())
            ),
            "a threshold the campaign moved is a policy the note did not run under"
        );

        let mut exceptions = std::collections::BTreeMap::new();
        exceptions.insert("main".to_owned(), vec!["example.com".to_owned()]);
        let mut thresholds = std::collections::BTreeMap::new();
        thresholds.insert(PiiCategory::Email, 1);
        let with_exception = policy_digest(
            &identity(),
            &budgets,
            &PrivacyPolicy::new(false, thresholds, exceptions),
        );
        assert_ne!(base, with_exception, "the exceptions are covered");
        assert!(
            !with_exception.contains("example.com"),
            "the digest is a hash: the patterns go into the pre-image, never out of \
             the result"
        );
        assert_eq!(
            base,
            policy_digest(&identity(), &budgets, &active_policy()),
            "the same activation fingerprints the same way"
        );
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
        let base = policy_digest(&identity(), &budgets, &active_policy());
        assert!(base.starts_with("sha256:") && base.len() == 71);

        let mut renamed = identity();
        renamed.detector = "another-detector".to_owned();
        assert_ne!(base, policy_digest(&renamed, &budgets, &active_policy()));

        let other_window = PrivacyBudgets::new(1_024, 512, 10_000, 2_000, 0, 20_000);
        assert_ne!(
            base,
            policy_digest(&identity(), &other_window, &active_policy())
        );
        assert_eq!(
            base,
            policy_digest(&identity(), &PrivacyBudgets::default(), &active_policy()),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
        assert!(
            !markable(&observation, &active_policy()),
            "score_m == 0 clears no threshold"
        );
        assert_eq!(
            verdict(
                &identity(),
                Status::Ok,
                &observation,
                &PrivacyBudgets::default(),
                &active_policy(),
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
                &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
            &active_policy(),
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
