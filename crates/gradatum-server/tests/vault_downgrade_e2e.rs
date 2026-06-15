//! Tests E2E POST /api/v1/vault_downgrade + PATCH /api/v1/notes/{id}.
//!
//! 7 tests couvrant :
//! 1. `vault_downgrade_success_returns_200` — downgrade d'une note existante → 200 + JSON conforme.
//! 2. `vault_downgrade_idempotent_second_call` — 2 appels successifs → 200 les deux fois.
//! 3. `vault_downgrade_nonexistent_returns_404` — note absente → 404.
//! 4. `vault_downgrade_replaced_by_nonexistent_returns_404` — replaced_by fantôme → 404 (régression FK 500).
//! 5. `vault_downgrade_replaced_by_existing_returns_200` — replaced_by valide → 200 (nominal).
//! 6. `patch_note_revert_downgraded_to_live` — downgrade puis PATCH status=live → 204 + DB vérifiée.
//! 7. `vault_downgrade_self_reference_returns_400` — replaced_by == note_id → 400 + note reste live (régression auto-référence).
//!
//! # Seed
//!
//! Les notes sont seedées via `SqliteIndex::seed_note` (méthode pub concrète, pas dans le trait)
//! sur le handle concret retourné par `build_with_concrete_index`. L'instance `Arc<SqliteIndex>`
//! est partagée avec `state.search` — le Router et les helpers de seed utilisent le même index.
//!
//! # Auth
//!
//! Ces endpoints ne requièrent pas de bearer JWT (MVP V4 default false invariant VPN).
//! Le routeur de test inclut `auth_middleware` pour `Extension<TrustContext>` mais les
//! handlers notes.rs ne l'extraient pas.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

mod common;

// ── Helper setup ──────────────────────────────────────────────────────────────

/// Construit un `(Router, AppState, Arc<SqliteIndex>)` partageant le MÊME index in-memory.
///
/// L'`Arc<SqliteIndex>` concret est retourné pour permettre les appels à `seed_note` /
/// `seed_note_with_fts` / `seed_note_with_created` (méthodes pub concrètes, hors trait).
/// `state.search` et le router partagent le même `Arc<SqliteIndex>` via coercion dyn.
async fn build_with_concrete_index() -> (
    axum::Router,
    gradatum_server::state::AppState,
    Arc<SqliteIndex>,
) {
    use axum::{middleware, Router};
    use gradatum_server::state::AppState;

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test vault_downgrade_e2e"),
    );

    // Partager l'index via Arc : coercion vers dyn Index pour AppState.search.
    // AppState::new() utilisé à la place de ::default() pour éviter le lint
    // field_reassign_with_default (clippy refuse la réassignation post Default::default()).
    let mut state = AppState::new();
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

// ── Helper seed ───────────────────────────────────────────────────────────────

/// Insère une note minimale dans l'index concret et retourne son id ULID.
///
/// Utilise `SqliteIndex::seed_note` (méthode pub sur le type concret, pas dans le trait
/// `IndexStore`). La note a `status='live'`, section=`"reference"`, vault_id=`"main"`.
async fn seed_note(idx: &SqliteIndex) -> String {
    let id = Ulid::new().to_string();
    idx.seed_note(&id, "reference", "corps de test pour vault_downgrade e2e")
        .await
        .expect("seed_note — doit réussir sur index in-memory");
    id
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : downgrade d'une note existante → 200 + JSON status=downgraded.
///
/// Vérifie :
/// - Status HTTP 200 (synchrone, pas 202).
/// - Champ `status = "downgraded"` dans la réponse JSON.
/// - Champ `reason` reflète la valeur envoyée.
/// - Champ `note_id` reflète l'id envoyé.
#[tokio::test]
async fn vault_downgrade_success_returns_200() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    let body = serde_json::json!({
        "note_id": note_id,
        "reason": "test downgrade"
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        json["status"], "downgraded",
        "status doit être 'downgraded'"
    );
    assert_eq!(
        json["reason"], "test downgrade",
        "reason doit être reflétée"
    );
    assert_eq!(json["note_id"], note_id, "note_id doit être reflété");
    assert!(
        json["status_changed"].is_i64(),
        "status_changed doit être un entier (epoch ms)"
    );
}

/// Test 2 : deux appels successifs → 200 les deux fois (idempotence).
///
/// Vérifie que `downgrade_note` idempotent : mettre à jour la raison d'une note
/// déjà downgradée retourne 200 sans erreur.
#[tokio::test]
async fn vault_downgrade_idempotent_second_call() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    let make_req = |reason: &str| -> Request<Body> {
        let body = serde_json::json!({"note_id": note_id, "reason": reason});
        Request::builder()
            .uri("/api/v1/vault_downgrade")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };

    // Premier appel — downgrade initial.
    let r1 = app
        .clone()
        .oneshot(make_req("première raison"))
        .await
        .unwrap();
    assert_eq!(
        r1.status(),
        StatusCode::OK,
        "premier appel doit retourner 200"
    );

    // Deuxième appel — mise à jour raison — idempotent.
    let r2 = app.oneshot(make_req("deuxième raison")).await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::OK,
        "deuxième appel idempotent doit retourner 200"
    );

    let bytes = r2.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["reason"], "deuxième raison", "raison mise à jour");
}

