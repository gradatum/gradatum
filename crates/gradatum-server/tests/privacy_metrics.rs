//! F-160 L-D.2 — the personal-data frontier made observable, and the sweep that pays
//! the debt it records.
//!
//! ## What this file is for
//!
//! The frontier leaves two facts on a corpus: which notes carry a marker, and which notes
//! no pass has judged. Neither is visible from the notes without reading every one of them,
//! and the second is work the deployment owes. Three surfaces answer for them, and each is
//! asserted on the wire, not on an intermediate:
//!
//! - **`/metrics`** — the pass series, under bounded labels. A category, a motive, a
//!   constant: never a detected fragment, never a body, never a detector's own label.
//! - **`/health/detailed`** — the effective mode, the mounted detector's name, and two
//!   figures saying how much of the corpus has been judged. A note left unscanned
//!   **degrades** the report and never makes it unhealthy: a missing scan is a debt, not an
//!   outage, and the 2026-09-14 boot false-red is why that is asserted rather than assumed.
//! - **The deferred sweep** — `rescan_tick` enqueues, `rescan_selected_notes` re-judges
//!   through the write primitive. The product enqueues its own work: no HTTP call, no human,
//!   and no authority the write path does not already have.
//!
//! ## Fixture
//!
//! Every corpus here is written through a **real** `Vault` with a **scripted** detector
//! mounted, exactly as `privacy_list_by_tag.rs` does it: the reserved names are posed and
//! retired by the product's own verdict, never seeded by a write the product could not
//! perform. A scripted detector is the only way to decide what a pass concludes — the
//! delivered regex detector keys on literals this file would then have to embed, which is
//! the leak these series exist to avoid.
//!
//! The queue is a real `SqliteQueueStore` on a file, the registry is the real `AppMetrics`,
//! and the pass reports into it through the real `MetricsPrivacyObserver`. Nothing here
//! stands in for the thing under test.
//!
//! ## Two ports this file never touches
//!
//! `ServerConfig::default()` points the embedder at a fixed port and the listener at
//! another. Wherever a test asks `probe_once` for a verdict, the configuration it hands over
//! disables the embedder and points the curator at an ephemeral loopback listener this file
//! owns — see [`healthy_config`], which refuses a reserved port even when the kernel offers
//! one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::job::{JOB_KIND_PRIVACY_RESCAN, JobClass, JobMode, JobPriority, JobScope};
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scheduled_health::ScheduledTaskHealth;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_core::{Job, JobFilter, JobRecord, QueueStore};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
// `gradatum_dto::health` is a private module: these two are re-exported at the crate root.
use gradatum_dto::{HealthReport, HealthStatus};
use gradatum_server::config::{LlmConfig, PrivacyConfig, PrivacyMode, ServerConfig};
use gradatum_server::health;
use gradatum_server::metrics::{AppMetrics, PrivacyPatternLabel, metrics_handler};
use gradatum_server::privacy_observer::MetricsPrivacyObserver;
use gradatum_server::privacy_rescan::{RESCAN_BATCH_LIMIT, rescan_selected_notes, rescan_tick};
use gradatum_server::scheduled_tasks::TASK_PRIVACY_RESCAN;
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

// ── Literals ─────────────────────────────────────────────────────────────────

/// The two reserved names, spelled literally. Deliberately not derived from the reserved
/// tag type: a name silently renamed there must not rename the string this file observes
/// on a note or asks a query for.
const PERSONAL_CLASSIFIED: &str = "personal-classified";
const PRIVACY_UNSCANNED: &str = "privacy-unscanned";

/// Body carrying no marker: a completed pass over it reports nothing.
const BODY: &str = "contenu banal sans marqueur de confidentialite";

/// The same body, carrying the needle the marking script keys on.
const MARKED_BODY: &str = "contenu banal sans marqueur-de-test de confidentialite";

/// The needle [`MARKED_BODY`] carries, and the literal no exposition may ever contain.
const MARK: &str = "marqueur-de-test";

/// The two ports this file must not touch, refused even when the kernel offers one.
const RESERVED_PORTS: [u16; 2] = [19_090, 8_436];

/// ACL preset of the fixture: one identity, read everywhere and write on `main/*`.
///
/// Just wide enough for the two public surfaces this file drives — `vault_list` (read) and
/// `POST /jobs` (write on `main/jobs`) — and no wider than the tests need.
const PRESET: &str = r#"
[[consumer]]
identity = "f160-metrics"
read_patterns  = ["**"]
write_patterns = ["main/*"]
"#;

/// Identity the fixture's tokens are signed for.
const TEST_IDENTITY: &str = "f160-metrics";

// ── Detectors ────────────────────────────────────────────────────────────────

/// What the fixture needs a pass to conclude.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    /// Report nothing. A **completed clean** pass: it retires both reserved names.
    Silent,
    /// Report one span above the marking threshold, on the needle. Where the needle is
    /// absent this is a completed clean pass; where it is, the pass poses
    /// `personal-classified` and retires `privacy-unscanned`.
    Marking,
    /// Fail the pass. An incomplete pass has no authority over what is personal data: it
    /// poses `privacy-unscanned` and keeps whatever `personal-classified` it found.
    Failing,
}

impl Script {
    fn code(self) -> u8 {
        match self {
            Self::Silent => 0,
            Self::Marking => 1,
            Self::Failing => 2,
        }
    }

    fn of(code: u8) -> Self {
        match code {
            1 => Self::Marking,
            2 => Self::Failing,
            _ => Self::Silent,
        }
    }
}

/// A detector scripted to a shared [`Script`], so a test can change what the next pass
/// concludes **while a vault is live** — the only way to write a corpus under a broken
/// sidecar and then watch the same vault recover.
struct Scripted(Arc<AtomicU8>);

impl PiiDetector for Scripted {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        match Script::of(self.0.load(Ordering::SeqCst)) {
            Script::Silent => Ok(Detection::new(Vec::new(), 0)),
            Script::Marking => Ok(match input.text.find(MARK) {
                Some(start) => Detection::new(
                    vec![PiiSpan::new(start, MARK.len(), PiiCategory::Person, 0.9)],
                    0,
                ),
                None => Detection::new(Vec::new(), 0),
            }),
            Script::Failing => Err(PiiDetectError::Timeout { timeout_ms: 1 }),
        }
    }
}

/// Reports the same four window-relative spans for every window it is handed.
///
/// Window-relative on purpose: the vault rebases them onto the body before validation, so
/// where a span lands — and therefore which rejection it earns — is a function of the
/// window's own offset. See
/// [`metrics_expose_rejected_spans_and_window_edge_by_bounded_reason`], which derives every
/// expected count from that geometry.
struct Crafted;

impl PiiDetector for Crafted {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(
            vec![
                PiiSpan::new(2, 4, PiiCategory::Person, 0.9),
                PiiSpan::new(8, 4, PiiCategory::Email, 0.9),
                PiiSpan::new(1, 2, PiiCategory::Phone, 0.9),
                PiiSpan::new(200, 4, PiiCategory::Url, 0.9),
            ],
            2,
        ))
    }
}

