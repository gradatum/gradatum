//! Tests d'intégration — incréments d'usage per-note recall + lessons (F-110).
//!
//! Couvre :
//! - `proactive_recall` → +`recall-surfaced` par note surfacée (post-ACL).
//! - `proactive_recall_feedback` 2 acceptés → 2 `recall-accepted`.
//! - feedback liste vide → aucun incrément (réponse valide F-46).
//! - feedback sur-ensemble (accepted ⊄ surfaced) → 400, aucun incrément (garde AVANT record).
//! - `GET /lessons/recall` → +`search-hit` par leçon retournée.
//!
//! Les orchestrateurs recall sont appelés directement (pattern `proactive_recall.rs`) ;
//! l'accumulateur (`state.note_usage_accumulators`) est inspecté via `swap()`.

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::error::GradatumError;
use gradatum_core::index::Index;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{ProactiveHit, ProactiveRecallFeedbackRequest, ProactiveRecallRequest};
use gradatum_embed::Noop as NoopEmbedder;
use gradatum_index::SqliteIndex;
use gradatum_server::note_usage_store::{
    KIND_RECALL_ACCEPTED, KIND_RECALL_SURFACED, KIND_SEARCH_HIT,
};
use gradatum_server::proactive_recall::{proactive_recall, proactive_recall_feedback};
use gradatum_server::proactive_recall_store::ProactiveRecallStore;
use gradatum_server::proactive_surface_store::ProactiveSurfaceStore;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const ULID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const ULID_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const ULID_C: &str = "01C9SDEHDR3A8V4RRFFQ69G5FB";

const ACL_MAIN_ALL: &str = r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#;

/// `AppState` réel avec ACL + stores surface/session branchés sur le même index.db.
async fn build_state(acl_preset: &str) -> (AppState, TempDir) {
    let tmp = TempDir::new().expect("TempDir");
    let index_path = tmp.path().join("index.db");

    let idx = Arc::new(
        SqliteIndex::open(&index_path)
            .await
            .expect("SqliteIndex::open"),
    );
    let surface_store = ProactiveSurfaceStore::open(&index_path)
        .await
        .expect("ProactiveSurfaceStore::open");
    let recall_store = ProactiveRecallStore::open(&index_path)
        .await
        .expect("ProactiveRecallStore::open");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(acl_preset).expect("AclEngine");

    let mut state = AppState::with_jwt_and_acl(jwt, acl);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.proactive_surface = Some(surface_store);
    state.proactive_recall = Some(recall_store);
    (state, tmp)
}

fn bearer_main() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "agent".into(),
        scopes: vec!["read".into()],
        tenant_id: "main".into(),
        jti: None,
    }
}

fn hit(ulid: &str, section: &str) -> ProactiveHit {
    ProactiveHit {
        ulid: ulid.into(),
        title: format!("titre {ulid}"),
        section: section.into(),
        snippet: String::new(),
        score: 1.0,
    }
}

fn req_feedback(recall_id: &str, accepted: &[&str]) -> ProactiveRecallFeedbackRequest {
    ProactiveRecallFeedbackRequest {
        tenant_id: Some("main".into()),
        recall_id: recall_id.into(),
        accepted_ulids: accepted.iter().map(|s| (*s).to_string()).collect(),
    }
}

async fn seed_session(state: &AppState, recall_id: &str, surfaced: &[String]) {
    state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .insert_session(recall_id, "main", "proactive", surfaced, 1_000)
        .await
        .expect("insert_session");
}

fn count_kind(
    batch: &std::collections::HashMap<(String, String, String), (u64, i64)>,
    kind: &str,
) -> usize {
    batch.iter().filter(|((_, _, k), _)| k == kind).count()
}

#[tokio::test]
async fn proactive_recall_increments_surfaced_per_note() {
    let (state, _tmp) = build_state(ACL_MAIN_ALL).await;
    let surface = vec![
        hit(ULID_A, "lessons-learned"),
        hit(ULID_B, "reasoning"),
        hit(ULID_C, "reference"),
    ];
    state
        .proactive_surface
        .as_ref()
        .expect("surface store")
        .upsert_surface("main", &surface, 1_000)
        .await
        .expect("upsert_surface");

    let resp = proactive_recall(
        &state,
        &bearer_main(),
        ProactiveRecallRequest {
            tenant_id: Some("main".into()),
            context: None,
            sections: None,
            limit: None,
        },
    )
    .await
    .expect("proactive_recall");
    assert_eq!(resp.items.len(), 3);

    let batch = state.note_usage_accumulators.swap();
    assert_eq!(
        count_kind(&batch, KIND_RECALL_SURFACED),
        3,
        "3 recall-surfaced (1 par note surfacée). batch={batch:?}"
    );
    for note_id in [ULID_A, ULID_B, ULID_C] {
        let (count, _) = batch
            .get(&("main".into(), note_id.into(), KIND_RECALL_SURFACED.into()))
            .copied()
            .unwrap_or_else(|| panic!("recall-surfaced manquant pour {note_id}"));
        assert_eq!(count, 1, "note {note_id} surfacée une fois");
    }
}

