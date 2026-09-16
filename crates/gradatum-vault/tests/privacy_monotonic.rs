//! F-160 — the reserved marker is **monotone**, and a human verdict **binds**.
//!
//! ## What these tests measure
//!
//! `personal-classified` is a payload marker: a note carrying it is excluded from
//! export and out of an orchestrator's search field. Two failures would each be
//! silent, and each is measured here.
//!
//! - **The marker is lost.** Detection is configured at the *mount*, so the common
//!   state of a running installation is "no detector mounted": no pass runs, and no
//!   pass will ever re-pose what a caller dropped. Any write path that re-posts a
//!   note's tags as strings therefore retires the marker permanently — unless the
//!   write primitive re-reports it from the note on disk, which is the only source
//!   a caller cannot filter. Turning the detector off, restarting without
//!   `[privacy]`, the admin reindex, and the three measured re-post pumps
//!   (`apalis_handlers.rs:547` and `:771`, `logic.rs:4934`) are the same failure
//!   seen from five places; the correction is in the primitive, and so are the
//!   tests.
//! - **The marker is retired by the wrong authority.** Only a completed pass, or a
//!   human rejection, may retire it. An incomplete pass adds `privacy-unscanned`
//!   and touches nothing else; a rewrite that recomputes no verdict leaves both the
//!   block and the tags exactly as they were.
//!
//! ## What is exercised, and what is replayed
//!
//! A note is written, read back, and rewritten. The rewrites reproduce, at the
//! vault boundary, the call shape each production path posts — the same funnel and
//! the same frontmatter a path that rebuilds its own frontmatter hands over. Sites
//! that live outside this crate (worker, admin, server) cannot be called from a
//! `gradatum-vault` integration test; the mechanism they all go through can, and
//! that is what the seven-path and re-post tests pin.
//!
//! ## A verdict, without a review surface
//!
//! The review surface (`vault_privacy_review`) is not delivered yet: no API sets
//! `review`. Its *effect* is reachable today, because a block is data in the note —
//! a test writes the note back with `review` set, exactly as the review surface
//! will, and the primitive is what has to honour it.

mod common;
use common::build_minimal_frontmatter;

use std::path::Path;
use std::sync::Arc;

use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::note::Note;
use gradatum_core::pii::{
    DetectInput, Detection, NoopDetector, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
    PrivacyBudgets,
};
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_vault::{Registry, Vault, WriteResult};
use tempfile::TempDir;

/// The marker a completed pass poses on a note holding personal data of an
/// activated category.
const CLASSIFIED: &str = "personal-classified";

/// The marker an incomplete pass poses, so that a rescan can find the note again.
const UNSCANNED: &str = "privacy-unscanned";

/// Key the block is written under in `frontmatter.extra`.
const BLOCK_KEY: &str = "privacy";

// ── Detectors ────────────────────────────────────────────────────────────────

/// Reports one span covering the whole of the window it is handed.
///
/// A stand-in for the delivered `RegexDetector`, whose spans carry `score == 1.0`
/// and are never subject to a threshold.
struct AnySpan(PiiCategory, f32);

impl PiiDetector for AnySpan {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(
            vec![PiiSpan::new(0, input.text.len(), self.0, self.1)],
            0,
        ))
    }
}

/// Reports a fixed span list, in body coordinates.
struct FixedSpans(Vec<PiiSpan>);

impl PiiDetector for FixedSpans {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(self.0.clone(), 0))
    }
}

/// A detector that never answers: an incomplete pass that is not a size state.
struct TimedOut;

impl PiiDetector for TimedOut {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Err(PiiDetectError::Timeout { timeout_ms: 10 })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fm_for(vault: &str, section: Section) -> Frontmatter {
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = VaultId::new(vault);
    fm.section = section;
    fm.status = NoteStatus::Live;
    fm
}

/// A vault with `detector` mounted — the only injection path.
async fn vault_with(root: &Path, detector: Arc<dyn PiiDetector>) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("vault creation")
        .with_detector(detector, PrivacyBudgets::default())
}

