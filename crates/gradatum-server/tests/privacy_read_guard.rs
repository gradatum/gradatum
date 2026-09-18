//! F-160 — the D-14 read guards: a marked note leaves the *query*, never the index.
//!
//! ## The three parts of D-14, and which one is a control
//!
//! - **(a) ergonomics, NOT a control.** The reserved names are removed from the `MATCH`
//!   expression built out of a caller's query, so a caller is not handed back results the
//!   server then takes away from them. It is not a guard: `notes_fts` is declared
//!   `tokenize='unicode61'` **without `tokenchars`** (`0001_phase1.sql:53`), and unicode61
//!   treats every ASCII non-alphanumeric character — the hyphen included — as a
//!   separator. `personal-classified` is therefore not indexed as one token but as two
//!   **adjacent** tokens, and the phrase `"personal classified"`, or the bare token
//!   `classified`, contains no reserved token at all and still matches the `tags` column.
//!   `unicode61_indexes_the_reserved_name_as_two_adjacent_tokens` measures that premise
//!   before anything else: if the phrase did not match, every "zero results" assertion in
//!   this file would be vacuously true.
//! - **(b) THE ONLY GUARD.** Any result carrying `personal-classified` is filtered for a
//!   consumer whose `evaluate(Read, "{vault}/personal-classified")` is not `Allow`, where
//!   `vault` is the **result's** vault — never the requester's tenant. One evaluation per
//!   distinct vault id, so a multi-vault result set carries results from vaults with
//!   different verdicts.
//! - **(c) rendering.** `metadata.tags` masks `personal-classified` **alone**.
//!   `privacy-unscanned` is an operational marker: a scan that did not complete must stay
//!   visible, otherwise an outage of the detection sidecar would silently empty every
//!   agent's search for the duration of the incident.
//!
//! ## What is deliberately NOT here
//!
//! `vault_search` **matches** the `tags` column, it does not render it: `SearchHit`
//! (`api_v1/dto.rs:91`) carries `vault_id, path, score, title, snippet, trust, status,
//! scores, anchor_ms` and **no `tags`**, and it gains none. The snippet assertion below is
//! a **sentinel**, not the closure of an open leak: the three `snippet(notes_fts, 0, …)`
//! call sites of the product all read column 0 (`body_text`). It keeps the day that `0`
//! becomes `-1`.
//!
//! The FTS5 projection is **untouched**: no `upsert_note` change, no migration. Marking a
//! note writes the same values to `notes` and `notes_fts`, so `fts_integrity_check` stays
//! green and a `rebuild` restores nothing that was removed — there is nothing to remove.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scope::{AgentId, TenantId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_core::trust::TrustContext;
use gradatum_server::config::{PrivacyConfig, PrivacyThresholds};
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Literals ─────────────────────────────────────────────────────────────────

/// The reserved names, spelled literally: these are what a note carries and what the
/// assertions look for. Deliberately not derived from `ReservedTag`, so that a name
/// silently renamed in the type does not rename the string these tests observe.
const PERSONAL_CLASSIFIED: &str = "personal-classified";
const PRIVACY_UNSCANNED: &str = "privacy-unscanned";

/// Keys that exist **only** inside the `extra.privacy` block. A read surface that rendered
/// the block — whole, or one key of it — would have to emit one of these names.
///
/// Preferred over a sentinel value: the block is written by the product's own detection
/// pass (the fixture mounts a detector), so a hand-planted marker would be a second,
/// invented block shape rather than the one the product writes.
const BLOCK_ONLY_KEYS: [&str; 8] = [
    "spans_digest",
    "sidecar_v",
    "mapping_v",
    "model_digest",
    "model_source",
    "score_mode",
    "scanned_at",
    "review",
];

/// Body text shared by the seeded notes. It carries **no** reserved token, so every match
/// below comes from the `tags` column — which is the point of (a).
const BODY: &str = "contenu banal sans marqueur de confidentialite";

/// The same body, carrying the needle the marking script keys on.
///
/// The discriminator has to be the body: `PiiDetector::detect` is handed a text, a surface
/// and a budget — never the note's tags — so a detector cannot know what the fixture seeded.
/// A script that marked unconditionally would mark the *control* note too, and every
/// "the unmarked note stays" assertion would then be green for the wrong reason.
const MARKED_BODY: &str = "contenu banal sans marqueur-de-test de confidentialite";

/// The needle [`MARKED_BODY`] carries. A note whose body lacks it is written by a completed
/// clean scan and stays unmarked.
const MARK: &str = "marqueur-de-test";

// ── ACL presets ──────────────────────────────────────────────────────────────

/// Two consumers, one negated, one not — the shape the shipped preset gives
/// `main-agent` / `admin`.
const PRESET: &str = r#"
[[consumer]]
identity = "f160-guard-denied"
read_patterns  = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "**"]
write_patterns = ["main/*"]

[[consumer]]
identity = "f160-guard-allowed"
read_patterns  = ["**"]
write_patterns = ["main/*"]
"#;

/// The shipped preset, read from disk: two of the tests below run against the **post-lot-F
/// preset generation** rather than a local copy, which is what makes them evidence about a
/// real deployment instead of about this file.
const SHIPPED_PRESET: &str = include_str!("../../gradatum-admin/presets/hierarchical.toml");

/// One negation scoped to the vault `other` alone, everything else open.
///
/// Both spellings are needed: globset matches a pattern against the whole locus, so
/// `other/**/personal-classified` covers nested loci while `other/personal-classified`
/// covers the synthetic locus itself.
const PRESET_ONE_VAULT_ONLY: &str = r#"
[[consumer]]
identity = "f160-guard-one-vault"
read_patterns  = ["**", "!other/personal-classified", "!other/**/personal-classified"]
write_patterns = ["main/*"]
"#;

// ── Detector ─────────────────────────────────────────────────────────────────

/// What the fixture needs the write-time pass to conclude.
///
/// The reserved tags are posed and retired by the product's own insertion point, never by
/// the test: seeding a tag is only half the story, because the same write that carries it
/// re-runs the pass. A detector that reports nothing is a **completed clean scan**, and a
/// completed clean scan is the first of the two legitimate removal paths — it retires both
/// names. A fixture that wants a marked note therefore has to make the pass *conclude*
/// something, which is what these two scripts do — and the category it concludes about has
/// to be activated, which is what [`marking_config`] does.
#[derive(Clone, Copy)]
enum Script {
    /// Report one span of an **activated** category, above its threshold, **on the needle**
    /// — a body without it is a completed clean scan that retires both names. Where the
    /// needle is, the pass poses `personal-classified` and retires `privacy-unscanned`.
    Marking,
    /// Fail the pass. An incomplete pass has no authority over what is personal data: it
    /// **poses** `privacy-unscanned` and **keeps** whatever `personal-classified` the note
    /// already carried — so a note seeded with both names leaves one write carrying both.
    Failing,
}

/// A detector scripted to [`Script`], so the fixture decides what each write yields rather
/// than inheriting the outcome of an empty scan.
struct Scripted(Script);

