//! Tests Task 8 — `proactive_refresh_once` compute in-process (B').
//!
//! Quatre propriétés vérifiées :
//!
//! 1. [`corpus_seeded_surface_excludes_source_ulids`] — corpus non vide → surface non vide,
//!    ULIDs sources (notes les plus récentes retournées par `list_recent_notes`) exclus de la surface.
//! 2. [`empty_corpus_returns_zero_no_error`] — corpus vide → `Ok(0)`, surface vide, aucune erreur.
//! 3. [`noop_embed_bm25_only_surface_non_empty`] — embedder Noop (`embed_fallback=true`) →
//!    BM25-only → surface non vide (dégradation gracieuse, pas d'erreur fatale).
//! 4. [`refresh_metrics_counter_incremented_on_success`] — Task 12 : `proactive_refresh`
//!    counter est incrémenté à chaque sortie `Ok(…)` de `proactive_refresh_once`.
//!
//! ## Setup
//!
//! Chaque test crée un fichier SQLite temporaire via `SqliteIndex::open` (qui exécute toutes
//! les migrations, y compris 0022 `proactive_surface`) puis ouvre un `ProactiveSurfaceStore`
//! sur le même fichier. L'embedder par défaut est Noop (`embed_fallback=true`, BM25-only).
//!
//! ## Règle B' (invariant de périmètre)
//!
//! Aucun fichier sous `crates/gradatum-worker/src` ni `crates/gradatum-core/src/job.rs`
//! n'est touché par ces tests — tout le calcul se fait in-process via `AppState`.

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use gradatum_server::proactive_recall::ProactiveRecallConfig;
use gradatum_server::proactive_recall::refresh::proactive_refresh_once;
use gradatum_server::proactive_surface_store::ProactiveSurfaceStore;
use gradatum_server::state::AppState;
use tempfile::TempDir;

/// ACL minimal — aucun consumer HTTP n'est requis (appel direct, pas via HTTP).
const MINIMAL_ACL: &str = r#"
[[consumer]]
identity = "test"
read_patterns  = ["main/*"]
write_patterns = []
"#;

/// Construit un `AppState` minimal avec `SqliteIndex` réel et `ProactiveSurfaceStore`.
///
/// L'embedder par défaut de `AppState::with_jwt_and_acl` est `NoopEmbedder`
/// (dim=384, `backend_kind=Noop` → `embed_fallback=true` dans `retrieve_candidates`).
/// C'est suffisant pour les tests BM25-only.
///
/// Le `SqliteIndex` est ouvert sur un fichier temporaire (exécute toutes les migrations).
/// Le `ProactiveSurfaceStore` s'ouvre sur le MÊME fichier (mode WAL, multi-connexion safe).
async fn build_state() -> (AppState, Arc<SqliteIndex>, TempDir) {
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

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(MINIMAL_ACL).expect("AclEngine — invariant test fixture");

    let mut state = AppState::with_jwt_and_acl(jwt, acl);
    // Brancher l'index réel (pas le placeholder SqliteIndex in-memory du constructeur).
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    // Injecter le store surface directement (le champ est pub).
    state.proactive_surface = Some(surface_store);

    (state, idx, tmp)
}

