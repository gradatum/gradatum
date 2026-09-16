//! E2E tests for `GET /api/v1/project-map/cards?version=<v>`.
//!
//! The endpoint has existed since 2.2.0 — this file cannot go red on a missing
//! route. The non-vacuity guard is that every assertion pins a **literal value**
//! (id, feature, status, kind, version, visibility, title), never `!is_empty()`.
//!
//! Fixture — five `project-map` notes:
//! - A `F-300` work card tracking `gradatum/2.4.0` (the "2.4.0" card),
//! - B `F-301` work card tracking `gradatum/backlog`,
//! - C `F-302` work card tracking `gradatum/2.5.0`, carrying `[[visibilite:interne]]`,
//! - D `F-303` work card carrying `[[project:autre]]` (off the `gradatum` filter),
//! - E `F-304` work card tracking `gradatum/2.4.0` but seeded with note `status = garbage`.
//!
//! `garbage` is a **note** status (filtered by the handler, `api_v1/project_map.rs`,
//! `r.status != "garbage"`), distinct from a card's `[[status:…]]`. Note E also targets
//! `2.4.0`: `?version=2.4.0` returning exactly card A therefore proves E is filtered out.
//!
//! Cases: (a) `?version=2.4.0` → exactly card A ; (b) `?version=v2.4.0` → same (the `v`
//! prefix is normalised) ; (c) no `version` → the `gradatum` cards including the interne
//! one and **without** the `autre` one ; (d) the garbage note never appears ; (e) an
//! unknown version → empty list and 200, never 404.
//!
//! The last two tests prove the discriminating power of `project_map_card_index` directly
//! (P1-6): mechanical, replayable by a third party, zero transient mutation of the tree.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::project_map::{ProjectMapCardEntry, project_map_card_index};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ── Preset ACL ───────────────────────────────────────────────────────────────

const TEST_ACL: &str = r#"
[[consumer]]
identity = "pm-tester"
read_patterns  = ["main/*", "main/project-map"]
write_patterns = []
"#;

// ── Embedder noop ──────────────────────────────────────────────────────────────

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-pm-cards"
    }
    fn dim(&self) -> u16 {
        8
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0f32; 8])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

async fn build_app(acl_preset: &str) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(acl_preset).expect("preset ACL valide");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test project_map cards"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx)
}

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "pm-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT — invariant test")
}

fn get_cards(token: &str, query: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/v1/project-map/cards{query}"))
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("builder — invariant test")
}

// ── Fixture ──────────────────────────────────────────────────────────────────
//
// Every role is placed on a single **structural line** (only separators between links),
// so `extract_structural_targets` counts it — a role cited in prose or fenced code would
// not be. `release` is *derived* from the `[[track:]]` (never read from the body), so the
// tracks below resolve to no structure card and the derived `release` is `null` — irrelevant
// to the version/project/status/kind/visibility axes under test here.

// Stable, human-readable ULIDs (ascending → deterministic tie-break order).
const ID_A: &str = "01AAAAAAAAAAAAAAAAAAAAAAAA"; // F-300, tracks 2.4.0
const ID_B: &str = "01BBBBBBBBBBBBBBBBBBBBBBBB"; // F-301, tracks backlog
const ID_C: &str = "01CCCCCCCCCCCCCCCCCCCCCCCC"; // F-302, interne, tracks 2.5.0
const ID_D: &str = "01DDDDDDDDDDDDDDDDDDDDDDDD"; // F-303, project:autre
const ID_E: &str = "01EEEEEEEEEEEEEEEEEEEEEEEE"; // F-304, note status = garbage

const BODY_A: &str = "[[feature:F-300]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[track:gradatum/2.4.0]]";
const BODY_B: &str = "[[feature:F-301]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[track:gradatum/backlog]]";
const BODY_C: &str = "[[feature:F-302]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[track:gradatum/2.5.0]] [[visibilite:interne]]";
const BODY_D: &str =
    "[[feature:F-303]] [[project:autre]] [[status:OPEN]] [[kind:FEATURE]] [[track:autre/1.0.0]]";