/// A vault built the way the CLI builds one: no `with_detector` call, so the
/// pass-through the three constructors write in hard is what is mounted.
async fn cli_vault(root: &Path) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("vault creation")
}

/// Reopens an existing root without mounting any detector — a restart on a
/// configuration that no longer carries `[privacy]`.
async fn restarted_vault(root: &Path) -> Vault {
    Vault::open(root).await.expect("vault reopening")
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

/// Whether the note carries `literal` among its tags.
///
/// Compared on the literal rather than by rebuilding a `Tag` and testing equality:
/// `Tag::new` refuses a reserved name now, so the old shape could not be written at
/// all — and comparing literals is what the frontier itself does, for the same reason
/// (a tag a note arrives carrying is recognised whatever produced it).
fn carries_tag(note: &Note, literal: &str) -> bool {
    note.frontmatter
        .tags
        .iter()
        .any(|tag| tag.as_str() == literal)
}

/// A tag as a note read back from disk carries it.
///
/// This is the shape a caller re-posting a note's tags actually hands the write
/// primitive, and the reason the transit property needs no cooperation from any
/// constructor: `Tag`'s `Deserialize` does not validate, deliberately, so a reserved
/// marker already written on disk comes back even though `Tag::new` and
/// `Tag::normalize` both refuse its literal. A test that means to re-post the marker
/// has no other honest way to build it.
fn as_read_from_a_note(literal: &str) -> Tag {
    serde_json::from_value(serde_json::Value::String(literal.to_owned()))
        .expect("a tag deserialises without validation")
}

/// The note's own frontmatter, with the block's `review` set to `review` — what the
/// review surface writes back.
///
/// The tag list is left exactly as the note carried it: what the surface does with
/// the marker is not what these tests measure, the primitive's verdict is.
fn with_verdict(note: &Note, review: &str) -> Frontmatter {
    let mut fm = note.frontmatter.clone();
    let mut block = block_of(note);
    block.insert("review".to_owned(), toml::Value::String(review.to_owned()));
    fm.extra = ExtraFields::empty();
    fm.extra
        .insert(BLOCK_KEY.to_owned(), toml::Value::Table(block));
    fm
}

/// A note that has been through a completed pass: a block, and its `status`.
fn assert_processed(note: &Note, context: &str) {
    let block = block_of(note);
    assert_eq!(
        string_field(&block, "status"),
        "ok",
        "{context}: the note comes out of the funnel with a completed pass on record"
    );
}

fn assert_written(result: &WriteResult, context: &str) {
    assert!(
        matches!(result, WriteResult::Written { .. }),
        "{context}: the rewrite must take effect, got {result:?}"
    );
}

// ── (a) the marker survives a detector that is not there ─────────────────────

#[tokio::test]
async fn tagged_note_plus_noop_detector_is_still_tagged_and_block_identical() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;

    let id = NoteId::new();
    frontier
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps initial".into(),
            id,
        )
        .await
        .expect("write");
    let before = block_of(&frontier.read_note(id).await.expect("read back"));
    assert!(
        carries_tag(
            &frontier.read_note(id).await.expect("read back"),
            CLASSIFIED
        ),
        "a completed pass over a span poses the marker"
    );

    // The detector is turned off. The note is rewritten from a frontmatter that
    // knows nothing of the marker — the shape every rebuild-from-scratch path
    // posts — and the body changes, so nothing but the primitive can carry the
    // marker and the block across.
    let off = vault_with(root, Arc::new(NoopDetector)).await;
    let current = frontier.read_note(id).await.expect("read back");
    let outcome = off
        .write_if_match(
            fm_for("main", Section::Decisions),
            "corps modifié sans détecteur".into(),
            id,
            Some(current.content_hash.0),
        )
        .await
        .expect("rewrite");
    assert_written(&outcome, "pass-through rewrite");
    let rewritten = off.read_note(id).await.expect("read back");

    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "no pass runs under the pass-through, so no pass can re-pose the marker: \
         dropping it here would retire it permanently and silently"
    );
    assert_eq!(
        block_of(&rewritten),
        before,
        "no pass recomputes the block, and turning the detector off must not erase \
         what a pass recorded"
    );
}

