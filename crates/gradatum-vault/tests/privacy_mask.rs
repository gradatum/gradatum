//! F-160 — the `mask` mode: what the policy destroys, what it spares, and what it
//! refuses.
//!
//! ## What these tests measure
//!
//! The reported mode rewrites the text it matched. That is the one irreversible act of
//! the frontier, so every claim about it is measured on bytes rather than on a flag:
//! the body read back, and the file on disk.
//!
//! - **The masked body is the persisted body.** A needle the activation marks is gone
//!   from the note, from the file the vault wrote, and from the digest the note carries.
//!   The digest is recomputed from what the write returned, so a hash that described the
//!   original would be caught here rather than by a later optimistic-lock mismatch.
//! - **The activation decides what is destroyed, span by span.** A mode set to `mask`
//!   over a threshold of `0` masks nothing, and two spans of the same category are
//!   masked or spared according to their own `score_m`. Without this, "the policy masks"
//!   would be indistinguishable from "the detector's output is destroyed wholesale" —
//!   and the thresholds calibrated by the campaign would stop constraining the
//!   destruction they were measured to bound.
//! - **A pass that did not complete is refused.** Under `mask`, an unanswered detector
//!   refuses the write instead of persisting a body nothing judged. Under `report_only`
//!   the same failure still writes: the two modes differ exactly there.
//! - **Nothing is left behind.** A masking rewrite leaves no clear copy of what it
//!   replaced — not in the note, and not in the version history the write path keeps for
//!   every other edit.
//!
//! ## What is out of reach here
//!
//! The detector is a fixture rather than a model, so the *quality* of the spans is not
//! measured — only what the write path does with the spans a policy marks. The embedding
//! path, the boot announcement and the two gate refusals live in their own crates.

mod common;
use common::build_minimal_frontmatter;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::note::Note;
use gradatum_core::pii::{
    DetectInput, Detection, MASK_FILLER, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
    PrivacyBudgets, PrivacyPolicy,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_vault::{Vault, VaultError};
use tempfile::TempDir;

/// The needle every fixture looks for. A literal rather than a shape: the assertion is
/// on the exact bytes, and a regex would leave the test unable to say which text it
/// expected to survive.
const NEEDLE: &str = "alice@example.com";

/// The marker a completed pass poses on a note holding personal data.
const CLASSIFIED: &str = "personal-classified";

/// The marker an incomplete pass poses, so a later sweep finds the note again.
const UNSCANNED: &str = "privacy-unscanned";

/// Key the block is written under in `frontmatter.extra`.
const BLOCK_KEY: &str = "privacy";

/// Number of spans a block carries, mirroring the ceiling the block applies.
///
/// Restated here rather than imported: the ceiling lives in the crate's private
/// surface, and a test that read it would be asserting the constant against itself.
/// What is measured below is how many passages the rewrite destroys on a pass that
/// reported more spans than this — so the number has to be stated, not fetched.
const BLOCK_SPAN_CEILING: usize = 256;

// ── Detectors ────────────────────────────────────────────────────────────────

/// Reports every occurrence of one needle, at the score it is given.
///
/// The score is a parameter because every threshold in this product is an integer in
/// thousandths compared against the span's `score_m`: a fixture that always reported
/// `1.0` could not tell "the threshold is applied to the masking" from "the masking
/// ignores the threshold".
struct Needle {
    needle: &'static str,
    category: PiiCategory,
    score: f32,
}

impl Needle {
    fn new(needle: &'static str, category: PiiCategory, score: f32) -> Self {
        Self {
            needle,
            category,
            score,
        }
    }
}

impl PiiDetector for Needle {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let mut spans = Vec::new();
        let mut from = 0usize;
        while let Some(offset) = input.text[from..].find(self.needle) {
            let start = from + offset;
            spans.push(PiiSpan::new(
                start,
                self.needle.len(),
                self.category,
                self.score,
            ));
            from = start + self.needle.len();
        }
        Ok(Detection::new(spans, 0))
    }
}

/// Reports one span per needle, each at its own score — the fixture that separates the
/// spans a threshold marks from the ones it leaves.
struct TwoNeedles {
    high: &'static str,
    low: &'static str,
    category: PiiCategory,
}