const BODY_E: &str = "[[feature:F-304]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[track:gradatum/2.4.0]]";

/// Seeds the five-note fixture. A–D are `live` with an explicit title; E is `garbage`.
async fn seed_fixture(idx: &SqliteIndex) {
    idx.seed_note_with_fts_title(ID_A, "project-map", "Carte 2.4.0", BODY_A)
        .await
        .expect("seed A");
    idx.seed_note_with_fts_title(ID_B, "project-map", "Carte backlog", BODY_B)
        .await
        .expect("seed B");
    idx.seed_note_with_fts_title(ID_C, "project-map", "Carte interne", BODY_C)
        .await
        .expect("seed C");
    idx.seed_note_with_fts_title(ID_D, "project-map", "Carte autre", BODY_D)
        .await
        .expect("seed D");
    idx.seed_note_with_status(ID_E, Section::ProjectMap, BODY_E, NoteStatus::Garbage, None)
        .await
        .expect("seed E (garbage)");
}

/// Corpus fed directly to `project_map_card_index` — the four **non-garbage** notes
/// (A–D), exactly what the handler passes after filtering `status = garbage`.
fn corpus_live() -> Vec<(String, String, String)> {
    vec![
        (
            ID_A.to_string(),
            BODY_A.to_string(),
            "Carte 2.4.0".to_string(),
        ),
        (
            ID_B.to_string(),
            BODY_B.to_string(),
            "Carte backlog".to_string(),
        ),
        (
            ID_C.to_string(),
            BODY_C.to_string(),
            "Carte interne".to_string(),
        ),
        (
            ID_D.to_string(),
            BODY_D.to_string(),
            "Carte autre".to_string(),
        ),
    ]
}

async fn cards_json(app: &axum::Router, token: &str, query: &str) -> Vec<serde_json::Value> {
    let resp = app
        .clone()
        .oneshot(get_cards(token, query))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "endpoint doit rendre 200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("réponse = tableau JSON de cartes")
}

// ─────────────────────────────────────────────────────────────────────────────
// E2E — the endpoint
// ─────────────────────────────────────────────────────────────────────────────

// (a) `?version=2.4.0` renders exactly card A, every axis pinned to its literal value.
//     Card E also tracks 2.4.0 but is `status = garbage` — its absence here proves the
//     handler's garbage filter (cases a + d combined on a single query).
#[tokio::test]
async fn version_filter_returns_exactly_the_targeted_card() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    seed_fixture(&idx).await;

    let entries = cards_json(&app, &token, "?version=2.4.0").await;

    assert_eq!(
        entries.len(),
        1,
        "exactement la carte 2.4.0 (garbage exclue) : {entries:?}"
    );
    let c = &entries[0];
    assert_eq!(c["id"], ID_A);
    assert_eq!(c["feature"], "F-300");
    assert_eq!(c["status"], "OPEN");
    assert_eq!(c["kind"], "FEATURE");
    assert_eq!(c["version"], "v2.4.0");
    assert_eq!(
        c["visibility"],
        serde_json::Value::Null,
        "A ne porte pas de visibilité"
    );
    assert_eq!(c["title"], "Carte 2.4.0");
}

// (b) `?version=v2.4.0` — the leading `v` is normalised, same single card as (a).
#[tokio::test]
async fn version_filter_tolerates_leading_v_prefix() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    seed_fixture(&idx).await;

    let entries = cards_json(&app, &token, "?version=v2.4.0").await;

    assert_eq!(entries.len(), 1, "préfixe v normalisé : {entries:?}");
    assert_eq!(entries[0]["id"], ID_A);
}