#[tokio::test]
async fn tagged_note_plus_detector_removed_from_config_and_restart_is_still_tagged() {
    let dir = TempDir::new().unwrap();
    let id = NoteId::new();

    {
        let frontier = vault_with(dir.path(), Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
        let note = frontier
            .write_note_with_id(fm_for("main", Section::Decisions), "corps".into(), id)
            .await
            .expect("write");
        assert!(carries_tag(&note, CLASSIFIED));
    } // the process ends here: nothing is carried over in memory

    // `[privacy]` is gone from the configuration, so the restart mounts no detector
    // at all — and the marker must still be on the note.
    let restarted = restarted_vault(dir.path()).await;
    let rewritten = restarted
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps après redémarrage".into(),
            id,
        )
        .await
        .expect("rewrite");
    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "removing the detector from the configuration is not a retraction of a \
         verdict: it must survive a restart"
    );
}

#[tokio::test]
async fn tagged_note_plus_admin_reindex_is_still_tagged() {
    let dir = TempDir::new().unwrap();
    let id = NoteId::new();

    let frontier = vault_with(dir.path(), Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(fm_for("main", Section::Decisions), "corps".into(), id)
        .await
        .expect("write");
    let on_disk = frontier.read_note(id).await.expect("read back");
    assert!(carries_tag(&on_disk, CLASSIFIED));

    // The admin reindex (`reindex_orphans.rs:315`) writes the note back through
    // `write_note_with_id`, from a `Vault` the CLI builds without `with_detector`,
    // and re-posts the tags it holds as `String`s. The primitive is what keeps the
    // marker.
    let cli = cli_vault(dir.path()).await;
    let mut reindexed = fm_for("main", Section::Decisions);
    reindexed.locus = on_disk.frontmatter.locus.clone();
    let rewritten = cli
        .write_note_with_id(reindexed, on_disk.body.markdown.clone(), id)
        .await
        .expect("reindex write");
    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "a reindex that does not know the marker must not retire it"
    );
}

#[tokio::test]
async fn tagged_note_plus_err_timeout_is_still_tagged_plus_privacy_unscanned() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let id = NoteId::new();

    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(fm_for("main", Section::Decisions), "corps".into(), id)
        .await
        .expect("write");

    // The same note, rewritten while the detector no longer answers: an incomplete
    // pass. It may add the rescan marker; it may never retire the payload marker.
    let failing = vault_with(root, Arc::new(TimedOut)).await;
    let rewritten = failing
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps inchangé".into(),
            id,
        )
        .await
        .expect("rewrite");

    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "an incomplete pass is not an authority on what is personal data"
    );
    assert!(
        carries_tag(&rewritten, UNSCANNED),
        "a pass that did not complete marks the note for a rescan"
    );
    assert_eq!(
        string_field(&block_of(&rewritten), "status"),
        "detector_unavailable",
        "the block records the failure it saw"
    );
}

// ── (b) a completed pass is the only authority that retires ──────────────────

