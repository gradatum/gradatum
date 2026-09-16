//! F-160 — the two reserved tag names are closed at the `Tag` constructor.
//!
//! `personal-classified` carries the privacy verdict the design's D-12 attaches to a
//! note holding personal data; `privacy-unscanned` is the operational marker of an
//! incomplete pass. Neither is an ordinary tag: only a detection pass, or the review
//! surface, may pose them. The reservation lives in the `Tag`
//! constructor, so **every** write path that turns a caller-supplied string into a tag
//! refuses the two names by construction — the public frontier is allowed to answer
//! `400` earlier and in duplicate, it is never the only door.
//!
//! ## What these tests measure
//!
//! - The **public frontier**: `PATCH /api/v1/notes/{id}` with `add_tags` answers `400`
//!   for every spelling the design names — including `personal--classified`, which
//!   passes the format check unchanged and is caught only by the *normalised*
//!   comparison.
//! - The **internal persist path** (`POST /internal/v1/persist/curated`, the door the
//!   worker's curated writes come through) leaves the reserved names out of the note
//!   instead of writing them.
//! - A **deserialised** reserved tag — the door D-12 deliberately leaves
//!   non-validating, because it carries the D-11 monotone marker — survives `add_tags`,
//!   `update_status` and `move_locus`, and comes back from `restore_archive`. Read
//!   back from disk, never from the write call's echo.
//! - A **client-controlled frontmatter scalar** carrying a whole crafted YAML block
//!   cannot terminate the frontmatter and forge the tag (SEC §4(c)). The defence is
//!   `serde_norway`'s quoting, assumed until now and never executed; this test is
//!   where it is executed.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AclCheckedVaultId, LocusId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_server::{internal::build_internal_router, state::AppState};
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use secrecy::SecretString;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

/// ACL preset granting read+write on `main/*` to the tests' single identity.
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "f160-tags-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// Token expected by the internal router.
const TEST_TOKEN: &str = "test-internal-token-f160";

/// The twelve spellings the design names — six per reserved name — spelled out.
///
/// Kept literal rather than derived from the reserved names: a derived list drifts
/// from what the design names, and it is the named spellings the guard is held to.
const RESERVED_SPELLINGS: [&str; 12] = [
    "Personal Classified",
    "PERSONAL_CLASSIFIED",
    "personal.classified",
    "personal--classified",
    " personal classified",
    "personal-classified",
    "Privacy Unscanned",
    "PRIVACY_UNSCANNED",
    "privacy.unscanned",
    "privacy--unscanned",
    " privacy unscanned",
    "privacy-unscanned",
];

/// The reserved names, as they appear in a note's frontmatter once posed.
const RESERVED_LITERALS: [&str; 2] = ["personal-classified", "privacy-unscanned"];

// ── Disk access ──────────────────────────────────────────────────────────────

/// Reads the note's `.md` **from disk**, found by walking the vault root.
///
/// The location is discovered rather than computed: the vault chooses its own layout
/// (the locus moves around during these tests), and a path built by the test would be
/// the test's assumption about that layout rather than the file the product wrote.
fn read_md_from_disk(root: &Path, id: &str) -> String {
    let name = format!("{id}.md");
    let mut found: Option<PathBuf> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name.as_str()) {
                found = Some(path);
            }
        }
    }
    let path = found.unwrap_or_else(|| panic!("no {name} on disk under {}", root.display()));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The tag literals of an on-disk note, parsed by the same parser the vault uses.
fn disk_tags(raw: &str) -> Vec<String> {
    let parsed = gradatum_markdown::parse(raw).expect("the on-disk note must parse");
    parsed
        .frontmatter
        .tags
        .iter()
        .map(|t| t.as_str().to_owned())
        .collect()
}

/// Asserts that a note read back from disk carries neither reserved name.
fn assert_no_reserved_tag(raw: &str, step: &str) {
    let tags = disk_tags(raw);
    for reserved in RESERVED_LITERALS {
        assert!(
            !tags.iter().any(|t| t == reserved),
            "after {step}, the note on disk carries the reserved tag {reserved:?}: {tags:?}"
        );
    }
}

/// Asserts that a note read back from disk **still** carries both reserved names.
fn assert_reserved_tags_survived(raw: &str, step: &str) {
    let tags = disk_tags(raw);
    for reserved in RESERVED_LITERALS {
        assert!(
            tags.iter().any(|t| t == reserved),
            "after {step}, the reserved tag {reserved:?} is gone from the note on disk: {tags:?}"
        );
    }
}

