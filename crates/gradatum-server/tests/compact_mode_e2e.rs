//! E2E + serialization proof for the opt-in `compact` mode on the four read tools.
//!
//! Proves three things per endpoint:
//! 1. **Backward-compat**: a request WITHOUT `compact` returns the historical full shape
//!    (byte-for-byte unchanged — the response types are untouched, see `git diff`).
//! 2. **Compact shape**: a request WITH `compact: true` returns `{ "compact": "<text>" }`.
//! 3. **Gain**: real byte counts before/after (serde is the exact axum `Json` path, so the
//!    serialized length equals the HTTP body length to the byte).
//!
//! `search` is measured through a real HTTP round-trip because `SearchHit` is
//! `#[non_exhaustive]` (not constructible from this external test crate). The other three
//! response types are constructed directly and serialized — the same `serde_json::to_vec`
//! axum runs for `Json<T>`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_dto::{LessonHit, LessonsRecallResponse};
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::api_v1::compact::{CompactBody, render_read, render_recall, render_timeline};
use gradatum_server::api_v1::dto::VaultReadResponse;
use gradatum_server::api_v1::timeline::{TimelineItem, VaultTimelineResponse};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "compact-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-compact"
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

async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");
    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("index in-memory"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(embedder);
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
            "compact-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

fn search_req(query: &str, token: &str, compact: bool) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 20,
        "tenant_id": "main",
        "include_corpus_count": true,
        "compact": compact,
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_bytes(app: &axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

/// vault_search — real HTTP round-trip, backward-compat + compact shape + real byte gain.
#[tokio::test]
async fn search_compact_end_to_end() {
    let (app, state, idx) = build_app(Arc::new(NoopBackend)).await;
    let token = sign(&state);
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Seed 12 realistic notes matching the query (realistic titles + bodies).
    for i in 0..12 {
        let id = Ulid::new().to_string();
        idx.seed_note_with_created(
            &id,
            "reference",
            &format!(
                "# Décision d'architecture numéro {i} sur le sujet gradatum compact\n\n\
                 gradatum compact mode token projection payload budget octets réponse — \
                 ce corps représente une note réelle de taille moyenne, paragraphe {i} \
                 avec suffisamment de texte pour produire un snippet FTS5 réaliste.",
            ),
            now_ms - i * 86_400_000,
        )
        .await
        .expect("seed");
    }

    // 1. Backward-compat: without compact → full shape (`items`, no `compact`).
    let (st, full) = body_bytes(&app, search_req("gradatum compact token", &token, false)).await;
    assert_eq!(st, StatusCode::OK);
    let full_json: serde_json::Value = serde_json::from_slice(&full).unwrap();
    assert!(
        full_json.get("items").is_some(),
        "full response keeps `items`"
    );
    assert!(
        full_json.get("compact").is_none(),
        "full response has no `compact` key"
    );
    // The historical fields are all present and unchanged (response type untouched).
    let hit0 = &full_json["items"][0];
    for f in [
        "vault_id",
        "path",
        "score",
        "title",
        "snippet",
        "trust",
        "status",
        "anchor_ms",
    ] {
        assert!(
            hit0.get(f).is_some(),
            "historical field `{f}` preserved: {hit0}"
        );
    }

    // 2. Compact: with compact → { "compact": "..." }, no `items`.
    let (st2, comp) = body_bytes(&app, search_req("gradatum compact token", &token, true)).await;
    assert_eq!(st2, StatusCode::OK);
    let comp_json: serde_json::Value = serde_json::from_slice(&comp).unwrap();
    assert!(
        comp_json.get("compact").is_some(),
        "compact response has `compact`"
    );
    assert!(
        comp_json.get("items").is_none(),
        "compact response drops `items`"
    );
    let rendered = comp_json["compact"].as_str().unwrap();
    assert!(rendered.contains("notes"), "header present: {rendered}");
    assert!(
        rendered.contains("corpus_match_count"),
        "absence hint preserved: {rendered}"
    );

    // 3. Gain.
    eprintln!(
        "[MESURE] vault_search : full={} o, compact={} o, ratio={:.2}x, gain={:.1}%",
        full.len(),
        comp.len(),
        full.len() as f64 / comp.len() as f64,
        100.0 * (full.len() - comp.len()) as f64 / full.len() as f64,
    );
    assert!(comp.len() < full.len(), "compact must be smaller");
}

// ── Serialization-level measurement (real bytes = axum Json path) ─────────────

fn measure(label: &str, full_bytes: usize, compact_bytes: usize) {
    eprintln!(
        "[MESURE] {label} : full={full_bytes} o, compact={compact_bytes} o, ratio={:.2}x, gain={:.1}%",
        full_bytes as f64 / compact_bytes as f64,
        100.0 * (full_bytes as f64 - compact_bytes as f64) / full_bytes as f64,
    );
}

/// vault_read — content-bound: measure the (near-constant) gain on a realistic note.
#[test]
fn read_compact_gain_and_shape() {
    let content = "# Note de décision\n\n".to_string()
        + &"Ligne de contenu markdown réaliste avec du texte utile. ".repeat(40);
    let full = VaultReadResponse {
        path: "decisions/01KYVENY45ABCDEF01234567".to_string(),
        title: Some("Arbitrage surface publique gradatum".to_string()),
        content: content.clone(),
        metadata: Some(serde_json::json!({
            "section": "decisions",
            "author": "main-agent",
            "tags": ["surface-publique", "preview"],
            "created": "2026-07-31T10:00:00+00:00",
            "trust": 0.9,
        })),
        size_bytes: content.len() as u64,
        sha256: "a".repeat(64),
    };
    let full_bytes = serde_json::to_vec(&full).unwrap().len();
    let compact = CompactBody {
        compact: render_read(&full),
    };
    let comp_bytes = serde_json::to_vec(&compact).unwrap().len();
    // sha256 preserved (needed for in-place update).
    assert!(compact.compact.contains(&full.sha256));
    assert!(
        compact.compact.contains(&content),
        "content is kept verbatim"
    );
    measure("vault_read (note ~2 Ko)", full_bytes, comp_bytes);

    // Same measurement on a SMALL note — where the fixed saving matters most.
    let small = VaultReadResponse {
        content: "# Titre\n\nCorps court.".to_string(),
        size_bytes: 20,
        ..full
    };
    let sf = serde_json::to_vec(&small).unwrap().len();
    let sc = serde_json::to_vec(&CompactBody {
        compact: render_read(&small),
    })
    .unwrap()
    .len();
    measure("vault_read (note courte)", sf, sc);
}

/// vault_timeline — 50 rows, drop anchor_src + next_cursor.
#[test]
fn timeline_compact_gain_and_shape() {
    let items: Vec<TimelineItem> = (0..50)
        .map(|i| TimelineItem {
            note_id: Ulid::new().to_string(),
            anchor_ms: 1_753_900_000_000 + i * 3_600_000,
            anchor_src: "created".to_string(),
            doc_kind: if i % 2 == 0 { "Event" } else { "Static" }.to_string(),
            title: Some(format!(
                "Événement de timeline numéro {i} sur le train v1.0.0"
            )),
        })
        .collect();
    let full = VaultTimelineResponse {
        items,
        next_cursor: Some("01KYCURSOR".to_string()),
    };
    let full_bytes = serde_json::to_vec(&full).unwrap().len();
    let comp_bytes = serde_json::to_vec(&CompactBody {
        compact: render_timeline(&full),
    })
    .unwrap()
    .len();
    measure("vault_timeline (50 lignes)", full_bytes, comp_bytes);
    assert!(comp_bytes < full_bytes);
}

/// vault_lessons_recall — 5 lessons, drop tags + anchor_ms, snippet clamped 120.
#[test]
fn recall_compact_gain_and_shape() {
    let items: Vec<LessonHit> = (0..5)
        .map(|i| LessonHit {
            ulid: Ulid::new().to_string(),
            title: format!("Leçon apprise numéro {i} sur les déploiements irréversibles"),
            snippet: format!(
                "Snippet FTS5 réaliste {i} : un gate qui réclame une intervention sans canal \
                 d'alerte équivaut à un silence — vaut pour les gates comme pour les services, \
                 leçon transverse du 31/07 avec beaucoup de contexte additionnel ici.",
            ),
            tags: vec!["deploy".to_string(), "process-discipline".to_string()],
            anchor_ms: 1_753_900_000_000 + i * 1000,
        })
        .collect();
    let full = LessonsRecallResponse { items };
    let full_bytes = serde_json::to_vec(&full).unwrap().len();
    let comp_bytes = serde_json::to_vec(&CompactBody {
        compact: render_recall(&full, "deploy"),
    })
    .unwrap()
    .len();
    measure("vault_lessons_recall (5 leçons)", full_bytes, comp_bytes);
    assert!(comp_bytes < full_bytes);
}