#[tokio::test]
async fn legitimate_removal_ok_pass_without_span_above_threshold_removes_the_tag() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let id = NoteId::new();
    let body = "un corps de longueur suffisante".to_string();

    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("write");
    assert!(carries_tag(
        &frontier.read_note(id).await.expect("read back"),
        CLASSIFIED
    ));

    // The next pass sees the same bytes and reports a span at score 0.0 — the shape
    // of an observation threshold: the span is recorded, and a threshold that is
    // not strictly positive activates nothing (spec §4.2). The block says what was
    // seen; the marker is not posed. The rewrite re-posts the marker, so what the
    // test measures is the pass's authority to retire it — not the caller's
    // forgetfulness.
    let below = vault_with(
        root,
        Arc::new(FixedSpans(vec![PiiSpan::new(
            0,
            3,
            PiiCategory::Email,
            0.0,
        )])),
    )
    .await;
    let mut reposted = fm_for("main", Section::Decisions);
    reposted.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    let rewritten = below
        .write_note_with_id(reposted, body, id)
        .await
        .expect("rewrite");

    assert!(
        !carries_tag(&rewritten, CLASSIFIED),
        "a completed pass that reports nothing above an activated threshold is the \
         first, and legitimate, removal path"
    );
    let block = block_of(&rewritten);
    assert_eq!(
        string_field(&block, "status"),
        "ok",
        "the removal is the act of a completed pass, not of a failure"
    );
    assert_eq!(
        block["counts"]["email"].as_integer(),
        Some(1),
        "the span was seen and counted — `counts` non-empty with no marker is a \
         state of its own (spec §4.1 rule 3)"
    );
    assert!(
        !carries_tag(&rewritten, UNSCANNED),
        "a completed pass retires the rescan marker it no longer justifies"
    );
}

// ── (c) the review verdict, and its binding ──────────────────────────────────

#[tokio::test]
async fn review_accepted_survives_a_curate_rewrite() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let id = NoteId::new();
    let body = "un corps de longueur suffisante".to_string();

    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("write");

    // The review surface accepts the verdict: the note's own block, with `review`
    // set. Written through a vault whose detector is off, so the block stands as
    // the surface left it.
    let on_disk = frontier.read_note(id).await.expect("read back");
    let off = vault_with(root, Arc::new(NoopDetector)).await;
    let accepted = off
        .write_note_with_id(with_verdict(&on_disk, "accepted"), body.clone(), id)
        .await
        .expect("verdict write");
    assert_eq!(
        string_field(&block_of(&accepted), "review"),
        "accepted",
        "the verdict is on record"
    );

    // The curate rewrite: a frontmatter rebuilt from scratch, no tags, no extra, and
    // the same body — so the same spans, and therefore the same `spans_digest`.
    let outcome = frontier
        .write_if_match(
            fm_for("main", Section::Decisions),
            body,
            id,
            Some(accepted.content_hash.0),
        )
        .await
        .expect("curate rewrite");
    assert_written(&outcome, "curate rewrite");
    let rewritten = frontier.read_note(id).await.expect("read back");

    assert_eq!(
        string_field(&block_of(&rewritten), "review"),
        "accepted",
        "a re-pass over the same spans is not a reason to undo a human acceptance"
    );
    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "the accepted verdict leaves the marker standing"
    );
}

#[tokio::test]
async fn review_resets_to_pending_when_body_changes() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let id = NoteId::new();

    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "un corps de longueur suffisante".into(),
            id,
        )
        .await
        .expect("write");

    let on_disk = frontier.read_note(id).await.expect("read back");
    let off = vault_with(root, Arc::new(NoopDetector)).await;
    let accepted = off
        .write_note_with_id(
            with_verdict(&on_disk, "accepted"),
            on_disk.body.markdown.clone(),
            id,
        )
        .await
        .expect("verdict write");
    assert_eq!(
        string_field(&block_of(&accepted), "review"),
        "accepted",
        "the verdict is on record before the body moves"
    );

    // The body changes: the spans the verdict was about are no longer the spans the
    // note holds, so the verdict is spent.
    let outcome = frontier
        .write_if_match(
            fm_for("main", Section::Decisions),
            "un tout autre corps, plus long que le précédent".into(),
            id,
            Some(accepted.content_hash.0),
        )
        .await
        .expect("curate rewrite");
    assert_written(&outcome, "curate rewrite");
    let rewritten = frontier.read_note(id).await.expect("read back");

    assert_eq!(
        string_field(&block_of(&rewritten), "review"),
        "pending",
        "`review` is preserved while `spans_digest` is unchanged, and resets the \
         moment it changes"
    );
}