/// Test 1 : corpus seedé → surface non vide, ULIDs sources exclus.
///
/// ## Setup
///
/// - 3 notes « sources » (timestamps élevés → retournées en tête par `list_recent_notes`).
/// - 3 notes « candidates » (timestamps plus anciens → éligibles pour la surface).
/// - Toutes en section `lessons-learned` (dans le filtre de sections F-46).
/// - Corps des candidates contient les mots des titres des sources (FTS les trouve).
///
/// ## Invariants
///
/// - `proactive_refresh_once` retourne `Ok(n)` avec `n > 0`.
/// - `get_surface("main")` retourne une surface non vide.
/// - Aucun ULID source n'apparaît dans les items de la surface (exclusion active).
#[tokio::test]
async fn corpus_seeded_surface_excludes_source_ulids() {
    let (state, idx, _tmp) = build_state().await;

    // ── Sources (récentes, exclues de la surface) ────────────────────────────
    //
    // created_ms élevé → retournées par list_recent_notes(k=3).
    // Corps différent des candidates pour éviter un classement BM25 parasitant l'exclusion.
    let src_ids = [
        "01KSRC00000000000000000001", // 26 chars Crockford base32
        "01KSRC00000000000000000002",
        "01KSRC00000000000000000003",
    ];
    for id in &src_ids {
        idx.seed_lesson(
            id,
            "rust async recall", // titre → contribue à la salience query
            "recall async",      // tags → contribue à la salience query
            "rust async recall source note body",
            3_000_000_000_000, // créées récemment → sources
        )
        .await
        .expect("seed source lesson");
    }

    // ── Candidates (plus anciennes, éligibles surface) ───────────────────────
    //
    // Corps contient les mots des titres sources → BM25 les trouve via la salience query.
    let cnd_ids = [
        "01KCND00000000000000000001",
        "01KCND00000000000000000002",
        "01KCND00000000000000000003",
    ];
    for id in &cnd_ids {
        idx.seed_lesson(
            id,
            "candidate recall note",
            "candidate recall",
            "rust async recall candidate note body", // corps → matchable via BM25
            1_000_000_000_000,                       // plus anciennes → pas dans les sources
        )
        .await
        .expect("seed candidate lesson");
    }

    let cfg = ProactiveRecallConfig {
        recent_k: 3,     // prend les 3 sources (les plus récentes)
        surface_size: 5, // surface > recent_k → inclut des candidates
        ..Default::default()
    };

    let result = proactive_refresh_once(&state, &cfg).await;
    assert!(
        result.is_ok(),
        "proactive_refresh_once doit réussir — erreur : {result:?}"
    );
    assert!(
        result.unwrap() > 0,
        "la surface doit contenir au moins 1 candidat non-source"
    );

    let surface = state
        .proactive_surface
        .as_ref()
        .expect("proactive_surface présent — invariant test")
        .get_surface("main")
        .await
        .expect("get_surface")
        .unwrap_or_default();

    assert!(
        !surface.is_empty(),
        "get_surface('main') doit retourner une surface non vide"
    );

    // Vérification d'exclusion : aucun ULID source ne doit figurer dans la surface.
    let surface_ulids: Vec<&str> = surface.iter().map(|h| h.ulid.as_str()).collect();
    for src_id in &src_ids {
        assert!(
            !surface_ulids.contains(src_id),
            "ULID source '{src_id}' ne doit PAS apparaître dans la surface (exclusion sources active)"
        );
    }
}

/// Test 2 : corpus vide → `Ok(0)`, surface vide, aucune erreur.
///
/// ## Invariants
///
/// - La fn retourne `Ok(0)` sans paniquer ni retourner une erreur.
/// - La surface persistée est vide (`Some([])` après l'upsert d'un tableau vide).
#[tokio::test]
async fn empty_corpus_returns_zero_no_error() {
    let (state, _idx, _tmp) = build_state().await;
    // Aucune note seedée → list_recent_notes retourne [].

    let cfg = ProactiveRecallConfig::default();
    let result = proactive_refresh_once(&state, &cfg).await;

    assert!(
        result.is_ok(),
        "corpus vide ne doit pas retourner une erreur : {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        0,
        "corpus vide doit retourner 0 items surfacés"
    );

    // La surface est upsertée vide (tableau vide JSON) → get_surface retourne Some([]).
    let surface = state
        .proactive_surface
        .as_ref()
        .expect("proactive_surface présent — invariant test")
        .get_surface("main")
        .await
        .expect("get_surface ne doit pas échouer sur un tenant connu");

    // Some([]) : la ligne existe mais le tableau est vide.
    let items = surface.unwrap_or_default();
    assert!(
        items.is_empty(),
        "surface vide attendue pour corpus vide, got {} items",
        items.len()
    );
}

