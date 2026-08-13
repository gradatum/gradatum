//! Tests des orchestrateurs `proactive_recall` / `proactive_recall_feedback` (B').
//!
//! Neuf propriétés vérifiées :
//!
//! 1. [`proactive_mode_reads_precomputed_surface`] — `context=None` → mode `"proactive"`,
//!    lit la surface pré-calculée.
//! 2. [`c3_unreadable_section_excluded_from_items_and_surfaced`] — **test sécurité C3** :
//!    une section non lisible par l'appelant est exclue du retour ET de `surfaced`.
//! 3. [`absent_surface_returns_empty_no_error`] — surface absente → items vides, pas d'erreur.
//! 4. [`contextual_mode_cross_section_no_leak`] — `context=Some` + `sections` → aucune note
//!    hors `sections` ne fuite.
//! 5. [`limit_caps_returned_items`] — `limit` borne le nombre d'items.
//! 6. [`acl_deny_tenant_returns_forbidden`] — ACL Read refusée → `Forbidden`.
//! 7. [`special_chars_context_does_not_break`] — caractères FTS spéciaux ne cassent pas.
//!
//! Tests feedback (8-13) + métriques (14-15) :
//!
//! 14. [`recall_metrics_surfaced_counter_incremented`] — `proactive_surfaced` counter incrémenté.
//! 15. [`feedback_metrics_accepted_counter_incremented`] — `proactive_accepted` counter incrémenté.
//!
//! ## Setup
//!
//! `SqliteIndex::open` exécute toutes les migrations (dont 0022 `proactive_surface` et
//! 0023 `proactive_recall_sessions`). Les stores s'ouvrent sur le MÊME fichier (WAL).
//! L'embedder par défaut est Noop (`embed_fallback=true` → BM25-only).

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::index::Index;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{ProactiveHit, ProactiveRecallFeedbackRequest, ProactiveRecallRequest};
use gradatum_index::SqliteIndex;
use gradatum_server::metrics::ProactiveRecallModeLabel;
use gradatum_server::proactive_recall::{proactive_recall, proactive_recall_feedback};
use gradatum_server::proactive_recall_store::ProactiveRecallStore;
use gradatum_server::proactive_surface_store::ProactiveSurfaceStore;
use gradatum_server::state::AppState;
use tempfile::TempDir;

/// Identité du consumer de test (= `sub` du BearerToken — ACL résout sur `sub`).
const TEST_IDENTITY: &str = "agent";

/// Construit un `AppState` réel avec ACL fournie + stores surface/session branchés.
///
/// `acl_preset` est un TOML de preset ACL (`[[consumer]] … read_patterns = …`).
async fn build_state(acl_preset: &str) -> (AppState, Arc<SqliteIndex>, TempDir) {
    let tmp = TempDir::new().expect("TempDir — invariant test fixture");
    let index_path = tmp.path().join("index.db");

    let idx = Arc::new(
        SqliteIndex::open(&index_path)
            .await
            .expect("SqliteIndex::open — invariant test fixture"),
    );
    let surface_store = ProactiveSurfaceStore::open(&index_path)
        .await
        .expect("ProactiveSurfaceStore::open — migration 0022");
    let recall_store = ProactiveRecallStore::open(&index_path)
        .await
        .expect("ProactiveRecallStore::open — migration 0023");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(acl_preset).expect("AclEngine — invariant test fixture");

    let mut state = AppState::with_jwt_and_acl(jwt, acl);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.proactive_surface = Some(surface_store);
    state.proactive_recall = Some(recall_store);

    (state, idx, tmp)
}

/// Construit un `TrustContext::BearerToken` pour le tenant `"main"`, identité `agent`.
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

/// Construit un `ProactiveHit` minimal pour seeder une surface contrôlée.
fn hit(ulid: &str, section: &str) -> ProactiveHit {
    ProactiveHit {
        ulid: ulid.into(),
        title: format!("titre {ulid}"),
        section: section.into(),
        snippet: String::new(),
        score: 1.0,
    }
}

