//! Tests E2E F-60 L2 — `GET /api/v1/lessons/recall`.
//!
//! Couvre :
//! 1. `recall_returns_lessons_by_class` — recall par classe, payload conforme
//!    (ulid/title/snippet/tags/anchor_ms), section restreinte à lessons-learned.
//! 2. `recall_excludes_codified` — leçon taguée `codified` jamais retournée.
//! 3. `recall_invalid_class_400` — classe hors vocabulaire → 400.
//! 4. `recall_unauthenticated_401` — sans JWT → 401.
//! 5. `recall_default_limit_5` — sans `limit`, défaut 5 appliqué.
//! 6. `recall_latency_under_50ms` — assert latence < 50 ms sur fixtures.
//!
//! ## Task 3 — F-68 semantic opt-in
//!
//! 7. `hydrate_lessons_by_ulids_returns_tags_anchor` — hydratation directe par ULID,
//!    retourne tags + anchor_ms.
//! 8. `recall_semantic_finds_paraphrase` — `semantic=true` + FakeEmbedder : note
//!    trouvée via cosine même si BM25 la rate sur la requête libre.
//! 9. `recall_semantic_preserves_codified_exclusion` — leçon `codified` exclue en mode sémantique.
//! 10. `recall_semantic_filters_class` — note dont le tag ≠ class écartée post-hydratation.

// Helpers partagés (FakeEmbedder, build_app_with_embedder, sign_token, TestEnv, etc.).
// Chargé en path explicite — convention identique à context_assembly.rs.
#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_embed::{Embedder, Noop as NoopEmbedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Preset ACL autorisant `lesson-tester` en lecture sur la section lessons-learned.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "lesson-tester"
read_patterns  = ["main/lessons-learned", "main/*", "main/main"]
write_patterns = []
"#;

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL — invariant test");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory()"),
    );

    let noop = Arc::new(NoopEmbedder::new(8));
    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(noop);
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
            "lesson-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT — invariant test")
}

