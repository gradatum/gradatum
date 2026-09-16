//! F-160 L-D.1 — the `tag` filter of `vault_list`: the rescan surface, under the guard.
//!
//! ## What this file is for
//!
//! The privacy lot leaves two reserved names on notes: `personal-classified` (a completed
//! pass judged the note to hold personal data) and `privacy-unscanned` (a pass did not
//! complete). An operator re-running an incomplete sweep has to be able to **find** the
//! notes that carry the second one, which is what `vault_list --tag privacy-unscanned` is
//! for. That surface has two ways to be wrong, and both are asserted here:
//!
//! - **Too closed.** A filter that refused the reserved names as *query values* — because
//!   the tag constructors refuse them as *values to write* — would `400` the operator's
//!   rescan. L-D would be specified and unreachable.
//! - **Too open.** A filter that bypassed the D-14(b) read guard would let a consumer
//!   negated on `personal-classified` enumerate the very notes the guard withholds, one
//!   reserved name over: ask for `privacy-unscanned`, receive the note that carries both.
//!
//! The third way — the field perturbing the listing for a client that does not use it — is
//! closed by [`vault_list_without_tag_is_byte_identical_to_before`], which is the only test
//! here that an existing deployment can be wrong about.
//!
//! ## Why the corpus is built through the vault
//!
//! Every note is written through `Vault::write_note` with a **scripted detector** mounted,
//! because the reserved names are posed and retired by the product's own write-time pass,
//! never by the test. Seeding a tag is half the story: the same write re-runs the pass. A
//! detector that reports nothing is a completed clean scan and retires both names; one that
//! fails poses `privacy-unscanned` and keeps whatever verdict the note already carried. The
//! two fixtures below are those two scripts, and each test asserts the premise it depends
//! on by reading the note back, so a change in the pass turns a red test into a red test
//! that says why.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Literals ─────────────────────────────────────────────────────────────────

/// The reserved names, spelled literally: these are what a note carries and what the
/// queries below ask for. Deliberately not derived from `ReservedTag`, so that a name
/// silently renamed in the type does not rename the string these tests observe.
const PERSONAL_CLASSIFIED: &str = "personal-classified";
const PRIVACY_UNSCANNED: &str = "privacy-unscanned";

/// Body text shared by the seeded notes. It carries no reserved token and no needle.
const BODY: &str = "contenu banal sans marqueur de confidentialite";

/// The same body, carrying the needle the marking script keys on.
const MARKED_BODY: &str = "contenu banal sans marqueur-de-test de confidentialite";

/// The needle [`MARKED_BODY`] carries.
const MARK: &str = "marqueur-de-test";

// ── ACL preset ───────────────────────────────────────────────────────────────

/// Two consumers, one negated, one not — the shape the shipped preset gives
/// `main-agent` / `admin`.
const PRESET: &str = r#"
[[consumer]]
identity = "f160-tag-denied"
read_patterns  = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "**"]
write_patterns = ["main/*"]

[[consumer]]
identity = "f160-tag-allowed"
read_patterns  = ["**"]
write_patterns = ["main/*"]
"#;

// ── Detector ─────────────────────────────────────────────────────────────────

/// What the fixture needs the write-time pass to conclude.
#[derive(Clone, Copy)]
enum Script {
    /// Report one span above the marker threshold, **on the needle** — a body without it is
    /// a completed clean scan that retires both names. Where the needle is, the pass poses
    /// `personal-classified` and retires `privacy-unscanned`.
    Marking,
    /// Fail the pass. An incomplete pass has no authority over what is personal data: it
    /// **poses** `privacy-unscanned` on every note it touches and **keeps** whatever
    /// `personal-classified` the note already carried.
    Failing,
}

/// A detector scripted to [`Script`], so the fixture decides what each write yields rather
/// than inheriting the outcome of an empty scan.
struct Scripted(Script);