// ── Fixture: real vaults, a real queue, a real registry ──────────────────────

/// How a fixture vault is mounted.
#[derive(Clone, Copy)]
enum Mount {
    /// No `with_detector` call at all: the vault keeps its pass-through mount, the shape a
    /// deployment with no configured category runs.
    PassThrough,
    /// The scripted detector, so the test decides what each pass concludes.
    Scripted,
    /// The detector reporting a fixed, window-relative span set.
    Crafted,
}

/// One vault of the fixture.
struct VaultSpec {
    vault_id: &'static str,
    mount: Mount,
    script: Script,
    budgets: PrivacyBudgets,
}

impl VaultSpec {
    /// A vault mounted with the scripted detector.
    fn new(vault_id: &'static str, script: Script) -> Self {
        Self {
            vault_id,
            mount: Mount::Scripted,
            script,
            budgets: PrivacyBudgets::default(),
        }
    }

    /// A vault left on its pass-through mount.
    fn pass_through(vault_id: &'static str) -> Self {
        Self {
            vault_id,
            mount: Mount::PassThrough,
            script: Script::Silent,
            budgets: PrivacyBudgets::default(),
        }
    }

    /// A vault mounted with the crafted detector.
    fn crafted(vault_id: &'static str, budgets: PrivacyBudgets) -> Self {
        Self {
            vault_id,
            mount: Mount::Crafted,
            script: Script::Silent,
            budgets,
        }
    }

    fn budgets(mut self, budgets: PrivacyBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

/// The world one test runs in.
struct Env {
    state: AppState,
    metrics: AppMetrics,
    /// One handle per spec, in the order the specs were given.
    vaults: Vec<Arc<Vault>>,
    /// The switch of `vaults[i]`'s scripted detector, meaningful for [`Mount::Scripted`].
    scripts: Vec<Arc<AtomicU8>>,
    /// Kept alive: the vault roots, the queue file and the studio directory.
    _dirs: Vec<TempDir>,
    /// Kept alive: the loopback listeners the curator probe dials.
    _listeners: Vec<tokio::net::TcpListener>,
}

impl Env {
    fn vault(&self, vault_id: &str) -> &Arc<Vault> {
        self.vaults
            .iter()
            .find(|v| v.vault_id().as_str() == vault_id)
            .unwrap_or_else(|| panic!("no vault {vault_id:?} in this fixture"))
    }

    /// Switches what the next pass over `vault_id` concludes.
    fn script(&self, vault_id: &str, script: Script) {
        let idx = self
            .vaults
            .iter()
            .position(|v| v.vault_id().as_str() == vault_id)
            .unwrap_or_else(|| panic!("no vault {vault_id:?} in this fixture"));
        self.scripts[idx].store(script.code(), Ordering::SeqCst);
    }

    /// The jobs the queue holds, selected by `filter`.
    async fn jobs(&self, filter: JobFilter) -> Vec<JobRecord> {
        self.state
            .job_store
            .list(filter)
            .await
            .expect("list the queue")
    }

    /// The scheduled-health row of `task`, or a panic naming the task.
    async fn task_row(&self, task: &str, now_ms: i64) -> ScheduledTaskHealth {
        self.state
            .search
            .list_scheduled_health(now_ms)
            .await
            .expect("list_scheduled_health")
            .into_iter()
            .find(|row| row.task_name == task)
            .unwrap_or_else(|| panic!("task {task:?} absent from scheduled health"))
    }
}

/// Builds the world: one real vault per spec, a real on-file queue, one metrics registry
/// shared by the observer and the state.
///
/// `privacy` is the *server* configuration the tick gates on and the health report
/// describes. It is deliberately independent of the mount each vault was given, because in
/// production those two answers come from two different places — the report names the
/// detector the configuration declares, and a vault is handed whichever handle the boot path
/// built. Tests that assert on the name keep the two aligned by construction.
async fn env_of(specs: &[VaultSpec], privacy: PrivacyConfig) -> Env {
    let metrics = AppMetrics::new();
    let mut dirs: Vec<TempDir> = Vec::new();
    let mut listeners: Vec<tokio::net::TcpListener> = Vec::new();
    let mut vaults: Vec<Arc<Vault>> = Vec::new();
    let mut scripts: Vec<Arc<AtomicU8>> = Vec::new();

    for spec in specs {
        let dir = TempDir::new().expect("TempDir vault");
        let switch = Arc::new(AtomicU8::new(spec.script.code()));
        let opened = Vault::create(dir.path(), VaultId::new(spec.vault_id))
            .await
            .expect("Vault::create");
        let observer = Arc::new(MetricsPrivacyObserver::new(metrics.clone()));
        let vault = match spec.mount {
            Mount::PassThrough => opened.with_privacy_observer(observer),
            Mount::Scripted => opened
                .with_detector(Arc::new(Scripted(Arc::clone(&switch))), spec.budgets)
                .with_privacy_observer(observer),
            Mount::Crafted => opened
                .with_detector(Arc::new(Crafted), spec.budgets)
                .with_privacy_observer(observer),
        };
        dirs.push(dir);
        vaults.push(Arc::new(vault));
        scripts.push(switch);
    }

    let registry = VaultRegistry::new();
    for vault in &vaults {
        registry
            .add_vault(vault.vault_id().clone(), Arc::clone(vault))
            .expect("add_vault");
    }

    // A real queue on a file: the contract of `list`/`enqueue`/`count_jobs_by_status` the
    // tick consumes is the one the deployment runs, not an in-memory approximation.
    let queue_dir = TempDir::new().expect("TempDir queue");
    let pool = QueueDb::open(&queue_dir.path().join("queue.db"))
        .await
        .expect("open the queue database");
    run_migrations(&pool).await.expect("migrate gradatum_jobs");

    // A live worker, so the probe does not call this fixture unhealthy for a reason that has
    // nothing to do with personal data. `worker_lease_fresh` answers `Some(false)` on an
    // **absent** row — that is the fail-closed reading of "no leader has ever claimed the
    // slot", and a running deployment always has one — so the fixture writes the single row
    // the probe reads, with the fresh expiry a live worker would have left there. Without
    // it every health assertion below would be measuring the fixture, not the frontier.
    let expires_at = chrono::Utc::now().timestamp_millis() + 300_000;
    pool.with_conn(move |conn| {
        // The leader-election table belongs to the queue crate's schema, not to the job
        // migrations `run_migrations` applies — taken from its own constant rather than
        // re-spelled here, so a column change cannot leave this fixture quietly wrong.
        conn.execute_batch(gradatum_queue::schema::SCHEMA_V1)?;
        conn.execute(
            "INSERT OR REPLACE INTO worker_leadership (slot, holder, expires_at) \
             VALUES (0, ?1, ?2)",
            rusqlite::params!["f160-metrics-fixture", expires_at],
        )
        .map(|_| ())
    })
    .await
    .expect("seed the worker leadership row");

    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    let cfg = healthy_config(&mut dirs, &mut listeners).await;
    let acl = AclEngine::from_preset_str(PRESET).expect("a valid ACL preset");

    let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_vault_arc(Arc::clone(&vaults[0]) as Arc<dyn Registry>)
        .with_privacy(privacy)
        .with_job_store(store as Arc<dyn QueueStore>, pool);
    state.vaults = Arc::new(registry);
    // The index the state reads must be a real one: a placeholder answers `NoteNotFound` and
    // turns a read into a 404.
    state.search = vaults[0].index().clone();
    state.metrics = metrics.clone();
    state.server_config = Arc::new(cfg);
    dirs.push(queue_dir);

    Env {
        state,
        metrics,
        vaults,
        scripts,
        _dirs: dirs,
        _listeners: listeners,
    }
}

/// A server config `probe_once` can reach `Ok` on: the embedder is disabled, the curator's
/// LLM answers on a live loopback socket, the studio assets exist, and the queue carries no
/// leadership lease.
///
/// The listener is bound on port 0 — an ephemeral port — and a reserved port is refused even
/// if the kernel offers it, so this fixture cannot reach a port it is forbidden to touch.
async fn healthy_config(
    dirs: &mut Vec<TempDir>,
    listeners: &mut Vec<tokio::net::TcpListener>,
) -> ServerConfig {
    let studio = TempDir::new().expect("TempDir studio");
    let (listener, port) = loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("local_addr").port();
        if !RESERVED_PORTS.contains(&port) {
            break (listener, port);
        }
    };

    let mut cfg = ServerConfig::default();
    cfg.embed.enabled = false;
    cfg.studio.ui_dir = studio.path().to_path_buf();
    cfg.curator.backend = "openai_compat".to_string();
    cfg.curator.llm = Some(LlmConfig {
        backend: "openai_compat".to_string(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "fixture".to_string(),
        api_key_env: None,
        timeout_ms: 500,
        fallback: None,
    });

    dirs.push(studio);
    listeners.push(listener);
    cfg
}

// ── Seeding ──────────────────────────────────────────────────────────────────

/// The reserved name as it arrives through the **deserialisation** door.
///
/// That door is deliberately non-validating: no constructor produces a reserved tag, so the
/// only way a test can seed a note "by transit" — carrying a name the frontier owns and no
/// pass posed — is through it.
fn deserialised_reserved(literal: &str) -> Tag {
    serde_json::from_value(serde_json::Value::String(literal.to_owned()))
        .expect("a reserved tag deserialises — the door is deliberately non-validating")
}

/// A minimal frontmatter in `section`, `live`, in `vault_id`, carrying `tags`.
fn fm_of(vault_id: &str, section: Section, tags: &[&str]) -> Frontmatter {
    let mut fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section,
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
    };
    fm.tags = tags
        .iter()
        .map(|t| match *t {
            PERSONAL_CLASSIFIED | PRIVACY_UNSCANNED => deserialised_reserved(t),
            other => Tag::new(other).expect("an ordinary tag"),
        })
        .collect::<Vec<_>>()
        .into();
    fm
}

/// Writes a note through the concrete vault and returns its ULID.
async fn seed(
    vault: &Vault,
    vault_id: &str,
    section: Section,
    tags: &[&str],
    body: &str,
) -> String {
    vault
        .write_note(fm_of(vault_id, section, tags), body.to_owned())
        .await
        .expect("write_note")
        .id
        .to_string()
}

/// The tags the vault really left on a note, read back from disk.
///
/// The premise of several assertions below: the reserved names are posed by the product's own
/// pass, so a test that assumed them would be asserting on a note that may not exist.
async fn tags_of(vault: &Vault, id: &str) -> Vec<String> {
    vault
        .read_note_by_id(id)
        .await
        .expect("read the note back")
        .frontmatter
        .tags
        .iter()
        .map(|t| t.as_str().to_owned())
        .collect()
}

/// The `status` the note's `privacy` block carries, or `None` when it carries no block.
async fn block_status(vault: &Vault, id: &str) -> Option<String> {
    let note = vault.read_note_by_id(id).await.expect("read the note back");
    note.frontmatter
        .extra
        .get("privacy")
        .and_then(|block| block.get("status"))
        .and_then(|status| status.as_str())
        .map(str::to_owned)
}

/// The content hash the index holds for a note, hex-encoded.
async fn sha256_of(vault: &Vault, vault_id: &str, id: &str) -> String {
    let note_id = NoteId(Ulid::from_string(id).expect("the vault returned a ULID"));
    vault
        .index()
        .get_content_hash(vault_id, note_id)
        .await
        .expect("get_content_hash")
        .expect("the note is indexed")
        .hex()
}

// ── Reading the two published surfaces ───────────────────────────────────────

/// The exposition `/metrics` serves for `metrics`.
async fn exposition(metrics: &AppMetrics) -> String {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics.clone());
    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .expect("build GET /metrics");
    let response = app.oneshot(request).await.expect("service /metrics");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/metrics must render the registry"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read /metrics")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("the exposition is UTF-8")
}