impl PiiDetector for TwoNeedles {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let mut spans = Vec::new();
        for (needle, score) in [(self.high, 1.0f32), (self.low, 0.4f32)] {
            let mut from = 0usize;
            while let Some(offset) = input.text[from..].find(needle) {
                let start = from + offset;
                spans.push(PiiSpan::new(start, needle.len(), self.category, score));
                from = start + needle.len();
            }
        }
        spans.sort_by_key(|span| span.start);
        Ok(Detection::new(spans, 0))
    }
}

/// Reports one single-byte span every other byte, up to `count` of them.
///
/// The only way to hand the write path more spans than a block carries: the ceiling is a
/// property of the record, so a body small enough to be scanned in one window has to
/// carry the spans densely.
struct DenseSpans {
    count: usize,
    category: PiiCategory,
}

impl PiiDetector for DenseSpans {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let spans = (0..self.count)
            .map(|i| PiiSpan::new(i * 2, 1, self.category, 1.0))
            .filter(|span| span.start + span.len <= input.text.len())
            .collect();
        Ok(Detection::new(spans, 0))
    }
}

/// A detector that never answers: an incomplete pass that is not a size state.
struct TimedOut;

impl PiiDetector for TimedOut {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Err(PiiDetectError::Timeout { timeout_ms: 10 })
    }
}

// ── Policies ─────────────────────────────────────────────────────────────────

/// An activation that marks every `email` span: the posture of a deployment that has
/// calibrated the category and runs the mode it calibrated it for.
fn masking_policy() -> PrivacyPolicy {
    PrivacyPolicy::new(
        true,
        std::collections::BTreeMap::from([(PiiCategory::Email, 1000)]),
        std::collections::BTreeMap::new(),
    )
}

/// The same activation, unchanged, with the mode left at its default.
fn reporting_policy() -> PrivacyPolicy {
    PrivacyPolicy::new(
        false,
        std::collections::BTreeMap::from([(PiiCategory::Email, 1000)]),
        std::collections::BTreeMap::new(),
    )
}

/// An activation that marks nothing: `email` is scanned and counted, and its threshold
/// is not calibrated.
fn uncalibrated_masking_policy() -> PrivacyPolicy {
    PrivacyPolicy::new(
        true,
        std::collections::BTreeMap::from([(PiiCategory::Email, 0)]),
        std::collections::BTreeMap::new(),
    )
}

/// A masking activation with a threshold below the fixture's low score: the mode
/// marked, and the calibration is what decides which spans.
fn threshold_masking_policy() -> PrivacyPolicy {
    PrivacyPolicy::new(
        true,
        std::collections::BTreeMap::from([(PiiCategory::Email, 900)]),
        std::collections::BTreeMap::new(),
    )
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fm_for(vault: &str, section: Section) -> Frontmatter {
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = VaultId::new(vault);
    fm.section = section;
    fm.status = NoteStatus::Live;
    fm
}

/// A vault with `detector` mounted under `policy` — the only injection path.
async fn vault_with(root: &Path, detector: Arc<dyn PiiDetector>, policy: PrivacyPolicy) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("vault creation")
        .with_detector(detector, PrivacyBudgets::default())
        .with_privacy_policy(policy)
}