impl PiiDetector for Scripted {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        match self.0 {
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

// ── Fixture: a real vault ────────────────────────────────────────────────────

struct Env {
    state: AppState,
    vault: Arc<Vault>,
    _dir: TempDir,
}

/// The vault whose write-time passes complete: a note seeded with `personal-classified`
/// keeps it, a note seeded without it stays unmarked.
async fn env_with(preset: &str) -> Env {
    env_scripted(preset, Script::Marking).await
}

/// The vault whose write-time passes **fail**: every note written through it carries
/// `privacy-unscanned`, and one seeded with the verdict carries both reserved names — the
/// only shape in which the tag filter can be asked to hand back a guarded note.
async fn env_unscanned(preset: &str) -> Env {
    env_scripted(preset, Script::Failing).await
}

async fn env_scripted(preset: &str, script: Script) -> Env {
    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create")
            .with_detector(Arc::new(Scripted(script)), PrivacyBudgets::default()),
    );
    let acl = AclEngine::from_preset_str(preset).expect("a valid ACL preset");
    let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);
    // The state's search index must be the vault's own: a placeholder answers
    // `NoteNotFound` and turns a read into a 404.
    state.search = vault.index().clone();
    Env {
        state,
        vault,
        _dir: dir,
    }
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

// ── Seeding ──────────────────────────────────────────────────────────────────

/// The reserved name as it arrives through the **deserialisation** door.
///
/// That door is deliberately non-validating (task 7a): a note cannot reach a reserved tag
/// through the constructor, so any other construction would test a note no production path
/// can produce.
fn deserialised_reserved(literal: &str) -> Tag {
    serde_json::from_value(serde_json::Value::String(literal.to_owned()))
        .expect("a reserved tag deserialises — the door is deliberately non-validating")
}

/// A minimal frontmatter in `section`, `live`, carrying `tags`.
fn fm_of(section: Section, tags: &[&str]) -> Frontmatter {
    let mut fm = Frontmatter {
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
async fn seed(vault: &Vault, section: Section, tags: &[&str], body: &str) -> String {
    vault
        .write_note(fm_of(section, tags), body.to_owned())
        .await
        .expect("write_note")
        .id
        .to_string()
}

/// The tags the vault really left on a note, read back from disk.
///
/// The premise of every assertion below: the reserved names are posed by the product's own
/// pass, so a test that assumed them would be asserting on a note that may not exist.
async fn tags_of(vault: &Vault, id: &str) -> Vec<String> {
    let note = vault.read_note_by_id(id).await.expect("read the note back");
    note.frontmatter
        .tags
        .iter()
        .map(|t| t.as_str().to_owned())
        .collect()
}

// ── HTTP calls ───────────────────────────────────────────────────────────────

async fn post(
    app: axum::Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, raw) = raw_request(app, "POST", path, token, body).await;
    let json = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn raw_request(
    app: axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
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

/// `POST /api/v1/vault_list` with `tag` (when given) and a page large enough to hold the
/// whole corpus.
async fn list(
    app: axum::Router,
    token: &str,
    tag: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut body = serde_json::json!({ "limit": 50 });
    if let Some(tag) = tag {
        body["tag"] = serde_json::Value::String(tag.to_owned());
    }
    post(app, "/api/v1/vault_list", token, body).await
}

/// `POST /api/v1/vault_list` carrying both a `tag` and a `role_kind`.
async fn list_with_role(
    app: axum::Router,
    token: &str,
    tag: &str,
    role_kind: &str,
) -> (StatusCode, serde_json::Value) {
    post(
        app,
        "/api/v1/vault_list",
        token,
        serde_json::json!({ "limit": 50, "tag": tag, "role_kind": role_kind }),
    )
    .await
}

/// `POST /api/v1/vault_list` with an explicit `limit` and optional `tag`/`cursor` — the
/// pagination surface the guard's cursor is asserted on.
async fn list_paged(
    app: axum::Router,
    token: &str,
    tag: Option<&str>,
    limit: u64,
    cursor: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut body = serde_json::json!({ "limit": limit });
    if let Some(tag) = tag {
        body["tag"] = serde_json::Value::String(tag.to_owned());
    }
    if let Some(cursor) = cursor {
        body["cursor"] = serde_json::Value::String(cursor.to_owned());
    }
    post(app, "/api/v1/vault_list", token, body).await
}

/// The `next_cursor` of a listing response, `None` when the listing has ended.
fn next_cursor(json: &serde_json::Value) -> Option<String> {
    json["next_cursor"].as_str().map(str::to_owned)
}

/// The `path` of every returned entry, sorted.
fn entry_paths(json: &serde_json::Value) -> Vec<String> {
    let mut v: Vec<String> = json["entries"]
        .as_array()
        .expect("entries must be an array")
        .iter()
        .map(|e| e["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    v.sort();
    v
}

fn path_of(section: &str, id: &str) -> String {
    format!("{section}/{id}")
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

// ── The field is inert when it is not used ───────────────────────────────────

/// A client that does not know the filter must receive exactly what it received before.
///
/// "Before" is reconstructed rather than replayed — the pre-lot binary is not runnable from
/// a test — so the assertion is anchored on the branch the code actually takes: with no
/// `tag`, `vault_list` must mirror `list_notes` **entry for entry and in order**, `total`
/// included, and the two wire shapes of "no filter" (key absent, key `null`) must be one
/// request rather than two behaviours.
#[tokio::test]
async fn vault_list_without_tag_is_byte_identical_to_before() {
    let env = env_with(PRESET).await;
    let clean = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &["ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-tag-allowed", "main");

    // A pre-F-160 body: no `tag` key at all. Then the same body with an explicit null.
    let (absent_status, absent) = post(
        app.clone(),
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 50 }),
    )
    .await;
    assert_eq!(absent_status, StatusCode::OK, "{absent}");
    let (null_status, explicit_null) = post(
        app.clone(),
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 50, "tag": null }),
    )
    .await;
    assert_eq!(null_status, StatusCode::OK, "{explicit_null}");
    assert_eq!(
        absent.to_string(),
        explicit_null.to_string(),
        "an absent key and an explicit null must be the same request"
    );

    // Anti-vacuity: the corpus is not empty, so the equality below compares two real pages.
    let (records, total) = env
        .state
        .search
        .list_notes("main", None, 50, None)
        .await
        .expect("list_notes");
    let expected: Vec<String> = records
        .iter()
        .map(|r| format!("{}/{}", r.section, r.id))
        .collect();
    assert_eq!(
        expected,
        sorted(vec![
            path_of("reference", &clean),
            path_of("reference", &marked)
        ]),
        "the fixture's two notes are the unfiltered corpus"
    );

    assert_eq!(
        entry_paths(&absent),
        expected,
        "without a tag, the listing mirrors `list_notes` entry for entry and in order"
    );
    assert_eq!(
        absent["total"].as_u64(),
        Some(total),
        "`total` is untouched on the unfiltered path: {absent}"
    );
    assert_eq!(
        absent["next_cursor"],
        serde_json::Value::Null,
        "a short page ends the listing: {absent}"
    );
}

// ── The form is validated, the reservation is not a ban ──────────────────────

/// The filter validates the **form** of its input and answers `400` for anything else —
/// while accepting the two reserved names as legitimate queries.
///
/// The two halves pull in opposite directions on purpose. A malformed name answered with an
/// empty page would name nothing and read as an empty vault, which is the wrong answer to
/// the wrong question. The reserved names, refused by the tag constructors, are exactly what
/// the rescan surface exists to ask for: refusing them here would put L-D's own acceptance
/// criterion out of reach.
#[tokio::test]
async fn vault_list_rejects_a_tag_that_is_not_a_valid_tag_400() {
    let env = env_with(PRESET).await;
    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-tag-allowed", "main");

    let refused = [
        "%",                    // LIKE wildcard: the motive of the rule
        "_",                    // LIKE single-character wildcard
        "knowledge%base",       //
        "knowledge_base",       //
        "\\",                   // LIKE escape character
        "Knowledge-Base",       // uppercase
        "PERSONAL_CLASSIFIED",  // uppercase spelling of a reserved name
        "Personal Classified",  // a *coercion* onto a reserved name, not the name
        "personal classified",  //
        "personal--classified", // well-formed, but coerces onto a reserved name
        " leading",             // leading separator
        "-leading",             // hyphen first: not of the form
        "",                     // empty
    ];
    for bad in refused {
        let (status, json) = list(app.clone(), &token, Some(bad)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad:?} is outside the tag form and must be refused — got {status}: {json}"
        );
    }

    let accepted = [
        "ordinary-tag",
        "p1",
        // The two reserved names, and a tag that merely mentions one.
        PRIVACY_UNSCANNED,
        PERSONAL_CLASSIFIED,
        "personal-classified-2026",
    ];
    for good in accepted {
        let (status, json) = list(app.clone(), &token, Some(good)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{good:?} is a legitimate query value — got {status}: {json}"
        );
    }
}

/// A tag combined with a role filter is refused, not served with one of the two dropped.
///
/// The two filters live in two different store methods, and neither accepts the other's
/// parameter. Serving such a request by applying only one of them would answer a question
/// the caller did not ask — "the `FEATURE` cards carrying X" coming back as every note
/// carrying X — and nothing in the response would say which of the two had been honoured.
/// A `400` is the only answer that cannot be misread, so the combination is refused; the
/// controls below pin the refusal to the *combination* and not to either filter alone.
#[tokio::test]
async fn vault_list_rejects_tag_combined_with_a_role_filter_400() {
    let env = env_with(PRESET).await;
    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-tag-allowed", "main");

    for role_kind in ["FEATURE", "TASK"] {
        let (status, json) = list_with_role(app.clone(), &token, "ordinary-tag", role_kind).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "tag + role_kind={role_kind} must be refused rather than served with one filter \
             silently dropped — got {status}: {json}"
        );
    }

    let (status, json) = post(
        app.clone(),
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 50, "tag": "ordinary-tag", "role_status": "DONE" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "tag + role_status is the same request — got {status}: {json}"
    );

    // Controls: each filter on its own is a legitimate request, so the refusal above is
    // attributable to the combination alone.
    let (status, json) = post(
        app.clone(),
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 50, "role_kind": "FEATURE" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the role filter alone is served — got {status}: {json}"
    );

    let (status, json) = list(app, &token, Some("ordinary-tag")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the tag filter alone is served — got {status}: {json}"
    );
}

// ── L-D's acceptance criterion ───────────────────────────────────────────────
/// `vault_list --tag privacy-unscanned` finds the unscanned corpus — the rescan surface.
///
/// The marker is an operational signal, not a verdict, so it is not withheld from any
/// consumer: both the negated and the un-negated one must see the same corpus. A filter that
/// returned it to one and not the other would turn a detection outage into a silent,
/// read-side blackout for whoever carries the negation.
#[tokio::test]
async fn vault_list_tag_privacy_unscanned_finds_the_unscanned_corpus() {
    let env = env_unscanned(PRESET).await;
    let first = seed(
        &env.vault,
        Section::Reference,
        &[PRIVACY_UNSCANNED, "ordinary-tag"],
        BODY,
    )
    .await;
    let second = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let elsewhere = seed(&env.vault, Section::LessonsLearned, &["ordinary-tag"], BODY).await;

    // Premise, not assumption: the failing pass is what poses the marker, so the corpus is
    // checked to be the unscanned one before any listing is asserted on.
    for id in [&first, &second, &elsewhere] {
        let tags = tags_of(&env.vault, id).await;
        assert!(
            tags.iter().any(|t| t == PRIVACY_UNSCANNED),
            "the fixture's failing pass must leave {PRIVACY_UNSCANNED} on {id}: {tags:?}"
        );
    }

    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-tag-denied", "main");
    let allowed = sign(&env.state, "f160-tag-allowed", "main");

    let whole_corpus = sorted(vec![
        path_of("reference", &first),
        path_of("reference", &second),
        path_of("lessons-learned", &elsewhere),
    ]);

    for (who, token) in [("denied", &denied), ("allowed", &allowed)] {
        let (status, json) = list(app.clone(), token, Some(PRIVACY_UNSCANNED)).await;
        assert_eq!(status, StatusCode::OK, "[{who}] {json}");
        assert_eq!(
            entry_paths(&json),
            whole_corpus,
            "[{who}] the rescan must find the whole unscanned corpus — got {json}"
        );
        assert_eq!(
            json["total"].as_u64(),
            Some(whole_corpus.len() as u64),
            "[{who}] the page count is the page's own: {json}"
        );
    }

    // The section filter composes with the tag filter (the other SQL branch).
    let (status, json) = post(
        app.clone(),
        "/api/v1/vault_list",
        &allowed,
        serde_json::json!({ "tag": PRIVACY_UNSCANNED, "section": "reference", "limit": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        sorted(vec![
            path_of("reference", &first),
            path_of("reference", &second)
        ]),
        "the tag and section filters compose: {json}"
    );

    // Anti-vacuity: the same request with a tag no note carries answers with an empty page,
    // so the non-empty answers above are the filter matching, not the field being ignored.
    let (status, json) = list(app, &allowed, Some("no-such-tag")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(entry_paths(&json), Vec::<String>::new(), "{json}");
    assert_eq!(json["total"].as_u64(), Some(0), "{json}");
}

// ── The filter is not a bypass of the read guard ─────────────────────────────

/// A consumer negated on `personal-classified` must not reach a marked note **through the
/// tag filter**, whichever reserved name it asks by.
///
/// This is the test the whole surface turns on. A note carrying both reserved names — the
/// shape a failed pass leaves behind — is reachable by `privacy-unscanned`, which is not
/// itself withheld. If the filter short-circuited the D-14(b) guard, that query would hand
/// the negated consumer the note the guard exists to withhold, and the leak would be one
/// reserved name away from the guard's own test. Both directions are asserted, and the
/// control (the unmarked note stays) is what keeps the denial from being a blanket
/// blackout.
#[tokio::test]
async fn unprivileged_consumer_gets_no_reserved_result_through_the_tag_filter() {
    let env = env_unscanned(PRESET).await;
    let both = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, PRIVACY_UNSCANNED],
        MARKED_BODY,
    )
    .await;
    let marker_only = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;

    // Premise: the failed pass keeps the verdict and poses the marker, so `both` really
    // carries the two names the two queries below are about.
    let both_tags = tags_of(&env.vault, &both).await;
    for reserved in [PERSONAL_CLASSIFIED, PRIVACY_UNSCANNED] {
        assert!(
            both_tags.iter().any(|t| t == reserved),
            "the fixture must leave {reserved} on the note it seeded with both: {both_tags:?}"
        );
    }
    let marker_tags = tags_of(&env.vault, &marker_only).await;
    assert!(
        !marker_tags.iter().any(|t| t == PERSONAL_CLASSIFIED),
        "the control note must carry no verdict: {marker_tags:?}"
    );

    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-tag-denied", "main");
    let allowed = sign(&env.state, "f160-tag-allowed", "main");

    // Control: for a consumer who may read the verdict, both queries answer in full — so
    // the empty page below is a guard and not a filter that never matches.
    let (status, json) = list(app.clone(), &allowed, Some(PERSONAL_CLASSIFIED)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        vec![path_of("reference", &both)],
        "control: the verdict is readable to an allowed consumer — got {json}"
    );

    let (status, json) = list(app.clone(), &allowed, Some(PRIVACY_UNSCANNED)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        sorted(vec![
            path_of("reference", &both),
            path_of("reference", &marker_only)
        ]),
        "control: the marker query answers with the whole marked corpus — got {json}"
    );

    // The guard, on the direct query first: asking for the verdict is not a way to see it.
    let (status, json) = list(app.clone(), &denied, Some(PERSONAL_CLASSIFIED)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the refusal is a filtered result, never a `400` — a status difference would be an \
         oracle of its own: {json}"
    );
    assert_eq!(entry_paths(&json), Vec::<String>::new(), "{json}");
    assert_eq!(
        json["total"].as_u64(),
        Some(0),
        "`total` must not count the entry the response withholds: {json}"
    );

    // …then the one that matters: the marker query must not become the side door.
    let (status, json) = list(app, &denied, Some(PRIVACY_UNSCANNED)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        vec![path_of("reference", &marker_only)],
        "the tagged note carrying the verdict must leave the page — got {json}"
    );
    assert_eq!(
        json["total"].as_u64(),
        Some(1),
        "the page count is adjusted by what the guard withheld: {json}"
    );
}

// ── The cursor is not a side door onto withheld ULIDs ─────────────────────────

/// A negated consumer must never receive, as a pagination cursor, the ULID of a note the
/// guard withholds.
///
/// `next_cursor` is the keyset position (`id > ?`) and it is **not** opaque: whatever ULID it
/// carries, the consumer reads. If it advanced onto the last **raw** record, a consumer
/// negated on `personal-classified` would be handed the ULID of a withheld note, and paging
/// would turn that into an enumeration of every marked note — the D-14(b) oracle, sharpened
/// by the `tag` filter, since on `?tag=personal-classified` every matching row is withheld.
#[tokio::test]
async fn denied_consumer_cannot_harvest_masked_ids_through_next_cursor() {
    let env = env_unscanned(PRESET).await;
    // Four notes that all carry the verdict: the failing pass keeps `personal-classified` and
    // poses `privacy-unscanned`, so each is withheld from the negated consumer.
    let mut masked: Vec<String> = Vec::new();
    for _ in 0..4 {
        masked.push(
            seed(
                &env.vault,
                Section::Reference,
                &[PERSONAL_CLASSIFIED],
                MARKED_BODY,
            )
            .await,
        );
    }
    for id in &masked {
        let tags = tags_of(&env.vault, id).await;
        assert!(
            tags.iter().any(|t| t == PERSONAL_CLASSIFIED),
            "the fixture must leave {PERSONAL_CLASSIFIED} on {id}: {tags:?}"
        );
    }
    let masked_ids: std::collections::HashSet<String> = masked.iter().cloned().collect();

    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-tag-denied", "main");

    // Page one note at a time, following the cursor. The page is always empty (every match is
    // withheld) and the cursor must never land on a withheld ULID. The loop is bounded above
    // the corpus, so failing to terminate is itself the bug.
    let mut cursor: Option<String> = None;
    for step in 0..(masked.len() + 3) {
        let (status, json) = list_paged(
            app.clone(),
            &denied,
            Some(PERSONAL_CLASSIFIED),
            1,
            cursor.as_deref(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "[step {step}] {json}");
        assert_eq!(
            entry_paths(&json),
            Vec::<String>::new(),
            "[step {step}] every matching row is withheld, so the page is empty: {json}"
        );
        match next_cursor(&json) {
            None => return, // terminated without ever handing back a withheld id
            Some(c) => {
                assert!(
                    !masked_ids.contains(&c),
                    "[step {step}] next_cursor handed back the ULID of a withheld note: {c}"
                );
                cursor = Some(c);
            }
        }
    }
    panic!("pagination did not terminate — the cursor never reached the end of the corpus");
}

/// The guard must not turn a page emptied of withheld rows into the end of the listing.
///
/// Visible notes behind a fully-withheld page have to be reached, the pagination has to
/// terminate, and the cursor must never be a withheld ULID along the way. This is the case a
/// cursor on the last raw id would either leak (return the withheld ULID) or, if simply
/// nulled, silently drop the visible notes sitting behind the withheld page.
#[tokio::test]
async fn pagination_still_terminates_and_covers_visible_notes_with_masked_rows() {
    let env = env_unscanned(PRESET).await;
    // Seed order is ULID order. Two withheld notes lead, so a `limit = 2` first page is fully
    // withheld before any visible note appears.
    let m1 = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED],
        MARKED_BODY,
    )
    .await;
    let m2 = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED],
        MARKED_BODY,
    )
    .await;
    let v1 = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let m3 = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED],
        MARKED_BODY,
    )
    .await;
    let v2 = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let v3 = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;

    let masked_ids: std::collections::HashSet<String> =
        [&m1, &m2, &m3].into_iter().cloned().collect();
    let visible = sorted(vec![
        path_of("reference", &v1),
        path_of("reference", &v2),
        path_of("reference", &v3),
    ]);

    // Premise: the withheld notes carry the verdict, the visible ones do not.
    for id in [&m1, &m2, &m3] {
        assert!(
            tags_of(&env.vault, id)
                .await
                .iter()
                .any(|t| t == PERSONAL_CLASSIFIED),
            "the fixture must leave the verdict on the withheld note {id}"
        );
    }
    for id in [&v1, &v2, &v3] {
        assert!(
            !tags_of(&env.vault, id)
                .await
                .iter()
                .any(|t| t == PERSONAL_CLASSIFIED),
            "the visible note {id} must carry no verdict"
        );
    }

    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-tag-denied", "main");

    // Page through the whole (untagged) listing at limit 2, following the cursor.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut terminated = false;
    for step in 0..8 {
        let (status, json) = list_paged(app.clone(), &denied, None, 2, cursor.as_deref()).await;
        assert_eq!(status, StatusCode::OK, "[step {step}] {json}");
        for path in entry_paths(&json) {
            let id = path.rsplit('/').next().unwrap_or_default().to_owned();
            assert!(
                !masked_ids.contains(&id),
                "[step {step}] a withheld note reached the page: {path}"
            );
            seen.push(path);
        }
        match next_cursor(&json) {
            None => {
                terminated = true;
                break;
            }
            Some(c) => {
                assert!(
                    !masked_ids.contains(&c),
                    "[step {step}] next_cursor is a withheld ULID: {c}"
                );
                cursor = Some(c);
            }
        }
    }
    assert!(terminated, "pagination did not terminate within the bound");
    assert_eq!(
        sorted(seen),
        visible,
        "every visible note must be covered exactly once, none dropped behind a withheld page"
    );
}