/// The value of one exposition sample, matched on the **whole** series name, labels included.
/// `None` when no such sample is rendered — a family with no children renders nothing at all,
/// which is not the same answer as a sample at zero.
fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            (name == series).then(|| value.parse::<f64>().ok())?
        })
}

/// The value of one exposition sample, or a panic naming the series that is missing.
fn count(text: &str, series: &str) -> f64 {
    sample(text, series)
        .unwrap_or_else(|| panic!("series {series:?} absent from the exposition:\n{text}"))
}

/// The label keys of one exposition sample, in the order rendered — the shape of a series,
/// read without assuming which labels it carries.
fn label_keys(text: &str, series: &str) -> Vec<String> {
    let line = text
        .lines()
        .find(|line| {
            !line.starts_with('#')
                && line
                    .strip_prefix(series)
                    .is_some_and(|r| r.starts_with('{'))
        })
        .unwrap_or_else(|| panic!("no sample of {series:?} carries labels:\n{text}"));
    let Some((_, rest)) = line.split_once('{') else {
        panic!("sample {series:?} carries no label set");
    };
    let Some((labels, _)) = rest.rsplit_once('}') else {
        panic!("sample {series:?} has an unterminated label set");
    };
    labels
        .split(',')
        .filter_map(|pair| pair.split_once('=').map(|(key, _)| key.to_owned()))
        .collect()
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

fn sign(state: &AppState, tenant: &str) -> String {
    state
        .jwt
        .sign(
            TEST_IDENTITY,
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign the test JWT")
}

/// `POST <path>` with a bearer token, returning the status and the raw body.
async fn post(
    app: Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("Idempotency-Key", format!("f160-{}", Ulid::generate()))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    let response = app.oneshot(request).await.expect("service");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The `path` of every entry `vault_list` returns for `tag`.
async fn listed_paths(state: AppState, token: &str, tag: &str) -> Vec<String> {
    let (status, raw) = post(
        build_router(state),
        "/api/v1/vault_list",
        token,
        serde_json::json!({ "limit": 50, "tag": tag }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "vault_list with a tag filter: {raw}"
    );
    let body: serde_json::Value = serde_json::from_str(&raw).expect("vault_list answers JSON");
    body["entries"]
        .as_array()
        .expect("entries must be an array")
        .iter()
        .filter_map(|entry| entry["path"].as_str().map(str::to_owned))
        .collect()
}

// ── /metrics ─────────────────────────────────────────────────────────────────

/// A completed pass that marks a note feeds `notes_tagged{category}` for the categories the
/// marker can rest on, and the scanned/unscanned pair advances once per pass.
///
/// The second half is the anti-leak claim: the exposition of a pass that found the needle and
/// one that did not are indistinguishable apart from the counters. No body, no matched
/// fragment and no detector label is representable in a privacy series.
#[tokio::test]
async fn metrics_expose_tagged_by_category() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Marking)],
        PrivacyConfig::default(),
    )
    .await;

    let first = seed(
        env.vault("main"),
        "main",
        Section::Decisions,
        &[],
        MARKED_BODY,
    )
    .await;
    assert!(
        tags_of(env.vault("main"), &first)
            .await
            .contains(&PERSONAL_CLASSIFIED.to_owned()),
        "premise: a marking pass left the payload marker"
    );

    let text = exposition(&env.metrics).await;
    assert_eq!(
        count(
            &text,
            "gradatum_privacy_notes_tagged_total{category=\"person\"}"
        ),
        1.0,
        "a completed pass carrying a person span above threshold marks the note:\n{text}"
    );
    assert_eq!(
        count(&text, "gradatum_privacy_scanned_total"),
        1.0,
        "the pass completed over the whole of its input"
    );
    assert_eq!(
        sample(&text, "gradatum_privacy_unscanned_total"),
        Some(0.0),
        "a completed pass poses no rescan marker"
    );

    // A second write, over a body the pass finds nothing in. The category counter does not
    // move — a retired marker is not a note left tagged — and the partition advances by one.
    seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;

    let text = exposition(&env.metrics).await;
    assert_eq!(
        count(
            &text,
            "gradatum_privacy_notes_tagged_total{category=\"person\"}"
        ),
        1.0,
        "the clean pass retires the marker and adds nothing to the category series"
    );
    assert_eq!(count(&text, "gradatum_privacy_scanned_total"), 2.0);
    assert_eq!(sample(&text, "gradatum_privacy_unscanned_total"), Some(0.0));

    // No privacy series carries the body, the matched fragment or a detector's own label.
    // Scoped to the `gradatum_privacy_` families: the registry holds other modules' series,
    // and this claim is about this one.
    let privacy_lines: String = text
        .lines()
        .filter(|line| line.starts_with("gradatum_privacy_"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !privacy_lines.contains(MARK),
        "no privacy series may carry the body or the matched fragment:\n{privacy_lines}"
    );
    assert!(
        !privacy_lines.contains(BODY),
        "and none may carry the body either:\n{privacy_lines}"
    );
    assert!(
        !privacy_lines.contains("detector"),
        "nor a detector's own label:\n{privacy_lines}"
    );
}

/// An oversized body and an unanswered detector are two different facts, counted apart.
///
/// Conflating them would make a large body look like a broken sidecar and a broken sidecar
/// look like a large body — and only one of the two is fixed by calibrating a threshold.
#[tokio::test]
async fn metrics_count_input_too_large_apart_from_detector_failures() {
    // A zero ceiling admits no window at all: the pass ends on its own size state without the
    // detector ever being called.
    let oversized = PrivacyBudgets::new(0, 0, 10_000, 2_000, 0, 20_000);
    let env = env_of(
        &[
            VaultSpec::new("main", Script::Marking).budgets(oversized),
            VaultSpec::new("other", Script::Failing),
        ],
        PrivacyConfig::default(),
    )
    .await;

    let big = seed(
        env.vault("main"),
        "main",
        Section::Decisions,
        &[],
        MARKED_BODY,
    )
    .await;
    assert_eq!(
        block_status(env.vault("main"), &big).await.as_deref(),
        Some("input_too_large"),
        "a body with no window to scan is a size state"
    );

    let text = exposition(&env.metrics).await;
    assert_eq!(
        count(&text, "gradatum_privacy_input_too_large_total"),
        1.0,
        "the size state is counted under its own series"
    );
    assert_eq!(count(&text, "gradatum_privacy_unscanned_total"), 1.0);
    assert_eq!(sample(&text, "gradatum_privacy_scanned_total"), Some(0.0));

    // Now a detector that does not answer. The size series must not move.
    let down = seed(env.vault("other"), "other", Section::Decisions, &[], BODY).await;
    assert_eq!(
        block_status(env.vault("other"), &down).await.as_deref(),
        Some("detector_unavailable"),
        "a detector fault is a different state from a size state"
    );

    let text = exposition(&env.metrics).await;
    assert_eq!(
        count(&text, "gradatum_privacy_input_too_large_total"),
        1.0,
        "a detector fault is not a size state:\n{text}"
    );
    assert_eq!(
        count(&text, "gradatum_privacy_unscanned_total"),
        2.0,
        "both incomplete passes are owed a re-judgement"
    );
}

/// The exception series carries an identifier, and its label set could hold nothing else.
///
/// **What is enforced here, and what is not.** The identifier names a pattern declared in the
/// configuration; the family is declared with **no producer** in 2.4.0 — the exception pass is
/// configured but its consumer is not delivered — so this test pins the *shape* a future
/// producer must respect, and cannot pin a producer that does not exist. Read it as a contract
/// on the label set, never as a guard on a value.
#[tokio::test]
async fn exception_hit_metric_carries_pattern_id_never_the_pattern() {
    let metrics = AppMetrics::new();
    // Fed under an identifier — the shape a producer enumerating the configured patterns
    // would use, never the pattern's own text.
    metrics
        .privacy_exception_hits
        .get_or_create(&PrivacyPatternLabel {
            pattern_id: "0".to_owned(),
        })
        .inc();

    let text = exposition(&metrics).await;
    assert_eq!(
        sample(
            &text,
            "gradatum_privacy_exception_hits_total{pattern_id=\"0\"}"
        ),
        Some(1.0),
        "the series is keyed by the identifier:\n{text}"
    );
    assert_eq!(
        label_keys(&text, "gradatum_privacy_exception_hits_total"),
        vec!["pattern_id".to_owned()],
        "the label set carries one key, the identifier — no key named for the pattern, the \
         matched text, or a fragment of either could exist beside it"
    );
}

/// Rejection motives and the window-edge count are **bounded**: two reasons, one constant
/// label value, one unlabelled counter — every expected figure derived here from the window
/// geometry, so a green-time mismatch is diagnosed as a wrong derivation step rather than
/// fixed by weakening an assertion.
///
/// ## The derivation
///
/// The body is 32 bytes of two-byte characters, the ceiling 16 bytes and the overlap 4, so the
/// window plan cuts it into three windows at offsets 0, 12 and 24, with byte lengths 16, 16 and
/// 8. The interior cut offsets are therefore 16 and 28. [`Crafted`] answers the same four
/// window-relative spans to each window, and the vault rebases them onto the body before
/// validating, so:
///
/// | window-relative | +0 | +12 | +24 |
/// |---|---|---|---|
/// | `(2, 4)`   | `(2, 4)` accepted  | `(14, 4)` accepted | `(26, 4)` accepted |
/// | `(8, 4)`   | `(8, 4)` accepted  | `(20, 4)` accepted | `(32, 4)` out of bounds |
/// | `(1, 2)`   | `(1, 2)` off-byte  | `(13, 2)` off-byte | `(25, 2)` off-byte |
/// | `(200, 4)` | out of bounds      | out of bounds      | out of bounds |
///
/// giving 4 out-of-bounds rejections, 3 rejections for an end off a code-point boundary, and 6
/// unmapped labels — two per window. Five spans survive validation, and no two of a category
/// overlap, so fusion merges none of them. Of those five, `(14, 4)` and `(26, 4)` each come
/// within one byte of a cut, and they are the two the edge counter reports.
#[tokio::test]
async fn metrics_expose_rejected_spans_and_window_edge_by_bounded_reason() {
    let budgets = PrivacyBudgets::new(16, 4, 10_000, 2_000, 0, 20_000);
    let env = env_of(
        &[VaultSpec::crafted("main", budgets)],
        PrivacyConfig::default(),
    )
    .await;

    // Sixteen two-byte characters: 32 bytes, every code-point boundary even.
    seed(
        env.vault("main"),
        "main",
        Section::Decisions,
        &[],
        &"é".repeat(16),
    )
    .await;

    let text = exposition(&env.metrics).await;
    assert_eq!(
        count(
            &text,
            "gradatum_privacy_spans_rejected_total{reason=\"out_of_bounds\"}"
        ),
        4.0,
        "one span per window falls past the end of the body:\n{text}"
    );
    assert_eq!(
        count(
            &text,
            "gradatum_privacy_spans_rejected_total{reason=\"utf8_boundary\"}"
        ),
        3.0,
        "one span per window lands off a code-point boundary:\n{text}"
    );
    assert_eq!(
        count(&text, "gradatum_privacy_spans_window_edge_total"),
        2.0,
        "a span within one byte of an interior cut is the overlap's doing, not the \
         detector's:\n{text}"
    );
    assert_eq!(
        count(
            &text,
            "gradatum_privacy_unmapped_labels_total{label=\"unmapped\"}"
        ),
        6.0,
        "two unmapped labels per window, summed over three windows:\n{text}"
    );
    assert_eq!(
        label_keys(&text, "gradatum_privacy_spans_rejected_total"),
        vec!["reason".to_owned()],
        "the rejection series is labelled by reason alone"
    );
    assert_eq!(
        sample(
            &text,
            "gradatum_privacy_spans_rejected_total{reason=\"detector_unavailable\"}"
        ),
        None,
        "a motive outside the closed vocabulary must not be renderable:\n{text}"
    );
}

// ── /health/detailed ─────────────────────────────────────────────────────────

/// The report states the mode the write path runs under, and a corpus with no privacy data
/// reports zeros rather than an absent field.
///
/// The distinction matters because an absent field and a zero would otherwise be the same
/// JSON: an external probe could not tell a deployment that has judged nothing from one that
/// does not report at all.
#[tokio::test]
async fn detailed_health_reports_effective_mode_and_zero_on_empty_corpus() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Silent)],
        PrivacyConfig::default(),
    )
    .await;
    let report = health::probe_once(&env.state, Duration::from_millis(800)).await;

    assert_eq!(
        report.privacy_mode, "report_only",
        "the delivered mode is the effective one"
    );
    assert_eq!(report.privacy_scanned_total, 0, "empty corpus ⇒ zero");
    assert_eq!(report.privacy_unscanned_total, 0, "empty corpus ⇒ zero");
    assert_eq!(
        report.status,
        HealthStatus::Ok,
        "an empty corpus is not a degraded one"
    );

    // An empty corpus still carries the two figures on the wire.
    let wire = serde_json::to_value(&report).expect("serialise the report");
    assert_eq!(wire["privacy_mode"], serde_json::json!("report_only"));
    assert_eq!(wire["privacy_scanned_total"], serde_json::json!(0));
    assert_eq!(wire["privacy_unscanned_total"], serde_json::json!(0));

    // `mask` is not reachable in 2.4.0, and the report says so from the configuration rather
    // than from a constant: the mode is *effective*, not asserted.
    let masked = env_of(
        &[VaultSpec::new("main", Script::Silent)],
        PrivacyConfig {
            mode: PrivacyMode::Mask,
            ..PrivacyConfig::default()
        },
    )
    .await;
    let report = health::probe_once(&masked.state, Duration::from_millis(800)).await;
    assert_eq!(report.privacy_mode, "mask");
}