/// Requête proactive (context absent) sur le tenant `main`.
fn req_proactive(limit: Option<u32>) -> ProactiveRecallRequest {
    let mut req = ProactiveRecallRequest::default();
    req.tenant_id = Some("main".into());
    req.limit = limit;
    req
}

// ── Test 1 : mode proactive lit la surface pré-calculée ──────────────────────

#[tokio::test]
async fn proactive_mode_reads_precomputed_surface() {
    // ACL large : tout `main/*` lisible.
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#,
    )
    .await;

    let surface = vec![
        hit("01KSURF0000000000000000001", "lessons-learned"),
        hit("01KSURF0000000000000000002", "reasoning"),
    ];
    state
        .proactive_surface
        .as_ref()
        .expect("surface store")
        .upsert_surface("main", &surface, 1_000)
        .await
        .expect("upsert_surface");

    let resp = proactive_recall(&state, &bearer_main(), req_proactive(None))
        .await
        .expect("proactive_recall");

    assert_eq!(resp.mode, "proactive", "mode doit être 'proactive'");
    assert_eq!(resp.items.len(), 2, "les 2 hits doivent être surfacés");
    assert!(!resp.recall_id.is_empty(), "recall_id généré");
}

// ── Test 2 : C3 — section non lisible exclue du retour ET de surfaced ─────────

#[tokio::test]
async fn c3_unreadable_section_excluded_from_items_and_surfaced() {
    // Appelant : `main/*` SAUF `decisions` (deny-wins). Peut lire lessons-learned,
    // PAS decisions. La surface pré-calculée contient les deux.
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*", "!main/decisions"]
write_patterns = []
"#,
    )
    .await;

    let lesson_ulid = "01KLESSON000000000000000001";
    let decision_ulid = "01KDECISION0000000000000001";
    let surface = vec![
        hit(lesson_ulid, "lessons-learned"),
        hit(decision_ulid, "decisions"),
    ];
    state
        .proactive_surface
        .as_ref()
        .expect("surface store")
        .upsert_surface("main", &surface, 1_000)
        .await
        .expect("upsert_surface");

    let resp = proactive_recall(&state, &bearer_main(), req_proactive(None))
        .await
        .expect("proactive_recall");

    // (a) Le retour ne contient AUCUNE note de section `decisions`.
    assert!(
        resp.items.iter().all(|h| h.section != "decisions"),
        "C3 : la section 'decisions' (non lisible) doit être exclue du retour"
    );
    assert!(
        resp.items.iter().any(|h| h.ulid == lesson_ulid),
        "la note 'lessons-learned' (lisible) doit rester présente"
    );
    assert!(
        !resp.items.iter().any(|h| h.ulid == decision_ulid),
        "la note 'decisions' (cachée) ne doit PAS apparaître — bypass ACL bloqué"
    );

    // (b) La session enregistrée (`surfaced`) exclut aussi la note cachée.
    let surfaced = state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .get_surfaced(&resp.recall_id, "main")
        .await
        .expect("get_surfaced")
        .expect("session enregistrée pour recall_id");
    assert!(
        surfaced.contains(&lesson_ulid.to_string()),
        "surfaced doit contenir la note lisible"
    );
    assert!(
        !surfaced.contains(&decision_ulid.to_string()),
        "C3 : surfaced enregistré doit exclure la note cachée (cohérence accepted⊆surfaced)"
    );
}

// ── Test 3 : surface absente → items vides, pas d'erreur ─────────────────────

#[tokio::test]
async fn absent_surface_returns_empty_no_error() {
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#,
    )
    .await;
    // Aucune surface upsertée → get_surface("main") = None.

    let resp = proactive_recall(&state, &bearer_main(), req_proactive(None))
        .await
        .expect("surface absente ne doit pas être une erreur");

    assert_eq!(resp.mode, "proactive");
    assert!(
        resp.items.is_empty(),
        "surface absente → 0 item (pas d'erreur)"
    );
}

// ── Test 4 : mode contextual — cross-section no-leak ─────────────────────────