/// The reserved name as it arrives through the **deserialisation** door.
///
/// That door is deliberately non-validating — it is what carries the D-11 monotone
/// marker from disk back into memory, and a validating `Deserialize` would retire the
/// marker on every re-read. Building the tag any other way would be testing a tag no
/// production path can produce.
fn deserialised_reserved(literal: &str) -> Tag {
    serde_json::from_value(serde_json::Value::String(literal.to_owned()))
        .expect("a reserved tag deserialises — the door is deliberately non-validating")
}

// ── Public frontier fixture ──────────────────────────────────────────────────

struct PublicEnv {
    app: axum::Router,
    vault: Arc<Vault>,
    token: String,
    dir: TempDir,
}

/// A public app backed by a real `Vault` and a signed JWT.
async fn build_public_env() -> PublicEnv {
    use axum::{Router, middleware};

    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );

    let mut state = AppState::new().with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("a valid ACL preset"));
    // `patch_note_impl` takes its partial-patch branch (`state.search.patch_note_status`)
    // even when only `add_tags` is supplied, so the state's search index must be the
    // vault's own — a placeholder answers `NoteNotFound` and turns the `400` under test
    // into a `404`.
    state.search = vault.index().clone();

    let token = state
        .jwt
        .sign(
            "f160-tags-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign the test JWT");

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    PublicEnv {
        app,
        vault,
        token,
        dir,
    }
}

/// Seeds a note through the concrete vault and returns its ULID.
async fn seed_note(vault: &Vault, body: &str) -> String {
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Draft,
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
    vault
        .write_note(fm, body.to_owned())
        .await
        .expect("write_note")
        .id
        .to_string()
}

/// `PATCH /api/v1/notes/{id}` adding `tag`.
fn patch_add_tags_req(id: &str, tag: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/v1/notes/{id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "add_tags": [tag] })).unwrap(),
        ))
        .unwrap()
}

// ── Internal persist fixture ─────────────────────────────────────────────────

struct InternalEnv {
    router: axum::Router,
    vault: Arc<Vault>,
    _dir: TempDir,
}

/// A minimal embedder — the persist path never embeds, but the state builder wants one.
struct NoopEmbed;

#[async_trait::async_trait]
impl gradatum_embed::Embedder for NoopEmbed {
    fn embedder_id(&self) -> &str {
        "noop-f160"
    }
    fn dim(&self) -> u16 {
        4
    }
    async fn embed(&self, _: &str) -> Result<Vec<f32>, gradatum_embed::error::EmbedError> {
        Ok(vec![0.0f32; 4])
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, gradatum_embed::error::EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 4]).collect())
    }
    fn backend_kind(&self) -> gradatum_embed::EmbedBackend {
        gradatum_embed::EmbedBackend::Noop
    }
}

async fn build_internal_env() -> InternalEnv {
    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(ACL_ALLOW).expect("a valid ACL preset");

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopEmbed))
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>)
        .with_internal_api_token(SecretString::from(TEST_TOKEN.to_string()));
    state.search = index;

    InternalEnv {
        router: build_internal_router(state),
        vault,
        _dir: dir,
    }
}