/// Which detector judged the corpus is the one thing a caller cannot infer from the counts,
/// and it is published on the same report.
///
/// The two names are the mount point's vocabulary, not the Rust type's: `noop` says nothing is
/// mounted — writes are byte-identical to pre-frontier — and `regex` says the delivered
/// detector is. The name is read from the declared configuration, which is the boot path's own
/// source for which handle it builds; this fixture keeps the two aligned by mounting the
/// pass-through for the empty category list and a detector for the non-empty one.
#[tokio::test]
async fn detailed_health_reports_the_mounted_detector_name() {
    let noop = env_of(&[VaultSpec::pass_through("main")], PrivacyConfig::default()).await;
    let report = health::probe_once(&noop.state, Duration::from_millis(800)).await;
    assert_eq!(
        report.privacy_detector, "noop",
        "an empty category list mounts the pass-through"
    );

    let regex = env_of(
        &[VaultSpec::new("main", Script::Silent)],
        PrivacyConfig {
            categories: vec!["email".to_owned()],
            ..PrivacyConfig::default()
        },
    )
    .await;
    let report = health::probe_once(&regex.state, Duration::from_millis(800)).await;
    assert_eq!(
        report.privacy_detector, "regex",
        "an explicitly activated category mounts the delivered detector"
    );

    // A report exposing no name omits the field rather than publishing an empty one: a client
    // can then tell "no name" from "a name that happens to be blank".
    let bare = HealthReport::new(HealthStatus::Ok, "2.4.0".to_owned());
    let wire = serde_json::to_value(&bare).expect("serialise the report");
    assert!(
        wire.get("privacy_detector").is_none(),
        "an unnamed detector is omitted, not blank: {wire}"
    );
}

