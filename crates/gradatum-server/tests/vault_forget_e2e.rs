//! Tests E2E F-44 — POST /api/v1/vault_forget · GET /api/v1/vault/forgotten
//!                   · POST /api/v1/vault/unforgot/{ulid}.
//!
//! # Cas de test
//!
//! 1. `dry_run_returns_preview` — dry-run retourne 200 + ForgetPreview.
//! 2. `protected_section_excluded_in_preview` — notes dans `council` absentes du lot.
//! 3. `dry_run_does_not_mutate_index` — index inchangé après dry-run.
//! 4. `mode_reel_confirm_ulids_exact_enqueues_202` — confirm exact → 202 + job_id.
//! 5. `mode_reel_confirm_ulids_mismatch_returns_400` — mismatch → 400.
//! 6. `unforgot_roundtrip` — mark_forgotten puis unforgot → restored.
//! 7. `forgotten_and_downgraded_coexistence` — deux statuts indépendants.
//! 8. `locus_scope_cross_vault_returns_403` (D3) — scope Locus vault≠tenant_id → 403.
//!
//! # Auth
//!
//! Le middleware de test `trust_all` injecte un `TrustContext` authentifié avec ACL Write.
//! Le test `mode_reel_confirm_ulids_exact_enqueues_202` câble un `SqliteQueueStore` in-memory.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_core::index::Index;
use gradatum_core::trust::TrustContext;
use gradatum_index::SqliteIndex;
use http_body_util::BodyExt;
use tower::ServiceExt;
use ulid::Ulid;

use gradatum_server::{api_v1, state::AppState};

// ── Preset ACL test ───────────────────────────────────────────────────────────

/// Preset ACL autorisant read + write sur `main/*` pour `test-admin`.
/// Consumer identity = `"test-admin"`, correspond au `sub` injecté par `trust_all`.
const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-admin"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Construit un router de test avec un `SqliteIndex` in-memory concret + job_store in-memory.
///
/// ACL configurée pour autoriser read + write sur `main/*` pour `TEST_SUB`.
///
/// Retourne `(Router, AppState, Arc<SqliteIndex>)`.
async fn build_app() -> (Router, AppState, Arc<SqliteIndex>) {
    use gradatum_auth::jwt::JwtService;
    use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
    use sqlx::sqlite::SqlitePoolOptions;

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory — vault_forget_e2e"),
    );

    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — vault_forget_e2e");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — vault_forget_e2e");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_PRESET)
        .expect("preset ACL vault_forget_e2e — invariant statique");

    let state = AppState::with_jwt_and_acl(jwt, acl);
    // Câbler l'index concret.
    let state = {
        let mut s = state;
        s.search = Arc::clone(&idx) as Arc<dyn Index>;
        s
    };
    let state = state.with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_all))
        .with_state(state.clone());

    (app, state, idx)
}

/// Middleware de test : injecte un `TrustContext` BearerToken authentifié (ACL Write accordé).
async fn trust_all(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "test-admin".to_string(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".to_string(),
    });
    next.run(req).await
}

/// Insère une note FTS dans l'index (vault_id='main', status='live').
async fn seed_fts(idx: &SqliteIndex, section: &str, body: &str) -> String {
    let id = Ulid::new().to_string();
    idx.seed_note_with_fts(&id, section, body)
        .await
        .expect("seed_note_with_fts — vault_forget_e2e");
    id
}

/// POST /api/v1/vault_forget avec le body JSON fourni.
async fn post_forget(app: Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/vault_forget")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : dry-run avec scope topic → 200 + preview {ulids, count, dry_run=true}.
#[tokio::test]
async fn dry_run_returns_preview() {
    let (app, _state, idx) = build_app().await;

    // Seed une note avec un contenu cherchable.
    let id = seed_fts(&idx, "decisions", "projet expérimental alpha").await;

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "expérimental", "vault": "main", "limit": 10 },
        "dry_run": true
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(status, StatusCode::OK, "dry-run doit retourner 200: {json}");
    assert_eq!(json["dry_run"], true, "dry_run flag doit être true");
    // La note seedée doit apparaître dans ulids ou au moins count >= 0.
    let count = json["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "au moins la note seedée doit être candidate: {json}"
    );
    let ulids = json["ulids"].as_array().expect("ulids doit être un array");
    assert!(
        ulids.iter().any(|u| u.as_str() == Some(&id)),
        "note seedée {id} doit apparaître dans ulids: {json}"
    );
}

