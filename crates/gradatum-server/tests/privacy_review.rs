//! F-160 — `vault_privacy_review`, the dedicated path that lifts a marking false positive.
//!
//! ## What the endpoint is for, and what it is not
//!
//! D-11 gives a reserved marker exactly **two** removal paths: a completed pass that
//! reports nothing above the activation thresholds, and a human `rejected` verdict. The
//! first is automatic and costs nothing; the second is the only way to undo a false
//! positive, and this file covers the surface that carries it.
//!
//! The verdict is **not** applied to the tags by this endpoint. It is written into
//! `extra.privacy.review` through the single insertion point, and the insertion point
//! is what decides the tags from what the note on disk carries (`binds_the_tag`, 6b):
//! a rejection retires `personal-classified` on the **following** write, never by the
//! review call itself. The tests below assert that order rather than assuming it —
//! a review that claimed to have retired the marker would be claiming an effect it
//! did not produce.
//!
//! ## Why the server's vault mounts no detector
//!
//! The delivered 2.4.0 configuration has an empty `categories` list, and an empty list
//! is what mounts the strict pass-through (`state.rs:2063`): no pass runs, so `extra`
//! transits and the verdict the caller wrote is the verdict on disk. That is the shape
//! these tests build the fixture in, and it is the shape the endpoint is delivered for.
//!
//! Where a detector **is** mounted, the write-time pass runs on the same write and is the
//! authority over the block (D-11). The verdict is then resolved from the block that write
//! states, so it is inscribed and binds the marker — as long as `spans_digest` still
//! covers the spans the pass re-reported. That digest is the whole of the guard: a pass
//! that reports **other** spans than the verdict was made on spends it, and the endpoint
//! must not report a verdict it did not record. The last tests below pin both directions,
//! and the refusal is why the implementation re-reads the note after the compare-and-swap
//! instead of trusting the write.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::note::Note;
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::config::{MultiTenantConfig, PrivacyConfig, PrivacyThresholds, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Literals ─────────────────────────────────────────────────────────────────

/// The reserved names, spelled literally: these are what a note carries and what the
/// assertions look for. Deliberately not derived from `ReservedTag`, so that a name
/// silently renamed in the type does not rename the string these tests observe.
const PERSONAL_CLASSIFIED: &str = "personal-classified";

/// The block key, spelled literally: `privacy_block` is `pub(crate)`, and a test that
/// reached it through a re-export would be testing the export.
const BLOCK_KEY: &str = "privacy";

/// The review key the endpoint exists to mutate, and the only one it may move.
const REVIEW_KEY: &str = "review";

/// The digest the block carries over what the pass saw. Never a value: offsets only.
const DIGEST_KEY: &str = "spans_digest";

/// The route under test.
const REVIEW_PATH: &str = "/api/v1/vault_privacy_review";

/// The needle the marking script keys on. The discriminator has to be the body:
/// `PiiDetector::detect` is handed a text, a surface and a budget — never a note's tags —
/// so a detector cannot know what the fixture seeded.
const MARK: &str = "marqueur-de-test";

/// A body carrying [`MARK`] as a substring, so the pass reports one span above zero on it.
const MARKED_BODY: &str = "contenu banal avec marqueur-de-test a l interieur";

// ── ACL preset ───────────────────────────────────────────────────────────────

/// One consumer carrying the read grant the review requires, one negated on the
/// synthetic locus the guard evaluates. The negated shape is the one the shipped preset
/// gives `main-agent`: the four spellings are needed because the predicate is evaluated
/// on `{vault}/personal-classified`, which only some of them cover.
const PRESET: &str = r#"
[[consumer]]
identity = "f160-review-denied"
read_patterns  = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "**"]
write_patterns = ["main/*"]

[[consumer]]
identity = "f160-review-admin"
read_patterns  = ["**"]
write_patterns = ["main/*"]
"#;

/// The identity the ACL refuses, and the one it admits.
const DENIED: &str = "f160-review-denied";
const ADMIN: &str = "f160-review-admin";

// ── Detector ─────────────────────────────────────────────────────────────────