/// A note written unscanned **degrades** the report and never makes it unhealthy.
///
/// An unscanned body is work the sweep owes, not a fault of the service; the 2026-09-14 boot
/// false-red is why this is asserted end to end rather than left to the builder's unit tests.
/// The direction is monotone too: a report already `unhealthy` for its own reasons is left
/// alone, and these figures can never improve a status either.
#[tokio::test]
async fn unscanned_write_degrades_but_never_unhealthy() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Silent)],
        PrivacyConfig::default(),
    )
    .await;

    let before = health::probe_once(&env.state, Duration::from_millis(800)).await;
    assert_eq!(
        before.status,
        HealthStatus::Ok,
        "the fixture's baseline must be healthy, or the degradation below proves nothing"
    );

    env.script("main", Script::Failing);
    seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;

    let after = health::probe_once(&env.state, Duration::from_millis(800)).await;
    assert_eq!(
        after.privacy_unscanned_total, 1,
        "the write left one note no pass could judge"
    );
    assert_eq!(
        after.status,
        HealthStatus::Degraded,
        "an unscanned note degrades the report"
    );
    assert_ne!(
        after.status,
        HealthStatus::Unhealthy,
        "a missing scan is a debt, not an outage — it must never page anyone"
    );

    // Direction, at the builder: a report already `unhealthy` is not improved by the privacy
    // figures, and one at `ok` with nothing unscanned is not degraded by them.
    let unhealthy = HealthReport::new(HealthStatus::Unhealthy, "2.4.0".to_owned());
    assert_eq!(
        unhealthy.with_privacy("report_only", 17, 3).status,
        HealthStatus::Unhealthy,
        "the privacy figures never raise a status"
    );
    let ok = HealthReport::new(HealthStatus::Ok, "2.4.0".to_owned());
    assert_eq!(
        ok.with_privacy("report_only", 17, 0).status,
        HealthStatus::Ok,
        "a fully judged corpus leaves the status where it was"
    );
}