#[tokio::test]
async fn contextual_mode_cross_section_no_leak() {
    let (state, idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#,
    )
    .await;

    // Deux notes FTS-matchables sur la même requête, sections différentes.
    idx.seed_note_with_fts(
        "01KCTX00000000000000000001",
        "lessons-learned",
        "rust async memory recall pattern",
    )
    .await
    .expect("seed lessons-learned");
    idx.seed_note_with_fts(
        "01KCTX00000000000000000002",
        "decisions",
        "rust async memory recall pattern",
    )
    .await
    .expect("seed decisions");

    let mut req = ProactiveRecallRequest::default();
    req.tenant_id = Some("main".into());
    req.context = Some("rust async memory recall".into());
    req.sections = Some(vec!["lessons-learned".into()]);

    let resp = proactive_recall(&state, &bearer_main(), req)
        .await
        .expect("contextual recall");

    assert_eq!(resp.mode, "contextual", "mode doit être 'contextual'");
    assert!(
        resp.items.iter().all(|h| h.section == "lessons-learned"),
        "no-leak : aucune note hors 'lessons-learned' ne doit apparaître (got {:?})",
        resp.items.iter().map(|h| &h.section).collect::<Vec<_>>()
    );
    assert!(
        !resp
            .items
            .iter()
            .any(|h| h.ulid == "01KCTX00000000000000000002"),
        "la note 'decisions' hors filtre sections ne doit pas fuiter"
    );
}

// ── Test 5 : respect du limit ────────────────────────────────────────────────

#[tokio::test]
async fn limit_caps_returned_items() {
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#,
    )
    .await;

    let surface: Vec<ProactiveHit> = (0..5)
        .map(|i| hit(&format!("01KLIMIT000000000000000{i:03}"), "lessons-learned"))
        .collect();
    state
        .proactive_surface
        .as_ref()
        .expect("surface store")
        .upsert_surface("main", &surface, 1_000)
        .await
        .expect("upsert_surface");

    let resp = proactive_recall(&state, &bearer_main(), req_proactive(Some(2)))
        .await
        .expect("proactive_recall");

    assert_eq!(resp.items.len(), 2, "limit=2 doit borner à 2 items");
}

// ── Test 6 : ACL deny tenant → Forbidden ─────────────────────────────────────

#[tokio::test]
async fn acl_deny_tenant_returns_forbidden() {
    // Consumer connu mais sans aucun read_pattern → `main/main` DenyImplicit.
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = []
write_patterns = []
"#,
    )
    .await;

    let err = proactive_recall(&state, &bearer_main(), req_proactive(None))
        .await
        .expect_err("ACL deny doit retourner une erreur");

    assert!(
        matches!(err, GradatumError::Forbidden(_)),
        "ACL deny → Forbidden, got {err:?}"
    );
}

// ── Test 7 : sanitization — caractères FTS spéciaux ne cassent pas ───────────

#[tokio::test]
async fn special_chars_context_does_not_break() {
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#,
    )
    .await;

    // Caractères réservés FTS5 : opérateurs, parenthèses, guillemets, colonnes, étoile.
    let mut req = ProactiveRecallRequest::default();
    req.tenant_id = Some("main".into());
    req.context = Some(r#"a:b AND (c*) OR "x y" NEAR(z)"#.into());

    let resp = proactive_recall(&state, &bearer_main(), req)
        .await
        .expect("caractères FTS spéciaux ne doivent pas provoquer d'erreur/panic");

    assert_eq!(resp.mode, "contextual");
    // Aucune note seedée → items vides, mais surtout : pas de 500/parse error.
}

// ── Tests Task 11 — orchestrateur feedback + `accepted ⊆ surfaced` ────────────
//
// Six propriétés : accepted⊆surfaced OK · sur-ensemble → 400 · recall_id inconnu
// → 400 · ULID mal formé → 400 · idempotence (2× = 1 enregistrement) · ACL deny
// → Forbidden.
//
// Les ULIDs de feedback DOIVENT parser (`Ulid::from_string`) → on utilise des ULIDs
// Crockford base32 valides (les fixtures Task 10 `01KSURF…` contiennent un `U`, exclu
// du jeu Crockford, donc non parsables — non réutilisables ici).

