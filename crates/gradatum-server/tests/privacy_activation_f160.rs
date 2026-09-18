//! F-160 — the `[privacy]` activation, from the TOML on disk to the marker on a note.
//!
//! ## The defect class these tests close
//!
//! A configuration section that deserialises, validates, and is read by nothing. `[privacy]`
//! carried `thresholds` and `exceptions` for a whole release while the pass marked on the
//! mere presence of a span: the settings parsed, the server booted, and no value in them
//! reached a decision. A guard over the *tokens* (`config_wiring.rs`) proves a field is
//! read; it does not prove the value read **decides** anything. That is what is measured
//! here, end to end and on the delivered path: the TOML text is parsed by the real
//! [`ServerConfig::load`], the policy is derived by the real `PrivacyConfig::policy`, a vault
//! is mounted exactly as the boot path mounts one, and the assertion is on the tag the note
//! ends up carrying.
//!
//! ## Why the score is a parameter
//!
//! Every threshold in this product is an integer in thousandths, and the comparison is made
//! on the `score_m` a note records. A fixture whose detector always reports 1.0 could not
//! distinguish "the threshold is read" from "the threshold is ignored", so [`Scored`] reports
//! whatever score a test asks for and the boundary is swept from both sides.

use std::io::Write;
use std::sync::Arc;

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::config::{ConfigError, ServerConfig};
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

/// The literal both the body and the detector's span are built on.
const NEEDLE: &str = "alice@example.com";

/// The reserved name a markable pass poses.
const PERSONAL_CLASSIFIED: &str = "personal-classified";

// ── The detector ─────────────────────────────────────────────────────────────

/// Reports one `person` span on the needle, at the score the test names.
struct Scored(f32);