// ── The reprise surface ──────────────────────────────────────────────────────

/// A corpus an incomplete pass left behind is findable by the very name the sweep selects on:
/// this is the operator's reprise surface, and what a job's batch bound is applied to.
#[tokio::test]
async fn sidecar_down_corpus_is_unscanned_and_found_back_by_tag_filter() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Failing)],
        PrivacyConfig::default(),
    )
    .await;
    let id = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;

    assert_eq!(
        tags_of(env.vault("main"), &id).await,
        vec![PRIVACY_UNSCANNED.to_owned()],
        "an incomplete pass poses the rescan marker and nothing else"
    );

    let token = sign(&env.state, "main");
    let paths = listed_paths(env.state.clone(), &token, PRIVACY_UNSCANNED).await;
    assert_eq!(
        paths.len(),
        1,
        "the unscanned note is exactly what the reprise surface must hand back: {paths:?}"
    );
    assert!(
        paths[0].ends_with(&id),
        "the returned entry is the note the failing pass left behind: {paths:?}"
    );
}

/// A body past the per-call ceiling is marked for a rescan, and the **vault enqueues nothing**
/// — the queue stays empty.
///
/// The vault holds no queue handle at all, so this is a property of its shape rather than of a
/// runtime check. It is asserted because that shape is what keeps the enqueue site in the
/// server, where the registry and the queue are both in hand.
#[tokio::test]
async fn oversized_body_is_marked_input_too_large_without_enqueueing_from_the_vault() {
    let oversized = PrivacyBudgets::new(0, 0, 10_000, 2_000, 0, 20_000);
    let env = env_of(
        &[VaultSpec::new("main", Script::Marking).budgets(oversized)],
        PrivacyConfig::default(),
    )
    .await;

    let id = seed(
        env.vault("main"),
        "main",
        Section::Decisions,
        &[],
        MARKED_BODY,
    )
    .await;

    assert_eq!(
        block_status(env.vault("main"), &id).await.as_deref(),
        Some("input_too_large"),
        "the verdict names the size state, not a detector fault"
    );
    assert_eq!(
        tags_of(env.vault("main"), &id).await,
        vec![PRIVACY_UNSCANNED.to_owned()],
        "a size state is an incomplete pass: the note is owed a re-judgement"
    );
    assert!(
        env.jobs(JobFilter::default()).await.is_empty(),
        "the vault enqueued nothing — it owns no queue handle"
    );
}

/// The product enqueues its own sweep: a tick, a corpus the tag filter answers for, and a
/// `Job::PrivacyRescan` in the queue — with no HTTP call anywhere in the test.
///
/// This is the corrected N-P0-1 requirement. `POST /api/v1/jobs` is closed to a client for
/// this kind, so the product must be the one that enqueues, along the path the guard's own
/// doc-comment names: the server holds both the registry and the queue.
#[tokio::test]
async fn privacy_rescan_is_enqueued_without_human_action() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Failing)],
        PrivacyConfig {
            rescan_enabled: true,
            categories: vec!["email".to_owned()],
            ..PrivacyConfig::default()
        },
    )
    .await;
    let id = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;
    assert!(
        tags_of(env.vault("main"), &id)
            .await
            .contains(&PRIVACY_UNSCANNED.to_owned()),
        "premise: the corpus carries work for the sweep"
    );

    let now_ms = chrono::Utc::now().timestamp_millis();
    let stats = rescan_tick(&env.state, now_ms).await;

    assert!(!stats.disabled, "the work is enabled for this fixture");
    assert_eq!(stats.vaults_considered, 1);
    assert_eq!(stats.vaults_selected, 1);
    assert_eq!(stats.notes_seen, 1);
    assert_eq!(stats.jobs_enqueued, 1);

    let jobs = env
        .jobs(JobFilter {
            kind: Some(JOB_KIND_PRIVACY_RESCAN.to_owned()),
            ..JobFilter::default()
        })
        .await;
    assert_eq!(
        jobs.len(),
        1,
        "the tick enqueued one sweep job, with no HTTP call and no human"
    );

    let spec = &jobs[0].spec;
    match spec.kind {
        Job::PrivacyRescan(ref inner) => {
            assert_eq!(inner.vault_id, "main");
            assert_eq!(
                inner.limit, RESCAN_BATCH_LIMIT,
                "the job carries the batch bound its execution honours"
            );
        }
        ref other => panic!("expected a PrivacyRescan job, got {other:?}"),
    }
    // A system cron job, batched, scoped to the one vault, at the lowest priority: a sweep
    // must never compete with an agent's or a human's write.
    assert_eq!(spec.class, JobClass::System);
    assert_eq!(spec.mode, JobMode::Batch);
    assert_eq!(spec.priority, JobPriority::Low);
    assert!(
        matches!(spec.scope, JobScope::Vault(ref v) if v == "main"),
        "one job serves exactly one vault: {:?}",
        spec.scope
    );
    // The payload is a vault name and a bound: no note content crosses into the queue.
    let serialised = serde_json::to_string(spec).expect("serialise the spec");
    assert!(
        !serialised.contains(BODY) && !serialised.contains(MARK),
        "the queue holds no note body: {serialised}"
    );
}