fn recall_req(query: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(format!("/api/v1/lessons/recall?{query}"))
        .method("GET");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Test 1 : recall par classe → payload conforme, restreint à lessons-learned.
#[tokio::test]
async fn recall_returns_lessons_by_class() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Leçon taguée `deploy` (le mot n'est PAS dans le corps → match par tag).
    idx.seed_lesson(
        "01KAAAAAAAAAAAAAAAAAAAAAAA",
        "Cutover discipline",
        "deploy release",
        "Toujours health-check avant le basculement.",
        1_700_000_000_000,
    )
    .await
    .expect("seed deploy lesson");

    // Note d'une autre section avec "deploy" dans le corps → exclue par la section.
    idx.seed_note_with_fts("01KBBBBBBBBBBBBBBBBBBBBBBB", "debug", "deploy crashed")
        .await
        .expect("seed debug note");

    let resp = app
        .oneshot(recall_req("class=deploy&limit=5", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP 200 attendu");

    let json = body_json(resp).await;
    let items = json["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "seule la leçon lessons-learned doit matcher"
    );

    let it = &items[0];
    assert_eq!(it["ulid"], "01KAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(it["title"], "Cutover discipline");
    assert_eq!(it["anchor_ms"], 1_700_000_000_000_i64);
    let tags: Vec<String> = it["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["deploy".to_string(), "release".to_string()]);
    assert!(
        !it["snippet"].as_str().unwrap().is_empty(),
        "snippet non vide attendu"
    );
}

/// Test 2 : leçon `codified` exclue du recall.
#[tokio::test]
async fn recall_excludes_codified() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    idx.seed_lesson(
        "01KCAAAAAAAAAAAAAAAAAAAAAA",
        "Migration active",
        "migration",
        "Ne jamais modifier une migration appliquée.",
        1_700_000_000_000,
    )
    .await
    .expect("seed active");
    idx.seed_lesson(
        "01KCBBBBBBBBBBBBBBBBBBBBBB",
        "Migration codifiée",
        "migration codified",
        "Leçon déjà intégrée migration.",
        1_700_000_001_000,
    )
    .await
    .expect("seed codified");

    let resp = app
        .oneshot(recall_req("class=migration", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let ulids: Vec<String> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["ulid"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ulids.contains(&"01KCAAAAAAAAAAAAAAAAAAAAAA".to_string()),
        "leçon active présente. ulids={ulids:?}"
    );
    assert!(
        !ulids.contains(&"01KCBBBBBBBBBBBBBBBBBBBBBB".to_string()),
        "leçon codified exclue. ulids={ulids:?}"
    );
}

/// Test 3 : classe hors vocabulaire → 400.
#[tokio::test]
async fn recall_invalid_class_400() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);

    let resp = app
        .oneshot(recall_req("class=not_a_real_class", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "classe inconnue → 400"
    );
}

/// Test 3b : tentative d'injection FTS via class → 400 (vocabulaire fermé).
#[tokio::test]
async fn recall_injection_attempt_400() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);

    // class=deploy OR release — encodé URL → rejeté car != valeur littérale du vocabulaire.
    let resp = app
        .oneshot(recall_req("class=deploy%20OR%20release", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "injection FTS rejetée par validation vocabulaire"
    );
}

/// Test 4 : sans JWT → 401.
#[tokio::test]
async fn recall_unauthenticated_401() {
    let (app, _state, _idx) = build_app().await;

    let resp = app.oneshot(recall_req("class=deploy", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "sans token → 401");
}

/// Test 5 : sans `limit`, défaut 5 appliqué (6 leçons seedées → 5 retournées).
#[tokio::test]
async fn recall_default_limit_5() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    for i in 0..6u8 {
        let id = format!("01KDDDDDDDDDDDDDDDDDDDDDD{i}");
        idx.seed_lesson(
            &id,
            &format!("Leçon archi {i}"),
            "archi",
            "Décision d'architecture documentée.",
            1_700_000_000_000 + i64::from(i),
        )
        .await
        .expect("seed loop");
    }

    let resp = app
        .oneshot(recall_req("class=archi", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        5,
        "défaut limit=5 attendu, got {}",
        items.len()
    );
}

// ── Task 2 — F-68 recency ranking ─────────────────────────────────────────────

/// Task 2 — F-68 : `rank=recency-boosted` met la leçon la plus fraîche en tête.
///
/// 2 leçons, même classe `deploy`, corpus identique (scores BM25 quasi-identiques).
/// Leçon A : `anchor_ms = 0` (epoch Unix, très ancienne — `recency_factor` ≈ 0).
/// Leçon B : `anchor_ms = now` (très fraîche — `recency_factor` = 1.0).
/// Avec `rank=recency-boosted`, B doit être en première position.
#[tokio::test]
async fn recall_rank_recency_boosted_surfaces_fresh() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Leçon ancienne — epoch 0 → recency_factor ≈ 0 (≈ 56 ans de decay λ=0.01)
    idx.seed_lesson(
        "01KRANKAAAAAAAAAAAAAAAAAAA",
        "Deploy Ancienne",
        "deploy",
        "Deploy rollout health-check discipline procedure.",
        0, // anchor_ms = epoch Unix 0
    )
    .await
    .expect("seed ancienne Task2");

    // Leçon fraîche — anchor_ms = now → recency_factor = 1.0
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_lesson(
        "01KRANKBBBBBBBBBBBBBBBBBBB",
        "Deploy Recente",
        "deploy",
        "Deploy rollout health-check discipline procedure.", // corpus identique → BM25 quasi-identique
        now_ms,
    )
    .await
    .expect("seed recente Task2");

    let resp = app
        .oneshot(recall_req(
            "class=deploy&limit=5&rank=recency-boosted",
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["items"].as_array().expect("items array Task2");
    assert_eq!(items.len(), 2, "2 leçons attendues Task2");

    // La leçon fraîche doit être en tête malgré un rang BM25 identique (ou légèrement inférieur).
    assert_eq!(
        items[0]["ulid"], "01KRANKBBBBBBBBBBBBBBBBBBB",
        "la leçon récente doit être en tête avec rank=recency-boosted"
    );
    assert_eq!(
        items[1]["ulid"], "01KRANKAAAAAAAAAAAAAAAAAAA",
        "la leçon ancienne doit être en second avec rank=recency-boosted"
    );
}

/// Task 2 — F-68 parité : `rank` absent == `rank=relevance` == ordre BM25 inchangé.
///
/// Rétro-compat BLOQUANTE : le hook LIVE `lesson-recall.sh` (F-60) appelle
/// `/lessons/recall` sans le champ `rank`. Son comportement ne doit pas changer.
///
/// Ce test vérifie que :
/// 1. `rank` absent → 200 + résultats BM25 normaux (pas d'erreur 400 unexpected field).
/// 2. `rank=relevance` → résultat bit-pour-bit identique à `rank` absent.
#[tokio::test]
async fn recall_rank_default_is_legacy_order() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Leçon ancienne (epoch 0)
    idx.seed_lesson(
        "01KRANKCCCCCCCCCCCCCCCCCC0",
        "Git Hygiene Legacy",
        "git-hygiene",
        "git-hygiene commit message discipline.",
        0,
    )
    .await
    .expect("seed legacy git-hygiene Task2");

    // Leçon récente — anchor très récent
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_lesson(
        "01KRANKDDDDDDDDDDDDDDDDDD0",
        "Git Hygiene Recente",
        "git-hygiene",
        "git-hygiene commit message discipline.", // corpus identique
        now_ms,
    )
    .await
    .expect("seed recent git-hygiene Task2");

    // Requête sans rank (comportement legacy)
    let resp_default = app
        .clone()
        .oneshot(recall_req("class=git-hygiene&limit=5", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp_default.status(),
        StatusCode::OK,
        "sans rank : 200 attendu"
    );
    let json_default = body_json(resp_default).await;
    let items_default = json_default["items"].as_array().expect("items default");

    // Requête avec rank=relevance (doit être un no-op strict)
    let resp_rel = app
        .oneshot(recall_req(
            "class=git-hygiene&limit=5&rank=relevance",
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp_rel.status(),
        StatusCode::OK,
        "rank=relevance : 200 attendu"
    );
    let json_rel = body_json(resp_rel).await;
    let items_rel = json_rel["items"].as_array().expect("items relevance");

    // Parité count
    assert_eq!(
        items_default.len(),
        items_rel.len(),
        "rank absent et rank=relevance doivent retourner le même nombre de leçons"
    );

    // Parité ordre bit-pour-bit
    let ulids_default: Vec<&str> = items_default
        .iter()
        .map(|i| i["ulid"].as_str().unwrap())
        .collect();
    let ulids_rel: Vec<&str> = items_rel
        .iter()
        .map(|i| i["ulid"].as_str().unwrap())
        .collect();
    assert_eq!(
        ulids_default, ulids_rel,
        "rank absent et rank=relevance doivent produire le même ordre (parité rétro-compat)"
    );
}

// ── Tests L2 originaux ─────────────────────────────────────────────────────────

/// Test 6 : latence < 50 ms sur fixtures (assert perf du contrat L2).
///
/// In-memory SQLite, ~20 leçons — le chemin BM25-only doit rester très en deçà
/// de la cible 50 ms. Marge large pour absorber la variance CI.
#[tokio::test]
async fn recall_latency_under_50ms() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    for i in 0..20u8 {
        let id = format!("01KEEEEEEEEEEEEEEEEEEEEE{:02}", i);
        idx.seed_lesson(
            &id,
            &format!("Leçon ci-cd {i}"),
            "ci-cd",
            "Pipeline runner discipline et isolation des jobs.",
            1_700_000_000_000 + i64::from(i),
        )
        .await
        .expect("seed loop");
    }

    let start = std::time::Instant::now();
    let resp = app
        .oneshot(recall_req("class=ci-cd&limit=5", Some(&token)))
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        elapsed.as_millis() < 50,
        "recall doit être < 50ms, mesuré {} ms",
        elapsed.as_millis()
    );
}

// ── Task 3 — F-68 semantic opt-in ─────────────────────────────────────────────
//
// Ces tests requièrent:
// - `hydrate_lessons_by_ulids` sur le trait IndexStore (avec default impl safe pour les mocks).
// - `semantic: Option<bool>` + `query: Option<String>` dans `LessonsRecallRequest`.
// - Chemin sémantique dans `lessons_recall_impl` : retrieve_candidates → hydrate → filtre.

/// Task 3 — Hydratation directe par ULIDs : retourne tags, anchor_ms, title.
///
/// Vérifie que `hydrate_lessons_by_ulids` retourne les métadonnées complètes d'une
/// leçon depuis l'index SQLite (pas via FTS/BM25 — lookup pur par id).
///
/// Ce test est purement unitaire (SqliteIndex::open_in_memory) — pas de stack HTTP.
#[tokio::test]
async fn hydrate_lessons_by_ulids_returns_tags_anchor() {
    use gradatum_core::index::Index;
    use gradatum_core::scope::VaultId;
    use gradatum_index::SqliteIndex;

    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("SqliteIndex::open_in_memory — invariant test");

    let ulid_a = "01KSEMANTAAAAAAAAAAAAAAAAA";
    idx.seed_lesson(
        ulid_a,
        "Hydrate Test Lesson",
        "deploy release",
        "Corps de la leçon deploy rollout.",
        1_700_100_000_000,
    )
    .await
    .expect("seed lesson hydrate test");

    let vault_id = VaultId::new("main");
    // Test via trait (Arc<dyn Index>) pour vérifier que la default impl est bridgée.
    let idx_arc = Arc::new(idx) as Arc<dyn Index>;

    let hits = idx_arc
        .hydrate_lessons_by_ulids(&vault_id, &[ulid_a])
        .await
        .expect("hydrate_lessons_by_ulids");

    assert_eq!(hits.len(), 1, "1 hit attendu pour 1 ULID seedé");

    let h = &hits[0];
    assert_eq!(
        h.note_id.to_string(),
        ulid_a,
        "note_id doit correspondre à l'ULID seedé"
    );
    assert_eq!(
        h.title,
        Some("Hydrate Test Lesson".to_string()),
        "titre attendu"
    );
    assert_eq!(
        h.tags,
        vec!["deploy".to_string(), "release".to_string()],
        "tags attendus (space-split)"
    );
    assert_eq!(h.anchor_ms, 1_700_100_000_000, "anchor_ms = created_ms");
    assert!(
        !h.snippet.is_empty(),
        "snippet doit être non-vide (extrait body_text)"
    );

    // ULID inconnu → retourne vide (pas d'erreur).
    let hits_empty = idx_arc
        .hydrate_lessons_by_ulids(&vault_id, &["01KZZZZZZZZZZZZZZZZZZZZZZZZ"])
        .await
        .expect("hydrate ULID inconnu");
    assert!(
        hits_empty.is_empty(),
        "ULID inconnu → résultat vide, pas d'erreur"
    );

    // Slice vide → retourne vide (guard early return).
    let hits_none = idx_arc
        .hydrate_lessons_by_ulids(&vault_id, &[])
        .await
        .expect("hydrate slice vide");
    assert!(hits_none.is_empty(), "slice vide → résultat vide");
}

/// Task 3 — `semantic=true` + FakeEmbedder : note trouvée via cosine même si BM25
/// seul (sur la requête libre) ne la trouverait pas.
///
/// ## Scénario
///
/// - Leçon L1: body="alpha-nonce-unique" (ne matche pas la query "beta-paraphrase-T3"),
///   embedding = FakeEmbedder.embed("beta-paraphrase-T3") → cosine=1.0 avec la requête.
/// - Leçon L2: body="beta-paraphrase-T3 deploy rollout", embedding propre. Trouvée BM25+sem.
/// - `semantic=true, query="beta-paraphrase-T3", class="deploy"` :
///   BM25 de retrieve_candidates rate L1 (body ne matche pas), sémantique la trouve → L1 incluse.
/// - `semantic=false` (défaut) : `recall_lessons("deploy")` → L1 et L2 trouvées via tag.
///
/// Assertion clé : L1 est dans le résultat `semantic=true` (chemin sémantique contribue).
#[tokio::test]
async fn recall_semantic_finds_paraphrase() {
    use gradatum_embed::EmbedBackend;

    let fake_emb = Arc::new(helpers::FakeEmbedder { dim: 8 });
    let env = helpers::build_app_with_embedder(
        Arc::clone(&fake_emb) as Arc<dyn gradatum_embed::Embedder>
    )
    .await;
    // Vérifier que le chemin sémantique sera activé (backend_kind != Noop).
    assert_ne!(
        env.state.embedder.backend_kind(),
        EmbedBackend::Noop,
        "FakeEmbedder doit être non-Noop"
    );

    let token = helpers::sign_token(&env.state);
    let idx = env._vault_typed.index();

    // L1 : corps ne contient PAS "beta-paraphrase-T3" → BM25 de retrieve_candidates rate.
    // Embedding = embed("beta-paraphrase-T3") → cosine=1.0 avec la requête sémantique.
    let ulid_l1 = "01KSEMANTJ1AAAAAAAAAAAAAAA";
    idx.seed_lesson(
        ulid_l1,
        "Leçon Deploy Alpha Nonce",
        "deploy",
        "alpha-nonce-unique rollout discipline smoke-test",
        1_700_200_000_000,
    )
    .await
    .expect("seed L1");

    // Calculer l'embedding de la requête via FakeEmbedder et le stocker pour L1.
    // Cela garantit cosine(L1.emb, embed(query)) = 1.0 → L1 trouvée via sémantique.
    let query_vec = fake_emb
        .embed("beta-paraphrase-T3")
        .await
        .expect("embed query pour L1");
    idx.seed_note_embedding(ulid_l1, fake_emb.embedder_id(), fake_emb.dim(), &query_vec)
        .await
        .expect("seed embedding L1");

    // L2 : corps contient la query → trouvée via BM25 + sémantique.
    let ulid_l2 = "01KSEMANTJ2AAAAAAAAAAAAAAA";
    idx.seed_lesson(
        ulid_l2,
        "Leçon Deploy Beta",
        "deploy",
        "beta-paraphrase-T3 deploy rollout discipline",
        1_700_200_001_000,
    )
    .await
    .expect("seed L2");
    let vec_l2 = fake_emb
        .embed("beta-paraphrase-T3 deploy rollout discipline")
        .await
        .expect("embed L2");
    idx.seed_note_embedding(ulid_l2, fake_emb.embedder_id(), fake_emb.dim(), &vec_l2)
        .await
        .expect("seed embedding L2");

    // Requête semantic=true + query="beta-paraphrase-T3", class="deploy".
    // L1 doit être dans le résultat (via cosine), même si son corps ne matche pas en BM25.
    let resp_semantic = env
        .app
        .clone()
        .oneshot(recall_req(
            "class=deploy&semantic=true&query=beta-paraphrase-T3&limit=10",
            Some(&token),
        ))
        .await
        .unwrap();
    // DEBUG TEMPORAIRE — afficher le body si 500 pour diagnostiquer.
    let status_sem = resp_semantic.status();
    if status_sem != StatusCode::OK {
        let bytes = resp_semantic
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        eprintln!("DEBUG 500 body: {}", String::from_utf8_lossy(&bytes));
        panic!("semantic=true doit retourner 200, got {status_sem}");
    }
    let resp_semantic = env
        .app
        .clone()
        .oneshot(recall_req(
            "class=deploy&semantic=true&query=beta-paraphrase-T3&limit=10",
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp_semantic.status(),
        StatusCode::OK,
        "semantic=true doit retourner 200"
    );

    let json = body_json(resp_semantic).await;
    let items = json["items"].as_array().expect("items array semantic");
    let ulids: Vec<&str> = items.iter().map(|i| i["ulid"].as_str().unwrap()).collect();

    assert!(
        ulids.contains(&ulid_l1),
        "L1 doit être dans le résultat semantic=true (trouvée via cosine) — ulids={ulids:?}"
    );

    // Rétro-compat : semantic=false (défaut) → chemin BM25 recall_lessons (classe).
    // L2 est trouvée via tag "deploy"; L1 aussi (tag "deploy").
    let resp_default = env
        .app
        .oneshot(recall_req("class=deploy&limit=10", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp_default.status(),
        StatusCode::OK,
        "semantic absent doit retourner 200 (rétro-compat)"
    );
    let json_def = body_json(resp_default).await;
    let items_def = json_def["items"].as_array().expect("items default");
    // Les deux leçons sont trouvées via BM25 recall_lessons (tag "deploy").
    assert!(
        !items_def.is_empty(),
        "chemin BM25 défaut doit retourner des résultats (L1+L2 ont tag deploy)"
    );
}

/// Task 3 — `semantic=true` : leçon taguée `codified` exclue même via le chemin sémantique.
///
/// Le filtre `codified` doit s'appliquer après hydratation ULID, identiquement au chemin BM25.
#[tokio::test]
async fn recall_semantic_preserves_codified_exclusion() {
    let fake_emb = Arc::new(helpers::FakeEmbedder { dim: 8 });
    let env = helpers::build_app_with_embedder(
        Arc::clone(&fake_emb) as Arc<dyn gradatum_embed::Embedder>
    )
    .await;
    let token = helpers::sign_token(&env.state);
    let idx = env._vault_typed.index();

    // Leçon codifiée : doit être exclue en mode sémantique.
    let ulid_cod = "01KSEMANTC0DAAAAAAAAAAAAAA";
    idx.seed_lesson(
        ulid_cod,
        "Leçon Codified Deploy",
        "deploy codified",
        "deploy rollout codified discipline",
        1_700_300_000_000,
    )
    .await
    .expect("seed codified");
    let vec_cod = fake_emb
        .embed("deploy rollout codified")
        .await
        .expect("embed codified");
    idx.seed_note_embedding(ulid_cod, fake_emb.embedder_id(), fake_emb.dim(), &vec_cod)
        .await
        .expect("seed embedding codified");

    // Leçon active (non-codifiée) : doit être incluse.
    let ulid_active = "01KSEMANTACT1VAAAAAAAAAAAA";
    idx.seed_lesson(
        ulid_active,
        "Leçon Active Deploy",
        "deploy",
        "deploy rollout active discipline",
        1_700_300_001_000,
    )
    .await
    .expect("seed active");
    let vec_act = fake_emb
        .embed("deploy rollout active")
        .await
        .expect("embed active");
    idx.seed_note_embedding(
        ulid_active,
        fake_emb.embedder_id(),
        fake_emb.dim(),
        &vec_act,
    )
    .await
    .expect("seed embedding active");

    let resp = env
        .app
        .oneshot(recall_req(
            "class=deploy&semantic=true&query=deploy+rollout&limit=10",
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let ulids: Vec<&str> = json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["ulid"].as_str().unwrap())
        .collect();

    assert!(
        !ulids.contains(&ulid_cod),
        "leçon codified doit être exclue en mode sémantique — ulids={ulids:?}"
    );
    assert!(
        ulids.contains(&ulid_active),
        "leçon active doit être incluse en mode sémantique — ulids={ulids:?}"
    );
}

/// Task 3 — `semantic=true` : hits dont le tag ≠ class sont écartés post-hydratation.
///
/// `retrieve_candidates` cherche dans toutes les notes de `lessons-learned` par cosine.
/// Les notes dont les tags ne contiennent PAS la classe demandée doivent être filtrées
/// après hydratation — elles ne doivent pas polluer la réponse.
#[tokio::test]
async fn recall_semantic_filters_class() {
    let fake_emb = Arc::new(helpers::FakeEmbedder { dim: 8 });
    let env = helpers::build_app_with_embedder(
        Arc::clone(&fake_emb) as Arc<dyn gradatum_embed::Embedder>
    )
    .await;
    let token = helpers::sign_token(&env.state);
    let idx = env._vault_typed.index();

    // Leçon hors-classe "release" (pas "deploy") — embedding proche de la query.
    // Le filtre de classe doit l'éjecter malgré la similarité sémantique.
    let ulid_wrong_class = "01KSEMANTREJEASE0000000000";
    idx.seed_lesson(
        ulid_wrong_class,
        "Leçon Release Hors Classe",
        "release",
        "deploy rollout release cutover",
        1_700_400_000_000,
    )
    .await
    .expect("seed wrong class");

    // Embedding identique à la requête → cosine=1.0, sera trouvée sémantiquement.
    let query_text = "deploy rollout";
    let vec_wrong = fake_emb.embed(query_text).await.expect("embed wrong class");
    idx.seed_note_embedding(
        ulid_wrong_class,
        fake_emb.embedder_id(),
        fake_emb.dim(),
        &vec_wrong,
    )
    .await
    .expect("seed embedding wrong class");

    // Leçon de la bonne classe "deploy".
    let ulid_deploy = "01KSEMANTDEPJ0Y00000000000";
    idx.seed_lesson(
        ulid_deploy,
        "Leçon Deploy Bonne Classe",
        "deploy",
        "deploy rollout discipline",
        1_700_400_001_000,
    )
    .await
    .expect("seed deploy class");
    let vec_dep = fake_emb
        .embed("deploy rollout discipline")
        .await
        .expect("embed deploy");
    idx.seed_note_embedding(
        ulid_deploy,
        fake_emb.embedder_id(),
        fake_emb.dim(),
        &vec_dep,
    )
    .await
    .expect("seed embedding deploy");

    let resp = env
        .app
        .oneshot(recall_req(
            "class=deploy&semantic=true&query=deploy%20rollout&limit=10",
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let ulids: Vec<&str> = json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["ulid"].as_str().unwrap())
        .collect();

    assert!(
        !ulids.contains(&ulid_wrong_class),
        "note de classe 'release' doit être exclue quand class=deploy — ulids={ulids:?}"
    );
    assert!(
        ulids.contains(&ulid_deploy),
        "note de classe 'deploy' doit être incluse — ulids={ulids:?}"
    );
}
