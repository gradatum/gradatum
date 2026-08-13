//! Harness d'acceptation simulée proactive recall (v0.7.1).
//!
//! ## Objectif
//!
//! Mesure le taux d'acceptation initial d'un cycle complet :
//! seed corpus → `proactive_refresh_once` → pull proactive → pull contextuel
//! → feedback (accepter un sous-ensemble) → taux > 0.
//!
//! ## Assertion sanity
//!
//! Taux d'acceptation > 0 : preuve que la chaîne refresh → surface → pull →
//! feedback fonctionne end-to-end sans erreur et produit au moins 1 item accepté.
//! Le but est un harness reproductible mesurant l'apport, pas un seuil strict.
//!
//! ## Corpus
//!
//! 7 notes au total :
//! - 2 sources récentes (`lessons-learned`, timestamp 3e12 ms ≈ an 2065) —
//!   titres/tags alimentent la salience query ; exclues de la surface par le refresh.
//! - 5 candidates (sections cibles F-46 : `lessons-learned`, `reasoning`, `decisions`) —
//!   corps FTS-searchable via les mots-clés des titres/tags sources ; retenues dans la surface.
//!
//! ## Setup
//!
//! Même pattern que `tests/proactive_refresh.rs` (build_state) + `tests/proactive_recall.rs`
//! (feedback) : `SqliteIndex` réel sur TempDir, `ProactiveSurfaceStore` + `ProactiveRecallStore`
//! sur le même fichier (WAL, multi-connexion safe). Embedder Noop (BM25-only, dégradation
//! gracieuse — suffisant pour retrouver les candidates par FTS).

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::index::Index;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{ProactiveRecallFeedbackRequest, ProactiveRecallRequest};
use gradatum_index::SqliteIndex;
use gradatum_server::proactive_recall::refresh::proactive_refresh_once;
use gradatum_server::proactive_recall::{
    ProactiveRecallConfig, proactive_recall, proactive_recall_feedback,
};
use gradatum_server::proactive_recall_store::ProactiveRecallStore;
use gradatum_server::proactive_surface_store::ProactiveSurfaceStore;
use gradatum_server::state::AppState;
use tempfile::TempDir;
use ulid::Ulid;

/// Identité du consumer de test (= `sub` du BearerToken — ACL résout sur `sub`).
const TEST_IDENTITY: &str = "agent";

/// Preset ACL : lecture large sur `main/*` (toutes sections du tenant `main`).
const ACL_MAIN_ALL: &str = r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#;

/// Construit un `AppState` avec `SqliteIndex` réel + stores surface/session branchés.
///
/// L'ordre est crucial : `SqliteIndex::open` exécute toutes les migrations (dont 0022
/// `proactive_surface` et 0023 `proactive_recall_sessions`) — les stores s'ouvrent ensuite
/// sur le même fichier (mode WAL). L'embedder par défaut (`AppState::with_jwt_and_acl`)
/// est `NoopEmbedder` → `embed_fallback=true` → BM25-only, suffisant pour ce harness.
async fn build_state_for_eval() -> (AppState, Arc<SqliteIndex>, TempDir) {
    let tmp = TempDir::new().expect("TempDir — invariant test fixture");
    let index_path = tmp.path().join("index.db");

    let idx = Arc::new(
        SqliteIndex::open(&index_path)
            .await
            .expect("SqliteIndex::open — invariant test fixture"),
    );
    let surface_store = ProactiveSurfaceStore::open(&index_path).await.expect(
        "ProactiveSurfaceStore::open — migration 0022 doit exister après SqliteIndex::open",
    );
    let recall_store = ProactiveRecallStore::open(&index_path)
        .await
        .expect("ProactiveRecallStore::open — migration 0023 doit exister après SqliteIndex::open");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(ACL_MAIN_ALL).expect("AclEngine — invariant test fixture");

    let mut state = AppState::with_jwt_and_acl(jwt, acl);
    // Brancher l'index réel — même pattern que proactive_refresh.rs build_state.
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.proactive_surface = Some(surface_store);
    state.proactive_recall = Some(recall_store);

    (state, idx, tmp)
}

/// TrustContext BearerToken pour le tenant `main`, identité `agent` (read scope).
fn bearer_main() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: TEST_IDENTITY.into(),
        scopes: vec!["read".into()],
        tenant_id: "main".into(),
        jti: None,
    }
}