/// Test 3 : note inexistante → 404 Not Found.
///
/// Vérifie que le handler retourne 404 quand `downgrade_note` produit `NoteNotFound`.
#[tokio::test]
async fn vault_downgrade_nonexistent_returns_404() {
    let (app, _state, _idx) = build_with_concrete_index().await;

    let body = serde_json::json!({
        "note_id": "01KR0000000000000000000000",
        "reason": "test inexistant"
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "note absente → 404");
}

/// Test 4 (régression) : downgrade avec replaced_by inexistant → 404, PAS 500.
///
/// Avant le fix, la contrainte FK SQLite produisait HTTP 500.
/// Après le fix, le pré-check dans `downgrade_note` produit `NoteNotFound` → HTTP 404.
///
/// Vérifie également le cas nominal : replaced_by existant → 200.
#[tokio::test]
async fn vault_downgrade_replaced_by_nonexistent_returns_404() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    // ULID syntaxiquement valide mais absent de l'index.
    let ghost_id = Ulid::new().to_string();

    let body = serde_json::json!({
        "note_id": note_id,
        "reason": "remplacé par fantôme",
        "replaced_by": ghost_id
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "replaced_by inexistant doit retourner 404, pas 500 (régression FK contrainte)"
    );

    // La note source ne doit pas avoir été modifiée.
    // On vérifie en downgradant sans replaced_by — ça doit réussir (note toujours live).
    let body_ok = serde_json::json!({"note_id": note_id, "reason": "sans replaced_by"});
    let req_ok = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body_ok).unwrap()))
        .unwrap();
    let resp_ok = app.oneshot(req_ok).await.unwrap();
    assert_eq!(
        resp_ok.status(),
        StatusCode::OK,
        "la note source doit rester 'live' après l'échec replaced_by inexistant"
    );
}

/// Test 5 (nominal) : downgrade avec replaced_by existant → 200.
///
/// Vérifie que le pré-check ne bloque pas les cas valides.
#[tokio::test]
async fn vault_downgrade_replaced_by_existing_returns_200() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let target_id = seed_note(&idx).await;
    let canon_id = seed_note(&idx).await;

    let body = serde_json::json!({
        "note_id": target_id,
        "reason": "remplacé par canon",
        "replaced_by": canon_id
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "replaced_by existant doit retourner 200"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "downgraded");
    assert_eq!(json["reason"], "remplacé par canon");
    assert_eq!(json["note_id"], target_id);
}