/// Test 4 (Task 12) : `proactive_refresh_once` incrémente le compteur de métriques.
///
/// Vérifie que `state.metrics.proactive_refresh` est incrémenté d'exactement 1
/// après un refresh réussi sur corpus vide (chemin le plus simple — Ok(0) garanti).
///
/// Séparé du test corpus-vide existant pour un diagnostic précis : une régression sur
/// la télémétrie serait masquée dans un test multitâche.
///
/// ## Invariants
///
/// - `proactive_refresh` counter = 1 après un seul appel `proactive_refresh_once`.
/// - Corpus vide → la fn retourne `Ok(0)`, le counter est tout de même incrémenté
///   (la sortie est un succès — le corpus vide est le comportement nominal, pas une erreur).
#[tokio::test]
async fn refresh_metrics_counter_incremented_on_success() {
    let (state, _idx, _tmp) = build_state().await;
    // Aucune note seedée → corpus vide → Ok(0) garanti.
    let cfg = ProactiveRecallConfig::default();

    let result = proactive_refresh_once(&state, &cfg).await;
    assert!(
        result.is_ok(),
        "corpus vide doit retourner Ok(0), got: {result:?}"
    );

    let count = state.metrics.proactive_refresh.get();
    assert_eq!(
        count, 1,
        "proactive_refresh counter doit être 1 après un appel réussi, got {count}"
    );
}

/// Test 3 : embedder Noop → `embed_fallback=true`, surface non vide (BM25-only).
///
/// Vérifie la dégradation gracieuse : même si le chemin sémantique est désactivé
/// (Noop → `embed_fallback=true`), le chemin BM25 produit une surface non vide.
/// L'AppState par défaut de `build_state` utilise `NoopEmbedder` — le test exploite
/// délibérément ce comportement.
///
/// ## Invariants
///
/// - La fn retourne `Ok(n)` avec `n > 0` (BM25 seul suffit pour produire une surface).
/// - Aucune erreur fatale (`Err` ne doit pas être retourné).
#[tokio::test]
async fn noop_embed_bm25_only_surface_non_empty() {
    // build_state() utilise NoopEmbedder par défaut → embed_fallback=true.
    let (state, idx, _tmp) = build_state().await;

    // 2 sources récentes.
    idx.seed_lesson(
        "01KEMB00000000000000000001",
        "async rust patterns",
        "async rust",
        "async rust patterns memory recall",
        3_000_000_000_000,
    )
    .await
    .expect("seed source 1");
    idx.seed_lesson(
        "01KEMB00000000000000000002",
        "async rust patterns",
        "async rust",
        "async rust patterns memory recall",
        3_000_000_000_000,
    )
    .await
    .expect("seed source 2");

    // 2 candidates plus anciennes — corps contient les mots des titres sources.
    idx.seed_lesson(
        "01KEMB00000000000000000003",
        "candidate knowledge",
        "knowledge",
        "async rust patterns memory recall candidate",
        1_000_000_000_000,
    )
    .await
    .expect("seed candidate 1");
    idx.seed_lesson(
        "01KEMB00000000000000000004",
        "candidate knowledge",
        "knowledge",
        "async rust patterns memory recall candidate",
        1_000_000_000_000,
    )
    .await
    .expect("seed candidate 2");

    let cfg = ProactiveRecallConfig {
        recent_k: 2,     // 2 sources les plus récentes
        surface_size: 3, // > recent_k → inclut des candidates
        ..Default::default()
    };

    let result = proactive_refresh_once(&state, &cfg).await;

    assert!(
        result.is_ok(),
        "Noop embedder ne doit pas produire d'erreur fatale — résultat : {result:?}"
    );
    assert!(
        result.unwrap() > 0,
        "BM25-only (embed_fallback=true) doit trouver au moins 1 candidat non-source"
    );
}