#[tokio::test]
async fn rejected_verdict_binds_the_next_pass() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let id = NoteId::new();
    let body = "un corps de longueur suffisante".to_string();

    let frontier = vault_with(root, Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;
    frontier
        .write_note_with_id(fm_for("main", Section::Decisions), body.clone(), id)
        .await
        .expect("write");
    let tagged = frontier.read_note(id).await.expect("read back");
    assert!(
        carries_tag(&tagged, CLASSIFIED),
        "the note is marked before the review speaks"
    );

    // The review rejects the span as a false positive. The verdict is written back
    // as the surface writes it: the note's own block, `review: rejected`. The
    // surface re-posts the note's frontmatter, so the marker is still in the tag
    // list it hands over.
    //
    // Recording the verdict is not itself a removal, and must not be: the block a
    // caller posts carries no authority the primitive can check — `spans_digest` is
    // readable in the note — so a request that merely *claims* `rejected` cannot be
    // allowed to retire the marker. What the primitive does with the verdict, it
    // does on the next pass, reading it back from the note.
    let off = vault_with(root, Arc::new(NoopDetector)).await;
    let rejected = off
        .write_note_with_id(with_verdict(&tagged, "rejected"), body.clone(), id)
        .await
        .expect("verdict write");
    assert_eq!(
        string_field(&block_of(&rejected), "review"),
        "rejected",
        "the verdict itself is preserved"
    );
    assert!(
        carries_tag(&rejected, CLASSIFIED),
        "a block claiming `rejected` is data, not an authority: the marker is \
         retired by the pass that reads the verdict back, never by the request"
    );

    // The next pass re-detects the very same spans. It must not re-pose the marker:
    // the human judgement was about these spans, and a rewrite is not a new
    // judgement.
    let replayed_outcome = frontier
        .write_if_match(
            fm_for("main", Section::Decisions),
            body.clone(),
            id,
            Some(rejected.content_hash.0),
        )
        .await
        .expect("curate rewrite");
    assert_written(&replayed_outcome, "curate rewrite");
    let replayed = frontier.read_note(id).await.expect("read back");
    assert!(
        !carries_tag(&replayed, CLASSIFIED),
        "identical spans, unchanged digest: the verdict the note carries binds the \
         next pass"
    );
    assert_eq!(
        string_field(&block_of(&replayed), "review"),
        "rejected",
        "the verdict survives the pass it binds"
    );

    // A write that runs no pass at all still reads the rejection from the note: the
    // marker cannot be re-posted onto it, whichever path re-posts what.
    let mut repost = fm_for("main", Section::Decisions);
    repost.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    let reposted = off
        .write_note_with_id(repost, body.clone(), id)
        .await
        .expect("re-post write");
    assert!(
        !carries_tag(&reposted, CLASSIFIED),
        "a non-spent rejection on disk retires the marker even on a write that \
         recomputes no verdict"
    );

    // The body moves. The verdict was about the old spans, so it is spent: the
    // review returns to `pending`, and the marker is posed normally again.
    let moved_outcome = frontier
        .write_if_match(
            fm_for("main", Section::Decisions),
            "un tout autre corps, plus long que le précédent".into(),
            id,
            Some(reposted.content_hash.0),
        )
        .await
        .expect("curate rewrite");
    assert_written(&moved_outcome, "curate rewrite");
    let moved = frontier.read_note(id).await.expect("read back");
    assert_eq!(
        string_field(&block_of(&moved), "review"),
        "pending",
        "a changed digest spends the verdict"
    );
    assert!(
        carries_tag(&moved, CLASSIFIED),
        "with the verdict spent, a completed pass over a span poses the marker again"
    );
}

// ── (d) the seven write paths ────────────────────────────────────────────────