/// Reports one span of an **activated** category, above its threshold, **on the needle**.
///
/// A span alone marks nothing: the category has to be activated by the configuration,
/// which is what [`marking_config`] does, before the pass can pose the marker. The
/// reserved tags are posed by the product's own insertion point, never by the test:
/// seeding a tag by hand would test a note no production path produces, since the tag
/// constructor refuses the spelling (D-12).
struct Marking;

impl PiiDetector for Marking {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(match input.text.find(MARK) {
            Some(start) => Detection::new(
                vec![PiiSpan::new(start, MARK.len(), PiiCategory::Person, 0.9)],
                0,
            ),
            None => Detection::new(Vec::new(), 0),
        })
    }
}

/// Reports one span **off** the needle, at fixed coordinates.
///
/// The same body, re-scanned by a detector that sees different spans: the digest a block
/// was recorded over moves, which is the only thing that spends a verdict. A verdict is
/// about the spans it was made on, so a pass that reports other spans is a pass whose
/// judgement does not apply — and the endpoint must refuse to claim it landed.
struct Elsewhere;

impl PiiDetector for Elsewhere {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(
            vec![PiiSpan::new(0, 4, PiiCategory::Person, 0.9)],
            0,
        ))
    }
}

// ── Fixture: two handles on one real vault ───────────────────────────────────

/// A capturing audit sink. One test asserts on the event the endpoint is required to
/// emit on every successful call; a sink that dropped it would make that test
/// unobservable, and the delivered default (`NoopAuditSink`) is exactly that.
#[derive(Default)]
struct Captured(Mutex<Vec<HttpAuditEvent>>);

#[async_trait::async_trait]
impl AuditSink for Captured {
    async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        self.0.lock().expect("audit mutex").push(event);
        Ok(())
    }
}

impl Captured {
    fn events(&self) -> Vec<HttpAuditEvent> {
        self.0.lock().expect("audit mutex").clone()
    }
}

struct Env {
    state: AppState,
    /// The same root, through a vault that mounts the marking detector: what seeds a
    /// marked note, and what replays a passing rewrite.
    frontier: Arc<Vault>,
    /// The handle the **server** writes through.
    vault: Arc<Vault>,
    audit: Arc<Captured>,
    _dir: TempDir,
}

/// Builds the fixture.
///
/// `server_mounts_the_pass` decides what the server's own vault mounts: `false` is the
/// delivered 2.4.0 shape (empty `categories` ⇒ the strict pass-through, no pass on
/// write), `true` is the shape a future activation produces.
async fn env(server_mounts_the_pass: bool) -> Env {
    let detector: Option<Arc<dyn PiiDetector>> = if server_mounts_the_pass {
        Some(Arc::new(Marking))
    } else {
        None
    };
    env_with_server_detector(detector).await
}

/// Builds the fixture with an explicit detector on the **server's** vault.
///
/// `None` is the delivered 2.4.0 shape (no pass runs on write, so the block transits and
/// the verdict the caller wrote is the verdict on disk); `Some(_)` is the shape a future
/// activation produces, where the write-time pass recomputes the block.
async fn env_with_server_detector(server_detector: Option<Arc<dyn PiiDetector>>) -> Env {
    let dir = TempDir::new().expect("TempDir");
    let root = dir.path();
    let frontier = Arc::new(marking_vault(root).await);
    let vault = Arc::new(match server_detector {
        Some(detector) => vault_mounting(root, detector).await,
        None => cli_vault(root).await,
    });

    let acl = AclEngine::from_preset_str(PRESET).expect("a valid ACL preset");
    let audit = Arc::new(Captured::default());
    let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>)
        .with_audit(Arc::clone(&audit) as Arc<dyn AuditSink>);
    // The state's search index must be the vault's own: a placeholder answers
    // `NoteNotFound` and turns a read into a 404.
    state.search = vault.index().clone();
    Env {
        state,
        frontier,
        vault,
        audit,
        _dir: dir,
    }
}

