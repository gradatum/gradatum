//! F-160 — the `mask` mode, from the TOML on disk to the body a caller reads back, and
//! the two things a deployment is told when it enables it.
//!
//! ## What these tests measure
//!
//! The mode is the one setting whose effect cannot be undone, so it is measured on the
//! text rather than on a flag, and the refusals are measured on the load path an operator
//! actually runs.
//!
//! - **The configuration decides the destruction.** A `[privacy]` block declaring `mask`
//!   over a calibrated category masks the body written through a vault mounted from that
//!   same block. Nothing is supplied by the fixture: the mode, the activation and the
//!   threshold all come from the TOML text.
//! - **A mode that could mask nothing is refused.** An operator who sets `mask` while
//!   every activated category sits at the uncalibrated threshold has written a posture
//!   that promises redaction and can never perform one. The refusal is on the value, not
//!   on the mode: a calibrated `mask` loads.
//! - **The delivered default is not the destructive one**, and it is proven on an empty
//!   configuration — a file with no `[privacy]` block at all.
//! - **A masking deployment is told what it runs.** The mode is warned at boot and
//!   recorded once in the audit trail; a reporting deployment, which changes no byte,
//!   gets neither.
//!
//! ## What is out of reach here
//!
//! The `/health/detailed` field is measured where the report is built, and the embedding
//! path in the worker that reads the note back before embedding it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::pii::{
    DetectInput, Detection, MASK_FILLER, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
    PrivacyBudgets,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::config::{ConfigError, PrivacyMode, ServerConfig};
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

/// The literal both the body and the detector's span are built on.
const NEEDLE: &str = "alice@example.com";

/// The reserved name a markable pass poses.
const PERSONAL_CLASSIFIED: &str = "personal-classified";

// ── The detector ─────────────────────────────────────────────────────────────

/// Reports one `email` span on the needle, at the score the test names.
struct Scored(f32);

impl PiiDetector for Scored {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(match input.text.find(NEEDLE) {
            Some(start) => Detection::new(
                vec![PiiSpan::new(
                    start,
                    NEEDLE.len(),
                    PiiCategory::Email,
                    self.0,
                )],
                0,
            ),
            None => Detection::new(Vec::new(), 0),
        })
    }
}

// ── Configuration ────────────────────────────────────────────────────────────

/// Loads a configuration from inline TOML through the delivered load path, so a test
/// observes what an operator's file actually produces — parsing and validation included.
fn load(toml: &str) -> Result<ServerConfig, ConfigError> {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write TOML");
    ServerConfig::load(Some(file.path()))
}