/// Test 2 : notes dans sections protégées (council) → exclues, pas dans ulids.
#[tokio::test]
async fn protected_section_excluded_in_preview() {
    let (app, _state, idx) = build_app().await;

    // Note dans section protégée.
    let protected_id = seed_fts(&idx, "council", "décision gouvernance confidentielle").await;
    // Note normale.
    let normal_id = seed_fts(&idx, "decisions", "décision normale").await;

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "décision", "vault": "main", "limit": 20 },
        "dry_run": true
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(status, StatusCode::OK, "preview doit être 200: {json}");
    let ulids = json["ulids"].as_array().expect("ulids doit être un array");
    let excluded = json["excluded"]
        .as_array()
        .expect("excluded doit être un array");

    // Note council dans excluded, pas dans ulids.
    let in_ulids = ulids.iter().any(|u| u.as_str() == Some(&protected_id));
    let in_excluded = excluded
        .iter()
        .any(|e| e["ulid"].as_str() == Some(&protected_id));
    assert!(
        !in_ulids,
        "note council ne doit PAS être dans ulids: {json}"
    );
    assert!(in_excluded, "note council doit être dans excluded: {json}");

    // Note normale dans ulids.
    assert!(
        ulids.iter().any(|u| u.as_str() == Some(&normal_id)),
        "note normale doit être dans ulids: {json}"
    );
}

/// Test 3 : dry-run ne modifie pas l'index (forgotten_at reste NULL).
#[tokio::test]
async fn dry_run_does_not_mutate_index() {
    let (app, _state, idx) = build_app().await;
    let id = seed_fts(&idx, "retrospectives", "rapport rétrospective projet").await;

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "rétrospective", "vault": "main", "limit": 10 },
        "dry_run": true
    });
    let (status, _json) = post_forget(app, body).await;
    assert_eq!(status, StatusCode::OK);

    // Vérifier que is_note_forgotten retourne false (aucune mutation).
    let is_forgotten = idx
        .is_note_forgotten("main", &id)
        .await
        .expect("is_note_forgotten doit réussir");
    assert!(
        !is_forgotten,
        "dry-run ne doit PAS marquer la note comme forgotten"
    );
}

/// Test 4 : mode réel avec confirm_ulids exacts → 202 + job_id présent.
#[tokio::test]
async fn mode_reel_confirm_ulids_exact_enqueues_202() {
    let (app, _state, idx) = build_app().await;
    let id = seed_fts(&idx, "decisions", "expérimentation cloud migration").await;

    // Étape 1 : dry-run pour obtenir les ULIDs.
    let dry_body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "expérimentation", "vault": "main", "limit": 10 },
        "dry_run": true
    });
    let (status_dry, json_dry) = post_forget(app.clone(), dry_body).await;
    assert_eq!(status_dry, StatusCode::OK, "dry-run préalable: {json_dry}");

    let ulids_from_preview: Vec<String> = json_dry["ulids"]
        .as_array()
        .expect("ulids doit être un array")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(
        !ulids_from_preview.is_empty(),
        "au moins 1 ULID attendu en preview"
    );
    assert!(
        ulids_from_preview.contains(&id),
        "note seedée doit être dans la preview"
    );

    // Étape 2 : mode réel avec confirm_ulids = ulids de la preview.
    let real_body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "expérimentation", "vault": "main", "limit": 10 },
        "dry_run": false,
        "confirm_ulids": ulids_from_preview
    });
    let (status_real, json_real) = post_forget(app, real_body).await;

    assert_eq!(
        status_real,
        StatusCode::ACCEPTED,
        "mode réel avec confirm_ulids exacts doit retourner 202: {json_real}"
    );
    let job_id = json_real["job_id"]
        .as_str()
        .expect("job_id doit être présent");
    assert!(!job_id.is_empty(), "job_id ne doit pas être vide");
    let poll_url = json_real["poll_url"]
        .as_str()
        .expect("poll_url doit être présent");
    assert!(
        poll_url.starts_with("/api/v1/jobs/"),
        "poll_url doit commencer par /api/v1/jobs/: {poll_url}"
    );
}