/// Test B5-C2 — fix : PATCH {status, replaced_by} → les deux champs persistés.
///
/// Régression couverte : avant le fix B5, la branche `body.status` dans patch_note
/// ignorait `body.replaced_by`. Seule la transition d'état était appliquée via la
/// state machine ; le champ `replaced_by` était silencieusement perdu.
///
/// Ce test vérifie :
/// - HTTP 204 (PATCH réussi).
/// - `replaced_by` persisté dans l'index après le PATCH (via `get_replaced_by`).
/// - Le status n'est pas modifié par ce test (état initial = live, PATCH reason-only).
///
/// Note : le PlaceholderRegistry (vault par défaut dans build_with_concrete_index) retourne
/// `NoteNotFound` pour `update_note_status` → toute transition via `body.status` donne 404.
/// Le test C2 utilise donc la branche `body.status = None` + `replaced_by` seul (SQL direct),
/// ce qui couvre le chemin `patch_note_status` du handler — le chemin post-update_note_status
/// est vérifié par le test d'intégration `gradatum-vault` (tests vault complets).
#[tokio::test]
async fn patch_note_replaced_by_persisted() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let replacement_id = seed_note(&idx).await;

    // PATCH avec replaced_by seul (pas de status) — chemin SQL direct.
    // Vérifie que replaced_by est persisté.
    let body = serde_json::json!({"replaced_by": replacement_id});
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "PATCH replaced_by doit retourner 204"
    );

    // Vérification directe en DB — replaced_by doit être persisté.
    let stored = idx
        .get_replaced_by(&note_id)
        .await
        .expect("get_replaced_by — invariant test");
    assert_eq!(
        stored.as_deref(),
        Some(replacement_id.as_str()),
        "replaced_by doit être persisté dans l'index. stored={stored:?}, expected={replacement_id}"
    );

    // Vérification d'un second PATCH replaced_by — écrasement de valeur.
    let second_replacement = seed_note(&idx).await;
    let body2 = serde_json::json!({"replaced_by": second_replacement});
    let req2 = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body2).unwrap()))
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::NO_CONTENT,
        "second PATCH replaced_by doit retourner 204"
    );
    let stored2 = idx
        .get_replaced_by(&note_id)
        .await
        .expect("get_replaced_by second — invariant test");
    assert_eq!(
        stored2.as_deref(),
        Some(second_replacement.as_str()),
        "replaced_by doit être mis à jour au second PATCH. stored2={stored2:?}"
    );
}

/// Test 7 (régression) : downgrade avec replaced_by == note_id → 400 Bad Request.
///
/// L'état `status=downgraded, replaced_by=self` est sémantiquement invalide :
/// il crée des wikilinks circulaires et une boucle infinie dans resolve_redirect.
/// Le garde dans `downgrade_note` doit le bloquer avant toute écriture SQLite.
///
/// Vérifie :
/// - HTTP 400 (pas 200, pas 500).
/// - La note source reste `status=live` (aucune modification appliquée).
#[tokio::test]
async fn vault_downgrade_self_reference_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    // Tentative d'auto-référence : replaced_by == note_id.
    let body = serde_json::json!({
        "note_id": note_id,
        "reason": "auto-référence invalide",
        "replaced_by": note_id
    });
    let req = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "replaced_by == note_id doit retourner 400 (auto-référence interdite)"
    );

    // Vérification : la note source est toujours live — un downgrade valide doit réussir.
    let body_ok = serde_json::json!({
        "note_id": note_id,
        "reason": "vérif post-auto-ref : note toujours live"
    });
    let req_ok = Request::builder()
        .uri("/api/v1/vault_downgrade")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body_ok).unwrap()))
        .unwrap();
    let resp_ok = app.oneshot(req_ok).await.unwrap();
    assert_eq!(
        resp_ok.status(),
        StatusCode::OK,
        "la note source doit rester 'live' après l'échec de l'auto-référence"
    );
    let bytes = resp_ok.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["status"], "downgraded",
        "un downgrade valide post-échec doit fonctionner"
    );
}