/// Every `.md` file under `root`, with its contents.
///
/// Walks the whole tree rather than one computed path: the claim under test is that the
/// text is gone from the vault, and a helper that looked at one file would miss the
/// version history the same write path maintains next to it.
fn markdown_files(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                out.push((path, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Asserts that no markdown file under `root` — the note, or any version of it — still
/// contains `needle`.
fn assert_absent_from_disk(root: &Path, needle: &str, context: &str) {
    for (path, text) in markdown_files(root) {
        assert!(
            !text.contains(needle),
            "{context}: {} still holds the masked text",
            path.display()
        );
    }
}

fn block_of(note: &Note) -> toml::value::Table {
    note.frontmatter
        .extra
        .get(BLOCK_KEY)
        .unwrap_or_else(|| panic!("the note must carry an `extra.{BLOCK_KEY}` block"))
        .as_table()
        .expect("the block is a table")
        .clone()
}

fn string_field(block: &toml::value::Table, key: &str) -> String {
    block
        .get(key)
        .unwrap_or_else(|| panic!("key `{key}` must be present"))
        .as_str()
        .unwrap_or_else(|| panic!("key `{key}` must be a string"))
        .to_owned()
}

fn carries_tag(note: &Note, literal: &str) -> bool {
    note.frontmatter
        .tags
        .iter()
        .any(|tag| tag.as_str() == literal)
}

// ── The masked body is the body on disk ──────────────────────────────────────

#[tokio::test]
async fn a_masked_write_persists_the_masked_body_and_its_digest() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        masking_policy(),
    )
    .await;

    let id = NoteId::new();
    let body = format!("carte de contact : {NEEDLE} — à joindre");
    vault
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("write");

    let note = vault.read_note(id).await.expect("read back");
    assert!(
        !note.body.markdown.contains(NEEDLE),
        "the note must not give the matched text back, got: {}",
        note.body.markdown
    );
    assert!(
        note.body.markdown.contains(MASK_FILLER),
        "the matched passage must be replaced by the filler, got: {}",
        note.body.markdown
    );

    // The digest travels with the body it describes. A hash computed before the rewrite
    // would describe text the note no longer holds, and the next optimistic-locked write
    // would be the first thing to notice.
    let recomputed = ContentHash::compute(&note.frontmatter, &note.body.markdown);
    assert_eq!(
        note.content_hash, recomputed,
        "the note's hash must be the hash of the masked body it returns"
    );

    // The claim is about the vault, not about the value returned to the caller.
    assert_absent_from_disk(root, NEEDLE, "masked write");
    assert_absent_from_disk(root, "alice", "masked write");

    let block = block_of(&note);
    assert_eq!(
        string_field(&block, "mode"),
        "mask",
        "the block records the mode its pass ran under"
    );
    assert_eq!(string_field(&block, "status"), "ok");
    assert!(
        carries_tag(&note, CLASSIFIED),
        "the pass marked the note it rewrote"
    );
    assert!(
        !carries_tag(&note, UNSCANNED),
        "a completed pass records no rescan marker"
    );
}

#[tokio::test]
async fn a_masked_write_records_counts_and_no_spans() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        masking_policy(),
    )
    .await;

    let id = NoteId::new();
    vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("contact {NEEDLE}"),
            id,
        )
        .await
        .expect("write");

    let block = block_of(&vault.read_note(id).await.expect("read back"));
    let spans = block
        .get("spans")
        .expect("the block states what it saw")
        .as_array()
        .expect("`spans` is an array");
    assert!(
        spans.is_empty(),
        "a masked body has no spans to point at, got {spans:?}"
    );

    let counts = block
        .get("counts")
        .expect("the block carries its counts")
        .as_table()
        .expect("`counts` is a table");
    assert!(
        counts.values().any(|value| value.as_integer() == Some(1)),
        "the observation survives the destruction, got {counts:?}"
    );
}

#[tokio::test]
async fn a_masked_write_masks_the_spans_the_activation_marks_and_spares_the_others() {
    const HIGH: &str = "bob@example.com";
    const LOW: &str = "carol@example.com";

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(
        root,
        Arc::new(TwoNeedles {
            high: HIGH,
            low: LOW,
            category: PiiCategory::Email,
        }),
        threshold_masking_policy(),
    )
    .await;

    let id = NoteId::new();
    vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("{HIGH} et {LOW}"),
            id,
        )
        .await
        .expect("write");

    let note = vault.read_note(id).await.expect("read back");
    assert!(
        !note.body.markdown.contains(HIGH),
        "the span above the threshold is destroyed, got: {}",
        note.body.markdown
    );
    assert!(
        note.body.markdown.contains(LOW),
        "the span below the threshold is an observation, not a destruction, got: {}",
        note.body.markdown
    );
    assert_eq!(
        note.body.markdown.matches(MASK_FILLER).count(),
        1,
        "exactly one passage was marked, got: {}",
        note.body.markdown
    );
}

#[tokio::test]
async fn a_mask_over_an_uncalibrated_activation_destroys_nothing() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        uncalibrated_masking_policy(),
    )
    .await;

    let id = NoteId::new();
    let body = format!("contact {NEEDLE}");
    vault
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("write");

    let note = vault.read_note(id).await.expect("read back");
    assert_eq!(
        note.body.markdown, body,
        "a threshold of `0` observes and destroys nothing"
    );
    assert!(
        !carries_tag(&note, CLASSIFIED),
        "nothing was marked, so nothing is tagged"
    );
    assert_eq!(
        string_field(&block_of(&note), "mode"),
        "mask",
        "the mode is recorded as it ran, whatever it had to destroy"
    );
}