/// The activation the marking vault is mounted with: [`PiiCategory::Person`] on, at the
/// smallest positive calibration, so the 0.9 span [`Marking`] reports marks.
///
/// Built from a [`PrivacyConfig`] rather than assembled by hand, so the fixture runs the
/// delivered configuration→policy conversion.
fn marking_config() -> PrivacyConfig {
    PrivacyConfig {
        categories: vec!["person".to_owned()],
        thresholds: PrivacyThresholds {
            default_m: 1,
            ..PrivacyThresholds::default()
        },
        ..PrivacyConfig::default()
    }
}

/// A vault with the marking detector mounted — the only injection path.
async fn marking_vault(root: &std::path::Path) -> Vault {
    vault_mounting(root, Arc::new(Marking)).await
}

/// A vault at `root` with `detector` mounted.
async fn vault_mounting(root: &std::path::Path, detector: Arc<dyn PiiDetector>) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("Vault::create")
        .with_detector(detector, PrivacyBudgets::default())
        .with_privacy_policy(marking_config().policy())
}

/// A vault built the way the CLI builds one: no `with_detector` call, so the
/// pass-through the constructors write in hard is what is mounted.
async fn cli_vault(root: &std::path::Path) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("Vault::create")
}

/// The fixture whose caller reads `main`, with one note in each of two vaults.
///
/// The second vault is what gives the endpoint a note that exists but lies outside the
/// perimeter its caller can read: a note is only unique *within* a vault, and the reader
/// is resolved from the caller's tenant. The tenant guard is flipped on for this fixture
/// only — the flip itself is a delivered configuration, and the note's `.md` is written
/// through the vault's own handle, so nothing here is staged by hand.
async fn env_with_a_note_outside_the_readable_perimeter() -> (Env, NoteId) {
    let dir = TempDir::new().expect("TempDir");
    let root = dir.path();
    let main = Arc::new(marking_vault(root).await);
    let elsewhere = Arc::new(
        Vault::with_shared_index(
            root,
            VaultId::parse("vault-b").expect("a well-formed vault id"),
            Arc::clone(main.index()),
        )
        .await
        .expect("the second vault"),
    );

    // The note lives in `vault-b`, seeded through that vault's own handle.
    let id = NoteId::new();
    let mut frontmatter = fm_of(Section::Decisions);
    frontmatter.vault_id = VaultId::parse("vault-b").expect("a well-formed vault id");
    elsewhere
        .write_note_with_id(frontmatter, MARKED_BODY.to_owned(), id)
        .await
        .expect("seed a note in the other vault");

    let registry = VaultRegistry::new();
    registry
        .insert(VaultId::new("main"), Arc::clone(&main))
        .expect("register main");
    registry
        .insert(
            VaultId::parse("vault-b").expect("a well-formed vault id"),
            Arc::clone(&elsewhere),
        )
        .expect("register vault-b");

    let acl = AclEngine::from_preset_str(PRESET).expect("a valid ACL preset");
    let audit = Arc::new(Captured::default());
    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig { enabled: true },
        ..ServerConfig::default()
    };
    let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_vault_arc(Arc::clone(&main) as Arc<dyn Registry>)
        .with_audit(Arc::clone(&audit) as Arc<dyn AuditSink>)
        .with_server_config(cfg);
    state.search = main.index().clone();
    state.vaults = Arc::new(registry);

    let env = Env {
        state,
        frontier: Arc::clone(&main),
        vault: main,
        audit,
        _dir: dir,
    };
    // The fixture is only meaningful if the note really is there, in its own vault: a note
    // that was never written would make the two 404s below identical for the trivial
    // reason.
    elsewhere
        .read_note(id)
        .await
        .expect("the out-of-perimeter note must exist in its own vault");
    (env, id)
}

fn build_router(state: AppState) -> axum::Router {
    use axum::{Router, middleware};

    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

fn sign(state: &AppState, identity: &str, tenant: &str) -> String {
    state
        .jwt
        .sign(
            identity,
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign the test JWT")
}

// ── Seeding, and reading the note back ───────────────────────────────────────

/// A minimal frontmatter in `section`, `live`, carrying no tags.
///
/// No tag is seeded by hand: the reserved ones are posed by the pass, and the note has
/// no other use for one.
fn fm_of(section: Section) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
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
    }
}