/// A `[privacy]` block in the masking mode over a calibrated `email` category.
const MASKING_TOML: &str = "\
[privacy]
mode = \"mask\"
categories = [\"email\"]

[privacy.thresholds]
email = 900
";

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

// ── The load path ────────────────────────────────────────────────────────────

#[test]
fn an_empty_configuration_is_report_only_and_masks_nothing() {
    let cfg = load("").expect("an empty file is a valid configuration");
    assert_eq!(
        cfg.privacy.mode,
        PrivacyMode::ReportOnly,
        "the delivered default observes"
    );
    assert!(
        !cfg.privacy.policy().masks(),
        "a configuration with no `[privacy]` block must not rewrite a single body"
    );
}

#[test]
fn a_mask_without_a_calibrated_category_is_refused_at_load() {
    // Two ways to declare a mode that could mask nothing: no activated category at all,
    // and an activated category whose threshold is the observation point.
    for (toml, context) in [
        (
            "[privacy]\nmode = \"mask\"\ncategories = []\n",
            "no category is activated",
        ),
        (
            "[privacy]\nmode = \"mask\"\ncategories = [\"email\"]\n[privacy.thresholds]\nemail = 0\n",
            "the only activated category is not calibrated",
        ),
    ] {
        let err = load(toml).expect_err("a mask that can mask nothing must be refused");
        assert!(
            matches!(err, ConfigError::PrivacyMaskWithoutActivation),
            "{context}: expected PrivacyMaskWithoutActivation, got: {err}"
        );
    }
}

#[test]
fn a_mask_over_a_calibrated_category_loads() {
    let cfg = load(MASKING_TOML).expect("a calibrated mask is a valid posture");
    assert_eq!(cfg.privacy.mode, PrivacyMode::Mask);
    assert!(
        cfg.privacy.policy().masks(),
        "the loaded mode reaches the policy, which is what the write path consults"
    );
}

#[test]
fn a_reporting_mode_over_an_uncalibrated_category_still_loads() {
    // The refusal is about `mask` alone: an observation point is a legitimate
    // configuration for a mode that destroys nothing.
    let cfg = load("[privacy]\nmode = \"report_only\"\ncategories = [\"email\"]\n")
        .expect("report_only accepts any activation, including none");
    assert_eq!(cfg.privacy.mode, PrivacyMode::ReportOnly);
}

// ── The destruction, end to end from the TOML ────────────────────────────────

#[tokio::test]
async fn a_configured_mask_destroys_what_the_activation_marks() {
    let cfg = load(MASKING_TOML).expect("the masking configuration loads");
    let dir = TempDir::new().expect("TempDir");
    // Mounted the way the boot path mounts a vault: the detector, and the activation
    // **the configuration declares** — never one supplied by the test.
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create")
        .with_detector(Arc::new(Scored(1.0)), PrivacyBudgets::default())
        .with_privacy_policy(cfg.privacy.policy());

    // Read back **from the vault**, not from the value the write returned: the claim is
    // about what was persisted, and only a fresh read answers that.
    let written = vault
        .write_note(fm(), format!("carte de contact : {NEEDLE}"))
        .await
        .expect("write");

    let note = vault
        .read_note_by_id(&written.id.to_string())
        .await
        .expect("read back");
    assert!(
        !note.body.markdown.contains(NEEDLE),
        "the configuration declared `mask`, so the matched text is gone, got: {}",
        note.body.markdown
    );
    assert!(
        note.body.markdown.contains(MASK_FILLER),
        "the passage is replaced rather than deleted, got: {}",
        note.body.markdown
    );
    assert!(
        note.frontmatter
            .tags
            .iter()
            .any(|tag| tag.as_str() == PERSONAL_CLASSIFIED),
        "the pass that destroyed the text also marked the note"
    );
}

#[tokio::test]
async fn a_configured_mask_spares_a_span_below_the_configured_threshold() {
    let cfg = load(MASKING_TOML).expect("the masking configuration loads");
    let dir = TempDir::new().expect("TempDir");
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create")
        .with_detector(Arc::new(Scored(0.4)), PrivacyBudgets::default())
        .with_privacy_policy(cfg.privacy.policy());

    let body = format!("carte de contact : {NEEDLE}");
    let written = vault.write_note(fm(), body.clone()).await.expect("write");

    let note = vault
        .read_note_by_id(&written.id.to_string())
        .await
        .expect("read back");
    assert_eq!(
        note.body.markdown, body,
        "the configured threshold is what bounds the destruction, span by span"
    );
}

// ── The boot announcement ────────────────────────────────────────────────────

/// An audit sink that keeps what it was given.
#[derive(Default)]
struct Recording {
    events: Mutex<Vec<HttpAuditEvent>>,
}

impl Recording {
    fn events(&self) -> Vec<HttpAuditEvent> {
        self.events.lock().expect("unpoisoned").clone()
    }
}

#[async_trait::async_trait]
impl AuditSink for Recording {
    async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        self.events.lock().expect("unpoisoned").push(event);
        Ok(())
    }
}

/// A log buffer a test can read back, installed as the thread's default subscriber.
///
/// The subscriber is set for the duration of one test and restored on drop, so a test
/// reads only the lines it produced — `#[tokio::test]` runs its future on the calling
/// thread, which is what makes the thread-local default apply to the code under test.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("unpoisoned")).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned buffer"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `f` under a capturing subscriber and returns what it logged.
async fn capture<F, Fut, T>(f: F) -> (T, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = f().await;
    (out, buffer.contents())
}

#[tokio::test]
async fn the_masking_mode_is_warned_and_audited_once_at_boot() {
    let cfg = load(MASKING_TOML).expect("the masking configuration loads");
    let audit = Arc::new(Recording::default());
    let state = AppState::new()
        .with_privacy(cfg.privacy.clone())
        .expect("this configuration mounts no sidecar, so nothing is reached")
        .with_audit(Arc::clone(&audit) as Arc<dyn AuditSink>);

    let (result, logs) = capture(|| state.announce_privacy_mode()).await;
    result.expect("the announcement is recorded");

    assert!(
        logs.contains("mask"),
        "a destructive mode must be visible in the boot log, got:\n{logs}"
    );

    let events = audit.events();
    assert_eq!(
        events.len(),
        1,
        "the mode is announced once, not once per vault"
    );
    let event = &events[0];
    assert_eq!(event.event, "privacy_mode");
    assert_eq!(
        event.curator.as_ref().and_then(|bag| bag.get("mode")),
        Some(&serde_json::json!("mask")),
        "the audit line states the mode that was enabled"
    );
}

#[tokio::test]
async fn the_reporting_mode_is_not_announced() {
    let cfg = load("").expect("an empty configuration loads");
    let audit = Arc::new(Recording::default());
    let state = AppState::new()
        .with_privacy(cfg.privacy.clone())
        .expect("this configuration mounts no sidecar, so nothing is reached")
        .with_audit(Arc::clone(&audit) as Arc<dyn AuditSink>);

    let (result, logs) = capture(|| state.announce_privacy_mode()).await;
    result.expect("the announcement is a no-op, not a failure");

    assert!(
        audit.events().is_empty(),
        "the delivered default must not fill the audit trail at every boot"
    );
    assert!(
        !logs.contains("mask") && !logs.contains("rewrites note bodies"),
        "nothing is announced about a mode that changes no byte, got:\n{logs}"
    );
}