#[tokio::test]
async fn the_identity_section_and_the_code_namespace_are_never_masked() {
    // One vault per case: the two exclusions are evaluated on the **resolved** write
    // namespace, so a case whose vault is not the one being excluded would measure
    // nothing.
    for (vault_id, section, context) in [
        ("main", Section::Identity, "the identity section"),
        ("code-main", Section::Decisions, "a code namespace"),
    ] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root).expect("vault root");
        let vault = Vault::create(root, VaultId::new(vault_id))
            .await
            .expect("vault creation")
            .with_detector(
                Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
                PrivacyBudgets::default(),
            )
            .with_privacy_policy(masking_policy());

        let id = NoteId::new();
        let body = format!("{context} holds {NEEDLE} verbatim");
        vault
            .write_note_with_id(fm_for(vault_id, section), body.clone(), id)
            .await
            .expect("write");

        let note = vault.read_note(id).await.expect("read back");
        assert_eq!(
            note.body.markdown, body,
            "{context}: no pass runs there, so no mode can rewrite it"
        );
        assert!(
            note.frontmatter.extra.get(BLOCK_KEY).is_none(),
            "{context}: no pass ran, so no block is written"
        );
        assert!(
            !carries_tag(&note, CLASSIFIED),
            "{context}: no pass ran, so no marker is posed"
        );
    }
}

#[tokio::test]
async fn a_truncated_pass_masks_what_the_block_carries_and_marks_the_note_unscanned() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let reported = 300usize;
    let vault = vault_with(
        root,
        Arc::new(DenseSpans {
            count: reported,
            category: PiiCategory::Email,
        }),
        masking_policy(),
    )
    .await;

    let id = NoteId::new();
    // One byte per span, every other byte: a body dense enough to be reported as more
    // spans than a block can carry.
    let body = "ab".repeat(reported);
    vault
        .write_note_with_id(fm_for("main", Section::Decisions), body, id)
        .await
        .expect("write");

    let note = vault.read_note(id).await.expect("read back");
    let block = block_of(&note);
    assert_eq!(
        string_field(&block, "status"),
        "spans_truncated",
        "a pass reporting more spans than a block carries says so"
    );
    assert!(
        carries_tag(&note, UNSCANNED),
        "an incomplete destruction is flagged for the sweep that will finish it"
    );
    assert!(
        block
            .get("spans")
            .and_then(toml::Value::as_array)
            .is_some_and(Vec::is_empty),
        "the spans are gone with the text they pointed at"
    );
    let masked = note.body.markdown.matches(MASK_FILLER).count();
    assert_eq!(
        masked, BLOCK_SPAN_CEILING,
        "the rewrite destroys exactly what the block carried, got {masked}"
    );
    assert!(
        masked < reported,
        "the fixture reported more spans than the ceiling, so the residual is a real case"
    );
}

// ── A pass that did not complete ─────────────────────────────────────────────

#[tokio::test]
async fn a_failed_pass_refuses_the_write_under_mask() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(root, Arc::new(TimedOut), masking_policy()).await;

    let id = NoteId::new();
    let body = format!("contact {NEEDLE}");
    let err = vault
        .write_note_with_id(fm_for("main", Section::Decisions), body, id)
        .await
        .expect_err("a body nothing judged must not be persisted under a masking mode");

    assert!(
        matches!(err, VaultError::Core(_)),
        "the refusal is stated as a typed vault error, got {err:?}"
    );
    assert!(
        err.to_string().contains("privacy"),
        "the refusal must name what refused it, got: {err}"
    );
    assert!(
        markdown_files(root).is_empty(),
        "nothing was written: {}",
        markdown_files(root)
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

#[tokio::test]
async fn a_failed_pass_still_writes_under_the_reporting_mode() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let vault = vault_with(root, Arc::new(TimedOut), reporting_policy()).await;

    let id = NoteId::new();
    let body = format!("contact {NEEDLE}");
    vault
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("the reporting mode is fail-open by design");

    let note = vault.read_note(id).await.expect("read back");
    assert_eq!(
        note.body.markdown, body,
        "a mode that destroys nothing has nothing to protect"
    );
    let block = block_of(&note);
    assert_eq!(string_field(&block, "status"), "detector_unavailable");
    assert!(
        carries_tag(&note, UNSCANNED),
        "the note is flagged so the sweep comes back for it"
    );
}