/// Writes a note through the marking handle and returns its ULID and content hash.
///
/// The marker is asserted here rather than after: a fixture whose pass did not conclude
/// what the test needs would make every assertion below vacuous — a "the marker is still
/// posed" check on a note that never carried one is green for the wrong reason.
async fn seed_marked(env: &Env) -> (NoteId, String) {
    let id = NoteId::new();
    let note = env
        .frontier
        .write_note_with_id(fm_of(Section::Decisions), MARKED_BODY.to_owned(), id)
        .await
        .expect("seed a marked note");
    assert!(
        carries(&note, PERSONAL_CLASSIFIED),
        "the fixture must seed a marked note: the pass reports the needle, so the marker \
         is posed. Got tags {:?}",
        note.frontmatter
            .tags
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
    );
    (id, note.content_hash.hex())
}

/// The note as the **server's** vault reads it.
async fn note_of(env: &Env, id: NoteId) -> Note {
    env.vault
        .read_note(id)
        .await
        .expect("read the note back through the server's vault")
}

/// The `extra.privacy` block of a note, as a table.
fn block(note: &Note) -> &toml::value::Table {
    note.frontmatter
        .extra
        .get(BLOCK_KEY)
        .unwrap_or_else(|| panic!("the note must carry an `extra.{BLOCK_KEY}` block"))
        .as_table()
        .expect("the block is a table")
}

/// The `privacy.review` value the note carries.
fn review_of(note: &Note) -> String {
    block(note)[REVIEW_KEY]
        .as_str()
        .expect("`review` is a string")
        .to_owned()
}

/// The `privacy.spans_digest` value the note carries.
fn digest_of(note: &Note) -> String {
    block(note)[DIGEST_KEY]
        .as_str()
        .expect("`spans_digest` is a string")
        .to_owned()
}

/// Whether the note carries `tag`, by exact token.
fn carries(note: &Note, tag: &str) -> bool {
    note.frontmatter.tags.iter().any(|t| t.as_str() == tag)
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn review_body(note_id: &str, verdict: &str, expected_sha256: &str) -> serde_json::Value {
    serde_json::json!({
        "note_id": note_id,
        "verdict": verdict,
        "expected_sha256": expected_sha256,
    })
}

async fn post(
    app: axum::Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, raw) = raw_request(app, "POST", path, Some(token), Some(body)).await;
    let json = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn raw_request(
    app: axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).expect("json body")))
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
    };
    let resp = app.oneshot(request).await.expect("service");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The tool names the MCP endpoint actually serves, through `tools/list` on the real
/// service — never through a list written in the test.
async fn mcp_tool_names(state: AppState, token: &str) -> Vec<String> {
    use axum::{Router, middleware};

    let (mcp_service, _cancel) = gradatum_server::api_v1::mcp::build_mcp_service(state.clone());
    let app = Router::new()
        .route_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state);

    // `host` is not decoration: rmcp rejects a request whose `Host` is outside its
    // loopback whitelist before any handler runs, and an in-process call carries none.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }))
            .expect("json body"),
        ))
        .expect("build request");

    let resp = app.oneshot(request).await.expect("mcp service");
    assert_eq!(resp.status(), StatusCode::OK, "tools/list must answer 200");
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("read mcp body")
        .to_bytes();
    let json = parse_sse_json(&String::from_utf8_lossy(&bytes));
    assert!(
        json.get("error").is_none(),
        "tools/list must not answer an error — got {json}"
    );
    json["result"]["tools"]
        .as_array()
        .expect("result.tools must be an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// Extracts the first JSON object from an SSE body (`data: {...}`).
fn parse_sse_json(text: &str) -> serde_json::Value {
    for line in text.lines() {
        if let Some(json_str) = line.strip_prefix("data: ")
            && let Ok(val) = serde_json::from_str(json_str)
        {
            return val;
        }
    }
    serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
}

// ── The ACL ──────────────────────────────────────────────────────────────────

/// The whole authorization of the endpoint, and the reason it is not delegable.
///
/// A verdict on personal data is a human act, and the identity that produced the content
/// is refused it: `read=*,!personal-classified` is the shape the fleet's presets give
/// every agent. The refusal must leave the note untouched — a 403 that had already
/// written the verdict would be the worst of both.
#[tokio::test]
async fn review_requires_the_personal_classified_read_grant() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, DENIED, "main");

    let (status, _) = post(
        app,
        REVIEW_PATH,
        &denied,
        review_body(&id.to_string(), "rejected", &sha),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a consumer whose `evaluate(Read, \"main/personal-classified\")` is not `Allow` must \
         be refused by the very predicate the read guard evaluates"
    );

    let after = note_of(&env, id).await;
    assert_eq!(
        review_of(&after),
        "pending",
        "a refused review must not record a verdict"
    );
    assert!(
        carries(&after, PERSONAL_CLASSIFIED),
        "…and must not retire the marker either"
    );
    assert_eq!(
        after.content_hash.hex(),
        sha,
        "…and must not write at all: the refusal happens before any byte reaches the vault"
    );
}