/// ULID valide A (exemple canonique Crockford base32).
const ULID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
/// ULID valide B (exemple canonique Crockford base32).
const ULID_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
/// ULID valide C — hors surface (pour le test sur-ensemble).
const ULID_C: &str = "01CADV00000000000000000000";

/// Preset ACL large : tout `main/*` lisible (suffisant pour les tests feedback non-ACL).
const ACL_MAIN_ALL: &str = r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*"]
write_patterns = []
"#;

/// Insère une session de rappel contrôlée (`recall_id` + `surfaced` choisis).
async fn seed_session(state: &AppState, recall_id: &str, surfaced: &[String]) {
    state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .insert_session(recall_id, "main", "proactive", surfaced, 1_000)
        .await
        .expect("insert_session");
}

/// Construit une requête de feedback pour le tenant `main`.
fn req_feedback(recall_id: &str, accepted: &[&str]) -> ProactiveRecallFeedbackRequest {
    let mut req = ProactiveRecallFeedbackRequest::new(
        recall_id.into(),
        accepted.iter().map(|s| (*s).to_string()).collect(),
    );
    req.tenant_id = Some("main".into());
    req
}

// ── Test 8 : accepted ⊆ surfaced → Ok + record_feedback persiste ─────────────

#[tokio::test]
async fn feedback_accepted_subset_ok_persists() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(
        &state,
        "recall-fb-001",
        &[ULID_A.to_string(), ULID_B.to_string()],
    )
    .await;

    // accepted = {A} ⊆ surfaced = {A, B}.
    proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-001", &[ULID_A]),
    )
    .await
    .expect("accepted ⊆ surfaced doit réussir");

    let count = state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .count_feedback()
        .await
        .expect("count_feedback");
    assert_eq!(
        count, 1,
        "record_feedback doit persister exactement 1 ligne"
    );
}

// ── Test 9 : sur-ensemble (accepted ⊄ surfaced) → 400 BadRequest ─────────────

#[tokio::test]
async fn feedback_superset_returns_bad_request() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(&state, "recall-fb-002", &[ULID_A.to_string()]).await;

    // accepted = {A, C} ; C ∉ surfaced = {A} → sur-ensemble.
    let err = proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-002", &[ULID_A, ULID_C]),
    )
    .await
    .expect_err("sur-ensemble doit échouer");

    assert!(
        matches!(err, GradatumError::InvalidInput(_)),
        "accepted ⊄ surfaced → InvalidInput (400), got {err:?}"
    );

    // Aucun feedback ne doit avoir été enregistré (échec avant record_feedback).
    let count = state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .count_feedback()
        .await
        .expect("count_feedback");
    assert_eq!(count, 0, "un sur-ensemble ne doit rien persister");
}

// ── Test 10 : recall_id inconnu → 400 BadRequest ─────────────────────────────

#[tokio::test]
async fn feedback_unknown_recall_id_returns_bad_request() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    // Aucune session seedée.

    let err = proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-inexistant", &[ULID_A]),
    )
    .await
    .expect_err("recall_id inconnu doit échouer");

    assert!(
        matches!(err, GradatumError::InvalidInput(_)),
        "recall_id inconnu → InvalidInput (400), got {err:?}"
    );
}

// ── Test 11 : ULID accepted mal formé → 400 BadRequest ───────────────────────

#[tokio::test]
async fn feedback_malformed_ulid_returns_bad_request() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(&state, "recall-fb-003", &[ULID_A.to_string()]).await;

    // "PAS-UN-ULID" n'est pas un ULID Crockford base32 valide.
    let err = proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-003", &["PAS-UN-ULID"]),
    )
    .await
    .expect_err("ULID mal formé doit échouer");

    assert!(
        matches!(err, GradatumError::InvalidInput(_)),
        "ULID accepté mal formé → InvalidInput (400), got {err:?}"
    );
}

// ── Test 12 : idempotence — 2× même feedback = 1 enregistrement, pas d'erreur ─