#[tokio::test]
async fn a_refused_write_leaves_the_previous_version_intact() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // A first version, judged under a mode that destroyed nothing.
    let vault = vault_with(root, Arc::new(TimedOut), reporting_policy()).await;
    let id = NoteId::new();
    let first = format!("première version {NEEDLE}");
    vault
        .write_note_with_id(fm_for("main", Section::Decisions), first.clone(), id)
        .await
        .expect("write");

    // The same note, rewritten under a masking mode whose detector is down: the refusal
    // must not leave the caller's new body on disk, and must not lose the old one.
    let masking = vault_with(root, Arc::new(TimedOut), masking_policy()).await;
    let current = masking.read_note(id).await.expect("read back");
    masking
        .write_note_with_id(
            current.frontmatter.clone(),
            format!("deuxième version {NEEDLE}"),
            id,
        )
        .await
        .expect_err("the refusal is not conditional on a creation");

    let after = masking.read_note(id).await.expect("read back");
    assert_eq!(
        after.body.markdown, first,
        "the version that was on disk is the version that stays"
    );
}

// ── No clear copy survives the rewrite ───────────────────────────────────────

#[tokio::test]
async fn a_masking_rewrite_leaves_no_clear_version_behind() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // The note enters the masking mode carrying text no pass has destroyed.
    let reporting = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        reporting_policy(),
    )
    .await;
    let id = NoteId::new();
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("version d'origine {NEEDLE}"),
            id,
        )
        .await
        .expect("write");

    let masking = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        masking_policy(),
    )
    .await;
    let current = masking.read_note(id).await.expect("read back");
    masking
        .write_note_with_id(
            current.frontmatter.clone(),
            format!("version réécrite {NEEDLE}"),
            id,
        )
        .await
        .expect("write");

    let after = masking.read_note(id).await.expect("read back");
    assert!(
        after.body.markdown.contains(MASK_FILLER),
        "the rewrite masked the new version, got: {}",
        after.body.markdown
    );
    // The version history is the one place a rewrite could keep what the policy
    // destroyed: the whole tree is checked, and the assertion names the files it found
    // so a failure says where the text survived.
    assert_absent_from_disk(root, NEEDLE, "masking rewrite");
}

/// A masking write destroys the versions that were already in `.history/`, not only the
/// one it replaced.
///
/// The note is readable back from disk at every step, so the fixture counts what it
/// hands the rewrite instead of assuming it: `history_versions` is read **before** the
/// masking write, and the count is asserted. A rewrite that purged only its own
/// predecessor would leave the two earlier snapshots behind, and the first two versions
/// of this note are the ones that carry the clear text.
///
/// The control note runs the same number of writes through the same reporting vault and
/// is never rewritten: it keeps its history, so the assertion above distinguishes "the
/// masked note was purged" from "history is broken for every note".
#[tokio::test]
async fn a_masking_rewrite_destroys_the_versions_already_in_history() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let reporting = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        reporting_policy(),
    )
    .await;

    // The note is written three times in clear text: the two writes after the creation
    // each snapshot the version they replace.
    let id = NoteId::new();
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("version un {NEEDLE}"),
            id,
        )
        .await
        .expect("write");
    for text in ["version deux", "version trois"] {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        reporting
            .write_note_with_id(
                fm_for("main", Section::Decisions),
                format!("{text} {NEEDLE}"),
                id,
            )
            .await
            .expect("write");
    }

    // The control note takes the same path, and is never rewritten by the masking vault.
    let control = NoteId::new();
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "témoin un".to_owned(),
            control,
        )
        .await
        .expect("write");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "témoin deux".to_owned(),
            control,
        )
        .await
        .expect("write");

    let before = reporting.history_versions(id).await.expect("history");
    assert_eq!(
        before.len(),
        2,
        "the fixture must hand the rewrite two older versions, got {before:?}"
    );
    assert_eq!(
        reporting
            .history_versions(control)
            .await
            .expect("history")
            .len(),
        1,
        "the control note must carry a snapshot of its own"
    );

    let masking = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        masking_policy(),
    )
    .await;
    let current = masking.read_note(id).await.expect("read back");
    masking
        .write_note_with_id(
            current.frontmatter.clone(),
            format!("version quatre {NEEDLE}"),
            id,
        )
        .await
        .expect("write");

    let after = masking.history_versions(id).await.expect("history");
    assert!(
        after.is_empty(),
        "a masking write destroys every older version, got {after:?}"
    );
    assert!(
        !masking
            .history_versions(control)
            .await
            .expect("history")
            .is_empty(),
        "a note no masking write touched keeps its history"
    );
    assert_absent_from_disk(root, NEEDLE, "masking rewrite over an existing history");
}