// ── The audit event ──────────────────────────────────────────────────────────

/// The decision to lift a marking is auditable, and the audit carries no data.
///
/// `event` names the operation, `actor` comes from the `TrustContext`, and the verdict
/// and the two digests ride in `curator` — the only free-form slot the flat HTTP event
/// has (D-13, SEC-P1-5). The line must never carry the body, a span or a detected value:
/// the whole point of the block's offsets-only design is that the audit can be read
/// without reading personal data into it.
#[tokio::test]
async fn review_emits_an_audit_event_carrying_the_actor_and_no_span() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let before = digest_of(&note_of(&env, id).await);
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");

    let (status, _) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "accepted", &sha),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the admin's review must be applied");

    let events = env.audit.events();
    assert_eq!(
        events.len(),
        1,
        "exactly one event per successful call — got {}",
        events.len()
    );
    let event = &events[0];
    assert_eq!(event.event, "vault_privacy_review");
    assert_eq!(
        event.actor.sub, ADMIN,
        "the actor is the authenticated subject"
    );
    assert_eq!(event.tenant_id, "main");
    assert_eq!(event.note_id.as_deref(), Some(id.to_string().as_str()));
    assert_eq!(
        event.outcome, "reviewed",
        "the outcome marks the operation as applied — the verdict is carried by `curator`, \
         where `rejected` cannot be misread as a failed operation"
    );

    let curator = event
        .curator
        .as_ref()
        .expect("the verdict and the digests ride in `curator`");
    assert_eq!(curator["verdict"].as_str(), Some("accepted"));
    assert_eq!(
        curator["spans_digest_before"].as_str(),
        Some(before.as_str()),
        "the audit carries what the block said before the verdict"
    );
    assert_eq!(
        curator["spans_digest_after"].as_str(),
        Some(before.as_str()),
        "…and after: a review moves the verdict, never the spans"
    );
    assert_eq!(curator["expected_sha256"].as_str(), Some(sha.as_str()));

    // Nothing of the note reaches the line. The needle is the strongest available
    // sentinel: it is what the detector matched, and it exists nowhere else in the event.
    let line = serde_json::to_string(event).expect("the event serialises");
    assert!(
        !line.contains(MARK),
        "the matched text must never reach the audit line — got {line}"
    );
    for key in [
        "\"spans\"",
        "\"start\"",
        "\"len\"",
        "\"category\"",
        "\"score_m\"",
    ] {
        assert!(
            !line.contains(key),
            "`{key}` is a span field and has no place in the audit line — got {line}"
        );
    }
}

// ── The compare-and-swap ─────────────────────────────────────────────────────