/// A client cannot trigger a sweep: the public job API refuses the kind.
///
/// ⚠️ **This test passes by construction and guards nothing.** `job_kind_is_triggerable` is a
/// `matches!` over five kinds and `PrivacyRescan` is not among them, so the assertion holds
/// whatever the sweep does. The request carries a **fully valid** spec precisely so the `400`
/// cannot come from deserialisation — but even so, it is kept for its null cost and for the
/// record, and must never be presented as evidence that the surface is closed.
#[tokio::test]
async fn privacy_rescan_is_rejected_by_post_jobs_400() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Silent)],
        PrivacyConfig::default(),
    )
    .await;
    let token = sign(&env.state, "main");

    let (status, raw) = post(
        build_router(env.state.clone()),
        "/api/v1/jobs",
        &token,
        serde_json::json!({
            "spec": { "kind": "PrivacyRescan", "vault_id": "main", "limit": RESCAN_BATCH_LIMIT }
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a client does not start a sweep: {raw}"
    );
    assert!(
        env.jobs(JobFilter::default()).await.is_empty(),
        "and the rejection enqueued nothing"
    );
}

/// With the work gated off the tick **runs**, selects nothing, and **says so**.
///
/// The task is always spawned; the work is what is gated. A task that were seeded, reported as
/// scheduled and never executed would be a divergence by default on every installation —
/// exactly what the scheduled-task SSOT promises not to have.
#[tokio::test]
async fn rescan_tick_is_a_noop_under_noop_detector() {
    // `categories = []` mounts the pass-through: no detector is configured, so no pass has an
    // identity to be attributed to and no sweep has anything to re-run. The vault is still
    // mounted with the scripted detector so the corpus really carries work — an empty queue is
    // then the tick's decision, not a missing premise.
    let env = env_of(
        &[VaultSpec::new("main", Script::Failing)],
        PrivacyConfig {
            rescan_enabled: true,
            ..PrivacyConfig::default()
        },
    )
    .await;
    let id = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;
    assert!(
        tags_of(env.vault("main"), &id)
            .await
            .contains(&PRIVACY_UNSCANNED.to_owned()),
        "premise: the corpus carries work, so an empty queue is the tick's decision"
    );

    let now_ms = chrono::Utc::now().timestamp_millis();
    let stats = rescan_tick(&env.state, now_ms).await;

    assert!(
        stats.disabled,
        "a gated-off tick reports that it selected nothing because the work is off"
    );
    assert_eq!(stats.jobs_enqueued, 0);
    assert_eq!(stats.vaults_selected, 0);
    assert!(
        env.jobs(JobFilter::default()).await.is_empty(),
        "nothing was enqueued"
    );

    // …and it reports its run: the task is observable as alive, not as never started.
    let row = env.task_row(TASK_PRIVACY_RESCAN, now_ms).await;
    assert_eq!(
        row.last_outcome.as_deref(),
        Some("ok"),
        "a tick that selected nothing is a successful tick"
    );
    assert!(
        row.last_run_ms.is_some_and(|ts| ts >= now_ms - 60_000),
        "the run is recorded at the tick's own instant"
    );
    assert_eq!(row.run_count, 1);
}

/// The perimeter is every vault but the code namespaces, and the guard is re-applied after
/// selection — so a note carrying the reserved name **by transit** in a `code-*` vault or in
/// the `identity` section is neither re-judged, nor rewritten, nor detagged.
///
/// Immunity by ricochet — "the guard prevents the pass, so the tag is never posed there" — is a
/// *derived* property, not a guard. These notes exist, and the sweep must leave them alone on
/// its own evidence.
#[tokio::test]
async fn privacy_rescan_never_touches_identity_or_code_star() {
    let env = env_of(
        &[
            VaultSpec::new("main", Script::Failing),
            VaultSpec::new("code-x", Script::Failing),
        ],
        PrivacyConfig {
            rescan_enabled: true,
            categories: vec!["email".to_owned()],
            ..PrivacyConfig::default()
        },
    )
    .await;

    // A note in the code namespace carrying the reserved name by transit: the write path's
    // Guard 0 sees the prefix and runs no pass, so nothing retires it.
    let coded = seed(
        env.vault("code-x"),
        "code-x",
        Section::Decisions,
        &[PRIVACY_UNSCANNED],
        BODY,
    )
    .await;
    // A note in the identity section of a sweepable vault, seeded the same way: the guard
    // excludes the section, not the vault, so the selection below does offer it.
    let identity = seed(
        env.vault("main"),
        "main",
        Section::Identity,
        &[PRIVACY_UNSCANNED],
        BODY,
    )
    .await;
    // …and one ordinary note of the same vault, which the sweep does owe a re-judgement.
    let ordinary = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;
    assert!(
        tags_of(env.vault("main"), &ordinary)
            .await
            .contains(&PRIVACY_UNSCANNED.to_owned()),
        "premise: the ordinary note carries work"
    );

    let codes_before = sha256_of(env.vault("code-x"), "code-x", &coded).await;
    let identity_before = sha256_of(env.vault("main"), "main", &identity).await;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let stats = rescan_tick(&env.state, now_ms).await;

    assert_eq!(stats.vaults_considered, 2, "both vaults are considered");
    assert_eq!(
        stats.vaults_selected, 1,
        "the code namespace is filtered out of the selection, not re-judged inside it"
    );
    assert_eq!(
        stats.notes_seen, 2,
        "the guard excludes a section, not a vault: both tagged notes of `main` are seen"
    );
    assert_eq!(stats.jobs_enqueued, 1, "one job covers `main` alone");

    // Re-judging the code namespace directly is the strongest form of the claim: even handed
    // the vault by name, the primitive re-applies the guard.
    let summary = rescan_selected_notes(&env.state, "code-x", RESCAN_BATCH_LIMIT)
        .await
        .expect("the sweep re-judges a code namespace without error");
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.rewritten, 0);
    assert_eq!(summary.unchanged, 0);
    assert_eq!(
        tags_of(env.vault("code-x"), &coded).await,
        vec![PRIVACY_UNSCANNED.to_owned()],
        "the reserved name a code note carries by transit is not retired by the sweep"
    );
    assert_eq!(
        sha256_of(env.vault("code-x"), "code-x", &coded).await,
        codes_before,
        "and the note was not rewritten"
    );

    let summary = rescan_selected_notes(&env.state, "main", RESCAN_BATCH_LIMIT)
        .await
        .expect("the sweep re-judges `main`");
    assert_eq!(
        summary.skipped, 1,
        "the identity note is offered by the selection and refused by the guard"
    );
    assert_eq!(summary.unchanged, 1, "the ordinary note is re-judged");
    assert_eq!(
        sha256_of(env.vault("main"), "main", &identity).await,
        identity_before,
        "the identity note was not rewritten"
    );
    assert_eq!(
        tags_of(env.vault("main"), &identity).await,
        vec![PRIVACY_UNSCANNED.to_owned()],
        "nor was its reserved name touched"
    );
}