#[tokio::test]
async fn feedback_2_accepted_increments_2() {
    let (state, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(
        &state,
        "recall-fb-1",
        &[ULID_A.to_string(), ULID_B.to_string()],
    )
    .await;

    proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-1", &[ULID_A, ULID_B]),
    )
    .await
    .expect("accepted ⊆ surfaced doit réussir");

    let batch = state.note_usage_accumulators.swap();
    assert_eq!(
        count_kind(&batch, KIND_RECALL_ACCEPTED),
        2,
        "2 recall-accepted"
    );
    for note_id in [ULID_A, ULID_B] {
        let (count, _) = batch
            .get(&("main".into(), note_id.into(), KIND_RECALL_ACCEPTED.into()))
            .copied()
            .unwrap_or_else(|| panic!("recall-accepted manquant pour {note_id}"));
        assert_eq!(count, 1);
    }
}

#[tokio::test]
async fn feedback_empty_list_increments_nothing() {
    let (state, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(&state, "recall-fb-2", &[ULID_A.to_string()]).await;

    // Liste vide = réponse valide F-46 (feedback « rien accepté »).
    proactive_recall_feedback(&state, &bearer_main(), req_feedback("recall-fb-2", &[]))
        .await
        .expect("liste vide doit réussir");

    let batch = state.note_usage_accumulators.swap();
    assert!(
        batch.is_empty(),
        "aucun incrément pour un feedback vide. batch={batch:?}"
    );
}

#[tokio::test]
async fn feedback_rejected_400_superset_increments_nothing() {
    let (state, _tmp) = build_state(ACL_MAIN_ALL).await;
    // surfaced = {A} seulement ; accepted = {A, B} → B ⊄ surfaced → InvalidInput.
    seed_session(&state, "recall-fb-3", &[ULID_A.to_string()]).await;

    let err = proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-3", &[ULID_A, ULID_B]),
    )
    .await
    .expect_err("accepted ⊄ surfaced doit être rejeté");
    assert!(
        matches!(err, GradatumError::InvalidInput(_)),
        "attendu InvalidInput, obtenu {err:?}"
    );

    let batch = state.note_usage_accumulators.swap();
    assert!(
        batch.is_empty(),
        "garde ⊄ surfaced passe AVANT record → aucun incrément. batch={batch:?}"
    );
}

// ── Lessons (via HTTP handler — l'incrément vit dans le .map du handler) ──────

const LESSONS_ACL: &str = r#"
[[consumer]]
identity = "lesson-tester"
read_patterns  = ["main/lessons-learned", "main/*", "main/main"]
write_patterns = []
"#;

#[tokio::test]
async fn lessons_recall_increments_search_hit() {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(LESSONS_ACL).expect("preset ACL lessons");
    let idx = Arc::new(SqliteIndex::open_in_memory().await.expect("open_in_memory"));
    let mut state =
        AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopEmbedder::new(8)));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    // Leçon taguée `deploy` (match par tag, section lessons-learned).
    let lesson_id = "01KAAAAAAAAAAAAAAAAAAAAAAA";
    idx.seed_lesson(
        lesson_id,
        "Cutover discipline",
        "deploy release",
        "Toujours health-check avant le basculement.",
        1_700_000_000_000,
    )
    .await
    .expect("seed_lesson");

    let token = state
        .jwt
        .sign(
            "lesson-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    let req = Request::builder()
        .uri("/api/v1/lessons/recall?class=deploy")
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.expect("lessons recall oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !json["items"].as_array().expect("items").is_empty(),
        "au moins une leçon retournée. body={json}"
    );

    let batch = state.note_usage_accumulators.swap();
    assert_eq!(
        count_kind(&batch, KIND_SEARCH_HIT),
        1,
        "1 search-hit sur la leçon retournée. batch={batch:?}"
    );
    let (count, _) = batch
        .get(&("main".into(), lesson_id.into(), KIND_SEARCH_HIT.into()))
        .copied()
        .expect("search-hit sur la leçon");
    assert_eq!(count, 1);
}