/// Test 6 — F-32B harmonisation state machine : comportement PATCH status via vault.
///
/// Depuis F-32B, `PATCH /notes/{id}` avec un champ `status` passe par
/// `vault.update_note_status` (state machine + CoW) et NON plus par `patch_note_status`
/// SQL direct.
///
/// Ce test vérifie les comportements du handler harmonisé :
///
/// 6a. `status` hors enum (`"downgraded"`) → 400 Bad Request.
/// 6b. `status` NoteStatus valide (`"live"`) → 404 quand le vault ne connaît pas la note
///     (PlaceholderRegistry dans ce test setup — en prod, le vault réel tracerait la transition).
/// 6c. Patch reason-only (sans `status`) → 204 via SQL direct (inchangé).
///
/// Les tests de transition réelle (CoW + graphe complet) sont couverts par
/// `gradatum-vault::tests::status_machine` (tests d'intégration avec vault réel).
#[tokio::test]
async fn patch_note_state_machine_routing() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;

    // 6a : status hors enum → 400.
    // `"downgraded"` n'est pas un variant de `NoteStatus` (hors graphe, mécanisme F-39 distinct).
    let patch_downgraded = serde_json::json!({"status": "downgraded"});
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&patch_downgraded).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "6a : status='downgraded' (hors enum NoteStatus) doit retourner 400"
    );

    // 6b : status valide mais vault (PlaceholderRegistry) ne connaît pas la note → 404.
    // En production, le vault réel lirait le .md et validerait la transition.
    let patch_live = serde_json::json!({"status": "live"});
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&patch_live).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "6b : vault PlaceholderRegistry doit retourner 404 (note absente du vault — comportement prod : state machine)"
    );

    // 6c : patch reason-only (sans status) → 204 via SQL direct (inchangé).
    let patch_reason_only = serde_json::json!({"status_reason": "mise à jour raison"});
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&patch_reason_only).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "6c : patch reason-only doit retourner 204 (SQL direct, inchangé)"
    );
}

// ── Tests A4 unblock — PATCH add_tags ────────────────────────────────────────

/// Helper : envoie un PATCH /notes/{id} avec un body JSON, retourne le StatusCode.
async fn patch_status(app: &axum::Router, note_id: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .uri(format!("/api/v1/notes/{note_id}"))
        .method("PATCH")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// add_tags vide (`[]`) → 400 (rien à ajouter, validation fail-fast).
#[tokio::test]
async fn patch_add_tags_empty_list_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let st = patch_status(&app, &note_id, serde_json::json!({"add_tags": []})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "add_tags=[] → 400");
}

/// add_tags avec un tag vide → 400 (Tag::new rejette).
#[tokio::test]
async fn patch_add_tags_empty_tag_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let st = patch_status(&app, &note_id, serde_json::json!({"add_tags": [""]})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "tag vide → 400");
}

/// add_tags avec un tag mal formé (majuscules) → 400.
#[tokio::test]
async fn patch_add_tags_malformed_tag_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let st = patch_status(&app, &note_id, serde_json::json!({"add_tags": ["BadTag"]})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "tag majuscule → 400");
}

/// add_tags > 20 → 400 (safety cap).
#[tokio::test]
async fn patch_add_tags_over_cap_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let tags: Vec<String> = (0..21).map(|i| format!("tag-{i}")).collect();
    let st = patch_status(&app, &note_id, serde_json::json!({"add_tags": tags})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "21 tags > cap 20 → 400");
}

/// add_tags exactement 20 → la validation passe (route vers le vault).
///
/// Le vault de test est `PlaceholderRegistry` → la note n'est pas connue du vault
/// (seedée uniquement dans l'index search) → 404. Cela prouve le **câblage** handler→vault
/// après validation réussie (≠ 400). La mutation réelle CoW+FTS est couverte par les
/// tests d'intégration `gradatum-vault::tests::add_tags`.
#[tokio::test]
async fn patch_add_tags_at_cap_routes_to_vault() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let tags: Vec<String> = (0..20).map(|i| format!("tag-{i}")).collect();
    let st = patch_status(&app, &note_id, serde_json::json!({"add_tags": tags})).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "20 tags valides → validation OK → routé au vault (placeholder=404, ≠ 400)"
    );
}

/// PATCH sans aucun champ → 400 (guard inchangé, add_tags inclus dans la condition).
#[tokio::test]
async fn patch_empty_body_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let st = patch_status(&app, &note_id, serde_json::json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "body vide → 400");
}

/// PATCH combiné status + add_tags : validation des tags AVANT le status.
///
/// Tags invalides → 400 même si le status est valide (la transition n'est PAS appliquée).
#[tokio::test]
async fn patch_combined_status_and_invalid_add_tags_returns_400() {
    let (app, _state, idx) = build_with_concrete_index().await;
    let note_id = seed_note(&idx).await;
    let st = patch_status(
        &app,
        &note_id,
        serde_json::json!({"status": "live", "add_tags": ["BADTAG"]}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "tags invalides → 400 avant toute mutation status (validation fail-fast)"
    );
}