/// An overwrite of a security verdict without a lock is refused before anything is read.
#[tokio::test]
async fn review_without_expected_sha256_is_rejected_400() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");

    let (status, _) = post(
        app,
        REVIEW_PATH,
        &admin,
        serde_json::json!({ "note_id": id.to_string(), "verdict": "accepted" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "`expected_sha256` is not optional: a review that cannot name the version it judged \
         must be refused, not applied to whatever the note happens to be now"
    );

    let after = note_of(&env, id).await;
    assert_eq!(review_of(&after), "pending");
    assert_eq!(after.content_hash.hex(), sha, "no write may have happened");
    assert!(
        env.audit.events().is_empty(),
        "the event is emitted on successful calls only"
    );
}

/// A stale hash is a conflict, and the note keeps the verdict it had.
#[tokio::test]
async fn review_with_stale_sha256_is_a_conflict_409() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    // A well-formed hash that is not this note's: the shape a client holds after someone
    // else rewrote the note between its read and its review.
    let stale = "0".repeat(64);
    assert_ne!(
        stale, sha,
        "the fixture must not accidentally hold the current hash"
    );

    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");
    let (status, _) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "rejected", &stale),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a version that moved is a conflict, never a silent overwrite"
    );

    let after = note_of(&env, id).await;
    assert_eq!(review_of(&after), "pending", "the note keeps its verdict");
    assert_eq!(after.content_hash.hex(), sha);
    assert!(env.audit.events().is_empty());
}

// ── What a verdict does, and what it does not ────────────────────────────────

/// The only field a review may move is `privacy.review`.
///
/// The block is the record of what the pass saw; a review that touched `spans_digest`
/// would move the very thing a verdict is about, and would spend a previous rejection on
/// the way (D-11). Byte-for-byte, the block minus `review` must be identical.
#[tokio::test]
async fn review_does_not_alter_spans_digest() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let before = note_of(&env, id).await;
    let digest_before = digest_of(&before);

    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");
    let (status, body) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "accepted", &sha),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["verdict"].as_str(),
        Some("accepted"),
        "the response echoes the verdict that was recorded — got {body}"
    );
    assert_eq!(
        body["spans_digest"].as_str(),
        Some(digest_before.as_str()),
        "…and the digest it was recorded over"
    );
    assert!(
        body.get(BLOCK_KEY).is_none() && body.get("spans").is_none(),
        "the response carries neither the block nor a span — got {body}"
    );

    let after = note_of(&env, id).await;
    assert_eq!(
        digest_of(&after),
        digest_before,
        "the digest covers the spans a verdict is made about; moving it would spend the \
         verdict with them"
    );

    let mut before_table = block(&before).clone();
    let mut after_table = block(&after).clone();
    assert_eq!(review_of(&before), "pending");
    assert_eq!(review_of(&after), "accepted");
    before_table.remove(REVIEW_KEY);
    after_table.remove(REVIEW_KEY);
    assert_eq!(
        before_table, after_table,
        "`review` is the only key a review may move: every other byte of the block is the \
         pass's record"
    );
}

/// A human acceptance is not undone by the rewrite that follows it.
///
/// A pass re-detects the same spans on the same body, and the insertion point carries the
/// verdict forward because the digest it was made about has not moved (D-11). Without
/// that, the acceptance would survive in the frontmatter while producing no effect — the
/// exact defect `A-R3-P1-1` names.
#[tokio::test]
async fn accepted_verdict_survives_a_rewrite_with_unchanged_spans() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");

    let (status, _) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "accepted", &sha),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let accepted = note_of(&env, id).await;
    assert_eq!(review_of(&accepted), "accepted");
    let digest_accepted = digest_of(&accepted);

    // The passing rewrite: the same body, through a handle that DOES mount the detector,
    // so a pass runs and re-reports the very same spans.
    let rewritten = env
        .frontier
        .write_note_with_id(fm_of(Section::Decisions), MARKED_BODY.to_owned(), id)
        .await
        .expect("replay the rewrite");

    assert_eq!(
        review_of(&rewritten),
        "accepted",
        "a pass over the spans the verdict was made about is not a reason to undo it"
    );
    assert_eq!(digest_of(&rewritten), digest_accepted);
    assert!(
        carries(&rewritten, PERSONAL_CLASSIFIED),
        "an accepted marking keeps the note marked — the acceptance is a judgement, not a \
         removal"
    );
}