/// Test 5 : confirm_ulids ne correspondent pas à la preview → 400.
#[tokio::test]
async fn mode_reel_confirm_ulids_mismatch_returns_400() {
    let (app, _state, idx) = build_app().await;
    let _id = seed_fts(&idx, "decisions", "mismatch guard test").await;

    // Mode réel avec un ULID fantôme.
    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "mismatch", "vault": "main", "limit": 10 },
        "dry_run": false,
        "confirm_ulids": ["01JFAKE00000000000000000XX"]
    });
    let (status, _json) = post_forget(app, body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "confirm_ulids mismatch doit retourner 400"
    );
}

/// Test 6 : mark_forgotten directe → GET /vault/forgotten retourne la note → unforgot → disparaît.
///
/// Ce test valide le round-trip complet unforgot :
/// 1. Marque la note forgotten directement via `idx.mark_forgotten`.
/// 2. GET /vault/forgotten → note présente.
/// 3. POST /vault/unforgot/{ulid} → 200 + status="restored".
/// 4. Note absente de is_note_forgotten.
#[tokio::test]
async fn unforgot_roundtrip() {
    let (app, _state, idx) = build_app().await;
    let id = seed_fts(&idx, "decisions", "note à restaurer").await;

    // 1. Marquer forgotten directement.
    idx.mark_forgotten("main", &id, Some("test-admin"))
        .await
        .expect("mark_forgotten doit réussir");
    assert!(
        idx.is_note_forgotten("main", &id).await.unwrap(),
        "note doit être forgotten après mark_forgotten"
    );

    // 2. GET /vault/forgotten → note présente.
    let req = Request::builder()
        .uri("/api/v1/vault/forgotten?limit=50")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let notes = list["notes"].as_array().expect("notes doit être un array");
    assert!(
        notes.iter().any(|n| n["ulid"].as_str() == Some(&id)),
        "note oubliée doit apparaître dans /vault/forgotten: {list}"
    );

    // 3. POST /vault/unforgot/{ulid} → 200 + status=restored.
    let req = Request::builder()
        .uri(format!("/api/v1/vault/unforgot/{id}"))
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "unforgot doit retourner 200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["status"], "restored",
        "status doit être 'restored': {json}"
    );
    assert_eq!(json["ulid"], id, "ulid doit correspondre");

    // 4. La note ne doit plus être forgotten.
    assert!(
        !idx.is_note_forgotten("main", &id).await.unwrap(),
        "note ne doit plus être forgotten après unforgot"
    );
}

/// Test — C8 : confirm_ulids avec 201 entrées → 400 Bad Request (borne max=200).
///
/// Protège contre des requêtes de confirmation pathologiques dépassant le cap
/// cohérent avec le scope Topic limit=200.
#[tokio::test]
async fn confirm_ulids_over_limit_returns_400() {
    let (app, _state, _idx) = build_app().await;

    // Générer 201 ULIDs fantômes.
    let too_many: Vec<String> = (0..201).map(|_| Ulid::new().to_string()).collect();

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "quelconque", "vault": "main", "limit": 10 },
        "dry_run": false,
        "confirm_ulids": too_many
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "confirm_ulids avec 201 entrées doit retourner 400: {json}"
    );
}