impl PiiDetector for Scripted {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        match self.0 {
            Script::Marking => Ok(match input.text.find(MARK) {
                // The span sits on the needle, at a score strictly above zero — the shape
                // the marker threshold is stated over.
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

/// The activation these vaults are mounted with.
///
/// A span alone marks nothing: a category has to be **activated** by the configuration,
/// with a strictly positive threshold, before a span of it can pose
/// `personal-classified`. The scripted detector reports [`PiiCategory::Person`] at 0.9, so
/// the fixture activates that single category at the smallest positive calibration — the
/// delivered reading of the "one span above the marker threshold" these tests are built on.
///
/// Built from a [`PrivacyConfig`] rather than assembled by hand, so the fixture runs the
/// delivered configuration→policy conversion: a drift there would show up here.
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

// ── Fixture: a real vault ────────────────────────────────────────────────────

struct Env {
    state: AppState,
    vault: Arc<Vault>,
    _dir: TempDir,
}

/// A single real vault, its SQLite index wired into the state, and a scripted detector so
/// that every note it writes carries the block the non-exposure sweep looks for.
///
/// The write-time pass concludes [`Script::Marking`]: a note seeded with
/// `personal-classified` keeps it, and a note seeded without it stays unmarked.
async fn env_with(preset: &str) -> Env {
    env_scripted(preset, Script::Marking).await
}

/// The same vault, with a detector that **fails** every pass. A note written through it
/// carries `privacy-unscanned`, and keeps any `personal-classified` it was seeded with —
/// the only way a fixture gets one note carrying both reserved names.
async fn env_unscanned(preset: &str) -> Env {
    env_scripted(preset, Script::Failing).await
}

async fn env_scripted(preset: &str, script: Script) -> Env {
    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create")
            .with_detector(Arc::new(Scripted(script)), PrivacyBudgets::default())
            .with_privacy_policy(marking_config().policy()),
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

/// The typed trust of the same identity, for the handlers called directly.
fn trust(identity: &str, tenant: &str) -> TrustContext {
    TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new(identity),
        scopes: vec!["read".to_owned(), "write".to_owned()],
        tenant_id: TenantId::new(tenant),
        jti: None,
    }
}

// ── Seeding ──────────────────────────────────────────────────────────────────

/// The reserved name as it arrives through the **deserialisation** door.
///
/// That door is deliberately non-validating (`D-12`, task 7a): it is what carries the
/// monotone marker from disk back into memory, and a validating `Deserialize` would retire
/// the marker on every re-read. A note can hold a reserved tag in no other way, since the
/// constructor refuses it — so any other construction would test a note no production path
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

/// Writes a note, then rewrites its body, so the vault holds a history snapshot the
/// history and diff surfaces have something to show. Returns the ULID.
async fn seed_with_history(vault: &Vault, section: Section, tags: &[&str]) -> String {
    let id = NoteId::new();
    let first = vault
        .write_note_with_id(fm_of(section, tags), format!("{BODY} v1"), id)
        .await
        .expect("write_note_with_id");
    let rewritten = vault
        .write_if_match(
            fm_of(section, tags),
            format!("{BODY} v2"),
            id,
            Some(first.content_hash.0),
        )
        .await
        .expect("write_if_match");
    assert!(
        matches!(rewritten, gradatum_vault::WriteResult::Written { .. }),
        "the rewrite must take effect, otherwise the history is one version deep and the \
         history/diff surfaces of the sweep are vacuous"
    );
    id.to_string()
}

// ── HTTP calls ───────────────────────────────────────────────────────────────

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

async fn get(app: axum::Router, path: &str, token: &str) -> (StatusCode, String) {
    raw_request(app, "GET", path, Some(token), None).await
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

async fn search(
    app: axum::Router,
    token: &str,
    query: &str,
    extra: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut body = serde_json::json!({ "query": query, "tenant_id": "main" });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            body[k] = v.clone();
        }
    }
    post(app, "/api/v1/vault_search", token, body).await
}

/// The `path` of every returned hit, sorted.
fn paths(json: &serde_json::Value) -> Vec<String> {
    let mut v: Vec<String> = json["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .map(|it| it["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    v.sort();
    v
}

fn read_path(id: &str) -> String {
    format!("reference/{id}")
}

// ── Step 0 — the unicode61 premise, measured before anything else ────────────

/// Step 0 of the task: rule `unicode61` empirically, through the product's own vault.
///
/// The note is written through `Vault::write_note`, which is what maintains `notes_fts`;
/// the queries below are FTS5 queries against that index. If `"personal classified"` did
/// not match, the design's (a) would be mis-assigned — presented as the guard while
/// carrying none of the proof — and every "zero results" assertion in this file would pass
/// without measuring anything. This test is therefore the non-vacuity proof of the whole
/// file, not a curiosity.
#[tokio::test]
async fn unicode61_indexes_the_reserved_name_as_two_adjacent_tokens() {
    let env = env_with(PRESET).await;
    seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;

    let db = gradatum_core::paths::vault_dir_index_path(env.vault.root());
    let conn = rusqlite::Connection::open(&db).expect("open the vault index");

    let count = |q: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH ?1",
            rusqlite::params![q],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("MATCH {q}: {e}"))
    };

    // The tokenizer splits on `-`: the name is two adjacent tokens, not one.
    let phrase = count("\"personal classified\"");
    let bare = count("classified");
    let personal = count("personal");
    // The whole name, quoted as a single FTS5 phrase, is the *same* adjacency. Bare, it is
    // a parse error: FTS5 reads the hyphenated bareword as `col:term` and the RHS
    // `classified` is no such column — which is itself part of the measurement.
    let quoted_name = count("\"personal-classified\"");
    // A look-alike spelling must not match — exact token matching, never a prefix.
    let neighbour = count("\"personal classified 2026\"");

    eprintln!(
        "unicode61 probe: \"personal classified\"={phrase} classified={bare} \
         personal={personal} \"personal-classified\"={quoted_name} \
         \"personal classified 2026\"={neighbour}"
    );

    assert_eq!(phrase, 1, "the phrase must match the note's tags column");
    assert_eq!(bare, 1, "the bare token must match the note's tags column");
    assert_eq!(personal, 1);
    assert_eq!(quoted_name, 1);
    assert_eq!(neighbour, 0, "no prefix or substring matching");
}

// ── (b) the single guard, on the search surface ──────────────────────────────

/// The phrase query is the one (a) cannot strip: it carries no reserved token, so it
/// reaches the index and matches. Only (b) can remove the note.
#[tokio::test]
async fn phrase_query_returns_zero_marked_notes_for_unprivileged_consumer() {
    let env = env_with(PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    // Non-vacuity, established first: the very same query DOES return the note to a
    // consumer whose verdict is `Allow`. Without this control the zero below would be
    // indistinguishable from "the query never matched anything".
    let (status, json) = search(
        app.clone(),
        &allowed,
        "personal classified",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        paths(&json),
        vec![read_path(&id)],
        "control: the phrase must reach the marked note for an allowed consumer"
    );

    let (status, json) = search(app, &denied, "personal classified", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        paths(&json),
        Vec::<String>::new(),
        "the marked note must be filtered — got {json}"
    );
}

/// The bare token `classified` is the test that proves something.
///
/// Querying the reserved name itself proves nothing: (a) removes it from the `MATCH`
/// expression, so the empty result would come from the ergonomics layer, not from the
/// guard. `classified` is not a reserved token, survives (a), matches the `tags` column,
/// and is removed by (b) alone.
#[tokio::test]
async fn bare_classified_token_returns_zero_marked_notes() {
    let env = env_with(PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    let (_, json) = search(app.clone(), &allowed, "classified", serde_json::json!({})).await;
    assert_eq!(
        paths(&json),
        vec![read_path(&id)],
        "control: the bare token reaches the note for an allowed consumer"
    );

    let (_, json) = search(app, &denied, "classified", serde_json::json!({})).await;
    assert_eq!(
        paths(&json),
        Vec::<String>::new(),
        "the bare token must not enumerate marked notes"
    );
}

/// `privacy-unscanned` is an operational marker, not a verdict: it stays searchable for
/// everyone. Filtering it would turn a detection outage into a silent, fleet-wide read-side
/// blackout of every note written during the incident.
#[tokio::test]
async fn privacy_unscanned_notes_remain_searchable_for_everyone() {
    let env = env_unscanned(PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PRIVACY_UNSCANNED, "ordinary-tag"],
        BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    for (who, token) in [("denied", &denied), ("allowed", &allowed)] {
        let (status, json) = search(
            app.clone(),
            token,
            "privacy unscanned",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            paths(&json),
            vec![read_path(&id)],
            "the unscanned marker must stay searchable for the {who} consumer"
        );
    }
}

/// A `rebuild` repopulates the index from `notes.tags` — it can only restore what the
/// projection still contains. Since the projection was never filtered, the rebuild changes
/// nothing: the guard is at the query, and the tokens are still there.
#[tokio::test]
async fn fts_rebuild_then_phrase_query_still_returns_zero() {
    let env = env_with(PRESET).await;
    seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    let probe = "personal classified";
    let before = search(app.clone(), &allowed, probe, serde_json::json!({})).await;
    assert_eq!(
        paths(&before.1).len(),
        1,
        "the note is reachable before the rebuild"
    );

    // Precedent: `0032_notes_composite_pk.sql:210` runs exactly this statement.
    let db = gradatum_core::paths::vault_dir_index_path(env.vault.root());
    let conn = rusqlite::Connection::open(&db).expect("open the vault index");
    conn.execute_batch("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')")
        .expect("rebuild the FTS index");

    let after = search(app.clone(), &allowed, probe, serde_json::json!({})).await;
    assert_eq!(
        paths(&after.1).len(),
        1,
        "the rebuild must not have removed anything: the projection was never filtered"
    );
    let denied_after = search(app, &denied, probe, serde_json::json!({})).await;
    assert_eq!(
        paths(&denied_after.1),
        Vec::<String>::new(),
        "the guard is unchanged by a rebuild"
    );
}

/// The corpus carries reserved tags, the projection is intact, and SQLite agrees.
///
/// Seen RED against a throwaway implementation that filtered the reserved tags out of the
/// `notes_fts` projection: that implementation makes the index disagree with its content
/// table, and `integrity-check` re-derives the tokens from `notes` and compares them
/// posting by posting — the very corruption class this guard exists to avoid. The
/// projection must therefore never be touched.
#[tokio::test]
async fn fts_integrity_check_is_ok_on_a_corpus_carrying_reserved_tags() {
    let env = env_unscanned(PRESET).await;
    seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, PRIVACY_UNSCANNED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;

    let db = gradatum_core::paths::vault_dir_index_path(env.vault.root());
    let conn = rusqlite::Connection::open(&db).expect("open the vault index");
    // `rank = 1` re-derives the tokens from `notes` and compares posting by posting.
    // SQLITE_CORRUPT / SQLITE_CORRUPT_VTAB is the negative verdict, not an outage.
    let verdict =
        conn.execute_batch("INSERT INTO notes_fts(notes_fts, rank) VALUES('integrity-check', 1)");
    assert!(
        verdict.is_ok(),
        "integrity-check must pass on a corpus carrying reserved tags: {verdict:?}"
    );

    // Anti-vacuity: the index really does carry the tokens the check just reconciled, so a
    // green verdict is a statement about a marked corpus and not about an empty one.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH '\"personal classified\"'",
            [],
            |r| r.get(0),
        )
        .expect("count the marked posting");
    assert_eq!(hits, 1, "the marked tokens are in the index");
}

/// SENTINEL — not the closure of an open leak.
///
/// `notes_fts` indexes `body_text` **and** `tags`, so a snippet *could* be drawn from the
/// `tags` column. It is not today: the three `snippet(notes_fts, 0, …)` call sites of the
/// product all read column 0, which is `body_text`. This test asserts on the snippet as
/// much as on the hit count, and keeps the day `0` becomes `-1` — the same role the
/// confusable sentinel plays for D-12.
#[tokio::test]
async fn snippet_is_drawn_from_the_body_not_from_the_tags_column() {
    let env = env_with(PRESET).await;
    seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    let (_, json) = search(app, &allowed, "classified", serde_json::json!({})).await;
    let items = json["items"].as_array().expect("items must be an array");
    assert_eq!(items.len(), 1, "the allowed consumer sees the note: {json}");
    let snippet = items[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        snippet.contains("banal"),
        "the snippet is drawn from the body: {snippet:?}"
    );
    assert!(
        !snippet.contains("classified"),
        "the snippet must not be drawn from the tags column: {snippet:?}"
    );
}

// ── (b) against the shipped preset generation (SEC-P0-1, R3-P2-1) ────────────

/// The verdicts here come from the **shipped** preset rather than from a local copy:
/// `main-agent` carries the four segment-scoped negations, `admin` carries `["**"]` and
/// no negation.
///
/// The trust is a plain bearer token for both — the ACL engine resolves a consumer by
/// identity, and it is the preset's *patterns* this test is about. `admin` and `studio`
/// reach the studio router through an API key at runtime, which is a transport fact, not
/// an ACL one.
#[tokio::test]
async fn a_consumer_with_the_negation_sees_zero_marked_notes() {
    let env = env_with(SHIPPED_PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let state = &env.state;

    let probe = |t: TrustContext| {
        let state = state.clone();
        async move {
            gradatum_server::api_v1::logic::vault_search_impl(
                &state,
                &t,
                gradatum_dto::VaultSearchRequest::new("classified".to_owned()),
            )
            .await
            .expect("vault_search")
        }
    };

    let ok = probe(trust("admin", "main")).await;
    assert_eq!(
        ok.items.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
        vec![read_path(&id)],
        "control: `admin` carries no negation and still sees the marked note"
    );

    let filtered = probe(trust("main-agent", "main")).await;
    assert!(
        filtered.items.is_empty(),
        "`main-agent` carries the negation and must see nothing — got {:?}",
        filtered
            .items
            .iter()
            .map(|i| i.path.clone())
            .collect::<Vec<_>>()
    );
}

/// The same, spelled from the other side: the un-negated consumers keep seeing.
#[tokio::test]
async fn a_consumer_without_the_negation_still_sees_them() {
    let env = env_with(SHIPPED_PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let state = &env.state;

    for identity in ["admin", "studio"] {
        let resp = gradatum_server::api_v1::logic::vault_search_impl(
            state,
            &trust(identity, "main"),
            gradatum_dto::VaultSearchRequest::new("classified".to_owned()),
        )
        .await
        .expect("vault_search");
        assert_eq!(
            resp.items
                .iter()
                .map(|i| i.path.clone())
                .collect::<Vec<_>>(),
            vec![read_path(&id)],
            "`{identity}` carries no negation and must still see the marked note"
        );
    }
}

/// The shipped preset still is what the two tests above assume. Without this, a preset edit
/// would silently turn them into evidence about the pre-lot-F world — where the denial was
/// a dedicated flag rather than four pattern negations (R3-P2-1).
#[test]
fn shipped_preset_carries_the_four_negations_and_the_two_unnegated_admins() {
    const FOUR_NEGATIONS: [&str; 4] = [
        "!personal-classified",
        "!personal-classified/**",
        "!**/personal-classified",
        "!**/personal-classified/**",
    ];
    let mut expected: Vec<&str> = FOUR_NEGATIONS.to_vec();
    expected.push("**");

    let consumers = preset_consumers(SHIPPED_PRESET);
    assert!(
        !consumers.is_empty(),
        "`hierarchical.toml` declares no `[[consumer]]`"
    );

    let main_agent = consumers
        .iter()
        .find(|block| preset_list(block, "identity") == ["main-agent"])
        .expect("the shipped preset declares a `main-agent` consumer");
    for key in ["read_patterns", "write_patterns"] {
        assert_eq!(
            preset_list(main_agent, key),
            expected,
            "the `main-agent` `{key}` of `hierarchical.toml` moved — the consumers this \
             file exercises no longer carry the four negations"
        );
    }
    // `admin` and `studio`: full access, no negation at all.
    assert_eq!(
        consumers
            .iter()
            .filter(|block| preset_list(block, "read_patterns") == ["**"])
            .count(),
        2,
        "exactly two shipped consumers carry unnegated full read access (`admin`, `studio`)"
    );
}

/// Splits a preset into its `[[consumer]]` blocks — everything before the first one is
/// preamble and is dropped.
fn preset_consumers(preset: &str) -> Vec<&str> {
    preset.split("[[consumer]]").skip(1).collect()
}

/// The entries of `key`'s list, inside one `[[consumer]]` block.
///
/// Whitespace-insensitive by construction: the shipped file aligns its `=` signs, so a test
/// that matched a rendered `"key = [...]"` would be asserting on the file's column padding
/// rather than on the preset's content. An empty vector means the block does not carry the
/// key at all — which is also how a comment line inside a block reads.
fn preset_list(block: &str, key: &str) -> Vec<String> {
    for line in block.lines() {
        let Some(rest) = line.trim().strip_prefix(key) else {
            continue;
        };
        // A key that merely *starts* with `key` (`read_patterns_x`) is not this key.
        let Some(rhs) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        // The list brackets are part of the syntax, not of an entry — and the first entry
        // is the one that carries the opening one.
        return rhs
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .filter_map(|entry| {
                entry
                    .trim()
                    .strip_prefix('"')?
                    .strip_suffix('"')
                    .map(str::to_owned)
            })
            .collect();
    }
    Vec::new()
}

// ── (b) the verdict follows the RESULT's vault ───────────────────────────────

/// Two vaults, one negation scoped to `other`, one result set covering both.
///
/// A verdict bound to the requester's tenant would be a single verdict applied to notes
/// from different vaults: it either filters the `main` note (too much) or returns the
/// `other` note (too little). Both directions are asserted.
#[tokio::test]
async fn a_consumer_negated_in_one_vault_only_is_filtered_there_and_not_elsewhere() {
    let env = index_env(PRESET_ONE_VAULT_ONLY, true).await;
    // `main` is the requester's own vault and is NOT negated; `other` is.
    let main_marked = seed_indexed_note(&env.index_path, "main", &[PERSONAL_CLASSIFIED]);
    seed_indexed_note(&env.index_path, "other", &[PERSONAL_CLASSIFIED]);
    let other_clean = seed_indexed_note(&env.index_path, "other", &["ordinary-tag"]);

    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-guard-one-vault", "main");

    let (status, json) = search(
        app,
        &token,
        "gravity probe",
        serde_json::json!({ "vault_ids": ["other"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let mut got: Vec<(String, String)> = json["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .map(|it| {
            (
                it["vault_id"].as_str().unwrap_or_default().to_owned(),
                it["path"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    got.sort();

    let mut expected = vec![
        ("main".to_owned(), read_path(&main_marked)),
        ("other".to_owned(), read_path(&other_clean)),
    ];
    expected.sort();

    assert_eq!(
        got, expected,
        "the marked note of `main` stays (not negated there); the marked note of `other` \
         goes; the unmarked note of `other` stays — got {json}"
    );
}

// ── (c) rendering: the two reserved names dissociate ─────────────────────────

/// `metadata.tags` masks `personal-classified` alone, for a consumer without the capacity —
/// and the note itself stays readable. D-14 closes enumeration, not reading.
#[tokio::test]
async fn metadata_tags_masks_personal_classified_only() {
    let env = env_unscanned(PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, PRIVACY_UNSCANNED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");
    let path = read_path(&id);

    let read = |app: axum::Router, token: String, path: String| async move {
        post(
            app,
            "/api/v1/vault_read",
            &token,
            serde_json::json!({ "path": path }),
        )
        .await
    };

    let (status, json) = read(app.clone(), allowed, path.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let tags = tag_strings(&json);
    assert!(
        tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "an allowed consumer still sees the verdict: {tags:?}"
    );

    let (status, json) = read(app, denied, path).await;
    assert_eq!(status, StatusCode::OK, "the note itself stays readable");
    let tags = tag_strings(&json);
    assert!(
        !tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "the verdict is masked: {tags:?}"
    );
    assert!(
        tags.contains(&PRIVACY_UNSCANNED.to_owned()),
        "the operational marker is NOT masked: {tags:?}"
    );
    assert!(
        tags.contains(&"ordinary-tag".to_owned()),
        "ordinary tags are untouched: {tags:?}"
    );
}

/// The dissociation of (c) is the same as in (b): `privacy-unscanned` is never hidden.
#[tokio::test]
async fn metadata_tags_still_shows_privacy_unscanned() {
    let env = env_unscanned(PRESET).await;
    let id = seed(
        &env.vault,
        Section::Reference,
        &[PRIVACY_UNSCANNED, "ordinary-tag"],
        BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");

    let (status, json) = post(
        app,
        "/api/v1/vault_read",
        &denied,
        serde_json::json!({ "path": read_path(&id) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tags = tag_strings(&json);
    assert!(
        tags.contains(&PRIVACY_UNSCANNED.to_owned()),
        "a consumer without the capacity must still be able to tell that a note was never \
         scanned: {tags:?}"
    );
}

fn tag_strings(json: &serde_json::Value) -> Vec<String> {
    json["metadata"]["tags"]
        .as_array()
        .expect("metadata.tags must be an array")
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_owned())
        .collect()
}

// ── (b) on the surfaces that list or enumerate rather than match ─────────────

/// The listing renders no tags, so the guard drops the entry instead — and `total` must not
/// keep counting what the response does not carry.
#[tokio::test]
async fn vault_list_hides_marked_entries_from_a_denied_consumer() {
    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let clean = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    let list = |app: axum::Router, token: String| async move {
        post(
            app,
            "/api/v1/vault_list",
            &token,
            serde_json::json!({ "limit": 50 }),
        )
        .await
    };

    let (_, json) = list(app.clone(), allowed).await;
    let mut seen = entry_paths(&json);
    seen.sort();
    let mut expected = vec![read_path(&marked), read_path(&clean)];
    expected.sort();
    assert_eq!(seen, expected, "control: the listing is not empty");

    let (_, json) = list(app, denied).await;
    assert_eq!(
        entry_paths(&json),
        vec![read_path(&clean)],
        "the marked note leaves the listing — got {json}"
    );
    assert_eq!(
        json["total"].as_u64(),
        Some(1),
        "`total` must not count an entry the response does not carry: {json}"
    );
}

fn entry_paths(json: &serde_json::Value) -> Vec<String> {
    json["entries"]
        .as_array()
        .expect("entries must be an array")
        .iter()
        .map(|e| e["path"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The tag inventory is an enumeration surface of its own: the reserved name must not
/// appear for a consumer without the capacity — nor be counted in `total`.
#[tokio::test]
async fn vault_tags_hides_the_reserved_name_from_a_denied_consumer() {
    let env = env_with(PRESET).await;
    seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let app = build_router(env.state.clone());
    let denied = sign(&env.state, "f160-guard-denied", "main");
    let allowed = sign(&env.state, "f160-guard-allowed", "main");

    let (status, raw) = get(app.clone(), "/api/v1/vault_tags", &allowed).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(
        tag_inventory(&json).contains(&PERSONAL_CLASSIFIED.to_owned()),
        "control: the inventory carries the reserved name for an allowed consumer: {json}"
    );

    let (status, raw) = get(app, "/api/v1/vault_tags", &denied).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let tags = tag_inventory(&json);
    assert!(
        !tags.contains(&PERSONAL_CLASSIFIED.to_owned()),
        "the reserved name must not be inventoried: {json}"
    );
    assert_eq!(
        json["total"].as_u64(),
        Some(tags.len() as u64),
        "`total` must not count what the response does not carry: {json}"
    );
}

fn tag_inventory(json: &serde_json::Value) -> Vec<String> {
    json["tags"]
        .as_array()
        .expect("tags must be an array")
        .iter()
        .map(|t| t["tag"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Trace renders no tags today, but it renders the lineage's ULIDs — an enumeration surface
/// all the same. A marked note reachable as a child must leave the trace for a consumer
/// without the capacity.
#[tokio::test]
async fn vault_trace_hides_marked_notes_from_a_denied_consumer() {
    let env = index_env(PRESET, false).await;
    let root = seed_indexed_note(&env.index_path, "main", &["ordinary-tag"]);
    let child = seed_indexed_note(&env.index_path, "main", &[PERSONAL_CLASSIFIED]);
    link_notes(&env.index_path, &root, &child);

    let state = &env.state;

    let traced = |t: TrustContext| {
        let state = state.clone();
        let seed = root.clone();
        async move {
            gradatum_server::api_v1::logic::vault_trace_impl(
                &state,
                &t,
                gradatum_dto::VaultTraceRequest::new(seed),
            )
            .await
            .expect("vault_trace")
        }
    };

    let entries = traced(trust("f160-guard-allowed", "main")).await;
    // `TraceEntry.path` is the BARE note ULID (the `{section}/{ulid}` shape is
    // `SearchHit.path`, not this one) — the assertion compares ids, not paths.
    assert_eq!(
        entries
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        vec![child.clone()],
        "control: the marked child is in the lineage for an allowed consumer"
    );

    let filtered = traced(trust("f160-guard-denied", "main")).await;
    assert!(
        filtered.entries.is_empty(),
        "the marked child must leave the lineage — got {:?}",
        filtered
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>()
    );
}

/// Lessons are recalled from a shared corpus; a marked lesson leaves the recall for a
/// consumer without the capacity.
#[tokio::test]
async fn lessons_recall_hides_marked_lessons_from_a_denied_consumer() {
    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::LessonsLearned,
        &["deploy", PERSONAL_CLASSIFIED],
        "a lesson the deploy class matches marqueur-de-test",
    )
    .await;
    let clean = seed(
        &env.vault,
        Section::LessonsLearned,
        &["deploy", "ordinary-tag"],
        "another lesson the deploy class matches",
    )
    .await;

    let state = &env.state;

    let recall = |t: TrustContext| {
        let state = state.clone();
        async move {
            gradatum_server::api_v1::logic::lessons_recall_impl(
                &state,
                &t,
                gradatum_dto::LessonsRecallRequest::new("deploy".to_owned()),
            )
            .await
            .expect("lessons_recall")
        }
    };

    let mut seen: Vec<String> = recall(trust("f160-guard-allowed", "main"))
        .await
        .items
        .iter()
        .map(|i| i.ulid.clone())
        .collect();
    seen.sort();
    let mut expected = vec![marked.clone(), clean.clone()];
    expected.sort();
    assert_eq!(seen, expected, "control: both lessons are recalled");

    let filtered: Vec<String> = recall(trust("f160-guard-denied", "main"))
        .await
        .items
        .iter()
        .map(|i| i.ulid.clone())
        .collect();
    assert_eq!(filtered, vec![clean], "the marked lesson leaves the recall");
}

// ── Non-exposure of the `privacy` block ──────────────────────────────────────

/// Every read surface that can reach a note, swept for the `extra.privacy` block.
///
/// The sweep proves the surfaces it drives, not the absence of a tenth one. The block is
/// **genuine**: the fixture mounts a detector, so the product's own pass wrote it, with the
/// product's own keys. The assertion is therefore on those keys — a surface that rendered
/// the block, whole or key by key, would have to emit one of them.
#[tokio::test]
async fn no_api_surface_returns_extra_privacy() {
    use gradatum_server::api_v1::logic;

    let env = env_with(PRESET).await;
    let id = seed_with_history(&env.vault, Section::Reference, &["ordinary-tag"]).await;
    let state = &env.state;
    let t = trust("f160-guard-allowed", "main");
    let path = read_path(&id);

    // Anti-vacuity: the note under sweep really does carry the block. Without this the
    // whole test would pass on a corpus that has none.
    let on_disk = env
        .vault
        .read_note_by_id(&id)
        .await
        .expect("read the note back");
    assert!(
        on_disk.frontmatter.extra.get("privacy").is_some(),
        "the fixture must actually plant an `extra.privacy` block"
    );

    fn scan(label: &str, rendered: &str) {
        for key in BLOCK_ONLY_KEYS {
            assert!(
                !rendered.contains(key),
                "[{label}] renders `{key}` — the `extra.privacy` block is exposed: {rendered}"
            );
        }
        for key in ["privacy", "extra"] {
            assert!(
                !rendered.contains(&format!("\"{key}\"")),
                "[{label}] renders a `{key}` key: {rendered}"
            );
        }
    }

    fn json_of<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_string(value).expect("serialise the response")
    }

    let read = logic::vault_read_impl(state, &t, gradatum_dto::VaultReadRequest::new(path.clone()))
        .await
        .expect("vault_read");
    scan("vault_read", &json_of(&read));

    let search = logic::vault_search_impl(
        state,
        &t,
        gradatum_dto::VaultSearchRequest::new("banal".to_owned()),
    )
    .await
    .expect("vault_search");
    assert!(
        !search.items.is_empty(),
        "the sweep must reach the note, otherwise it proves nothing"
    );
    scan("vault_search", &json_of(&search));

    let list = logic::vault_list_impl(state, &t, gradatum_dto::VaultListRequest::default())
        .await
        .expect("vault_list");
    scan("vault_list", &json_of(&list));

    let trace =
        logic::vault_trace_impl(state, &t, gradatum_dto::VaultTraceRequest::new(id.clone()))
            .await
            .expect("vault_trace");
    scan("vault_trace", &json_of(&trace));

    let tags = logic::vault_tags_impl(state, &t, gradatum_dto::VaultTagsRequest::default())
        .await
        .expect("vault_tags");
    scan("vault_tags", &json_of(&tags));

    let context = logic::vault_context_impl(
        state,
        &t,
        gradatum_dto::VaultContextRequest::new("banal".to_owned()),
    )
    .await
    .expect("vault_context");
    scan("vault_context", &json_of(&context));

    let history = logic::vault_history_impl(
        state,
        &t,
        gradatum_dto::VaultHistoryRequest::new(id.clone()),
    )
    .await
    .expect("vault_history");
    scan("vault_history", &json_of(&history));

    // `vault_history_get` and `vault_diff` only show anything once a snapshot exists;
    // `seed_with_history` guarantees one, and the assertion keeps it that way.
    assert!(
        !history.versions.is_empty(),
        "the fixture must hold a history snapshot, otherwise history_get and diff are vacuous"
    );
    let oldest = history.versions[0];
    let got = logic::vault_history_get_impl(
        state,
        &t,
        gradatum_dto::VaultHistoryGetRequest::new(id.clone(), oldest),
    )
    .await
    .expect("vault_history_get");
    scan("vault_history_get", &json_of(&got));

    let diff = logic::vault_diff_impl(
        state,
        &t,
        gradatum_dto::VaultDiffRequest::new(id.clone(), oldest.to_string(), "current".to_owned()),
    )
    .await
    .expect("vault_diff");
    scan("vault_diff", &json_of(&diff));

    // ── the surfaces driven through the router ───────────────────────────────
    let app = build_router(env.state.clone());
    let token = sign(&env.state, "f160-guard-allowed", "main");

    let (status, raw) = get(app.clone(), "/api/v1/dashboard", &token).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    scan("dashboard", &raw);

    let (status, raw) = get(app.clone(), "/api/v1/vault/forgotten", &token).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    scan("vault/forgotten (deletion tombstone)", &raw);

    let (status, raw) = get(app.clone(), "/api/v1/lessons/recall?class=deploy", &token).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    scan("lessons/recall", &raw);

    let (status, raw) = get(app, "/api/v1/messages", &token).await;
    // The mailbox may legitimately answer `Forbidden` — an uncredited caller is not this
    // test's business. Whatever it answers, it does not carry the block.
    assert!(
        status == StatusCode::OK || status == StatusCode::FORBIDDEN,
        "unexpected messaging status {status}: {raw}"
    );
    scan("messages", &raw);

    // Rappels (proactive recall): the surface the design names alongside search.
    match gradatum_server::proactive_recall::proactive_recall(
        state,
        &t,
        gradatum_dto::ProactiveRecallRequest::default(),
    )
    .await
    {
        Ok(resp) => scan("proactive_recall", &json_of(&resp)),
        Err(e) => scan("proactive_recall", &format!("{e:?}")),
    }
}

// ── The second fixture: an index opened by path ──────────────────────────────

struct IndexEnv {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// Fixed timestamp — a reproducible corpus with no dependence on the clock.
const TS: i64 = 1_700_000_000_000;

/// An index opened by path rather than through a `Vault`, so that several vault ids can
/// coexist in one index — the only shape in which a single result set covers vaults with
/// different verdicts.
async fn index_env(preset: &str, multi_tenant: bool) -> IndexEnv {
    use gradatum_server::config::{MultiTenantConfig, ServerConfig};

    let dir = TempDir::new().expect("TempDir");
    let index_path = dir.path().join("index.db");
    let acl = AclEngine::from_preset_str(preset).expect("a valid ACL preset");
    let state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open")
        .with_server_config(ServerConfig {
            multi_tenant: MultiTenantConfig {
                enabled: multi_tenant,
            },
            ..ServerConfig::default()
        });

    if multi_tenant {
        let conn = rusqlite::Connection::open(&index_path).expect("open index.db");
        conn.execute(
            "INSERT OR IGNORE INTO tenants (id, status, created_at) VALUES ('other', 'active', ?1)",
            rusqlite::params![TS],
        )
        .expect("register the `other` tenant");
        conn.execute(
            "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES ('main', 'other', 'read')",
            [],
        )
        .expect("grant read on `other`");
    }

    IndexEnv {
        state,
        index_path,
        _dir: dir,
    }
}

/// Seeds a note directly in the index, with its tags written to **both** `notes` and
/// `notes_fts` — the same values, exactly as the write path leaves them.
/// Seeds one note (and its `notes_fts` projection) directly in the index, and returns the
/// **allocated** ULID.
///
/// The id is generated rather than written as a literal: the corpus has to be addressable
/// by `read_path`/`vault_read`, and a hand-written 26-character Crockford string is a
/// silent trap — `I`, `L`, `O` and `U` are not in the alphabet, so a plausible-looking
/// literal fails to parse and the failure surfaces far from its cause.
fn seed_indexed_note(index_path: &std::path::Path, vault: &str, tags: &[&str]) -> String {
    let id = ulid::Ulid::generate().to_string();
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    let tags_str = tags.join(" ");
    conn.execute(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title, tags)
           VALUES (?1, ?2, NULL, 'reference', 'live', 1, ?3, X'00', 'isolation gravity probe corpus', 'probe', ?4)",
        rusqlite::params![id, vault, TS, tags_str],
    )
    .expect("seed notes");
    conn.execute(
        "INSERT INTO notes_fts (rowid, body_text, tags)
           SELECT rowid, body_text, tags FROM notes WHERE id = ?1 AND vault_id = ?2",
        rusqlite::params![id, vault],
    )
    .expect("seed notes_fts");
    id
}

/// Links `src → dst` in `note_links`, so that `trace_lineage` has a lineage to render.
fn link_notes(index_path: &std::path::Path, src: &str, dst: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    conn.execute(
        "INSERT INTO note_links (src_note_id, dst_note_id, vault_id, created_at)
           VALUES (?1, ?2, 'main', ?3)",
        rusqlite::params![src, dst, TS],
    )
    .expect("seed note_links");
}

// ── G23 — the remaining enumeration surfaces ─────────────────────────────────
//
// D-14(b) is a property of the **result set**, not of a list of handlers. The first pass of
// this task guarded the six sites the brief named (`vault_search` mono and multi, `vault_read`
// for (c), `vault_list`, `vault_tags`, `vault_trace`, lessons recall). Five more surfaces
// render a note — its ULID, its title, an excerpt of its body, or its cardinality — and each
// is tested below under the same predicate and the same fixture as the guarded ones.
//
// Every test carries its allowed-consumer control in the same body: without it, "the surface
// returned nothing" would be indistinguishable from "the query never matched".

/// Gives a seeded note a temporal anchor, so that `vault_timeline` has a window to render.
///
/// `temporal_index` is written directly rather than through a write path, exactly as
/// [`seed_indexed_note`] writes `notes`: the fixture must control the anchor, and the anchor
/// is what the chronology orders and pages on.
fn seed_temporal(
    index_path: &std::path::Path,
    vault: &str,
    note_id: &str,
    anchor_ms: i64,
    doc_kind: &str,
) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    conn.execute(
        "INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind)
           VALUES (?1, ?2, ?3, 'created', ?4)",
        rusqlite::params![note_id, vault, anchor_ms, doc_kind],
    )
    .expect("seed temporal_index");
}

/// The proactive recall is a pull surface with no caller at computation time: its two modes
/// (`read_surface`, `contextual_hits`) both hand back `ulid` + `title` + `snippet`, so a
/// marked note reaching it leaks its existence and an excerpt of its body.
#[tokio::test]
async fn proactive_recall_hides_personal_classified_from_unauthorised_consumer() {
    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let clean = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let state = &env.state;

    // Contextual mode: the same `retrieve_candidates` path `vault_context` uses, so the
    // query below is a real FTS retrieval and not a replay of a pre-computed surface.
    let recall = |t: TrustContext| {
        let state = state.clone();
        async move {
            let mut req = gradatum_dto::ProactiveRecallRequest::default();
            req.context = Some("banal".to_owned());
            gradatum_server::proactive_recall::proactive_recall(&state, &t, req)
                .await
                .expect("proactive_recall")
        }
    };

    let mut seen: Vec<String> = recall(trust("f160-guard-allowed", "main"))
        .await
        .items
        .iter()
        .map(|h| h.ulid.clone())
        .collect();
    seen.sort();
    let mut expected = vec![marked.clone(), clean.clone()];
    expected.sort();
    assert_eq!(
        seen, expected,
        "control: both notes are surfaced for an allowed consumer"
    );

    let filtered: Vec<String> = recall(trust("f160-guard-denied", "main"))
        .await
        .items
        .iter()
        .map(|h| h.ulid.clone())
        .collect();
    assert_eq!(
        filtered,
        vec![clean],
        "the marked note leaves the proactive surface — its ULID and title go with it"
    );
}

/// The same surface, same fixture, consumer with the verdict: the marked note is surfaced.
///
/// This is the half that keeps the guard from being a blanket blackout — the exclusion is
/// decided by the predicate, so a consumer who may read the marker keeps seeing its notes.
#[tokio::test]
async fn proactive_recall_shows_personal_classified_to_authorised_consumer() {
    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let state = &env.state;

    let mut req = gradatum_dto::ProactiveRecallRequest::default();
    req.context = Some("banal".to_owned());
    let resp = gradatum_server::proactive_recall::proactive_recall(
        state,
        &trust("f160-guard-allowed", "main"),
        req,
    )
    .await
    .expect("proactive_recall");

    assert_eq!(resp.mode, "contextual");
    assert!(
        resp.items.iter().any(|h| h.ulid == marked),
        "an allowed consumer must keep receiving the marked note: {resp:?}"
    );
}

/// `vault_context` fills `assembled_text` with the **body** of each candidate — a marked note
/// reaching it puts personal data into a consumer's RAG context, which is the leak D-14(b)
/// exists to close on a body-bearing surface.
#[tokio::test]
async fn vault_context_hides_personal_classified_from_unauthorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let clean = seed(&env.vault, Section::Reference, &["ordinary-tag"], BODY).await;
    let state = &env.state;

    let assemble = |t: TrustContext| {
        let state = state.clone();
        async move {
            logic::vault_context_impl(
                &state,
                &t,
                gradatum_dto::VaultContextRequest::new("banal".to_owned()),
            )
            .await
            .expect("vault_context")
        }
    };

    let allowed = assemble(trust("f160-guard-allowed", "main")).await;
    let allowed_ids: Vec<&str> = allowed.included.iter().map(|n| n.ulid.as_str()).collect();
    assert!(
        allowed_ids.contains(&marked.as_str()),
        "control: the marked note is assembled for an allowed consumer"
    );
    assert!(
        allowed.assembled_text.contains(MARK),
        "control: the marked body reaches the assembled text for an allowed consumer"
    );

    let denied = assemble(trust("f160-guard-denied", "main")).await;
    let denied_ids: Vec<&str> = denied.included.iter().map(|n| n.ulid.as_str()).collect();
    assert!(
        !denied_ids.contains(&marked.as_str()),
        "the marked note must leave `included` — got {denied_ids:?}"
    );
    assert!(
        !denied.assembled_text.contains(MARK),
        "no excerpt of the marked body may reach `assembled_text` — got {}",
        denied.assembled_text
    );
    assert_eq!(
        denied_ids,
        vec![clean.as_str()],
        "the unmarked candidate stays: the context is not emptied wholesale"
    );
    // `candidates_considered` is a cardinality of its own, rendered to the caller: counting a
    // candidate the response does not carry would re-open the oracle one field over.
    assert_eq!(
        denied.diagnostics.candidates_considered, 1,
        "the candidate count must not include the note the response withholds"
    );
}

/// The same surface, same fixture, consumer with the verdict: body and ULID both come back.
#[tokio::test]
async fn vault_context_shows_personal_classified_to_authorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let state = &env.state;

    let resp = logic::vault_context_impl(
        state,
        &trust("f160-guard-allowed", "main"),
        gradatum_dto::VaultContextRequest::new("banal".to_owned()),
    )
    .await
    .expect("vault_context");

    assert!(
        resp.included.iter().any(|n| n.ulid == marked),
        "an allowed consumer must keep receiving the marked note: {resp:?}"
    );
    assert!(
        resp.assembled_text.contains(MARK),
        "an allowed consumer must keep receiving the marked body"
    );
}

/// `vault_graph` enumerates the neighbourhood of a root as ULIDs: a marked neighbour rendered
/// as a node or an edge reveals its existence and its identifier, which is exactly why
/// `filter_identity_nodes` already removes cross-agent souls here.
#[tokio::test]
async fn vault_graph_hides_personal_classified_from_unauthorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = index_env(PRESET, false).await;
    let root = seed_indexed_note(&env.index_path, "main", &["ordinary-tag"]);
    let marked = seed_indexed_note(&env.index_path, "main", &[PERSONAL_CLASSIFIED]);
    link_notes(&env.index_path, &root, &marked);
    let state = &env.state;

    let graph = |t: TrustContext| {
        let state = state.clone();
        let root = root.clone();
        async move {
            logic::vault_graph_impl(&state, &t, gradatum_dto::VaultGraphRequest::new(root))
                .await
                .expect("vault_graph")
        }
    };

    let allowed = graph(trust("f160-guard-allowed", "main")).await;
    assert!(
        allowed.nodes.contains(&marked),
        "control: the marked neighbour is a node for an allowed consumer"
    );
    assert!(
        allowed.edges.iter().any(|e| e.to == marked),
        "control: the edge to the marked neighbour is rendered for an allowed consumer"
    );

    let denied = graph(trust("f160-guard-denied", "main")).await;
    assert!(
        !denied.nodes.contains(&marked),
        "the marked neighbour must leave `nodes` — got {:?}",
        denied.nodes
    );
    assert!(
        denied
            .edges
            .iter()
            .all(|e| e.from != marked && e.to != marked),
        "no edge may name the marked neighbour — got {:?}",
        denied.edges
    );
    assert!(
        denied.nodes.contains(&root),
        "the root itself survives: the graph is not emptied wholesale"
    );
}

/// `vault_links` is the same enumeration one hop deep, with `include_backlinks` defaulting to
/// `true`: the outbound target and every backlink are ULIDs of neighbouring notes.
#[tokio::test]
async fn vault_links_hides_personal_classified_from_unauthorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = index_env(PRESET, false).await;
    let root = seed_indexed_note(&env.index_path, "main", &["ordinary-tag"]);
    let marked = seed_indexed_note(&env.index_path, "main", &[PERSONAL_CLASSIFIED]);
    // Outbound: root → marked. Backlink: backlink → root, so both directions are exercised.
    let backlink = seed_indexed_note(&env.index_path, "main", &["ordinary-tag"]);
    link_notes(&env.index_path, &root, &marked);
    link_notes(&env.index_path, &backlink, &root);
    let state = &env.state;

    let links = |t: TrustContext| {
        let state = state.clone();
        let root = root.clone();
        async move {
            logic::vault_links_impl(&state, &t, gradatum_dto::VaultLinksRequest::new(root))
                .await
                .expect("vault_links")
        }
    };

    let allowed = links(trust("f160-guard-allowed", "main")).await;
    let mut nodes = allowed.nodes.clone();
    nodes.sort();
    let mut expected = vec![root.clone(), marked.clone(), backlink.clone()];
    expected.sort();
    assert_eq!(
        nodes, expected,
        "control: the neighbourhood is rendered in full"
    );

    let denied = links(trust("f160-guard-denied", "main")).await;
    let mut nodes = denied.nodes.clone();
    nodes.sort();
    let mut expected = vec![root.clone(), backlink.clone()];
    expected.sort();
    assert_eq!(nodes, expected, "the marked note leaves the links");
    assert!(
        denied
            .edges
            .iter()
            .all(|e| e.from != marked && e.to != marked),
        "no edge may name the marked note — got {:?}",
        denied.edges
    );
}

/// `vault_timeline` renders `note_id` + `title` over a time window. The SQL excludes the
/// protected sections for **every** caller; the reserved marker is a verdict, not a blackout,
/// so its exclusion follows the caller's own read decision.
#[tokio::test]
async fn vault_timeline_hides_personal_classified_from_unauthorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = index_env(PRESET, false).await;
    let marked = seed_indexed_note(&env.index_path, "main", &[PERSONAL_CLASSIFIED]);
    let clean = seed_indexed_note(&env.index_path, "main", &["ordinary-tag"]);
    seed_temporal(&env.index_path, "main", &marked, TS + 1_000, "Event");
    seed_temporal(&env.index_path, "main", &clean, TS, "Event");
    let state = &env.state;

    let timeline = |t: TrustContext| {
        let state = state.clone();
        async move {
            logic::vault_timeline_impl(&state, &t, gradatum_dto::VaultTimelineRequest::default())
                .await
                .expect("vault_timeline")
        }
    };

    let allowed: Vec<String> = timeline(trust("f160-guard-allowed", "main"))
        .await
        .items
        .iter()
        .map(|i| i.note_id.clone())
        .collect();
    assert_eq!(
        allowed,
        vec![marked.clone(), clean.clone()],
        "control: the window renders both notes, most recent anchor first"
    );

    let denied: Vec<String> = timeline(trust("f160-guard-denied", "main"))
        .await
        .items
        .iter()
        .map(|i| i.note_id.clone())
        .collect();
    assert_eq!(
        denied,
        vec![clean.clone()],
        "the marked note leaves the chronology — its ULID and title go with it"
    );

    // Pagination must still advance: the guard removes rows from the page, it does not stall
    // the cursor. With `limit: 1` and the marked note newest, the denied consumer's first page
    // is the unmarked note and its cursor points past the marked row.
    let mut first = gradatum_dto::VaultTimelineRequest::default();
    first.limit = Some(1);
    let page = logic::vault_timeline_impl(state, &trust("f160-guard-denied", "main"), first)
        .await
        .expect("vault_timeline");
    assert_eq!(
        page.items
            .iter()
            .map(|i| i.note_id.clone())
            .collect::<Vec<_>>(),
        vec![clean.clone()],
        "a one-row window must not stall on a withheld row"
    );
}

/// `corpus_match_count` is an opt-in *cardinality*: a consumer who cannot read the marker
/// must not learn from it that marked notes match the query. Same rule as `vault_list.total`
/// — an adjustment scoped to what this response withheld, never a second authority.
#[tokio::test]
async fn corpus_match_count_hides_personal_classified_from_unauthorised_consumer() {
    use gradatum_server::api_v1::logic;

    let env = env_with(PRESET).await;
    let marked = seed(
        &env.vault,
        Section::Reference,
        &[PERSONAL_CLASSIFIED, "ordinary-tag"],
        MARKED_BODY,
    )
    .await;
    let state = &env.state;

    // `classified` is the query of `bare_classified_token_returns_zero_marked_notes`: it is not
    // a reserved token, so it survives (a) and reaches the index — the one note matching it is
    // the marked one, which is precisely the case the count must not betray.
    let counted = |t: TrustContext| {
        let state = state.clone();
        async move {
            let mut req = gradatum_dto::VaultSearchRequest::new("classified".to_owned());
            req.include_corpus_count = true;
            logic::vault_search_impl(&state, &t, req)
                .await
                .expect("vault_search")
        }
    };

    let allowed = counted(trust("f160-guard-allowed", "main")).await;
    assert_eq!(
        allowed
            .items
            .iter()
            .map(|h| h.path.clone())
            .collect::<Vec<_>>(),
        vec![read_path(&marked)],
        "control: the query matches the marked note for an allowed consumer"
    );
    assert_eq!(
        allowed.corpus_match_count,
        Some(1),
        "control: the corpus count sees the match"
    );

    let denied = counted(trust("f160-guard-denied", "main")).await;
    assert!(
        denied.items.is_empty(),
        "the marked note leaves the results — got {:?}",
        denied.items
    );
    assert_eq!(
        denied.corpus_match_count,
        Some(0),
        "`corpus_match_count > len(items)` would announce the withheld match: {denied:?}"
    );
}