/// A rejection is recorded, and it is the following write that retires the marker.
///
/// This is the whole point of the surface: no other API can durably declassify a note
/// holding personal data, and it takes a human identity to do it. The order matters and is
/// asserted rather than assumed — the endpoint writes the verdict, and the insertion point
/// reads it back from the note on the **next** write (`binds_the_tag`, 6b). A test that
/// expected the marker gone on the review call itself would be pinning an effect this
/// surface does not produce.
#[tokio::test]
async fn rejected_verdict_is_recorded_and_binds_the_following_write() {
    let env = env(false).await;
    let (id, sha) = seed_marked(&env).await;
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");

    let (status, body) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "rejected", &sha),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let rejected = note_of(&env, id).await;
    assert_eq!(
        review_of(&rejected),
        "rejected",
        "the rejection is what the note now records"
    );
    assert_eq!(body["verdict"].as_str(), Some("rejected"));
    assert!(
        carries(&rejected, PERSONAL_CLASSIFIED),
        "the review call itself does not retire the marker: the insertion point decides the \
         tags, and it reads the verdict off the note on the write that follows"
    );

    // The following write, through the marking handle: the pass re-detects the very same
    // spans, and the verdict binds it off.
    let rewritten = env
        .frontier
        .write_note_with_id(fm_of(Section::Decisions), MARKED_BODY.to_owned(), id)
        .await
        .expect("the write that follows the rejection");

    assert!(
        !carries(&rewritten, PERSONAL_CLASSIFIED),
        "a rejection binds the pass that re-detects the same spans — otherwise the marker \
         would be re-posed indefinitely and the human judgement would produce no effect"
    );
    assert_eq!(
        review_of(&rewritten),
        "rejected",
        "…and the verdict stands while the digest it was made about is unchanged"
    );
}

/// A verdict the write did not record is reported as a conflict, never as a success.
///
/// A verdict is about the spans it was made on, and `spans_digest` is the whole of that
/// comparison. Where a detector is mounted, the write-time pass is the authority over the
/// block: if it re-reports **other** spans than the ones the block records, the verdict is
/// spent and the block is recomputed `pending`. The endpoint re-reads the note after the
/// compare-and-swap and refuses to claim what it did not record — a 200 that recorded
/// nothing would be the worst possible answer on the one surface that decides whether a
/// note stays marked.
#[tokio::test]
async fn review_never_reports_a_verdict_the_note_does_not_carry() {
    // The server's vault mounts a detector that sees other spans than the block records,
    // so the pass that runs on the review write spends the verdict.
    let env = env_with_server_detector(Some(Arc::new(Elsewhere))).await;
    let (id, sha) = seed_marked(&env).await;
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");

    let (status, _) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "accepted", &sha),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the pass recomputed the block over spans the verdict was not made about, so the \
         verdict did not land and must not be reported as applied"
    );

    let after = note_of(&env, id).await;
    assert_eq!(
        review_of(&after),
        "pending",
        "the note records the verdict the pass resolved, not the one that was requested"
    );
    assert!(
        env.audit.events().is_empty(),
        "no event is emitted for a verdict that was not recorded"
    );
}

/// A verdict posed while a detector is mounted is inscribed by that write, and it retires
/// the marker it was made about.
///
/// This is the review surface doing its job under the shape a future activation produces:
/// the pass runs on the very write that poses the verdict, resolves it from the block the
/// write states, and — the digest being unchanged — carries it forward instead of
/// recomputing `pending` over it. The marker follows the verdict, since the insertion
/// point is what decides the tags.
#[tokio::test]
async fn rejected_verdict_posed_under_a_mounted_detector_binds_the_note() {
    let env = env(true).await;
    let (id, sha) = seed_marked(&env).await;
    let before = note_of(&env, id).await;
    let digest_before = digest_of(&before);

    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");
    let (status, body) = post(
        app,
        REVIEW_PATH,
        &admin,
        review_body(&id.to_string(), "rejected", &sha),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a rejection the pass re-reports the same spans for is a verdict the write records \
         — got {body}"
    );
    assert_eq!(body["verdict"].as_str(), Some("rejected"));
    assert_eq!(
        body["spans_digest"].as_str(),
        Some(digest_before.as_str()),
        "the verdict was recorded over the spans it was made about"
    );

    let after = note_of(&env, id).await;
    assert_eq!(
        review_of(&after),
        "rejected",
        "the pass resolved the verdict from the block this write states"
    );
    assert!(
        !carries(&after, PERSONAL_CLASSIFIED),
        "a rejection that still covers the re-reported spans retires the marker on the \
         write that poses it, since the insertion point is what decides the tags"
    );
    assert!(
        !env.audit.events().is_empty(),
        "a recorded verdict is an auditable act — the event must be emitted"
    );
}