/// A loopback `POST` against the internal router, carrying the internal bearer.
fn internal_post(uri: &str, body: serde_json::Value, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Internal", format!("Bearer {token}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ── 1. The public frontier ───────────────────────────────────────────────────

/// `PATCH …/notes/{id}` with `add_tags` refuses all twelve reserved spellings in `400`.
///
/// The path is the whole chain the design names: the handler's `validate_add_tags`
/// (`notes.rs`), then the vault-side union (`lifecycle.rs`). The reservation lives at
/// the constructor both ends funnel through, so the frontier's `400` is a duplicate of
/// that guard and never its only carrier.
#[tokio::test]
async fn patch_add_tags_rejects_each_reserved_name_400() {
    let env = build_public_env().await;
    let id = seed_note(&env.vault, "corps patch add_tags").await;

    for spelling in RESERVED_SPELLINGS {
        let resp = env
            .app
            .clone()
            .oneshot(patch_add_tags_req(&id, spelling, &env.token))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "add_tags {spelling:?} must be refused at the frontier"
        );
        let _ = resp.into_body().collect().await;
    }

    // Anti-vacuity: the same route accepts a tag that merely *names* a reserved name,
    // and lands it on the note. Without this the loop above would also pass on a route
    // that answered `400` to everything.
    let resp = env
        .app
        .clone()
        .oneshot(patch_add_tags_req(
            &id,
            "personal-classified-2026",
            &env.token,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "an ordinary tag that mentions a reserved name must be accepted"
    );
    let _ = resp.into_body().collect().await;

    let note = env
        .vault
        .read_note_by_id(&id)
        .await
        .expect("read the note back");
    let tags: Vec<&str> = note.frontmatter.tags.iter().map(Tag::as_str).collect();
    assert!(
        tags.contains(&"personal-classified-2026"),
        "the accepted tag must be on the note: {tags:?}"
    );
}

// ── 2. The internal persist path ─────────────────────────────────────────────

/// The worker's curated persist door does not write a reserved name into a note.
///
/// This is `vault_write`'s own path — `persist.rs::parse_tags` normalises the
/// caller-supplied `tags` — so it is the door a client's `vault_write` reaches once the
/// job is picked up. The ordinary tag rides along in the same call to prove the
/// assembly is live and not simply producing an empty list.
#[tokio::test]
async fn vault_write_tags_cannot_carry_a_reserved_name() {
    let env = build_internal_env().await;
    let note_id = Ulid::generate().to_string();

    let req = internal_post(
        "/internal/v1/persist/curated",
        serde_json::json!({
            "note_id": note_id,
            "tenant_id": "main",
            "section": "decisions",
            "status": "live",
            "title": "F-160 reserved tags",
            "body": "corps",
            "tags": ["personal-classified", "Personal Classified", "ordinary-tag"],
            "author": null,
            "provenance": null,
            "temporal": null,
            "links": [],
            "trust": null,
        }),
        TEST_TOKEN,
    );
    let resp = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "persist/curated with a reserved tag in the list must still succeed"
    );
    let _ = resp.into_body().collect().await;

    let note = env
        .vault
        .read_note(NoteId(Ulid::from_string(&note_id).expect("a ULID")))
        .await
        .expect("the note must have been written");
    let tags: Vec<&str> = note.frontmatter.tags.iter().map(Tag::as_str).collect();
    for reserved in RESERVED_LITERALS {
        assert!(
            !tags.contains(&reserved),
            "persist/curated wrote the reserved tag {reserved:?}: {tags:?}"
        );
    }
    assert!(
        tags.contains(&"ordinary-tag"),
        "the ordinary tag must have survived the assembly: {tags:?}"
    );
}

// ── 3. Deserialisation transit (D-11) ────────────────────────────────────────