// (c) No `version`: the `gradatum` cards — including the interne one — sorted by F-XX,
//     without the `autre` card (project filter) and without the garbage note.
#[tokio::test]
async fn no_version_lists_gradatum_cards_including_interne_excluding_autre() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    seed_fixture(&idx).await;

    let entries = cards_json(&app, &token, "").await;

    let features: Vec<&str> = entries
        .iter()
        .map(|e| e["feature"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        features,
        vec!["F-300", "F-301", "F-302"],
        "cartes gradatum triées F-XX, sans F-303 (autre) ni F-304 (garbage) : {entries:?}"
    );
    // La carte interne (F-302) est bien listée avec son axe visibilité nommé — cet endpoint
    // n'applique aucun filtre miroir.
    assert_eq!(entries[2]["visibility"], "interne");
}

// (d) The garbage note (F-304) never appears, whatever the query.
#[tokio::test]
async fn garbage_note_never_appears() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    seed_fixture(&idx).await;

    let entries = cards_json(&app, &token, "").await;

    assert!(
        entries.iter().all(|e| e["feature"] != "F-304"),
        "la note status=garbage ne doit jamais apparaître : {entries:?}"
    );
}

// (e) Unknown version → empty list and 200, never 404.
#[tokio::test]
async fn unknown_version_returns_empty_list_and_200() {
    let (app, state, idx) = build_app(TEST_ACL).await;
    let token = sign(&state);
    seed_fixture(&idx).await;

    // 200 is asserted inside `cards_json`; the emptiness is the literal here.
    let entries = cards_json(&app, &token, "?version=9.9.9").await;

    assert!(entries.is_empty(), "version inexistante → [] : {entries:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Discriminating power — `project_map_card_index` called directly (P1-6)
// ─────────────────────────────────────────────────────────────────────────────
//
// DETTE NOMMÉE (Step 4) : le handler `list_cards` câble `project_filter` en dur à
// `Some("gradatum")` (`crates/gradatum-server/src/api_v1/project_map.rs`, l. ~408) — il
// n'existe aucun paramètre de requête pour lister les cartes d'un autre projet. Non traité
// dans ce lot. Les tests ci-dessous appellent la projection directement pour exercer, et
// discriminer, les deux filtres (version, projet) que le handler fige.

// (i) Project filter — cardinality AND content differ. `version_filter` is `None` in both:
//     the version filter applies *before* the project filter, so a non-null version here
//     would drop the `autre` card and make the test green without discriminating anything.
#[tokio::test]
async fn card_index_project_filter_discriminates() {
    let corpus = corpus_live();

    let unfiltered: Vec<ProjectMapCardEntry> = project_map_card_index(&corpus, None, None);
    let gradatum_only: Vec<ProjectMapCardEntry> =
        project_map_card_index(&corpus, None, Some("gradatum"));

    // Cardinality: the `autre` card is present unfiltered, absent under the project filter.
    assert_eq!(unfiltered.len(), 4, "sans filtre projet : les 4 cartes");
    assert_eq!(
        gradatum_only.len(),
        3,
        "filtre gradatum : la carte autre écartée"
    );
    // Content: F-303 (project:autre) appears only in the unfiltered projection.
    assert!(
        unfiltered
            .iter()
            .any(|e| e.feature.as_deref() == Some("F-303")),
        "F-303 (autre) présente sans filtre"
    );
    assert!(
        gradatum_only
            .iter()
            .all(|e| e.feature.as_deref() != Some("F-303")),
        "F-303 (autre) absente sous filtre gradatum"
    );
}

// (ii) Version filter — cardinality differs (3 gradatum cards vs the single 2.4.0 one).
#[tokio::test]
async fn card_index_version_filter_discriminates() {
    let corpus = corpus_live();

    let all_gradatum: Vec<ProjectMapCardEntry> =
        project_map_card_index(&corpus, None, Some("gradatum"));
    let only_240: Vec<ProjectMapCardEntry> =
        project_map_card_index(&corpus, Some("2.4.0"), Some("gradatum"));

    assert_eq!(all_gradatum.len(), 3, "toutes les cartes gradatum");
    assert_eq!(only_240.len(), 1, "seule la carte tracking 2.4.0");
    assert_eq!(only_240[0].feature.as_deref(), Some("F-300"));
}
