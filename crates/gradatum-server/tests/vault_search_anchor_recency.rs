//! Tests E2E vault_search — F-17 Temporal Decay (Recency on anchor_ms).
//!
//! Couvre :
//! 1. `event_note_fts_recency_uses_anchor_not_created` — note FTS Event (anchor 200j,
//!    created récent) → `recency_factor` dans scores reflète l'anchor ancien (< 0.5).
//!    Preuve : `recency_factor` est appelé avec `anchor_ms` plutôt que `created_ms`.
//! 2. `event_note_semantic_only_recency_uses_anchor` — note semantic-only Event (anchor 200j,
//!    created récent) → `recency_factor` < 0.5. Preuve de l'enrichissement anchor_ms avant
//!    scoring pour les hits semantic-only).
//! 3. `anchor_src_created_backward_compat` — note avec anchor_ms == created_ms →
//!    recency_factor identique à `recency_factor(created_ms)` pré-F-17 (ε = 1e-4).
//! 4. `no_temporal_index_fallback_no_panic` — note sans entrée temporal_index → fallback sur
//!    created_ms, pas de panique, score cohérent.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::{AnchorSrc, Index, TemporalEntry};
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "f17-tester"
read_patterns  = ["main/*", "main/main", "*/decisions", "decisions/*"]
write_patterns = []
"#;

/// Embedder déterministe NON-Noop — active la branche sémantique dans `vault_search_impl`.
///
/// Retourne `EmbedBackend::Http` (≠ Noop) pour forcer l'entrée dans la branche sémantique
/// (`logic.rs:~221`). Vecteur `[1.0, 0.0, …]` de norme 1.0 → cosine = 1.0 avec lui-même.
/// L'`embedder_id` doit correspondre à celui utilisé dans `seed_note_embedding`.
struct F17DeterministicEmbedder;

const F17_EMBEDDER_ID: &str = "det-f17-v1";
const F17_DIM: u16 = 8;

#[async_trait]
impl Embedder for F17DeterministicEmbedder {
    fn embedder_id(&self) -> &str {
        F17_EMBEDDER_ID
    }
    fn dim(&self) -> u16 {
        F17_DIM
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Vecteur unitaire fixe — norme = 1.0, cosine similarity = 1.0 avec lui-même.
        let mut v = vec![0.0f32; F17_DIM as usize];
        v[0] = 1.0;
        Ok(v)
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let v: Vec<f32> = {
            let mut x = vec![0.0f32; F17_DIM as usize];
            x[0] = 1.0;
            x
        };
        Ok(texts.iter().map(|_| v.clone()).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        // NON Noop → active la branche sémantique dans vault_search_impl.
        EmbedBackend::Http
    }
}

async fn build_app(embedder: Arc<dyn Embedder>) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL f17");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test f17 anchor_recency"),
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
            "f17-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT f17")
}

fn search_req_with_scores(token: &str, query: &str) -> Request<Body> {
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "query": query,
                "tenant_id": "main",
                "limit": 10,
                "include_scores": true
            }))
            .unwrap(),
        ))
        .unwrap()
}