/// Every write path funnels into the same insertion point, so a note written
/// through any of them comes out processed.
///
/// The same table is replayed here at the vault boundary, each entry
/// with the frontmatter that path posts: 1-2 curate CREATE and RMW-CAS
/// (`persist.rs:481` / `:447`), 3 forget (`:691`), 4 distill (`:1114`), 5
/// inter-agent messaging (`api_v1/messaging.rs:223`, the one synchronous surface),
/// 6 copy-on-write restore (`gradatum-vault/src/lib.rs:613`), 7 admin reindex
/// (`reindex_orphans.rs:315`).
#[tokio::test]
async fn each_of_the_seven_write_paths_yields_a_processed_note() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;

    // 1. curate CREATE — the worker's pre-allocated ULID.
    let created_id = NoteId::new();
    let created = vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps create".into(),
            created_id,
        )
        .await
        .expect("curate create");
    assert_processed(&created, "curate CREATE (persist.rs:481)");

    // 2. curate RMW — the compare-and-swap rewrite.
    let rmw_outcome = vault
        .write_if_match(
            fm_for("main", Section::Decisions),
            "corps rmw".into(),
            created_id,
            Some(created.content_hash.0),
        )
        .await
        .expect("curate rmw");
    assert_written(&rmw_outcome, "curate RMW-CAS");
    let rmw = vault.read_note(created_id).await.expect("read back");
    assert_processed(&rmw, "curate RMW-CAS (persist.rs:447)");

    // 3. forget — the write carries the tombstone flags and a fresh body.
    let forgotten_id = NoteId::new();
    let mut tombstone = fm_for("main", Section::Decisions);
    tombstone.forgotten = Some(true);
    let forgotten = vault
        .write_note_with_id(tombstone, String::new(), forgotten_id)
        .await
        .expect("forget");
    assert_processed(&forgotten, "forget (persist.rs:691)");

    // 4. distill — the derived note, written under its own ULID.
    let distilled_id = NoteId::new();
    let distilled = vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps distillé".into(),
            distilled_id,
        )
        .await
        .expect("distill");
    assert_processed(&distilled, "distill (persist.rs:1114)");

    // 5. inter-agent messaging — the one path that writes a `Messaging` section,
    // and therefore the one that runs on the synchronous surface.
    let message_id = NoteId::new();
    let message = vault
        .write_note_with_id(
            fm_for("main", Section::Messaging),
            "corps message".into(),
            message_id,
        )
        .await
        .expect("message");
    assert_processed(&message, "messaging (api_v1/messaging.rs:223)");

    // 6. copy-on-write restore — the snapshot's frontmatter is written back as the
    // current version. The rewrite above left a snapshot behind, since the body
    // changed.
    let versions = vault
        .history_versions(created_id)
        .await
        .expect("history readable");
    assert!(
        !versions.is_empty(),
        "the rewrite must have snapshotted the previous version"
    );
    let witness = AclCheckedVaultId::for_system_task(VaultId::new("main"));
    vault
        .history_restore(&witness, &created_id.to_string(), versions[0])
        .await
        .expect("restore");
    let restored = vault.read_note(created_id).await.expect("read back");
    assert_processed(&restored, "restore (gradatum-vault/src/lib.rs:613)");

    // 7. admin reindex — an orphan written back through the same funnel, under the
    // id and section its `.md` carries.
    let reindexed_id = NoteId::new();
    let reindexed = vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps réindexé".into(),
            reindexed_id,
        )
        .await
        .expect("reindex");
    assert_processed(&reindexed, "admin reindex (reindex_orphans.rs:315)");
}

// ── (e) the re-post pumps ────────────────────────────────────────────────────