/// Test — G10 : forgotten_by dépassant la borne → 400 Bad Request (anti DoS stockage).
///
/// `forgotten_by` est persisté une fois par note du batch (colonne + frontmatter).
/// Une valeur non bornée est amplifiée sur tout le périmètre. La borne (512 octets)
/// est rejetée de façon déterministe à la frontière HTTP.
#[tokio::test]
async fn forgotten_by_over_limit_returns_400() {
    let (app, _state, _idx) = build_app().await;

    // forgotten_by de 513 octets (> MAX_FORGOTTEN_BY_LEN = 512).
    let oversized = "a".repeat(513);

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "quelconque", "vault": "main", "limit": 10 },
        "dry_run": true,
        "forgotten_by": oversized
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "forgotten_by de 513 octets doit retourner 400: {json}"
    );
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("forgotten_by") && error.contains("borne"),
        "le message d'erreur doit expliquer le dépassement de borne: {json}"
    );
}

/// Test — G10 : forgotten_by à la borne exacte (512 octets) → accepté (pas de 400).
#[tokio::test]
async fn forgotten_by_at_limit_is_accepted() {
    let (app, _state, idx) = build_app().await;
    let _id = seed_fts(&idx, "decisions", "note borne forgotten_by").await;

    // forgotten_by de 512 octets (== MAX_FORGOTTEN_BY_LEN) → autorisé.
    let at_limit = "a".repeat(512);

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": { "type": "topic", "query": "borne", "vault": "main", "limit": 10 },
        "dry_run": true,
        "forgotten_by": at_limit
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "forgotten_by de 512 octets (borne exacte) doit être accepté: {json}"
    );
}

/// Test D3 : scope Locus ciblant un vault ≠ tenant_id → 403 Forbidden (mono-tenant v0.4.x).
///
/// L'ACL est évaluée sur `tenant_id="main"` (authentifié), mais le scope cible
/// `vault="autre"`. Le handler doit refuser avec 403 + message explicite.
#[tokio::test]
async fn locus_scope_cross_vault_returns_403() {
    let (app, _state, _idx) = build_app().await;

    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": {
            "type": "locus",
            "vault": "autre",        // ≠ tenant_id → cross-vault interdit v0.4.x
            "locus": "inbox/old/"
        },
        "dry_run": true
    });
    let (status, json) = post_forget(app, body).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "scope Locus cross-vault doit retourner 403: {json}"
    );
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("cross-vault") || error.contains("v0.4.x") || error.contains("autre"),
        "message d'erreur doit expliquer le refus cross-vault: {json}"
    );
}

/// Test 7 : une note peut être à la fois forgotten ET downgraded (indépendance statuts).
///
/// Le statut `forgotten` (colonne `forgotten`) est orthogonal à `status` ('live' / 'downgraded').
/// L'index peut représenter les deux états simultanément.
#[tokio::test]
async fn forgotten_and_downgraded_coexistence() {
    let (app, _state, idx) = build_app().await;
    let id = seed_fts(&idx, "decisions", "note double statut").await;

    // Downgrade via l'index.
    use gradatum_core::identity::NoteId;
    let nid = NoteId(Ulid::from_string(&id).expect("ULID parse coexistence"));
    idx.downgrade_note(&nid, "test raison coexistence", None)
        .await
        .expect("downgrade_note doit réussir");

    // Marquer forgotten.
    idx.mark_forgotten("main", &id, None)
        .await
        .expect("mark_forgotten doit réussir");

    // Vérifier les deux états.
    assert!(
        idx.is_note_forgotten("main", &id).await.unwrap(),
        "note doit être forgotten"
    );

    // GET /vault/forgotten → note présente (pas filtrée par downgraded).
    let req = Request::builder()
        .uri("/api/v1/vault/forgotten?limit=50")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let notes = list["notes"].as_array().expect("notes doit être un array");
    assert!(
        notes.iter().any(|n| n["ulid"].as_str() == Some(&id)),
        "note downgraded+forgotten doit apparaître dans /vault/forgotten: {list}"
    );
}