/// A re-judgement that decides exactly what the note already carried writes nothing: no
/// `updated`, no content hash, no index upsert.
///
/// This is clause 3, and it is the clause that lets the sweep run every few minutes over a
/// corpus that is almost always already judged. The amplification it closes is real:
/// `write_note_judged` stamps `updated` **before** hashing, so an unconditional rewrite would
/// move every unscanned note's hash on every tick, invalidating every `expected_sha256` a
/// client holds — a cascade of 409s, plus a disk write and an index upsert per pass.
#[tokio::test]
async fn rescan_of_an_unchanged_note_writes_nothing_and_keeps_its_sha256() {
    let env = env_of(
        &[VaultSpec::new("main", Script::Failing)],
        PrivacyConfig::default(),
    )
    .await;
    let id = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;

    let before = sha256_of(env.vault("main"), "main", &id).await;
    let updated_before = env
        .vault("main")
        .read_note_by_id(&id)
        .await
        .expect("read the note back")
        .frontmatter
        .updated;

    for tick in 1..=3 {
        let summary = rescan_selected_notes(&env.state, "main", RESCAN_BATCH_LIMIT)
            .await
            .expect("the sweep re-judges the batch");
        assert_eq!(
            summary.unchanged, 1,
            "tick {tick}: a failing sidecar re-judges to the same block"
        );
        assert_eq!(summary.rewritten, 0, "tick {tick}: nothing was written");
        assert_eq!(
            sha256_of(env.vault("main"), "main", &id).await,
            before,
            "tick {tick}: the content hash of an unchanged note must not move — a client \
             holding it must not be handed a 409 for a sweep that changed nothing"
        );
    }

    let updated_after = env
        .vault("main")
        .read_note_by_id(&id)
        .await
        .expect("read the note back")
        .frontmatter
        .updated;
    assert_eq!(
        updated_before, updated_after,
        "nor may `updated` move: it is stamped before the hash is computed, so a rewrite with \
         nothing to say costs the whole corpus"
    );
}

/// The sweep can only ever **add** `personal-classified` or **clear** `privacy-unscanned`. It
/// has no authority of its own: it re-runs the pass through the write primitive, so the two
/// legitimate removal paths of D-11 are the only ones reachable — and an incomplete pass
/// retires neither.
///
/// ## One vault per phase, and why
///
/// A sweep selects *every* unscanned note of the vault it is handed, so three notes in one
/// vault could not be driven to three different verdicts in one test without the earlier
/// phases moving the later ones' premises. Each phase therefore owns a vault and flips only
/// that vault's script, which is the switch the rescan reads.
///
/// ## The one state this test does not reach
///
/// A third shape would be a *rewriting* incomplete pass: a sweep that fails, writes, and must
/// still not retire the payload marker. It is not reachable in 2.4.0, and the reason is worth
/// recording — a block written by a failing pass carries the status, an empty span set and the
/// instant, all of which are a constant for a given detector, so re-judging a failed note
/// under a still-failing detector reproduces the block byte for byte and clause 3 decides
/// `Unchanged` before any write. The claim is therefore pinned on the two paths that do exist:
/// the write path (a note arriving with the marker by transit under a failing pass keeps it)
/// and the sweep path (`Unchanged`, same direction). Reaching the third shape needs a detector
/// that fails one way and then another, which is a fixture, not a guard — and a fixture cannot
/// close a hole a guard leaves open.
#[tokio::test]
async fn privacy_rescan_sweeps_by_tag_and_can_only_add_or_clear_never_declassify() {
    let env = env_of(
        &[
            VaultSpec::new("main", Script::Failing),
            VaultSpec::new("other", Script::Failing),
            VaultSpec::new("third", Script::Failing),
        ],
        PrivacyConfig::default(),
    )
    .await;

    // (a) A note left unscanned by a failing sidecar, which a clean pass must clear.
    let cleared = seed(env.vault("main"), "main", Section::Decisions, &[], BODY).await;
    // (b) A note the same sidecar leaves unscanned, which a marking pass must mark. Its
    // body carries the needle the marking script keys on — a script that finds nothing is
    // a completed clean pass, and a clean pass retires the marker instead of posing it.
    let marked = seed(
        env.vault("other"),
        "other",
        Section::Decisions,
        &[],
        MARKED_BODY,
    )
    .await;
    // (c) The payload marker arriving **by transit** on a note whose pass then fails: the
    // write path must keep it, and pose the rescan marker beside it.
    let kept = seed(
        env.vault("third"),
        "third",
        Section::Decisions,
        &[PERSONAL_CLASSIFIED],
        MARKED_BODY,
    )
    .await;
    let kept_tags = tags_of(env.vault("third"), &kept).await;
    assert!(
        kept_tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "premise: an incomplete pass has no authority to retire the marker it found — the \
         write path itself must not declassify: {kept_tags:?}"
    );
    assert!(
        kept_tags.contains(&PRIVACY_UNSCANNED.to_owned()),
        "and it owes the note a re-judgement: {kept_tags:?}"
    );

    // (a) The sidecar comes back and finds nothing markable. That is the first removal path
    // of D-11 — a *completed* pass reporting nothing — and it retires both reserved names.
    env.script("main", Script::Silent);
    let summary = rescan_selected_notes(&env.state, "main", RESCAN_BATCH_LIMIT)
        .await
        .expect("the sweep re-judges the batch");
    assert_eq!(summary.rewritten, 1, "(a) changed verdicts and was written");
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        tags_of(env.vault("main"), &cleared).await,
        Vec::<String>::new(),
        "a completed pass over a body with nothing markable retires both reserved names"
    );

    // (b) The sidecar recovers over `other`, and now finds the needle.
    env.script("other", Script::Marking);
    let summary = rescan_selected_notes(&env.state, "other", RESCAN_BATCH_LIMIT)
        .await
        .expect("the sweep re-judges the batch");
    assert_eq!(summary.rewritten, 1);

    // (c) `third` keeps its failing sidecar. The re-judgement reproduces the block it
    // already carries, so clause 3 writes nothing — and the payload marker is still there.
    let summary = rescan_selected_notes(&env.state, "third", RESCAN_BATCH_LIMIT)
        .await
        .expect("the sweep re-judges the batch");
    assert_eq!(
        summary.unchanged, 1,
        "a sweep that decides what the note already carried takes the clause-3 path"
    );

    // Every tag assertion last, so a failure reports the moved state rather than an
    // outcome count that would only say something moved.
    let tags = tags_of(env.vault("other"), &marked).await;
    assert!(
        tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "the sweep may ADD the payload marker: {tags:?}"
    );
    assert!(
        !tags.contains(&PRIVACY_UNSCANNED.to_owned()),
        "and it may CLEAR the rescan marker: {tags:?}"
    );

    let kept_tags = tags_of(env.vault("third"), &kept).await;
    assert!(
        kept_tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "a sweep running under a broken sidecar must never declassify a note: an incomplete \
         pass is not an authority on what is personal data: {kept_tags:?}"
    );
    assert!(
        kept_tags.contains(&PRIVACY_UNSCANNED.to_owned()),
        "and the note is still owed its re-judgement: {kept_tags:?}"
    );
}