/// A reserved tag that came in through deserialisation survives every lifecycle
/// rewrite, and is on disk after each one.
///
/// The deserialisation door is non-validating **on purpose**: it is how a note
/// already marked on disk comes back into memory. A validating `Deserialize` would
/// drop the marker on the first re-read of a note no pass is running on — and since
/// "no detector mounted" is the ordinary state of an installation, nothing would ever
/// re-pose it. This test walks the four rewrites the design names and reads the file
/// back from disk after each.
#[tokio::test]
async fn deserialised_reserved_tag_transits_unchanged() {
    let dir = TempDir::new().expect("TempDir");
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create");

    // The note carries what a pass would have left: both reserved names, plus an
    // ordinary tag so the assertion cannot pass on an empty list.
    let mut fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Draft,
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
    fm.tags = vec![
        deserialised_reserved("personal-classified"),
        deserialised_reserved("privacy-unscanned"),
        Tag::new("ordinary-tag").expect("an ordinary tag"),
    ]
    .into();

    let note = vault
        .write_note(fm, "corps transit".to_owned())
        .await
        .expect("write_note");
    let id = note.id;
    let id_str = id.to_string();
    let checked = AclCheckedVaultId::attest_write_checked(VaultId::new("main"));

    let raw = read_md_from_disk(dir.path(), &id_str);
    assert_reserved_tags_survived(&raw, "the initial write");

    // (a) add_tags — the union path, which re-validates only what it adds.
    vault
        .add_tags(id, &["added-later".to_owned()])
        .await
        .expect("add_tags");
    let raw = read_md_from_disk(dir.path(), &id_str);
    assert_reserved_tags_survived(&raw, "add_tags");
    assert!(
        disk_tags(&raw).iter().any(|t| t == "ordinary-tag"),
        "add_tags must have kept the ordinary tag: {:?}",
        disk_tags(&raw)
    );

    // (b) the status transition — a full rewrite through the CoW path.
    vault
        .update_note_status(
            &checked,
            &id_str,
            NoteStatus::PendingReview,
            Some("reviewed".into()),
        )
        .await
        .expect("update_note_status");
    let raw = read_md_from_disk(dir.path(), &id_str);
    assert_reserved_tags_survived(&raw, "update_note_status");

    // (c) the physical relocation — the `.md` moves and is rewritten at its new home.
    vault
        .move_locus(
            id,
            &LocusId::parse("knowledge/rust").expect("a valid locus"),
        )
        .await
        .expect("move_locus");
    let raw = read_md_from_disk(dir.path(), &id_str);
    assert_reserved_tags_survived(&raw, "move_locus");
    assert!(
        disk_tags(&raw).iter().any(|t| t == "added-later"),
        "move_locus must have kept the tags the note had: {:?}",
        disk_tags(&raw)
    );

    // (d) archive then restore — the note leaves the tree and comes back.
    vault
        .archive_note_by_id(id, Some("f160-test".to_owned()), i64::MAX)
        .await
        .expect("archive_note_by_id");
    // Replicates the server's index cascade: archiving deliberately does not de-index —
    // the server choke point runs the deletion as a separate step — and without it the
    // restore refuses on a ULID collision. This mirrors the real archive flow.
    vault
        .index()
        .delete_note_from_index("main", &id_str)
        .await
        .expect("de-index (server cascade)");
    vault
        .restore_archive_by_id(&id_str)
        .await
        .expect("restore_archive_by_id");
    let raw = read_md_from_disk(dir.path(), &id_str);
    assert_reserved_tags_survived(&raw, "restore_archive");
}

// ── 4. SEC §4(c) — the crafted frontmatter scalar ────────────────────────────

/// A client-controlled frontmatter scalar cannot close the YAML block and forge a tag.
///
/// Nothing in the write path builds the frontmatter by hand: it is serialised by
/// `serde_norway`, whose quoting is what stands between a scalar and the document
/// structure. That defence was called *plausible* by the design's security review and
/// explicitly left unmeasured; this is where it is executed.
///
/// The payload is a whole block — a client-supplied `status_reason` that, if it were
/// emitted verbatim, would close the frontmatter early and open a `tags:` entry in the
/// body. Two things are asserted, and the second is the discriminating one:
///
/// 1. the tag is not on the note, read back from disk;
/// 2. the reason survived **as a scalar**, character for character.
///
/// (2) is what makes (1) mean something. Were the payload emitted unquoted, the block
/// would indeed close early — and the parsed frontmatter would then be the truncated
/// prefix, which carries no reserved tag either. The first assertion alone would pass
/// on a corrupt file. It is the round-trip of the scalar that distinguishes "quoted
/// and inert" from "leaked and truncated".
#[tokio::test]
async fn a_newline_bearing_frontmatter_scalar_cannot_terminate_the_yaml_block() {
    const PAYLOAD: &str = "\n---\ntags:\n- personal-classified\n---\n";

    let env = build_public_env().await;
    let id = seed_note(&env.vault, "corps injection YAML").await;

    let req = Request::builder()
        .uri(format!("/api/v1/notes/{id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", env.token))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "status": "pending-review",
                "status_reason": PAYLOAD,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = env.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "the crafted scalar is accepted as a scalar — refusing it is not the defence under test"
    );
    let _ = resp.into_body().collect().await;

    let raw = read_md_from_disk(env.dir.path(), &id);

    // The discriminating assertion: the scalar round-tripped whole.
    let parsed = gradatum_markdown::parse(&raw).expect("the on-disk note must parse");
    assert_eq!(
        parsed.frontmatter.status_reason.as_deref(),
        Some(PAYLOAD),
        "the crafted scalar must come back as one scalar, not as document structure:\n{raw}"
    );

    // …and so the reserved tag is nowhere on the note.
    assert_no_reserved_tag(&raw, "the crafted-scalar write");

    // A tag entry the writer actually interpreted would sit at column 0. Quoted or
    // indented, the payload cannot produce one.
    for line in raw.lines() {
        assert_ne!(
            line, "- personal-classified",
            "the payload opened a real tag entry in the on-disk frontmatter:\n{raw}"
        );
    }
}