/// `apalis_handlers.rs:547` — the worker's reclassification re-posts the note's
/// tags as `String`s (`let mut merged_tags = dto.tags.clone()`), and the
/// reservation guard refuses a reserved tag on the way in. The primitive
/// re-reports the marker from the note on disk, which no caller can filter.
#[tokio::test]
async fn worker_reclassification_preserves_a_reserved_tag() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(NoopDetector)).await;
    let id = NoteId::new();

    let mut marked = fm_for("main", Section::Decisions);
    marked.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    let note = vault
        .write_note_with_id(marked, "corps".into(), id)
        .await
        .expect("write");
    assert!(carries_tag(&note, CLASSIFIED));

    // The reclassification: the same note, re-posted with the tags the caller
    // holds — the marker is not among them.
    let mut reposted = fm_for("main", Section::Decisions);
    reposted.status = NoteStatus::Live;
    let rewritten = vault
        .write_note_with_id(reposted, "corps".into(), id)
        .await
        .expect("reclassification write");

    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "a caller that re-posts tags as strings cannot be the authority on a \
         reserved marker: the re-report reads the note on disk"
    );
}

/// `apalis_handlers.rs:771` — the same re-post on the `Pending` path, whose note
/// is in review rather than live.
#[tokio::test]
async fn pending_path_reclassification_preserves_a_reserved_tag() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(NoopDetector)).await;
    let id = NoteId::new();

    let mut marked = fm_for("main", Section::Decisions);
    marked.status = NoteStatus::PendingReview;
    marked.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    vault
        .write_note_with_id(marked, "corps".into(), id)
        .await
        .expect("write");

    // The pending path re-posts the entry with the tags it holds, and clasps the
    // note forward.
    let mut reposted = fm_for("main", Section::Decisions);
    reposted.status = NoteStatus::Live;
    let rewritten = vault
        .write_note_with_id(reposted, "corps".into(), id)
        .await
        .expect("pending write");

    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "the pending path is the same re-post, and gets the same re-report"
    );
}

/// `logic.rs:4934` — `vault_mutate_role` recopies `note.frontmatter.tags` into the
/// write request as `Vec<String>`, and appends a line to the body. A metadata
/// mutation is not a judgement on the payload.
#[tokio::test]
async fn vault_mutate_role_preserves_a_reserved_tag() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(NoopDetector)).await;
    let id = NoteId::new();

    let mut marked = fm_for("main", Section::Decisions);
    marked.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    vault
        .write_note_with_id(marked, "corps".into(), id)
        .await
        .expect("write");

    // The role mutation: the body gains its history line, the frontmatter is
    // rebuilt from the request.
    let rewritten = vault
        .write_note_with_id(
            fm_for("main", Section::Decisions),
            "corps\n\n## Historique\n\n- statut : DONE\n".into(),
            id,
        )
        .await
        .expect("role mutation");

    assert!(
        rewritten.body.markdown.contains("Historique"),
        "the mutation itself must take effect"
    );
    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "changing a status role says nothing about whether the note holds personal \
         data"
    );
}

/// `reindex_orphans.rs:315` — the fourth re-post, on a note that carries no block
/// at all: there is no verdict to read, so the marker rests on the re-report alone.
/// The reindex parses the `.md` and writes the frontmatter back under the vault the
/// CLI builds, which mounts no detector.
#[tokio::test]
async fn admin_reindex_preserves_a_reserved_tag() {
    let dir = TempDir::new().unwrap();
    let cli = cli_vault(dir.path()).await;
    let id = NoteId::new();

    let mut marked = fm_for("main", Section::Decisions);
    marked.tags = vec![as_read_from_a_note(CLASSIFIED)].into();
    let note = cli
        .write_note_with_id(marked, "corps réindexable".into(), id)
        .await
        .expect("write");
    assert!(
        note.frontmatter.extra.get(BLOCK_KEY).is_none(),
        "this note carries no block: the case under test is the re-report, not a \
         verdict"
    );

    let mut reindexed = fm_for("main", Section::Decisions);
    reindexed.locus = note.frontmatter.locus.clone();
    let rewritten = cli
        .write_note_with_id(reindexed, note.body.markdown.clone(), id)
        .await
        .expect("reindex write");

    assert!(
        carries_tag(&rewritten, CLASSIFIED),
        "with no block and no detector, the marker survives only because the \
         primitive re-reports it from the note on disk"
    );
}