/// Seeds une note FTS + entrée `temporal_index` avec `created_ms` et `anchor_ms` distincts.
///
/// `created_ms` correspond à `notes.created` (date d'ingestion).
/// `anchor_ms`  correspond à `temporal_index.anchor_ms` (ancre canonique).
/// Les deux peuvent différer — c'est le cas des notes Event (F-17).
async fn seed_fts_with_separate_anchor(
    idx: &Arc<SqliteIndex>,
    id: &str,
    body: &str,
    created_ms: i64,
    anchor_ms: i64,
    anchor_src: AnchorSrc,
) {
    idx.seed_note_with_created(id, "decisions", body, created_ms)
        .await
        .expect("seed_note_with_created (f17 fts)");

    let entry = TemporalEntry {
        note_id: id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src,
        doc_kind: "Event".to_string(),
        valid_until_ms: None,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry (f17 fts)");
}

/// Seeds une note semantic-only + `temporal_index` + embedding.
///
/// Le body ("xyzzy-f17-semantic-only-irrelevant-body") ne contient pas les tokens de la
/// query FTS ("zzqanchor17recency") → la note n'est PAS dans `bm25_hits` → `is_semantic_only`.
/// L'embedding `[1.0, 0.0, …]` correspond à ce que `F17DeterministicEmbedder` retourne
/// → cosine = 1.0 → trouvée dans `semantic_hits`.
async fn seed_semantic_only_with_anchor(
    idx: &Arc<SqliteIndex>,
    id: &str,
    created_ms: i64,
    anchor_ms: i64,
) {
    // Body distinct des tokens de query → pas de match FTS.
    idx.seed_note_with_created(
        id,
        "decisions",
        "xyzzy-f17-semantic-only-irrelevant-body",
        created_ms,
    )
    .await
    .expect("seed_note_with_created (f17 sem-only)");

    let entry = TemporalEntry {
        note_id: id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::OccurredAt,
        doc_kind: "Event".to_string(),
        valid_until_ms: None,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry (f17 sem-only)");

    // Vecteur identique à ce que F17DeterministicEmbedder retourne → cosine = 1.0.
    let mut vector = vec![0.0f32; F17_DIM as usize];
    vector[0] = 1.0;
    idx.seed_note_embedding(id, F17_EMBEDDER_ID, F17_DIM, &vector)
        .await
        .expect("seed_note_embedding (f17 sem-only)");
}

// IDs F-17 — Crockford base32 strict (26 chars, pas I/L/O/U).
// Valid chars: 0123456789ABCDEFGHJKMNPQRSTVWXYZ
const ID_F17_FTS_EVENT: &str = "01HX000000000000000F17EVNT"; // Note FTS Event (anchor ancien)
const ID_F17_SEM_EVENT: &str = "01HX000000000000000F17SEMA"; // Note semantic-only Event
const ID_F17_BC: &str = "01HX000000000000000F17BKCP"; // Backward-compat (anchor == created)
const ID_F17_NOANCH: &str = "01HX000000000000000F17NAN0"; // Fallback (pas de temporal_index)

/// Nombre de jours de décalage anchor pour les notes Event de test.
/// exp(-0.01 × 200) ≈ 0.135 — largement en dessous du seuil discriminant 0.5.
const OLD_ANCHOR_DAYS: i64 = 200;

// ── F-17 Task 2 — recency sur anchor_ms (chemin FTS) ─────────────────────────

// F17-T1 : note FTS Event (anchor 200j, created récent) → recency_factor reflète anchor_ms.
//
// Avant F-17 : `recency_factor(created_ms=now, now)` ≈ 1.0 → assertion `< 0.5` échoue (rouge).
// Après F-17 : `recency_factor(anchor_ms=200j, now)` ≈ 0.135 → assertion `< 0.5` passe (vert).
#[tokio::test]
async fn event_note_fts_recency_uses_anchor_not_created() {
    let (app, state, idx) = build_app(Arc::new(F17DeterministicEmbedder)).await;
    let token = sign(&state);

    let now_ms = chrono::Utc::now().timestamp_millis();
    let old_ms = now_ms - OLD_ANCHOR_DAYS * 86_400_000i64;

    // Note Event : created = maintenant (récent), anchor_ms = 200 jours (ancien).
    seed_fts_with_separate_anchor(
        &idx,
        ID_F17_FTS_EVENT,
        "zzqanchor17recency",
        now_ms, // created_ms : récent
        old_ms, // anchor_ms  : 200 jours
        AnchorSrc::OccurredAt,
    )
    .await;

    let req = search_req_with_scores(&token, "zzqanchor17recency");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "F17-T1: status non-200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("F17-T1: items absent");

    let hit = items
        .iter()
        .find(|it| {
            it["path"]
                .as_str()
                .is_some_and(|p| p.contains(ID_F17_FTS_EVENT))
        })
        .unwrap_or_else(|| panic!("F17-T1: note FTS Event non trouvée. json={json}"));

    let scores = hit
        .get("scores")
        .unwrap_or_else(|| panic!("F17-T1: champ `scores` absent. hit={hit}"));
    let recency = scores["recency_factor"]
        .as_f64()
        .unwrap_or_else(|| panic!("F17-T1: recency_factor non-f64. scores={scores}"));

    // Seuil discriminant : 0.5 (largement entre 0.135 et 1.0).
    // Si ≈ 1.0 → recency utilise encore created_ms=now (Task 2 non implémenté).
    // Si ≈ 0.135 → recency utilise anchor_ms=200j (F-17 actif).
    assert!(
        recency < 0.5,
        "F17-T1: recency_factor doit refléter anchor_ms=200j (< 0.5), got {recency:.4}. \
         Attendu ≈ 0.135 (exp(-0.01×200)). Si ≈ 1.0 → Task 2 non implémenté. hit={hit}"
    );
}

// ── F-17 Task 1 — enrichissement anchor_ms semantic-only avant scoring ────────

// F17-T2 : note semantic-only Event (anchor 200j, created récent) → recency reflète anchor.
//
// Avant Task 1 : `hit.anchor_ms = None` au moment du scoring → fallback `created_ms=now`
//               → recency ≈ 1.0 → assertion `< 0.5` échoue (rouge).
// Après Task 1+2 : `hit.anchor_ms = old_ms` enrichi avant scoring → recency ≈ 0.135 (vert).
//
// Prouve (a) que l'enrichissement sémantique avant scoring est actif (Task 1)
// et (b) que le hit est bien semantic-only (`sem_rank` présent, `bm25_rank` absent).
#[tokio::test]
async fn event_note_semantic_only_recency_uses_anchor() {
    let (app, state, idx) = build_app(Arc::new(F17DeterministicEmbedder)).await;
    let token = sign(&state);

    let now_ms = chrono::Utc::now().timestamp_millis();
    let old_ms = now_ms - OLD_ANCHOR_DAYS * 86_400_000i64;

    // Note semantic-only : body ≠ query FTS → pas dans bm25_hits.
    // created_ms = maintenant (récent), anchor_ms = 200 jours (ancien).
    seed_semantic_only_with_anchor(&idx, ID_F17_SEM_EVENT, now_ms, old_ms).await;

    let req = search_req_with_scores(&token, "zzqanchor17recency");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "F17-T2: status non-200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("F17-T2: items absent");

    let hit = items
        .iter()
        .find(|it| {
            it["path"]
                .as_str()
                .is_some_and(|p| p.contains(ID_F17_SEM_EVENT))
        })
        .unwrap_or_else(|| panic!("F17-T2: note semantic-only non trouvée. json={json}"));

    let scores = hit
        .get("scores")
        .unwrap_or_else(|| panic!("F17-T2: champ `scores` absent. hit={hit}"));
    let recency = scores["recency_factor"]
        .as_f64()
        .unwrap_or_else(|| panic!("F17-T2: recency_factor non-f64. scores={scores}"));

    // Prouve Task 1 : anchor_ms enrichi AVANT le scoring pour les hits semantic-only.
    // Si ≈ 1.0 → anchor_ms non enrichi avant scoring (Task 1 absent → fallback created_ms=now).
    // Si ≈ 0.135 → anchor_ms=200j bien utilisé (Task 1 + Task 2 actifs).
    assert!(
        recency < 0.5,
        "F17-T2: recency_factor doit refléter anchor_ms=200j pour semantic-only (< 0.5), \
         got {recency:.4}. Si ≈ 1.0 → Task 1 manquant (anchor_ms non enrichi avant scoring). \
         hit={hit}"
    );

    // Prouve que le hit est bien semantic-only : sem_rank présent, bm25_rank absent.
    // Le body "xyzzy-f17-semantic-only-irrelevant-body" ne matche pas "zzqanchor17recency".
    let bm25_rank_absent = scores.get("bm25_rank").and_then(|v| v.as_u64()).is_none();
    assert!(
        bm25_rank_absent,
        "F17-T2: note semantic-only ne doit pas avoir bm25_rank (body ≠ query FTS). scores={scores}"
    );
    let sem_rank_present = scores.get("sem_rank").and_then(|v| v.as_u64()).is_some();
    assert!(
        sem_rank_present,
        "F17-T2: note semantic-only doit avoir sem_rank (trouvée via embedding). scores={scores}"
    );
}

// ── F-17 Task 3 — Backward-compat + Fallback ─────────────────────────────────

// F17-T3 : note anchor_src=Created (anchor_ms == created_ms) → recency identique à pré-F-17.
//
// Si anchor_ms == created_ms, `recency_factor(anchor_ms, now)` = `recency_factor(created_ms, now)`
// → score inchangé. Garantit la backward-compatibility pour la majorité des notes.
#[tokio::test]
async fn anchor_src_created_backward_compat() {
    let (app, state, idx) = build_app(Arc::new(F17DeterministicEmbedder)).await;
    let token = sign(&state);

    let now_ms = chrono::Utc::now().timestamp_millis();
    // anchor_ms == created_ms = 50 jours — assez vieux pour recency visible (≠ 1.0), ≈ 0.606.
    let ts_ms = now_ms - 50 * 86_400_000i64;

    // Note avec anchor_ms == created_ms (source = Created).
    seed_fts_with_separate_anchor(
        &idx,
        ID_F17_BC,
        "zzqanchor17recencybc",
        ts_ms, // created_ms
        ts_ms, // anchor_ms == created_ms
        AnchorSrc::Created,
    )
    .await;

    let req = search_req_with_scores(&token, "zzqanchor17recencybc");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "F17-T3: status non-200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("F17-T3: items absent");

    let hit = items
        .iter()
        .find(|it| it["path"].as_str().is_some_and(|p| p.contains(ID_F17_BC)))
        .unwrap_or_else(|| panic!("F17-T3: note backward-compat non trouvée. json={json}"));

    let scores = hit
        .get("scores")
        .unwrap_or_else(|| panic!("F17-T3: champ `scores` absent. hit={hit}"));
    let actual_recency = scores["recency_factor"]
        .as_f64()
        .unwrap_or_else(|| panic!("F17-T3: recency_factor non-f64. scores={scores}"));

    // Calcul attendu : recency_factor(ts_ms, now_ms_test).
    // now_ms_server ≈ now_ms_test (quelques ms de diff) → erreur < 1e-8, tolérance 1e-4.
    let days_old = (now_ms - ts_ms).max(0) as f64 / 86_400_000.0;
    let expected_recency = (-0.01 * days_old).exp();

    assert!(
        (actual_recency - expected_recency).abs() < 1e-4,
        "F17-T3 backward-compat: recency_factor={actual_recency:.6} doit égaler \
         recency_factor(created_ms)={expected_recency:.6} à ε=1e-4. hit={hit}"
    );
}

// F17-T4 : note sans entrée temporal_index → fallback sur created_ms, pas de panique.
//
// `hit.anchor_ms` = None → `anchor_for_recency = created_ms` → recency normal.
// Vérifie : pas de panique, recency cohérent avec la formule appliquée sur created_ms.
#[tokio::test]
async fn no_temporal_index_fallback_no_panic() {
    let (app, state, idx) = build_app(Arc::new(F17DeterministicEmbedder)).await;
    let token = sign(&state);

    let now_ms = chrono::Utc::now().timestamp_millis();
    // Note créée il y a 30 jours, SANS entrée temporal_index.
    let created_ms = now_ms - 30 * 86_400_000i64;

    // Seed uniquement dans notes + FTS (pas de temporal_index → anchor_ms = None).
    idx.seed_note_with_created(
        ID_F17_NOANCH,
        "decisions",
        "zzqanchor17recencyfallback",
        created_ms,
    )
    .await
    .expect("seed_note_with_created (f17 no-anchor)");

    let req = search_req_with_scores(&token, "zzqanchor17recencyfallback");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "F17-T4: status non-200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("F17-T4: items absent");

    let hit = items
        .iter()
        .find(|it| {
            it["path"]
                .as_str()
                .is_some_and(|p| p.contains(ID_F17_NOANCH))
        })
        .unwrap_or_else(|| panic!("F17-T4: note fallback non trouvée. json={json}"));

    let scores = hit
        .get("scores")
        .unwrap_or_else(|| panic!("F17-T4: champ `scores` absent. hit={hit}"));
    let recency = scores["recency_factor"]
        .as_f64()
        .unwrap_or_else(|| panic!("F17-T4: recency_factor non-f64. scores={scores}"));

    // Fallback sur created_ms = il y a 30 jours → exp(-0.01×30) ≈ 0.741.
    // Vérification : recency dans (0, 1) et proche de la formule (tolérance 0.05).
    assert!(
        recency > 0.0 && recency < 1.0,
        "F17-T4 fallback: recency_factor doit être dans (0, 1), got {recency:.4}. hit={hit}"
    );
    let expected = (-0.01_f64 * 30.0).exp(); // ≈ 0.741
    assert!(
        (recency - expected).abs() < 0.05,
        "F17-T4 fallback: recency ≈ exp(-0.01×30)={expected:.4}, got {recency:.4}. hit={hit}"
    );
}