#[tokio::test]
async fn feedback_idempotent_double_submit() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(
        &state,
        "recall-fb-004",
        &[ULID_A.to_string(), ULID_B.to_string()],
    )
    .await;

    let req = req_feedback("recall-fb-004", &[ULID_A]);

    // Premier feedback.
    proactive_recall_feedback(&state, &bearer_main(), req.clone())
        .await
        .expect("1er feedback OK");
    // Second feedback identique — ne doit PAS erreur (UPSERT idempotent).
    proactive_recall_feedback(&state, &bearer_main(), req)
        .await
        .expect("2e feedback identique ne doit pas erreur");

    let count = state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .count_feedback()
        .await
        .expect("count_feedback");
    assert_eq!(count, 1, "2× même feedback → 1 seul enregistrement");
}

// ── Test 13 : ACL deny tenant → Forbidden ────────────────────────────────────

#[tokio::test]
async fn feedback_acl_deny_returns_forbidden() {
    // Consumer connu mais sans aucun read_pattern → `main/main` DenyImplicit.
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = []
write_patterns = []
"#,
    )
    .await;
    seed_session(&state, "recall-fb-005", &[ULID_A.to_string()]).await;

    let err = proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-fb-005", &[ULID_A]),
    )
    .await
    .expect_err("ACL deny doit retourner une erreur");

    assert!(
        matches!(err, GradatumError::Forbidden(_)),
        "ACL deny → Forbidden, got {err:?}"
    );
}

// ── Tests Step 5 (corrections d'audit) — sécurité feedback ───────────────────

/// FIX 1 (IDOR, security-reviewer A01) : un `recall_id` appartenant au tenant A
/// n'est PAS adressable par un feedback émis sous le tenant B.
///
/// Avant le fix, `get_surfaced` ignorait le tenant → un appelant pouvait poster un
/// feedback sur la session d'un autre tenant. Le filtre tenant rend `get_surfaced`
/// → `None` → `InvalidInput` (400), et rien n'est persisté.
#[tokio::test]
async fn feedback_cross_tenant_recall_id_rejected() {
    // ACL : identité "agent" lit main/* ET other/* (le gate de base passe pour les 2 tenants).
    let (state, _idx, _tmp) = build_state(
        r#"
[[consumer]]
identity = "agent"
read_patterns  = ["main/*", "other/*"]
write_patterns = []
"#,
    )
    .await;

    // Session appartenant au tenant "main".
    state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .insert_session(
            "recall-xt-001",
            "main",
            "proactive",
            &[ULID_A.to_string()],
            1_000,
        )
        .await
        .expect("insert_session tenant main");

    // Feedback tenté sous le tenant "other" avec le recall_id de "main".
    let bearer_other = TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: TEST_IDENTITY.into(),
        scopes: vec!["read".into()],
        tenant_id: "other".into(),
        jti: None,
    };
    let mut req =
        ProactiveRecallFeedbackRequest::new("recall-xt-001".into(), vec![ULID_A.to_string()]);
    req.tenant_id = Some("other".into());

    let err = proactive_recall_feedback(&state, &bearer_other, req)
        .await
        .expect_err("recall_id d'un autre tenant doit être rejeté");
    assert!(
        matches!(err, GradatumError::InvalidInput(_)),
        "cross-tenant recall_id → InvalidInput (get_surfaced None), got {err:?}"
    );

    // IDOR bloqué : aucun feedback persisté sous le tenant attaquant.
    let count = state
        .proactive_recall
        .as_ref()
        .expect("recall store")
        .count_feedback()
        .await
        .expect("count_feedback");
    assert_eq!(
        count, 0,
        "aucun feedback ne doit être enregistré cross-tenant"
    );
}