// ── A sparse tag paginates in full pages, never a truncated one ────────────────

/// A tag carried by a small fraction of a large corpus is returned in full, across as many
/// pages as it takes — not truncated to whatever matched inside a bounded raw window.
///
/// This is the P1 the fix closes. The match used to be applied in Rust, after a bounded
/// over-fetch of raw rows: a window saturated by untagged notes returned a page shorter than
/// `limit`, which the caller reads as end-of-store and stops — dropping every tagged note
/// sitting behind the window. With the match moved into the SQL predicate before the `LIMIT`,
/// each page is full while tagged rows remain, and the keyset cursor advances exactly.
///
/// The corpus is seeded with a 2 ms gap between writes so that ULID order equals insertion
/// order: the seven tagged notes then sit at known, dispersed ranks, and the first page's
/// raw window (the old code's over-fetch) holds only one of them — far fewer than `limit`.
#[tokio::test]
async fn tag_sparse_pages_are_not_truncated() {
    const TOTAL: usize = 100;
    const SPARSE_TAG: &str = "sparse-tag";
    let tagged_ranks = [14usize, 28, 42, 56, 70, 84, 98];

    let env = env_with(PRESET).await;
    let mut tagged_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 0..TOTAL {
        // Strictly increasing ULID timestamps → ULID order is insertion order, so the tagged
        // ranks above are the ranks the store paginates by.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let tags: &[&str] = if tagged_ranks.contains(&i) {
            &[SPARSE_TAG, "ordinary-tag"]
        } else {
            &["ordinary-tag"]
        };
        let id = seed(&env.vault, Section::Reference, tags, BODY).await;
        if tagged_ranks.contains(&i) {
            tagged_ids.insert(id);
        }
    }
    assert_eq!(
        tagged_ids.len(),
        tagged_ranks.len(),
        "the fixture placed a distinct tagged note at every rank"
    );

    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-tag-allowed", "main");

    let mut collected: Vec<String> = Vec::new();
    let mut page_sizes: Vec<usize> = Vec::new();
    let mut cursor: Option<String> = None;
    for step in 0..10 {
        let (status, json) =
            list_paged(app.clone(), &token, Some(SPARSE_TAG), 5, cursor.as_deref()).await;
        assert_eq!(status, StatusCode::OK, "[step {step}] {json}");
        let paths = entry_paths(&json);
        page_sizes.push(paths.len());
        for p in paths {
            collected.push(p.rsplit('/').next().unwrap_or_default().to_owned());
        }
        match next_cursor(&json) {
            None => break,
            Some(c) => cursor = Some(c),
        }
    }

    assert_eq!(
        page_sizes,
        vec![5, 2],
        "seven sparse matches paginate as one full page then a short one — not a single \
         truncated page"
    );
    assert_eq!(
        collected.len(),
        tagged_ranks.len(),
        "no tagged note is returned twice across the pages"
    );
    let collected_set: std::collections::HashSet<String> = collected.into_iter().collect();
    assert_eq!(
        collected_set, tagged_ids,
        "every dispersed tagged note is reached, none truncated behind the window"
    );
}

/// The tag filter matches a whole token, never a note whose tag merely contains the query as
/// a substring — in either direction.
///
/// The match is `'% <tag> %'` against the space-wrapped tags column, not `'%<tag>%'`: a
/// query for `meeting` must not return the note tagged `meeting-notes`, and the reverse must
/// hold too. A substring predicate would make the two interchangeable and, on the reserved
/// names, would let a `personal-classified` query drag in `personal-classified-2026`.
#[tokio::test]
async fn tag_match_is_exact_token_not_substring() {
    let env = env_with(PRESET).await;
    let exact = seed(&env.vault, Section::Reference, &["meeting"], BODY).await;
    let superstring = seed(&env.vault, Section::Reference, &["meeting-notes"], BODY).await;
    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-tag-allowed", "main");

    let (status, json) = list(app.clone(), &token, Some("meeting")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        vec![path_of("reference", &exact)],
        "the query matches the whole token, not a note whose tag only starts with it: {json}"
    );

    let (status, json) = list(app, &token, Some("meeting-notes")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        entry_paths(&json),
        vec![path_of("reference", &superstring)],
        "the superstring tag is reachable only by its own whole name: {json}"
    );
}