impl PiiDetector for Scored {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(match input.text.find(NEEDLE) {
            Some(start) => Detection::new(
                vec![PiiSpan::new(
                    start,
                    NEEDLE.len(),
                    PiiCategory::Person,
                    self.0,
                )],
                0,
            ),
            None => Detection::new(Vec::new(), 0),
        })
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

/// Loads a configuration from inline TOML through the delivered load path, so a test
/// observes what an operator's file actually produces — parsing and validation included.
fn load(toml: &str) -> Result<ServerConfig, ConfigError> {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write TOML");
    ServerConfig::load(Some(file.path()))
}

/// A minimal frontmatter for the vault root.
fn fm() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: chrono::Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// What one write left on the note: its tags, and its `extra.privacy` block if it has one.
///
/// `extra` is carried as TOML, and asserted on as TOML: rendering it into another shape
/// first would put a conversion between the block and the assertion.
struct Written {
    tags: Vec<String>,
    block: Option<toml::Value>,
}

/// Writes the body through a vault mounted the way the boot path mounts one — the detector
/// the fixture names, and the activation **the configuration declares**.
///
/// The two must come from the same place, which is the whole point: `mounted_detector`
/// derives the policy from the same `[privacy]` section that decided which detector to
/// build, so a fixture that supplied one and not the other would be testing a frontier no
/// deployment assembles.
async fn write_with(cfg: &ServerConfig, score: f32, body: &str) -> Written {
    let dir = TempDir::new().expect("TempDir");
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create")
        .with_detector(Arc::new(Scored(score)), PrivacyBudgets::default())
        .with_privacy_policy(cfg.privacy.policy());
    let id = vault
        .write_note(fm(), body.to_owned())
        .await
        .expect("write_note")
        .id
        .to_string();
    let note = vault
        .read_note_by_id(&id)
        .await
        .expect("read the note back");
    Written {
        tags: note
            .frontmatter
            .tags
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect(),
        block: note.frontmatter.extra.get("privacy").cloned(),
    }
}

/// The body the detector finds its needle in.
fn body() -> String {
    format!("contact {NEEDLE} for the audit")
}

// ── The thresholds decide ────────────────────────────────────────────────────

/// The boundary is swept from both sides: the same span, the same body, one thousandth
/// apart, and opposite verdicts. A configuration whose `thresholds` reached no decision
/// would answer the same way twice.
#[tokio::test]
async fn a_configured_threshold_decides_whether_a_span_marks() {
    // 0.9 is 900 thousandths, the value the pass compares against the configured one.
    let marks =
        load("[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 900\n")
            .expect("a calibrated threshold loads");
    let written = write_with(&marks, 0.9, &body()).await;
    assert!(
        written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "a span at 900 thousandths meets a threshold of 900 — the comparison is inclusive. \
         Got: {:?}",
        written.tags
    );

    let misses =
        load("[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 901\n")
            .expect("a calibrated threshold loads");
    let written = write_with(&misses, 0.9, &body()).await;
    assert!(
        !written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "one thousandth higher and the same span no longer marks. Got: {:?}",
        written.tags
    );
}

/// `default_m = 0` is "not calibrated", never "everything passes".
///
/// The two readings are opposite and only one of them is safe: a threshold of zero read as a
/// floor would mark every note the detector reports anything about, which is the posture of a
/// deployment that has calibrated nothing. The span here is reported at 1.0 — the highest
/// score the contract allows — so the test is not passing by under-scoring the detector.
#[tokio::test]
async fn a_zero_threshold_enables_nothing() {
    let cfg = load("[privacy]\ncategories = [\"person\"]\n").expect("the default threshold loads");
    assert_eq!(
        cfg.privacy.thresholds.default_m, 0,
        "premise: the delivered default is the value under test"
    );
    let written = write_with(&cfg, 1.0, &body()).await;
    assert!(
        !written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "an uncalibrated category marks nothing, whatever the detector reports. Got: {:?}",
        written.tags
    );
    assert!(
        written.block.is_some(),
        "and it is still **scanned**: the pass ran and the block records what it saw — \
         stillness is not the same answer as a completed clean scan"
    );
}

/// The per-category entry overrides the default in **both** directions, so neither field can
/// be reading the other's value.
#[tokio::test]
async fn a_per_category_threshold_overrides_the_default() {
    let raised = load(
        "[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1000\nperson = 1\n",
    )
    .expect("a per-category override loads");
    let written = write_with(&raised, 0.9, &body()).await;
    assert!(
        written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "`default_m = 1000` must not decide a category that names its own threshold. Got: {:?}",
        written.tags
    );

    let lowered = load(
        "[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1\nperson = 1000\n",
    )
    .expect("a per-category override loads");
    let written = write_with(&lowered, 0.9, &body()).await;
    assert!(
        !written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "`default_m = 1` must not decide it either. Got: {:?}",
        written.tags
    );
    let written = write_with(&lowered, 1.0, &body()).await;
    assert!(
        written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "1.0 is 1000 thousandths, which meets a threshold of 1000. Got: {:?}",
        written.tags
    );
}

/// `categories` is the **activation list**, not a filter on detection.
///
/// A category the configuration does not activate is still reported, fused and recorded —
/// the block is the matter a campaign calibrates its thresholds on, and a pass that dropped
/// what it was not yet allowed to mark would leave nothing to measure. What the absence
/// costs is the marker, and nothing else.
#[tokio::test]
async fn an_unactivated_category_is_observed_but_never_marks() {
    let cfg = load("[privacy]\ncategories = []\n").expect("an empty activation loads");
    let written = write_with(&cfg, 1.0, &body()).await;
    assert!(
        !written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "an empty activation marks nothing at the highest score. Got: {:?}",
        written.tags
    );

    let block = written
        .block
        .expect("the pass ran and the block records what it saw");
    let spans = block
        .get("spans")
        .and_then(toml::Value::as_array)
        .expect("the block carries a span list");
    assert_eq!(
        spans.len(),
        1,
        "the span is still recorded — it is the campaign's matter, not the marker's: {block}"
    );
    let counts = block
        .get("counts")
        .and_then(toml::Value::as_table)
        .expect("the block carries per-category counts");
    assert_eq!(
        counts.get("person").and_then(toml::Value::as_integer),
        Some(1),
        "and it is counted under its own category: {block}"
    );
}

// ── The exceptions decide last ───────────────────────────────────────────────

/// An exception is applied **last** and to the spans themselves: what it covers is declared
/// not personal data, so it leaves the pass's output entirely — no marker, no span in the
/// block, no count, and nothing in the digest the next pass compares against.
///
/// The literal is a value the operator wrote down, which is why nothing derived from it may
/// be readable back: the block records the *pattern identifier*, and the test asserts the
/// literal appears nowhere in the note's own metadata.
#[tokio::test]
async fn an_exception_literal_removes_the_span_it_covers() {
    let cfg = load(&format!(
        "[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1\n\n\
         [privacy.exceptions]\nmain = [\"{NEEDLE}\"]\n"
    ))
    .expect("an exception list loads");
    let written = write_with(&cfg, 1.0, &body()).await;

    assert!(
        !written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "the span an exception covers must not mark. Got: {:?}",
        written.tags
    );
    let block = written.block.expect("the pass ran");
    assert_eq!(
        block
            .get("spans")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(0),
        "and it leaves the block: an exception says the literal is not personal data, so \
         there is no observation left to record. Got: {block}"
    );
    assert_eq!(
        block
            .get("counts")
            .and_then(toml::Value::as_table)
            .map(toml::Table::len),
        Some(0),
        "nor is it counted under its category: {block}"
    );
    assert!(
        !block.to_string().contains(NEEDLE),
        "nothing derived from a configured literal may be readable back — not even the \
         pattern that fired: {block}"
    );
}

/// A pattern that covers nothing excepts nothing: the map states what fired, not what was
/// declared. Otherwise a stale literal in the configuration would silently exempt a body it
/// has nothing to do with.
#[tokio::test]
async fn an_exception_that_covers_nothing_leaves_the_pass_alone() {
    let cfg = load(
        "[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1\n\n\
         [privacy.exceptions]\nmain = [\"bob@example.com\"]\n",
    )
    .expect("an exception list loads");
    let written = write_with(&cfg, 1.0, &body()).await;
    assert!(
        written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "an exception for a literal this body does not contain changes nothing. Got: {:?}",
        written.tags
    );
}

/// The exception list is keyed by tenant, and a tenant that is not named excepts nothing.
///
/// Read as a global list, one tenant's literals would exempt another tenant's data — the
/// one direction of this feature that could not be undone by configuration.
#[tokio::test]
async fn an_exception_declared_for_another_tenant_does_not_apply() {
    let cfg = load(&format!(
        "[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1\n\n\
         [privacy.exceptions]\nother = [\"{NEEDLE}\"]\n"
    ))
    .expect("an exception list loads");
    let written = write_with(&cfg, 1.0, &body()).await;
    assert!(
        written.tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "the note was written to `main`, so a list declared for `other` must not cover it. \
         Got: {:?}",
        written.tags
    );
}

// ── What the configuration refuses ───────────────────────────────────────────

/// A label outside the product taxonomy is refused at load rather than read as a category
/// that marks nothing. The failure mode it closes is a silent one: a typo in a threshold
/// table would leave the operator convinced a category was calibrated.
#[tokio::test]
async fn an_unknown_category_label_is_refused_at_load() {
    let err = load("[privacy]\ncategories = [\"emails\"]\n").expect_err("a typo must be refused");
    match err {
        ConfigError::PrivacyUnknownCategory { ref label } => assert_eq!(label, "emails"),
        other => panic!("expected an unknown-category refusal, got: {other}"),
    }
}

/// A threshold outside `0..=1000` is outside the contract the note records, so it is refused
/// where it is written rather than compared against a value it can never meet.
#[tokio::test]
async fn a_threshold_outside_the_contract_is_refused_at_load() {
    let err =
        load("[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\ndefault_m = 1001\n")
            .expect_err("a threshold above the maximum must be refused");
    assert!(
        matches!(err, ConfigError::PrivacyThresholdOutOfRange { .. }),
        "expected an out-of-range refusal, got: {err}"
    );
}

/// A per-category threshold for a category the configuration does not activate is refused:
/// it is a calibration of something nothing scans, and the two fields would otherwise
/// disagree about what is on.
#[tokio::test]
async fn a_threshold_for_an_unactivated_category_is_refused_at_load() {
    let err = load("[privacy]\ncategories = []\n\n[privacy.thresholds]\nperson = 900\n")
        .expect_err("a threshold for an unactivated category must be refused");
    match err {
        ConfigError::PrivacyThresholdWithoutCategory { ref category } => {
            assert_eq!(category, "person");
        }
        other => panic!("expected a threshold-without-category refusal, got: {other}"),
    }
}

/// A threshold of **zero** is deliberately accepted: it is the delivered default and the
/// state every category starts in, so refusing it would refuse the configuration the product
/// ships. What it buys is an observation without a marker, not an error.
#[tokio::test]
async fn a_zero_threshold_is_accepted() {
    let cfg = load("[privacy]\ncategories = [\"person\"]\n\n[privacy.thresholds]\nperson = 0\n")
        .expect("an explicit zero is the documented default, not a mistake");
    assert_eq!(cfg.privacy.thresholds.default_m, 0);
}