/// The endpoint is not an existence oracle for the notes it cannot read.
///
/// A note outside the caller's readable perimeter and a ULID that was never issued answer
/// the same `404` with the same body: the surface is administrative, and its refusals must
/// not tell a caller apart which of the two it hit. The property is asserted on the bytes,
/// not on the status alone — a differing message would restore the oracle the status
/// withholds.
#[tokio::test]
async fn privacy_review_unknown_and_out_of_scope_are_indistinguishable() {
    let (env, elsewhere) = env_with_a_note_outside_the_readable_perimeter().await;
    let app = build_router(env.state.clone());
    let admin = sign(&env.state, ADMIN, "main");
    // The digest is well-formed and non-empty: the refusals below must come from the read,
    // never from a malformed body. Any stale value does, since nothing is reached.
    let sha = "0".repeat(64);

    let (unknown_status, unknown_body) = raw_request(
        app.clone(),
        "POST",
        REVIEW_PATH,
        Some(&admin),
        Some(review_body(&NoteId::new().to_string(), "rejected", &sha)),
    )
    .await;
    let (outside_status, outside_body) = raw_request(
        app.clone(),
        "POST",
        REVIEW_PATH,
        Some(&admin),
        Some(review_body(&elsewhere.to_string(), "rejected", &sha)),
    )
    .await;

    // The perimeter is not empty: the same caller, on the same router, reviews a note of
    // its own vault. Without this arm the two 404s below would be identical for the
    // trivial reason that nothing is reachable.
    let (inside, inside_sha) = seed_marked(&env).await;
    let (inside_status, inside_body) = raw_request(
        app,
        "POST",
        REVIEW_PATH,
        Some(&admin),
        Some(review_body(&inside.to_string(), "rejected", &inside_sha)),
    )
    .await;
    assert_eq!(
        inside_status,
        StatusCode::OK,
        "a note of the caller's own vault is reviewable — got {inside_body}"
    );

    assert_eq!(
        unknown_status,
        StatusCode::NOT_FOUND,
        "a ULID that was never issued is a 404"
    );
    assert_eq!(
        outside_status,
        StatusCode::NOT_FOUND,
        "the note exists, but in a vault this caller's reader does not resolve — a 404 too, \
         never a distinguishable refusal"
    );
    assert_eq!(
        unknown_body, outside_body,
        "the two refusals must be byte-identical: any difference is an existence oracle"
    );
}

// ── The deliberate gap with MCP ──────────────────────────────────────────────

/// The review is a REST surface only, and its absence from MCP is deliberate.
///
/// Lifting a marking is an act reserved to an administrative identity (D-13): exposing it
/// as an MCP tool would put it one tool call away from every agent on the fleet, which is
/// the delegation the design refuses. The check runs against the real `tools/list`, never
/// against a list written in this test.
#[tokio::test]
async fn review_is_absent_from_the_mcp_tool_catalog() {
    let env = env(false).await;
    let admin = sign(&env.state, ADMIN, "main");
    let names = mcp_tool_names(env.state.clone(), &admin).await;

    assert!(
        names.iter().any(|name| name == "vault_mutate_role"),
        "the catalog must be the real one — got {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "vault_privacy_review"),
        "`vault_privacy_review` is a REST-only surface by design — got {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.contains("privacy")),
        "no tool name may expose the review — got {names:?}"
    );
}