/// A masking write still lands when the history it must destroy cannot be destroyed.
///
/// The mode's irreversibility is owed to the snapshots the write removes, but a write is
/// not owed to a **failure** of that removal: the purge is non-fatal, and the note must
/// still be rewritten and masked. The fixture makes the history directory read-only, so
/// `unlink` fails on the snapshot it holds — and the surviving snapshot is asserted to
/// still carry the clear text, which is what makes the write's non-failure a real path
/// rather than a setup that quietly did nothing.
///
/// What the write *reports* about the snapshots it could not remove is a pair of counts;
/// that count is asserted where it is produced, in the crate's own tests.
///
/// Skipped as root: `CAP_DAC_OVERRIDE` makes the read-only directory writable, the purge
/// succeeds, and the assertions below would then measure the bypass instead of the path.
#[tokio::test]
async fn a_masking_write_lands_when_the_history_cannot_be_destroyed() {
    use std::os::unix::fs::PermissionsExt;

    /// Whether this process runs as root, which bypasses the permission bits.
    fn runs_as_root() -> bool {
        use std::os::unix::fs::MetadataExt;
        let probe = std::env::temp_dir().join(format!("gradatum-uid-probe-{}", std::process::id()));
        if std::fs::write(&probe, b"").is_err() {
            return false;
        }
        let uid = std::fs::metadata(&probe).map(|m| m.uid());
        let _ = std::fs::remove_file(&probe);
        matches!(uid, Ok(0))
    }

    if runs_as_root() {
        return;
    }

    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let reporting = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        reporting_policy(),
    )
    .await;

    // Two clear-text writes: the second snapshots the version it replaces, and that
    // snapshot is the file the read-only directory will refuse to unlink.
    let id = NoteId::new();
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("un {NEEDLE}"),
            id,
        )
        .await
        .expect("write");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    reporting
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            format!("deux {NEEDLE}"),
            id,
        )
        .await
        .expect("write");

    let versions = reporting.history_versions(id).await.expect("history");
    assert_eq!(
        versions.len(),
        1,
        "the fixture must hand the rewrite one snapshot, got {versions:?}"
    );

    let history_dir = root.join("main").join(".history").join(id.to_string());
    let snapshots: Vec<PathBuf> = std::fs::read_dir(&history_dir)
        .expect("history directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert_eq!(
        snapshots.len(),
        1,
        "one snapshot to refuse, got {snapshots:?}"
    );
    std::fs::set_permissions(&history_dir, std::fs::Permissions::from_mode(0o500))
        .expect("read-only history directory");

    let masking = vault_with(
        root,
        Arc::new(Needle::new(NEEDLE, PiiCategory::Email, 1.0)),
        masking_policy(),
    )
    .await;
    let current = masking.read_note(id).await.expect("read back");
    let outcome = masking
        .write_note_with_id(current.frontmatter.clone(), format!("trois {NEEDLE}"), id)
        .await;

    // Restored before asserting: a failing assertion must not leave a read-only directory
    // for the TempDir cleanup to trip over.
    let _ = std::fs::set_permissions(&history_dir, std::fs::Permissions::from_mode(0o755));

    let note = outcome.expect("a history the write cannot destroy is not a write that fails");
    assert!(
        note.body.markdown.contains(MASK_FILLER),
        "the rewrite still masked the body, got: {}",
        note.body.markdown
    );
    assert!(
        snapshots[0].exists(),
        "the snapshot the read-only directory protected is still there — the write was \
         exercised against a history it could not destroy"
    );
}