/// FIX 3 (DoS léger, cap accepted_ulids) : au-delà de la borne, rejet `InvalidInput`
/// AVANT toute lecture SQL ou boucle de validation.
///
/// Le cap se déclenche en premier : on utilise un volume au-dessus de la borne (65)
/// et on cible le message d'erreur du cap pour distinguer des autres `InvalidInput`.
#[tokio::test]
async fn feedback_too_many_accepted_ulids_rejected() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;

    // 65 entrées > MAX_ACCEPTED_ULIDS (64). Le cap rejette avant le parse ULID,
    // donc des valeurs arbitraires suffisent à exercer la borne.
    let accepted: Vec<String> = (0..65).map(|i| format!("entry-{i}")).collect();
    let mut req = ProactiveRecallFeedbackRequest::new("recall-cap".into(), accepted);
    req.tenant_id = Some("main".into());

    let err = proactive_recall_feedback(&state, &bearer_main(), req)
        .await
        .expect_err("trop d'accepted_ulids doit échouer");
    assert!(
        matches!(&err, GradatumError::InvalidInput(msg) if msg.contains("too many accepted_ulids")),
        "cap accepted_ulids → InvalidInput (message cap), got {err:?}"
    );
}

// ── Tests Task 12 : métriques câblées dans les orchestrateurs ─────────────────

/// Test 14 : `proactive_recall` incrémente le counter `proactive_surfaced`.
///
/// Surface pré-chargée avec 2 hits, ACL large (`main/*`) → les 2 hits passent le filtre.
/// Vérifie que `state.metrics.proactive_surfaced{mode="proactive"}` = 2 après l'appel.
///
/// ## Invariants
///
/// - Le counter est incrémenté du nombre d'items POST-filtrage ACL.
/// - Le label `mode="proactive"` est utilisé (chemin surface pré-calculée).
#[tokio::test]
async fn recall_metrics_surfaced_counter_incremented() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;

    // Seeder une surface de 2 hits (section lisible par l'ACL large).
    let surface = vec![
        ProactiveHit {
            ulid: ULID_A.into(),
            title: "Note A".into(),
            section: "lessons-learned".into(),
            snippet: String::new(),
            score: 1.0,
        },
        ProactiveHit {
            ulid: ULID_B.into(),
            title: "Note B".into(),
            section: "reasoning".into(),
            snippet: String::new(),
            score: 0.9,
        },
    ];
    state
        .proactive_surface
        .as_ref()
        .expect("surface store")
        .upsert_surface("main", &surface, 1_000)
        .await
        .expect("upsert_surface");

    // Pull proactif (context=None → mode "proactive").
    let resp = proactive_recall(&state, &bearer_main(), req_proactive(None))
        .await
        .expect("proactive_recall");
    assert_eq!(resp.items.len(), 2, "pré-condition : 2 items surfacés");

    // Vérification du counter `proactive_surfaced{mode="proactive"}`.
    let count = state
        .metrics
        .proactive_surfaced
        .get_or_create(&ProactiveRecallModeLabel { mode: "proactive" })
        .get();
    assert_eq!(
        count, 2,
        "proactive_surfaced{{mode=\"proactive\"}} doit être 2 après un pull avec 2 items, got {count}"
    );
}

/// Test 15 : `proactive_recall_feedback` incrémente le counter `proactive_accepted`.
///
/// Session seedée avec 2 ULIDs surfacés ; feedback accepte 1 ULID (`ULID_A`).
/// Vérifie que `state.metrics.proactive_accepted` = 1 après l'appel.
///
/// ## Invariants
///
/// - Le counter est incrémenté du nombre d'`accepted_ulids` validés.
/// - Pas de label mode (accepted est cross-mode — agrégé toutes sessions).
#[tokio::test]
async fn feedback_metrics_accepted_counter_incremented() {
    let (state, _idx, _tmp) = build_state(ACL_MAIN_ALL).await;
    seed_session(
        &state,
        "recall-metrics-001",
        &[ULID_A.to_string(), ULID_B.to_string()],
    )
    .await;

    // Feedback avec 1 ULID accepté.
    proactive_recall_feedback(
        &state,
        &bearer_main(),
        req_feedback("recall-metrics-001", &[ULID_A]),
    )
    .await
    .expect("feedback OK");

    // Vérification du counter `proactive_accepted`.
    let count = state.metrics.proactive_accepted.get();
    assert_eq!(
        count, 1,
        "proactive_accepted doit être 1 après un feedback avec 1 ULID accepté, got {count}"
    );
}