/// Cycle complet proactive recall → taux d'acceptation initial > 0.
///
/// Vérifie que la chaîne `proactive_refresh_once` → `proactive_recall` (proactive)
/// → `proactive_recall` (contextuel) → `proactive_recall_feedback` fonctionne
/// end-to-end et produit un taux d'acceptation > 0 (au moins 1 item accepté
/// parmi les items surfacés).
///
/// ## Invariants
///
/// - `proactive_refresh_once` retourne `Ok(n)` avec `n > 0` (surface non vide).
/// - `proactive_recall` (mode proactive) retourne ≥ 1 item depuis la surface calculée.
/// - `proactive_recall` (mode contextuel) ne retourne pas d'erreur.
/// - `proactive_recall_feedback` réussit (accepted ⊆ surfaced, ULIDs Crockford valides).
/// - Taux d'acceptation `accepted / surfacés > 0`.
#[tokio::test]
async fn end_to_end_proactive_cycle_acceptance_rate_above_zero() {
    let (state, idx, _tmp) = build_state_for_eval().await;

    // ── 1. Seed candidates (sections cibles F-46, timestamp courant ≈ 1.75e12 ms) ───
    //
    // Corps contient les mots-clés qui seront dans les titres/tags des sources.
    // `seed_note_with_fts` utilise `chrono::Utc::now()` → timestamp ≈ 1.75e12 ms <
    // timestamp source 3e12 ms → ces notes ne figurent PAS dans `list_recent_notes`
    // (sources toujours en tête de tri). Elles sont donc candidates, pas sources.
    let cnd_ids: [String; 5] = std::array::from_fn(|_| Ulid::generate().to_string());

    idx.seed_note_with_fts(
        &cnd_ids[0],
        "lessons-learned",
        "async rust mémoire pattern candidate note leçon",
    )
    .await
    .expect("seed candidate lessons-learned 1 — invariant corpus");

    idx.seed_note_with_fts(
        &cnd_ids[1],
        "reasoning",
        "async rust mémoire raisonnement pattern candidate",
    )
    .await
    .expect("seed candidate reasoning 1 — invariant corpus");

    idx.seed_note_with_fts(
        &cnd_ids[2],
        "decisions",
        "rust mémoire décision async pattern candidate",
    )
    .await
    .expect("seed candidate decisions 1 — invariant corpus");

    idx.seed_note_with_fts(
        &cnd_ids[3],
        "lessons-learned",
        "async rust apprentissage mémoire recall pattern candidate",
    )
    .await
    .expect("seed candidate lessons-learned 2 — invariant corpus");

    idx.seed_note_with_fts(
        &cnd_ids[4],
        "reasoning",
        "rust performance mémoire reasoning async recall candidate",
    )
    .await
    .expect("seed candidate reasoning 2 — invariant corpus");

    // ── 2. Seed sources récentes (timestamp 3e12 ms → toujours en tête de tri) ───
    //
    // Les titres et tags alimentent `derive_salience_query`. La salience query sera
    // `"async rust mémoire async rust mémoire"` environ → BM25 retrouve les candidates
    // dont le corps contient ces mots-clés. Timestamp 3_000_000_000_000 ms ≈ an 2065 →
    // `COALESCE(updated, created) DESC` les place en premier → elles sont les sources.
    // Elles seront exclues de la surface par le filtre `source_set` du refresh.
    let src_ids: [String; 2] = std::array::from_fn(|_| Ulid::generate().to_string());

    for id in &src_ids {
        idx.seed_lesson(
            id,
            "async rust mémoire", // titre → contribue à la salience query
            "async rust mémoire", // tags  → contribue à la salience query
            "note source récente active rust async mémoire pattern",
            3_000_000_000_000_i64, // timestamp élevé → toujours en tête de list_recent_notes
        )
        .await
        .expect("seed source lesson — invariant corpus");
    }

    // ── 3. Calcul de la surface proactive ─────────────────────────────────────
    //
    // recent_k=2 → 2 notes les plus récentes (les sources au timestamp 3e12 ms).
    // surface_size=5 → jusqu'à 5 hits retenus (candidates après exclusion sources).
    // Embedder Noop → embed_fallback=true → BM25-only (suffisant pour FTS-match).
    let cfg = ProactiveRecallConfig {
        recent_k: 2,
        surface_size: 5,
        ..Default::default()
    };

    let surface_n = proactive_refresh_once(&state, &cfg)
        .await
        .expect("proactive_refresh_once doit réussir sur corpus valide");

    assert!(
        surface_n > 0,
        "la surface proactive doit contenir ≥ 1 candidat après refresh.\n\
         Vérifier : les candidates sont FTS-searchables via la salience query \
         ('async rust mémoire') et ne sont pas dans source_set.\n\
         surface_n={surface_n}",
    );

    // ── 4. Pull proactive (context=None → lit la surface pré-calculée) ────────
    let mut req_proactive = ProactiveRecallRequest::default();
    req_proactive.tenant_id = Some("main".into());

    let resp_proactive = proactive_recall(&state, &bearer_main(), req_proactive)
        .await
        .expect("proactive_recall (mode proactive) doit réussir");

    assert_eq!(
        resp_proactive.mode, "proactive",
        "mode doit être 'proactive' quand context=None"
    );
    assert!(
        !resp_proactive.items.is_empty(),
        "proactive_recall doit retourner ≥ 1 item depuis la surface calculée (surface_n={surface_n})"
    );
    assert!(
        !resp_proactive.recall_id.is_empty(),
        "recall_id doit être généré (non vide)"
    );

    // ── 5. Pull contextuel (context=Some(...)) ────────────────────────────────
    //
    // Vérifie que le mode contextuel ne retourne pas d'erreur. Non bloquant pour
    // l'assertion principale (le taux est mesuré sur le mode proactive).
    let mut req_contextual = ProactiveRecallRequest::default();
    req_contextual.tenant_id = Some("main".into());
    req_contextual.context = Some("async rust mémoire pattern".into());
    req_contextual.sections = Some(vec!["lessons-learned".into(), "reasoning".into()]);
    req_contextual.limit = Some(5);

    let resp_contextual = proactive_recall(&state, &bearer_main(), req_contextual)
        .await
        .expect("proactive_recall (mode contextuel) doit réussir sans erreur");

    assert_eq!(
        resp_contextual.mode, "contextual",
        "mode doit être 'contextual' quand context=Some(_)"
    );

    // ── 6. Feedback : accepter un sous-ensemble de la surface proactive ───────
    //
    // Les ULIDs surfacés sont ceux des candidates (générés par Ulid::generate() →
    // Crockford base32 valides → parsables par Ulid::from_string dans le handler).
    // Invariant accepted ⊆ surfaced : on accepte le premier item → {first} ⊆ surfaced.
    let surfaced_ulids: Vec<String> = resp_proactive
        .items
        .iter()
        .map(|h| h.ulid.clone())
        .collect();

    assert!(
        !surfaced_ulids.is_empty(),
        "surfaced_ulids ne doit pas être vide (pré-condition : surface non vide vérifiée step 4)"
    );

    // Accepter 1 item (sous-ensemble minimal valide).
    let accepted = vec![surfaced_ulids[0].clone()];

    let mut feedback_req =
        ProactiveRecallFeedbackRequest::new(resp_proactive.recall_id.clone(), accepted.clone());
    feedback_req.tenant_id = Some("main".into());

    proactive_recall_feedback(&state, &bearer_main(), feedback_req)
        .await
        .expect("proactive_recall_feedback (accepted ⊆ surfaced) doit réussir");

    // ── 7. Assertion sanity : taux d'acceptation > 0 ─────────────────────────
    let surfaced_count = surfaced_ulids.len();
    let accepted_count = accepted.len();
    // Pré-condition : surfaced_count > 0 vérifiée ci-dessus → division safe.
    let taux = accepted_count as f64 / surfaced_count as f64;

    println!(
        "Taux acceptation initial : {accepted_count}/{surfaced_count} = {taux:.3} \
         | sections surfacées : {:?} \
         | mode contextuel items : {}",
        resp_proactive
            .items
            .iter()
            .map(|h| h.section.as_str())
            .collect::<Vec<_>>(),
        resp_contextual.items.len(),
    );

    assert!(
        taux > 0.0,
        "taux d'acceptation doit être > 0 : {accepted_count}/{surfaced_count} = {taux:.3}.\n\
         Si 0 : vérifier que step 4 surface bien des items et que step 6 accepte le premier.",
    );
}
