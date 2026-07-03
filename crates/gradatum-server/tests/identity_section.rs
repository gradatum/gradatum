//! Tests E2E section `identity` — F-34 v0.7.3.
//!
//! Couvre Task 3 + Task 4 du plan `2026-06-27-v0.7.3-identity-f50-deport.md`.
//!
//! # Cas de test
//!
//! 1. `identity_write_bad_schema_rejected_400` — body sans GATES/INV-CANARY → 400.
//! 2. `identity_write_good_schema_accepted_202` — soul valide → 202.
//! 3. `identity_write_foreign_agent_denied_403` — frontend tente d'écrire identity/main → 403.
//! 4. `identity_forget_blocked_protected` — note identity dans `excluded` (PROTECTED_FORGET).
//!
//! # Cross-tenant IDOR (skippé)
//!
//! `identity_read_cross_tenant_denied` est skippé : le middleware `auth_middleware` en
//! production rejette en amont tout token avec `tenant_id != "main"` (via
//! `tenant_is_authorized`). `effective_tenant` bloque la divergence body/JWT et est
//! déjà couvert par les tests unitaires de `tenant_guard.rs`. Un test E2E vault_read
//! identity cross-tenant ne testerait pas la logique identity spécifiquement —
//! il testerait le comportement transversal de `effective_tenant`.
//!
//! # Auth
//!
//! Les tests injectent directement `TrustContext` via des middlewares de test dédiés,
//! sans passer par le JWT `auth_middleware` — permet de contrôler précisément le `sub`
//! et le `tenant_id` pour chaque scénario.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_server::{api_v1, state::AppState};
use http_body_util::BodyExt;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

// ── Preset ACL ───────────────────────────────────────────────────────────────

/// ACL de test : main-agent et frontend ont tous deux accès Write sur `main/**`.
/// La garde write-restrictive identity (task 3) différencie les deux, pas l'ACL.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "main-agent"
read_patterns  = ["**"]
write_patterns = ["**"]

[[consumer]]
identity = "frontend"
read_patterns  = ["**"]
write_patterns = ["**"]

[[consumer]]
identity = "test-identity"
read_patterns  = ["**"]
write_patterns = ["**"]
"#;

// ── Soul body de test ─────────────────────────────────────────────────────────

/// Soul bien formée : 3 sections + INV-CANARY + pas de champ dynamique (C4/C8).
const GOOD_SOUL: &str = "\
## INVARIANTS
INV-CANARY | REQUIRED | response.prefix matches ^\\(TODAY\\):
INV-LANG | REQUIRED | response.language == fr

## GATES
GATE-PIPELINE | multi_step OR service_live -> invoke gov-pipeline-agents

## NARRATIVE
Tu es le Général en Chef. Ton: direct, FR.
";

// ── Middlewares d'injection de TrustContext ───────────────────────────────────

/// Injecte `TrustContext` pour `main-agent` (sub="main-agent", tenant="main").
/// Owner privilégié SSI — autorisé à écrire toute âme.
async fn inject_main_agent(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "main-agent".to_string(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".to_string(),
    });
    next.run(req).await
}

/// Injecte `TrustContext` pour `frontend` (sub="frontend", tenant="main").
/// Agent non-privilégié : ne peut écrire que `identity/frontend`.
async fn inject_frontend(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "frontend".to_string(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".to_string(),
    });
    next.run(req).await
}

/// Injecte `TrustContext` pour `test-identity` (sub="test-identity", tenant="main").
/// Utilisé pour le test PROTECTED_FORGET — sub distinct pour ne pas interférer.
async fn inject_test_identity(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "test-identity".to_string(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".to_string(),
    });
    next.run(req).await
}

// ── Helpers : build_app* ──────────────────────────────────────────────────────

/// Construit un `SqliteIndex` en mémoire + un `AppState` avec `SqliteQueueStore` in-memory.
///
/// Retourne `(state, Arc<SqliteIndex>)` — les apps sont construites séparément
/// pour permettre d'injecter des middlewares de trust différents.
async fn build_base(acl: AclEngine) -> (AppState, Arc<SqliteIndex>) {
    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory — identity_section"),
    );

    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — identity_section");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — identity_section");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    let state = state.with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool);

    (state, idx)
}

/// App avec TrustContext `main-agent` — acl permissive.
async fn app_main_agent() -> (Router, Arc<SqliteIndex>) {
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL identity_section");
    let (state, idx) = build_base(acl).await;
    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(inject_main_agent))
        .with_state(state);
    (app, idx)
}

/// App avec TrustContext `frontend`.
async fn app_frontend() -> Router {
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL frontend");
    let (state, _idx) = build_base(acl).await;
    Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(inject_frontend))
        .with_state(state)
}

/// App avec TrustContext `test-identity` pour le test forget.
async fn app_test_identity() -> (Router, Arc<SqliteIndex>) {
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL test-identity");
    let (state, idx) = build_base(acl).await;
    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(inject_test_identity))
        .with_state(state);
    (app, idx)
}

// ── Helpers : HTTP ────────────────────────────────────────────────────────────

/// POST JSON sur `path` — retourne `(StatusCode, Value)`.
async fn post_json(
    app: &Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(path)
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("serde body")))
        .expect("request builder");
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ── Tests Task 3 — vault_write identity ──────────────────────────────────────

/// Body soul invalide (section GATES absente + INV-CANARY manquant) → 400.
///
/// Vérifie que le validateur soul est branché avant l'enqueue (A1/C4, bypass LLM).
#[tokio::test]
async fn identity_write_bad_schema_rejected_400() {
    let (app, _idx) = app_main_agent().await;
    let body = serde_json::json!({
        "title": "identity/main",
        "body": "## INVARIANTS\nINV-LANG | x\n## NARRATIVE\nn\n",
        "section_hint": "identity",
        "author": "main-agent",
        "tenant_id": "main"
    });
    let (status, json) = post_json(&app, "/api/v1/vault_write", body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "soul invalide (GATES absent + INV-CANARY manquant) doit retourner 400: {json}"
    );
}

/// Soul valide écrite par main-agent sur identity/main → 202 enqueued.
///
/// Vérifie que le chemin happy-path passe le validateur soul et atteint l'enqueue.
///
/// # Note M3 — `doc_kind` not asserted here
///
/// `vault_write` is asynchronous: the server enqueues a job (202) and returns immediately.
/// The actual note persistence (including `doc_kind` assignment by `section_to_doc_kind`)
/// happens in the worker, which is not driven in this harness.  Asserting `doc_kind="Static"`
/// here would require draining the job queue and reading back the stored note — an
/// integration pattern already validated by `e2e_write.rs`, not specific to identity.
///
/// The casing of `doc_kind` in the SQL backfill is guarded at the source by
/// `identity_backfill_sql_0024_correct_case` in `gradatum-core/src/section.rs`
/// (P1-bis finding reviewer v0.7.3), which reads the real `.sql` file and fails on
/// any wrong-case literal.  That test provides stronger coverage than an E2E
/// job-drain round-trip for this particular regression.
#[tokio::test]
async fn identity_write_good_schema_accepted_202() {
    let (app, _idx) = app_main_agent().await;
    let body = serde_json::json!({
        "title": "identity/main",
        "body": GOOD_SOUL,
        "section_hint": "identity",
        "author": "main-agent",
        "tenant_id": "main"
    });
    let (status, json) = post_json(&app, "/api/v1/vault_write", body).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "soul valide écrite par main-agent doit retourner 202: {json}"
    );
    assert!(
        json["job_id"].is_string(),
        "job_id doit être présent dans la réponse 202: {json}"
    );
}

/// Agent `frontend` tente d'écrire `identity/main` (âme de `main-agent`) → 403.
///
/// Prouve la write-restrictive ACL (C6) : caller_sub="frontend" != target_agent="main".
/// L'agent privilégié `"main-agent"` est le seul à pouvoir écrire identity/main.
#[tokio::test]
async fn identity_write_foreign_agent_denied_403() {
    let app = app_frontend().await;
    let body = serde_json::json!({
        "title": "identity/main",
        "body": GOOD_SOUL,
        "section_hint": "identity",
        "author": "frontend",
        "tenant_id": "main"
    });
    let (status, json) = post_json(&app, "/api/v1/vault_write", body).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas pouvoir écrire identity/main (âme étrangère) — attendu 403: {json}"
    );
}

/// Titre `identity/...` sans `section_hint="identity"` → 400 (anti-bypass write-restrictive).
///
/// Prouve que la garde pré-bloc (P1 sécu) rejette les titres `identity/` sans section_hint,
/// empêchant de contourner la check write-restrictive en omettant section_hint.
#[tokio::test]
async fn identity_write_no_section_hint_with_identity_title_rejected() {
    let app = app_frontend().await;
    let body = serde_json::json!({
        "title": "identity/main-agent",
        "body": GOOD_SOUL,
        "author": "frontend",
        "tenant_id": "main"
    });
    let (status, _json) = post_json(&app, "/api/v1/vault_write", body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "titre identity/ sans section_hint=identity doit être rejeté 400"
    );
}

// ── Tests Task 4 — sécurité section identity ──────────────────────────────────

/// Note identity dans l'index → présente dans `excluded` lors d'un forget dry-run.
///
/// Prouve que `Section::PROTECTED_FORGET` inclut `Identity` (Task 1) et que
/// `vault_forget` exclut silencieusement les sections protégées (non-régression).
///
/// Note : `vault_forget` exclut les notes protégées dans le champ `excluded` de la
/// réponse preview — pas de 403/400. C'est le comportement canonique de PROTECTED_FORGET
/// (voir `vault_forget_e2e.rs::protected_section_excluded_in_preview`).
#[tokio::test]
async fn identity_forget_blocked_protected() {
    let (app, idx) = app_test_identity().await;

    // Générer l'ULID de la note identity et la seeder dans l'index.
    let identity_ulid = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &identity_ulid,
        "identity",
        "âme agent test-identity identité",
    )
    .await
    .expect("seed_note_with_fts identity — identity_section");

    // Dry-run forget par topic couvrant la note identity.
    let body = serde_json::json!({
        "tenant_id": "main",
        "scope": {
            "type": "topic",
            "query": "âme agent identité",
            "vault": "main",
            "limit": 10
        },
        "dry_run": true
    });
    let (status, json) = post_json(&app, "/api/v1/vault_forget", body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "vault_forget dry-run doit retourner 200: {json}"
    );

    let ulids = json["ulids"].as_array().expect("ulids doit être un array");
    let excluded = json["excluded"]
        .as_array()
        .expect("excluded doit être un array");

    // La note identity ne doit PAS être dans les ulids éligibles (PROTECTED_FORGET).
    let in_ulids = ulids
        .iter()
        .any(|u| u.as_str() == Some(identity_ulid.as_str()));
    assert!(
        !in_ulids,
        "note identity ne doit PAS être dans ulids éligibles: {json}"
    );

    // La note identity DOIT être dans excluded (PROTECTED_FORGET).
    let in_excluded = excluded
        .iter()
        .any(|e| e["ulid"].as_str() == Some(identity_ulid.as_str()));
    assert!(
        in_excluded,
        "note identity doit être dans excluded (PROTECTED_FORGET): {json}"
    );
}

// ── Tests read-restrictive identity (F-34 v0.7.3, A2/C6) ─────────────────────

/// Environnement de lecture : Vault réel sur TempDir + JWT `auth_middleware`.
///
/// Contrairement aux apps d'écriture (qui injectent un `TrustContext` figé sur un
/// `PlaceholderRegistry`), ce harness branche un vrai `Vault` disque pour que
/// `read_note_by_id` réussisse, et utilise le JWT réel pour pouvoir signer des tokens
/// avec un `sub` arbitraire (`main-agent`, `frontend`, …) tout en gardant l'ACL
/// permissive `["**"]` (la garde read-restrictive identity est le seul discriminant).
///
/// Le `TempDir` est retourné et DOIT rester vivant pendant le test (sinon le vault
/// disque disparaît et `read_note_by_id` échoue).
struct ReadEnv {
    app: Router,
    state: AppState,
    vault: Arc<gradatum_vault::Vault>,
    _tmp: TempDir,
}

/// Construit un [`ReadEnv`] : Vault réel + ACL `TEST_ACL` + JWT éphémère.
async fn build_read_env() -> ReadEnv {
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir read-env identity_section");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create — read-env identity_section"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL read-env");
    let mut state =
        AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl).with_vault_arc(vault_registry);
    state.search = index;

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    ReadEnv {
        app,
        state,
        vault,
        _tmp: tmp,
    }
}

/// Signe un JWT `read`-scope pour le `sub` fourni, tenant `main`.
fn sign_for(state: &AppState, sub: &str) -> String {
    state
        .jwt
        .sign(sub, &["read".to_string()], TokenScope::Service, "main")
        .expect("sign JWT read-env")
}

/// Seed une âme `identity/<agent>` lisible : écrit le fichier `.md` via
/// `Vault::write_note` (section `Identity`) puis upsert le titre dans l'index — de
/// sorte que `title_lookup` et `get_titles_sections` résolvent `identity/<agent>`.
///
/// Retourne l'ULID de la note seedée (pour la lecture par ULID nu, Finding #2).
async fn seed_soul(env: &ReadEnv, agent: &str) -> String {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let title = format!("identity/{agent}");
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Identity,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let body = format!("# {title}\n{GOOD_SOUL}");
    let note = env
        .vault
        .write_note(frontmatter, body)
        .await
        .expect("vault.write_note seed_soul");
    env.state
        .search
        .upsert_note_title(&note.id, &title)
        .await
        .expect("upsert_note_title seed_soul");
    note.id.to_string()
}

/// POST JSON authentifié (Bearer) sur `path` — retourne `(StatusCode, Value)`.
async fn post_json_auth(
    app: &Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(path)
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).expect("serde body")))
        .expect("request builder");
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// `frontend` lit `identity/main` par PATH → 403 (Finding #1).
///
/// L'ACL autorise `["**"]` en lecture : seule la garde read-restrictive identity
/// (section réelle = `identity`, caller `frontend` ≠ propriétaire `main`) refuse.
#[tokio::test]
async fn identity_read_foreign_agent_denied_403() {
    let env = build_read_env().await;
    let _ulid = seed_soul(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token,
        serde_json::json!({
            "path": "identity/main",
            "section": "identity",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas pouvoir lire identity/main (âme étrangère) — attendu 403: {json}"
    );
}

/// `frontend` lit l'âme de `main` par ULID nu (`section` omis) → 403 (Finding #2).
///
/// Prouve que la garde se fonde sur la section RÉELLE de la note résolue, pas sur
/// `req.section` : l'adressage par ULID (qui court-circuiterait une garde basée sur
/// `req.section`) ne contourne pas la restriction.
#[tokio::test]
async fn identity_read_by_ulid_foreign_denied_403() {
    let env = build_read_env().await;
    let ulid = seed_soul(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token,
        serde_json::json!({
            "path": ulid,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "lecture par ULID nu d'une âme étrangère doit être refusée 403 (Finding #2): {json}"
    );
}

/// Chemins nominaux préservés : owner privilégié `main-agent` lit `identity/main`,
/// et un agent lit sa propre âme → 200 dans les deux cas.
#[tokio::test]
async fn identity_read_own_and_owner_ok() {
    let env = build_read_env().await;
    let _ulid_main = seed_soul(&env, "main").await;
    let _ulid_frontend = seed_soul(&env, "frontend").await;

    // (a) Owner privilégié `main-agent` lit identity/main (soul_instructions Task 6′).
    let token_owner = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token_owner,
        serde_json::json!({
            "path": "identity/main",
            "section": "identity",
            "tenant_id": "main"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (owner privilégié) doit lire identity/main — attendu 200: {json}"
    );

    // (b) Un agent lit sa PROPRE âme.
    let token_frontend = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token_frontend,
        serde_json::json!({
            "path": "identity/frontend",
            "section": "identity",
            "tenant_id": "main"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "frontend doit lire sa propre âme identity/frontend — attendu 200: {json}"
    );
}

// ── Tests non-régression : convention H1 + title_lookup ──────────────────────
//
// Root-cause gap F-34 : `title_lookup` (gradatum-index/src/queries.rs) matche
// `body_text LIKE '# {title}\n%'`. Une âme sans H1 `# identity/<agent>` en tête
// de body est introuvable par path → `soul_instructions` retourne `None` silencieusement
// → injection MCP désactivée sans aucun message d'erreur.
//
// Les tests E2E existants (`identity_read_*`) passaient car `seed_soul` embarquait
// déjà un H1 — ils ne couvraient PAS le cas « soul sans H1 ».

/// Soul avec H1 canonique → résolution par path réussie, content non vide.
///
/// `seed_soul` génère toujours `body = "# identity/{agent}\n{GOOD_SOUL}"`.
/// `title_lookup` matche `body_text LIKE '# identity/main\n%'` → résout l'ULID
/// → `vault_read_impl` lit la note → 200 + content non vide.
///
/// Ce test nomme explicitement la convention H1 comme condition de résolvabilité
/// par path — la propriété que les tests de garde ACL ne documentent pas.
#[tokio::test]
async fn identity_soul_with_h1_resolvable_by_path() {
    let env = build_read_env().await;
    let _ulid = seed_soul(&env, "main").await; // body = "# identity/main\n{GOOD_SOUL}"

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token,
        serde_json::json!({
            "path": "identity/main",
            "section": "identity",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "soul avec H1 `# identity/main` doit être résolvable par path — attendu 200: {json}"
    );
    let content = json["content"].as_str().unwrap_or("");
    assert!(
        !content.is_empty(),
        "content ne doit pas être vide quand la note est résolue par path: {json}"
    );
}

/// Soul valide au schéma soul (INV-CANARY + sections) mais sans H1 → 404 par path.
///
/// `title_lookup` exige `body_text LIKE '# identity/main\n%'` (H1 en première position).
/// Un body commençant par `## INVARIANTS` (sans `# identity/main`) ne satisfait PAS ce
/// pattern → `Ok(None)` → `Storage("introuvable : identity/main")` → HTTP 404.
///
/// Conséquence opérationnelle : `soul_instructions` (mcp.rs) appelle `vault_read_impl`
/// par `path="identity/<agent>"`. Si l'âme n'a pas de H1, `soul_instructions` retourne
/// `None` silencieusement, désactivant l'injection MCP sans message d'erreur.
///
/// Note : `validate_soul` accepte un body avec H1 en tête car `extract_section`
/// cherche `## SECTION` (niveau 2) — une ligne `# identity/main` (niveau 1) est
/// ignorée par le parser. Le H1 n'est donc pas un champ soul, c'est la convention
/// de nommage de `title_lookup`. Le seed de production DOIT inclure le H1.
///
/// Ce test aurait attrapé le défaut décrit dans la root-cause F-34.
#[tokio::test]
async fn identity_soul_without_h1_unreachable_by_path() {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let env = build_read_env().await;

    // Seed une note identity VALIDE au schéma soul (INV-CANARY + GATES + NARRATIVE)
    // mais SANS H1 — body commence directement par `## INVARIANTS` (= GOOD_SOUL pur).
    // `validate_soul` l'accepte ; `title_lookup` ne le trouvera PAS par path.
    // Note : le path "identity/main" est codé directement dans le json vault_read ci-dessous.
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Identity,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    // Corps valide soul sans H1 `# identity/main` — title_lookup ne matchera pas.
    // `let _ =` : l'appel est conservé pour son effet de bord (seed de la note) ;
    // la valeur (NoteId) n'est pas utilisée dans ce test.
    let _ = env
        .vault
        .write_note(frontmatter, GOOD_SOUL.to_string())
        .await
        .expect(
            "vault.write_note seed note sans H1 — identity_soul_without_h1_unreachable_by_path",
        );
    // Ne PAS appeler upsert_note_title ici.
    // Depuis v0.7.3 Slice A (Task 1), title_lookup cherche la colonne `title` en priorité.
    // Si la colonne était peuplée, la note serait résolvable même sans H1 (comportement voulu).
    // Ce test documente le cas : soul sans H1 ET sans colonne title → introuvable par path.
    //
    // Voir `title_lookup_resolution::title_lookup_resolves_by_title_column_when_populated`
    // pour le cas inverse (colonne peuplée, pas de H1 → résolvable).

    // Lecture par path (non-ULID) :
    // passe 1 colonne title : aucune note (colonne vide) → Ok(None).
    // passe 2 LIKE H1 : aucun match (body commence par ## INVARIANTS) → Ok(None).
    // → resolve_redirect → Ok(None) → Storage("introuvable") → 404.
    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_read",
        &token,
        serde_json::json!({
            "path": "identity/main",
            "section": "identity",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "soul sans H1 `# identity/main` doit être introuvable par path — attendu 404: {json}"
    );
}

// ── Tests P1-B — vault_search exclut section identity ────────────────────────
//
// security-reviewer v0.7.3 : `vault_search_impl` renvoyait les notes section=identity
// (titre + snippet corps) pour tout caller authentifié, alors que `vault_read_impl`
// possède une garde read-restrictive depuis Task 4 (F-34). Parité rétablie : filtrage
// post-items dans `vault_search_impl` symétrique au guard vault_read_impl (~L671).
//
// Callers autorisés : TrustContext::Studio (admin) OU subject == SOUL_PRIVILEGED_WRITER.
// Callers exclus    : tout autre BearerToken (agents non-privilégiés, ex: "frontend").

/// Construit un app JWT + index in-memory pour les tests de recherche identity.
///
/// Différence vs `build_read_env` : pas de vault disque réel (pas besoin de lire
/// des notes, uniquement de les indexer via `seed_note_with_fts`).
/// L'index renvoyé est le MÊME pointeur que `state.search` — seeds immédiats.
async fn build_search_env() -> (Router, Arc<SqliteIndex>, AppState) {
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL search-env");
    let (state, idx) = build_base(acl).await;
    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());
    (app, idx, state)
}

/// vault_search : agent non-privilégié → les notes section=`identity` sont exclues.
///
/// Prouve la parité avec `vault_read_impl` : avant le fix P1-B, le chemin search
/// exposait le corps des âmes à tout agent ayant `read_patterns=["**"]`, même si
/// `vault_read` refusait la lecture directe de la même note.
///
/// `frontend` a `read_patterns=["**"]` dans `TEST_ACL` → l'ACL générale autorise la
/// requête (200 HTTP), mais le post-filtre identity doit vider les hits identity.
#[tokio::test]
async fn vault_search_excludes_identity_for_non_privileged() {
    let (app, idx, state) = build_search_env().await;

    // Seed : une note identity + une note decisions (contrôle), même corpus lexical.
    let id_identity = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    let corpus = "gradatum soul invariant canary p1b";
    idx.seed_note_with_fts(&id_identity, "identity", corpus)
        .await
        .expect("seed identity P1-B");
    idx.seed_note_with_fts(&id_decisions, "decisions", corpus)
        .await
        .expect("seed decisions P1-B");

    let token = state
        .jwt
        .sign(
            "frontend",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt frontend P1-B");

    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_search",
        &token,
        serde_json::json!({
            "query": "soul invariant p1b",
            "limit": 10,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_search doit retourner 200: {json}"
    );
    let items = json["items"].as_array().expect("items array P1-B non-priv");

    // Invariant P1-B : aucun item identity visible pour un agent non-privilégié.
    let has_identity = items.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.starts_with("identity/"))
            .unwrap_or(false)
    });
    assert!(
        !has_identity,
        "P1-B : agent 'frontend' ne doit PAS voir les notes identity en search: {json}"
    );

    // Contrôle : la note decisions DOIT être visible.
    let has_decisions = items.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.contains(&id_decisions))
            .unwrap_or(false)
    });
    assert!(
        has_decisions,
        "P1-B : la note decisions doit être visible pour 'frontend': {json}"
    );
}

/// vault_search : `main-agent` (SOUL_PRIVILEGED_WRITER) → voit les notes identity.
///
/// Symétrique : le caller privilégié ne doit PAS être filtré. Ce test garantit
/// que le post-filtre n'est pas un blackout total de la section identity mais
/// bien un filtrage conditionnel (symétrique au guard vault_read_impl).
#[tokio::test]
async fn vault_search_allows_identity_for_main_agent() {
    let (app, idx, state) = build_search_env().await;

    let id_identity = Ulid::new().to_string();
    let corpus = "gradatum soul invariant canary p1b privileged mainagent";
    idx.seed_note_with_fts(&id_identity, "identity", corpus)
        .await
        .expect("seed identity P1-B privileged");

    let token = state
        .jwt
        .sign(
            "main-agent",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt main-agent P1-B");

    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_search",
        &token,
        serde_json::json!({
            "query": "soul invariant p1b privileged mainagent",
            "limit": 10,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_search doit retourner 200: {json}"
    );
    let items = json["items"]
        .as_array()
        .expect("items array P1-B privileged");

    // Invariant P1-B (chemin privilégié) : main-agent DOIT voir les notes identity.
    let has_identity = items.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.starts_with("identity/"))
            .unwrap_or(false)
    });
    assert!(
        has_identity,
        "P1-B : 'main-agent' (SOUL_PRIVILEGED_WRITER) DOIT voir les notes identity en search: {json}"
    );
}

/// vault_search : section indéterminée (vide) → exclue pour caller non-privilégié (fail-closed).
///
/// Guard P1-B durci (2026-06-28) : le filtre original opérait sur `hit.path` après
/// application du fallback `section="" → "main"`. Un hit dont `hit.section=""` devenait
/// `path="main/<ulid>"` et ÉCHAPPAIT au filtre `path.starts_with("identity/")`.
///
/// Fix durci : filtre sur `hit.section` AVANT le fallback, dans le `filter_map`.
/// `hit.section.is_empty()` → section indéterminée → exclure pour non-privilégiés
/// (fail-closed : confidentialité > complétude search).
///
/// Simulation : seeder une note avec `section=""` (SQLite NOT NULL mais sans CHECK)
/// représente le scénario production où `get_titles_sections` soft-fail retourne
/// `HashMap::new()` → `hit.section` reste vide pour un hit sémantique.
#[tokio::test]
async fn vault_search_excludes_identity_when_section_empty_non_privileged() {
    let (app, idx, state) = build_search_env().await;

    let id_empty = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    let corpus = "gradatum soul failclosed section vide p1b durci";
    // Note avec section="" simule un hit soul dont la section ne peut être déterminée.
    idx.seed_note_with_fts(&id_empty, "", corpus)
        .await
        .expect("seed section-empty P1-B durci");
    idx.seed_note_with_fts(&id_decisions, "decisions", corpus)
        .await
        .expect("seed decisions P1-B durci");

    let token_frontend = state
        .jwt
        .sign(
            "frontend",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt frontend P1-B durci");

    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_search",
        &token_frontend,
        serde_json::json!({
            "query": "soul failclosed section p1b durci",
            "limit": 10,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "vault_search 200 attendu: {json}");
    let items = json["items"].as_array().expect("items array P1-B durci");

    // Fail-closed : note section-vide NE doit PAS apparaître pour 'frontend'.
    let has_empty_section_note = items.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.contains(&*id_empty))
            .unwrap_or(false)
    });
    assert!(
        !has_empty_section_note,
        "P1-B durci : note section-vide NE doit PAS être visible pour 'frontend' (fail-closed): {json}"
    );

    // Contrôle : la note decisions reste visible pour 'frontend'.
    let has_decisions = items.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.contains(&*id_decisions))
            .unwrap_or(false)
    });
    assert!(
        has_decisions,
        "P1-B durci : la note decisions DOIT être visible pour 'frontend': {json}"
    );

    // Caller privilégié (main-agent) : la note section-vide EST visible (pas de filtre).
    let token_main = state
        .jwt
        .sign(
            "main-agent",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt main-agent P1-B durci");

    let (status_main, json_main) = post_json_auth(
        &app,
        "/api/v1/vault_search",
        &token_main,
        serde_json::json!({
            "query": "soul failclosed section p1b durci",
            "limit": 10,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status_main,
        StatusCode::OK,
        "vault_search 200 main-agent: {json_main}"
    );
    let items_main = json_main["items"]
        .as_array()
        .expect("items array P1-B durci main");

    // main-agent DOIT voir la note section-vide (pas de filtre pour privilégiés).
    let has_empty_main = items_main.iter().any(|i| {
        i["path"]
            .as_str()
            .map(|p| p.contains(&*id_empty))
            .unwrap_or(false)
    });
    assert!(
        has_empty_main,
        "P1-B durci : 'main-agent' DOIT voir la note section-vide (pas de filtre): {json_main}"
    );
}

// ── Tests guard identité — HISTORY (CoW) ─────────────────────────────────────
//
// Round P1 précédent : le guard read-restrictive identity était appliqué à
// `vault_read`/`vault_search`/`vault_context`/`proactive_recall` mais MANQUAIT sur le
// chemin historique CoW (`vault_history` timeline + `vault_history_get` corps) — un agent
// non privilégié pouvait exfiltrer le corps complet + la timeline d'une âme cross-agent
// via l'historique. Ces tests verrouillent la parité avec `vault_read_impl`.

/// Construit une frontmatter `identity/<agent>` (section `Identity`, statut `Live`).
fn identity_frontmatter() -> gradatum_core::frontmatter::Frontmatter {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Identity,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Seed une âme `identity/<agent>` AVEC un snapshot d'historique CoW.
///
/// Écrit deux versions (bodies différents, `created` identique) sous le même ULID :
/// le 2ᵉ write déclenche le copy-on-write → snapshot de la v1 dans `.history/<id>/`.
/// Upsert ensuite le titre `identity/<agent>` dans l'index (pour que
/// `get_titles_sections` résolve le `target_agent`).
///
/// Retourne `(ulid, ts_snapshot_v1)` — `ts` alimente `vault_history_get`.
async fn seed_soul_with_history(env: &ReadEnv, agent: &str) -> (String, i64) {
    use gradatum_core::identity::NoteId;

    let id = NoteId::new();
    let title = format!("identity/{agent}");
    let fm = identity_frontmatter();
    let body_v1 = format!("# {title}\n{GOOD_SOUL}\nversion-1");
    let body_v2 = format!("# {title}\n{GOOD_SOUL}\nversion-2");

    env.vault
        .write_note_with_id(fm.clone(), body_v1, id)
        .await
        .expect("write_note_with_id v1 seed_soul_with_history");
    env.vault
        .write_note_with_id(fm, body_v2, id)
        .await
        .expect("write_note_with_id v2 seed_soul_with_history");
    env.state
        .search
        .upsert_note_title(&id, &title)
        .await
        .expect("upsert_note_title seed_soul_with_history");

    let versions = env
        .vault
        .history_versions(id)
        .await
        .expect("history_versions seed_soul_with_history");
    let ts = *versions
        .first()
        .expect("au moins 1 snapshot CoW après 2 writes");
    (id.to_string(), ts)
}

/// `vault_history` : agent non privilégié → timeline d'une âme cross-agent refusée (403).
///
/// Sans le guard, `vault_history_impl` renvoyait `{versions, count}` 200 → divulgation de
/// l'existence + timeline de l'âme. Le guard résout la section RÉELLE via l'index
/// (`get_titles_sections`) → section `identity` + owner `main` ≠ caller `frontend` → 403.
#[tokio::test]
async fn vault_history_identity_foreign_agent_denied_403() {
    let env = build_read_env().await;
    let ulid = seed_soul(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_history",
        &token,
        serde_json::json!({ "note_id": ulid, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas voir la timeline de l'âme identity/main — attendu 403: {json}"
    );
}

/// `vault_history` : owner privilégié `main-agent` → timeline accessible (200).
///
/// Chemin nominal préservé : le caller privilégié n'est jamais filtré (pas de blackout).
#[tokio::test]
async fn vault_history_identity_privileged_ok() {
    let env = build_read_env().await;
    let ulid = seed_soul(&env, "main").await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_history",
        &token,
        serde_json::json!({ "note_id": ulid, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (privilégié) doit accéder à la timeline identity/main — attendu 200: {json}"
    );
}

/// `vault_history_get` : agent non privilégié → corps d'un snapshot d'âme cross-agent refusé (403).
///
/// Sans le guard, `vault_history_get_impl` renvoyait `body: snapshot.body.markdown` 200 →
/// exfiltration du corps COMPLET de l'âme via l'historique. Le guard résout la section +
/// le `target_agent` depuis le snapshot lui-même (`# identity/main`) → refus pour frontend.
#[tokio::test]
async fn vault_history_get_identity_foreign_agent_denied_403() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_history_get",
        &token,
        serde_json::json!({ "note_id": ulid, "ts_ms": ts, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas lire le corps d'un snapshot d'âme cross-agent — attendu 403: {json}"
    );
    // Le corps ne doit surtout pas fuiter dans la réponse d'erreur.
    assert!(
        json["body"].is_null(),
        "aucun champ body ne doit apparaître sur un refus 403: {json}"
    );
}

/// `vault_history_get` : owner privilégié `main-agent` → corps du snapshot accessible (200).
///
/// Chemin nominal préservé : le snapshot v1 (`# identity/main`) est lisible par le privilégié.
#[tokio::test]
async fn vault_history_get_identity_privileged_ok() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_history_get",
        &token,
        serde_json::json!({ "note_id": ulid, "ts_ms": ts, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (privilégié) doit lire le corps du snapshot identity/main — attendu 200: {json}"
    );
    let body = json["body"].as_str().unwrap_or("");
    assert!(
        body.contains("identity/main"),
        "le corps du snapshot doit être renvoyé pour un caller privilégié: {json}"
    );
}

// ── Tests guard identité — vault_list (vecteur d'énumération) ─────────────────

/// `vault_list` : agent non privilégié → les paths `identity/*` sont exclus du listing.
///
/// Sans le filtre, un non-privilégié découvrait existence + ULID + mtime de toute âme.
/// Parité avec `vault_search`/`vault_context`.
#[tokio::test]
async fn vault_list_excludes_identity_for_non_privileged() {
    let env = build_read_env().await;
    let _ulid_soul = seed_soul(&env, "main").await;
    // Note de contrôle non-identity (section `decisions`) via seed index direct.
    let id_decisions = Ulid::new().to_string();
    env.vault
        .index()
        .seed_note_with_fts(&id_decisions, "decisions", "controle listing public")
        .await
        .expect("seed decisions vault_list");

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 100, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_list doit retourner 200: {json}"
    );
    let entries = json["entries"].as_array().expect("entries array");

    let has_identity = entries.iter().any(|e| {
        e["path"]
            .as_str()
            .map(|p| p.starts_with("identity/"))
            .unwrap_or(false)
    });
    assert!(
        !has_identity,
        "frontend ne doit PAS voir de path identity/* dans vault_list: {json}"
    );
    // Contrôle : la note decisions DOIT rester visible.
    let has_decisions = entries.iter().any(|e| {
        e["path"]
            .as_str()
            .map(|p| p.contains(&id_decisions))
            .unwrap_or(false)
    });
    assert!(
        has_decisions,
        "la note decisions doit rester visible pour frontend: {json}"
    );
}

/// `vault_list` : caller privilégié `main-agent` → les paths `identity/*` restent visibles.
#[tokio::test]
async fn vault_list_allows_identity_for_privileged() {
    let env = build_read_env().await;
    let _ulid_soul = seed_soul(&env, "main").await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_list",
        &token,
        serde_json::json!({ "limit": 100, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_list doit retourner 200: {json}"
    );
    let entries = json["entries"].as_array().expect("entries array priv");
    let has_identity = entries.iter().any(|e| {
        e["path"]
            .as_str()
            .map(|p| p.starts_with("identity/"))
            .unwrap_or(false)
    });
    assert!(
        has_identity,
        "main-agent (privilégié) DOIT voir les paths identity/* dans vault_list: {json}"
    );
}

// ── Tests régression — vault_context (3 modes) + proactive_recall ────────────
//
// Guards livrés au round P1 précédent mais SANS couverture de test dédiée. Ces tests
// verrouillent l'exclusion identité sur les surfaces RAG génériques.

/// Corpus identité — contient un token secret unique absent du corpus public.
const CTX_IDENTITY_CORPUS: &str = "gradatum recall context guardtest soulsecrettoken";
/// Corpus public (section decisions) — partage les termes de requête, sans le secret.
const CTX_PUBLIC_CORPUS: &str = "gradatum recall context guardtest publicfait";
/// Requête partagée par les deux corpus.
const CTX_QUERY: &str = "gradatum recall context guardtest";

/// `vault_context` (Raw + Assembled + Compact) : un non-privilégié ne reçoit ni corps
/// ni titre d'une âme cross-agent — quel que soit le mode d'assemblage.
#[tokio::test]
async fn vault_context_excludes_identity_all_modes_non_privileged() {
    let (app, idx, state) = build_search_env().await;

    let id_identity = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    idx.seed_note_with_fts(&id_identity, "identity", CTX_IDENTITY_CORPUS)
        .await
        .expect("seed identity vault_context");
    idx.seed_note_with_fts(&id_decisions, "decisions", CTX_PUBLIC_CORPUS)
        .await
        .expect("seed decisions vault_context");

    let token = state
        .jwt
        .sign(
            "frontend",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt frontend vault_context");

    for mode in ["raw", "assembled", "compact"] {
        let mut body = serde_json::json!({
            "query": CTX_QUERY,
            "budget_tokens": 4000,
            "mode": mode,
            "tenant_id": "main"
        });
        if mode == "compact" {
            // Compact exige un session_id (ULID).
            body["session_id"] = serde_json::Value::String(Ulid::new().to_string());
        }
        let (status, json) = post_json_auth(&app, "/api/v1/vault_context", &token, body).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "vault_context mode={mode} doit retourner 200: {json}"
        );
        // Le corps ET le titre de l'âme (ULID de repli inclus) ne doivent apparaître nulle
        // part dans la réponse — assembled_text, included, references.
        let dump = json.to_string();
        assert!(
            !dump.contains("soulsecrettoken"),
            "mode={mode} : le corps de l'âme ne doit pas fuiter: {json}"
        );
        assert!(
            !dump.contains(&id_identity),
            "mode={mode} : l'ULID/titre de l'âme ne doit pas apparaître: {json}"
        );
        // Contrôle : la note decisions (publique) doit, elle, être exploitable.
        assert!(
            dump.contains(&id_decisions) || dump.contains("publicfait"),
            "mode={mode} : la note decisions publique doit rester présente: {json}"
        );
    }
}

/// `vault_context` : caller privilégié `main-agent` → l'âme reste candidate (pas de blackout).
#[tokio::test]
async fn vault_context_allows_identity_for_privileged() {
    let (app, idx, state) = build_search_env().await;

    let id_identity = Ulid::new().to_string();
    idx.seed_note_with_fts(&id_identity, "identity", CTX_IDENTITY_CORPUS)
        .await
        .expect("seed identity vault_context priv");

    let token = state
        .jwt
        .sign(
            "main-agent",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt main-agent vault_context priv");

    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_context",
        &token,
        serde_json::json!({
            "query": CTX_QUERY,
            "budget_tokens": 4000,
            "mode": "assembled",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "vault_context 200 attendu: {json}");
    let dump = json.to_string();
    assert!(
        dump.contains("soulsecrettoken") || dump.contains(&id_identity),
        "main-agent (privilégié) DOIT voir l'âme candidate (pas de blackout): {json}"
    );
}

/// `proactive_recall` (contextuel) : un non-privilégié ne reçoit ni titre ni corps d'une
/// âme cross-agent, même quand l'ACL de section autorise la lecture (`["**"]`).
#[tokio::test]
async fn proactive_recall_excludes_identity_for_non_privileged() {
    let (app, idx, state) = build_search_env().await;

    let id_identity = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    idx.seed_note_with_fts(&id_identity, "identity", CTX_IDENTITY_CORPUS)
        .await
        .expect("seed identity proactive");
    idx.seed_note_with_fts(&id_decisions, "decisions", CTX_PUBLIC_CORPUS)
        .await
        .expect("seed decisions proactive");

    let token = state
        .jwt
        .sign(
            "frontend",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt frontend proactive");

    let (status, json) = post_json_auth(
        &app,
        "/api/v1/proactive_recall",
        &token,
        serde_json::json!({
            "context": CTX_QUERY,
            "sections": ["identity", "decisions"],
            "limit": 10,
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "proactive_recall doit retourner 200: {json}"
    );
    let items = json["items"].as_array().expect("items array proactive");
    let has_identity = items.iter().any(|i| {
        i["section"].as_str() == Some("identity")
            || i["ulid"].as_str() == Some(id_identity.as_str())
    });
    assert!(
        !has_identity,
        "frontend ne doit PAS voir d'item identity dans proactive_recall: {json}"
    );
    let dump = json.to_string();
    assert!(
        !dump.contains("soulsecrettoken"),
        "le corps/snippet de l'âme ne doit pas fuiter dans proactive_recall: {json}"
    );
}

// ── Tests guard identité — vault_diff (diff de corps CoW, READ) ───────────────
//
// Round P1 courant : `vault_diff_impl` renvoyait les lignes de diff du CORPS d'une note
// entre 2 versions sans aucune restriction — fuite du corps d'âme cross-agent identique à
// `vault_history_get`. Le guard résout la section RÉELLE via l'index et refuse au
// non-propriétaire (parité `vault_history_get_impl`).

/// `vault_diff` : agent non privilégié → diff du corps d'une âme cross-agent refusé (403).
#[tokio::test]
async fn vault_diff_identity_foreign_agent_denied_403() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_diff",
        &token,
        serde_json::json!({
            "note_id": ulid,
            "a": ts.to_string(),
            "b": "current",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas obtenir le diff du corps d'une âme cross-agent — attendu 403: {json}"
    );
    // Le diff ne doit surtout pas fuiter dans la réponse d'erreur.
    assert!(
        json["lines"].is_null(),
        "aucun champ lines ne doit apparaître sur un refus 403: {json}"
    );
}

/// `vault_diff` : owner privilégié `main-agent` → diff accessible (200), pas de blackout.
#[tokio::test]
async fn vault_diff_identity_privileged_ok() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_diff",
        &token,
        serde_json::json!({
            "note_id": ulid,
            "a": ts.to_string(),
            "b": "current",
            "tenant_id": "main"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (privilégié) doit obtenir le diff de identity/main — attendu 200: {json}"
    );
}

// ── Tests guard identité — vault_restore (restauration CoW, WRITE) ────────────
//
// Round P1 courant : `vault_restore_impl` appelait `history_restore` directement,
// court-circuitant la garde write-restrictive par-agent de `vault_write_impl` — un
// non-privilégié avec ACL Write pouvait restaurer/écraser une version de l'âme d'un AUTRE
// agent. Le guard write applique la même règle de privilège identité (C6).

/// `vault_restore` : agent non privilégié → restauration d'une âme cross-agent refusée (403).
#[tokio::test]
async fn vault_restore_identity_foreign_agent_denied_403() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_restore",
        &token,
        serde_json::json!({ "note_id": ulid, "ts_ms": ts, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas restaurer une version de l'âme identity/main — attendu 403: {json}"
    );
}

/// `vault_restore` : owner privilégié `main-agent` → restauration accessible (200).
#[tokio::test]
async fn vault_restore_identity_privileged_ok() {
    let env = build_read_env().await;
    let (ulid, ts) = seed_soul_with_history(&env, "main").await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_restore",
        &token,
        serde_json::json!({ "note_id": ulid, "ts_ms": ts, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (privilégié) doit restaurer identity/main — attendu 200: {json}"
    );
}

// ── Tests guard identité — vault_timeline (vecteur de découverte de titres) ───
//
// Découverte de balayage P1 : `vault_timeline` n'a PAS besoin d'un filtre serveur — la
// requête SQL `SqliteIndex::timeline` exclut déjà `n.section NOT IN (PROTECTED_FORGET)`,
// et `Section::PROTECTED_FORGET` inclut `Identity`. Les âmes sont donc invisibles dans la
// timeline pour TOUS les appelants (blackout SQL, plus fort que le filtre par-privilège de
// `vault_list`). Ces tests verrouillent ce comportement côté serveur (régression : si le
// filtre SQL sautait, ils échoueraient).

/// Seed une note temporelle (temporal_index) sous un ULID donné.
async fn seed_temporal(env: &ReadEnv, note_id: &str, anchor_ms: i64) {
    use gradatum_core::index::{AnchorSrc, TemporalEntry};
    env.state
        .search
        .write_temporal_entry(&TemporalEntry {
            note_id: note_id.to_string(),
            vault_id: "main".to_string(),
            anchor_ms,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Event".to_string(),
            valid_until_ms: None,
        })
        .await
        .expect("write_temporal_entry seed_temporal");
}

/// `vault_timeline` : agent non privilégié → les titres/ULID `identity/*` sont exclus.
///
/// Sans le filtre, un non-privilégié découvrait l'existence + le titre `identity/<agent>`
/// + l'ULID de toute âme dotée d'un ancrage temporel. La note `decisions` (contrôle) reste
/// visible — le filtre est conditionnel à la section, pas un blackout.
#[tokio::test]
async fn vault_timeline_excludes_identity_for_non_privileged() {
    let env = build_read_env().await;
    // Âme identity/main (title upserté "identity/main") + ancrage temporel.
    let ulid_soul = seed_soul(&env, "main").await;
    seed_temporal(&env, &ulid_soul, 2_000).await;
    // Note de contrôle non-identity (section decisions) + ancrage temporel.
    let id_decisions = Ulid::new().to_string();
    env.vault
        .index()
        .seed_note_with_fts(&id_decisions, "decisions", "controle timeline public")
        .await
        .expect("seed decisions timeline");
    seed_temporal(&env, &id_decisions, 1_000).await;

    let token = sign_for(&env.state, "frontend");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_timeline",
        &token,
        serde_json::json!({ "limit": 100, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_timeline doit retourner 200: {json}"
    );
    let items = json["items"].as_array().expect("items array timeline");

    // Aucun item identity : ni par ULID, ni par titre `identity/*`.
    let leaks = items.iter().any(|i| {
        i["note_id"].as_str() == Some(ulid_soul.as_str())
            || i["title"]
                .as_str()
                .map(|t| t.starts_with("identity/"))
                .unwrap_or(false)
    });
    assert!(
        !leaks,
        "frontend ne doit voir ni l'ULID ni le titre de l'âme dans timeline: {json}"
    );

    // Contrôle : la note decisions DOIT rester visible.
    let has_decisions = items
        .iter()
        .any(|i| i["note_id"].as_str() == Some(id_decisions.as_str()));
    assert!(
        has_decisions,
        "la note decisions doit rester visible pour frontend: {json}"
    );
}

/// `vault_timeline` : même le caller privilégié `main-agent` ne voit PAS l'âme —
/// exclusion SQL `PROTECTED_FORGET` (blackout total, indépendant du privilège).
///
/// Documente que la timeline n'est pas une surface de lecture d'âme : les identités sont
/// exclues pour tous. Ce test aurait échoué si la timeline exposait les identity aux
/// privilégiés — verrouille l'absence de régression inverse (ouverture involontaire).
#[tokio::test]
async fn vault_timeline_excludes_identity_even_for_privileged() {
    let env = build_read_env().await;
    let ulid_soul = seed_soul(&env, "main").await;
    seed_temporal(&env, &ulid_soul, 2_000).await;
    // Contrôle : une note decisions temporelle DOIT rester visible (la timeline fonctionne).
    let id_decisions = Ulid::new().to_string();
    env.vault
        .index()
        .seed_note_with_fts(&id_decisions, "decisions", "controle timeline priv")
        .await
        .expect("seed decisions timeline priv");
    seed_temporal(&env, &id_decisions, 1_000).await;

    let token = sign_for(&env.state, "main-agent");
    let (status, json) = post_json_auth(
        &env.app,
        "/api/v1/vault_timeline",
        &token,
        serde_json::json!({ "limit": 100, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_timeline doit retourner 200: {json}"
    );
    let items = json["items"].as_array().expect("items array timeline priv");
    let has_identity = items.iter().any(|i| {
        i["note_id"].as_str() == Some(ulid_soul.as_str())
            || i["title"]
                .as_str()
                .map(|t| t.starts_with("identity/"))
                .unwrap_or(false)
    });
    assert!(
        !has_identity,
        "l'âme ne doit JAMAIS apparaître en timeline (exclusion SQL PROTECTED_FORGET), même pour main-agent: {json}"
    );
    // Contrôle : la timeline renvoie bien les notes non protégées.
    let has_decisions = items
        .iter()
        .any(|i| i["note_id"].as_str() == Some(id_decisions.as_str()));
    assert!(
        has_decisions,
        "la note decisions doit rester visible (timeline fonctionnelle): {json}"
    );
}

// ── Tests P0 — fuite d'âme via listings transverses (by-status / review) ──────
//
// Contexte : `get_notes_by_status` (`/notes/by-status`) et `list_review`
// (`/review`) sont des surfaces de LISTING TRANSVERSE (toutes sections) qui
// émettent titre + section (+ snippet pour by-status). Sans le guard identité,
// un appelant non privilégié disposant d'un ACL large (`read_patterns=["**"]`)
// pouvait exfiltrer les âmes d'agents (H1 `identity/<agent>` + corps). Ces tests
// prouvent l'exclusion par différentiel privilégié / non privilégié — ils
// ÉCHOUENT sans le filtre `identity_section_hidden` posé côté serveur.

/// Helper GET authentifié (miroir `post_json_auth`) → `(StatusCode, Value)`.
async fn get_json_auth(app: &Router, path: &str, token: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(path)
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builder GET");
    let resp = app.clone().oneshot(req).await.expect("oneshot GET");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body collect GET")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// `GET /notes/by-status?status=live&section=identity` — agent non privilégié →
/// AUCUNE âme émise (ni titre ni snippet). Le param `section` est
/// attaquant-contrôlé : sans le guard, l'âme fuiterait. Contrôle no-op : une
/// section non-identity reste listée pour le même appelant.
#[tokio::test]
async fn notes_by_status_excludes_identity_for_non_privileged() {
    let (app, idx, state) = build_search_env().await;

    // Corps distinctif de l'âme — s'il apparaît dans la réponse, le snippet a fui.
    let soul_marker = "IDENTITY_SOUL_SECRET_BODY_MARKER";
    let id_identity = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    idx.seed_note(&id_identity, "identity", soul_marker)
        .await
        .expect("seed identity live (by-status)");
    idx.seed_note(
        &id_decisions,
        "decisions",
        "controle decision live by-status",
    )
    .await
    .expect("seed decisions live (by-status)");

    let token_np = sign_for(&state, "frontend");

    let (status, json) = get_json_auth(
        &app,
        "/api/v1/notes/by-status?status=live&section=identity",
        &token_np,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "by-status doit retourner 200: {json}"
    );
    let entries = json["entries"].as_array().expect("entries array (np)");
    assert!(
        entries.is_empty(),
        "aucune âme émise pour un appelant non privilégié (ni titre ni snippet): {json}"
    );
    // Défense en profondeur : le corps de l'âme ne fuit nulle part dans la réponse.
    assert!(
        !json.to_string().contains(soul_marker),
        "le corps de l'âme ne doit JAMAIS apparaître dans by-status pour non-privilégié: {json}"
    );

    // Contrôle no-op : la section non-identity reste visible pour le même appelant.
    let (status_ctl, json_ctl) = get_json_auth(
        &app,
        "/api/v1/notes/by-status?status=live&section=decisions",
        &token_np,
    )
    .await;
    assert_eq!(status_ctl, StatusCode::OK, "{json_ctl}");
    let entries_ctl = json_ctl["entries"]
        .as_array()
        .expect("entries decisions (contrôle)");
    assert!(
        entries_ctl
            .iter()
            .any(|e| e["ulid"].as_str() == Some(id_decisions.as_str())),
        "la note decisions doit rester visible (guard no-op hors identity): {json_ctl}"
    );
}

/// `GET /notes/by-status?section=identity` — `main-agent` (SOUL_PRIVILEGED_WRITER)
/// → l'âme est visible (guard conditionnel, pas blackout total).
#[tokio::test]
async fn notes_by_status_allows_identity_for_main_agent() {
    let (app, idx, state) = build_search_env().await;

    let soul_marker = "IDENTITY_SOUL_PRIV_BODY_MARKER";
    let id_identity = Ulid::new().to_string();
    idx.seed_note(&id_identity, "identity", soul_marker)
        .await
        .expect("seed identity live (by-status priv)");

    let token = sign_for(&state, "main-agent");
    let (status, json) = get_json_auth(
        &app,
        "/api/v1/notes/by-status?status=live&section=identity",
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let entries = json["entries"].as_array().expect("entries array (priv)");
    assert!(
        entries
            .iter()
            .any(|e| e["ulid"].as_str() == Some(id_identity.as_str())),
        "main-agent (privilégié) DOIT voir l'âme via by-status: {json}"
    );
    assert!(
        json.to_string().contains(soul_marker),
        "le snippet privilégié doit contenir le corps de l'âme: {json}"
    );
}

/// `GET /review` — une âme en statut `pending-review` ne doit PAS apparaître dans
/// la file de revue pour un appelant non privilégié, mais reste visible pour
/// `main-agent`. Différentiel priv/non-priv sur la même surface transverse.
#[tokio::test]
async fn review_queue_excludes_identity_for_non_privileged() {
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let (app, idx, state) = build_search_env().await;

    let id_identity = Ulid::new().to_string();
    let id_decisions = Ulid::new().to_string();
    idx.seed_note_with_status(
        &id_identity,
        Section::Identity,
        "IDENTITY_SOUL_REVIEW_MARKER",
        NoteStatus::PendingReview,
        None,
    )
    .await
    .expect("seed identity pending-review");
    idx.seed_note_with_status(
        &id_decisions,
        Section::Decisions,
        "controle review decision pending",
        NoteStatus::PendingReview,
        None,
    )
    .await
    .expect("seed decisions pending-review");

    // Non privilégié → l'âme est exclue, la décision reste listée.
    let token_np = sign_for(&state, "frontend");
    let (status, json) = get_json_auth(&app, "/api/v1/review?limit=100", &token_np).await;
    assert_eq!(status, StatusCode::OK, "review doit retourner 200: {json}");
    let items = json["items"].as_array().expect("items review (np)");
    assert!(
        !items
            .iter()
            .any(|i| i["ulid"].as_str() == Some(id_identity.as_str())),
        "l'âme ne doit PAS apparaître dans la file de revue pour non-privilégié: {json}"
    );
    assert!(
        items
            .iter()
            .any(|i| i["ulid"].as_str() == Some(id_decisions.as_str())),
        "la note decisions doit rester dans la file de revue (guard no-op hors identity): {json}"
    );

    // Privilégié → l'âme est visible (guard conditionnel, pas blackout).
    let token_priv = sign_for(&state, "main-agent");
    let (status_p, json_p) = get_json_auth(&app, "/api/v1/review?limit=100", &token_priv).await;
    assert_eq!(status_p, StatusCode::OK, "{json_p}");
    let items_p = json_p["items"].as_array().expect("items review (priv)");
    assert!(
        items_p
            .iter()
            .any(|i| i["ulid"].as_str() == Some(id_identity.as_str())),
        "main-agent (privilégié) DOIT voir l'âme dans la file de revue: {json_p}"
    );
}

// ── Tests F-1 — parité guard identité read : trace / graph / links ────────────
//
// F-1 (finding sécu LOW) : `vault_trace`, `vault_graph` et `vault_links` laissaient un
// non-privilégié CONFIRMER l'existence + l'ULID (+ le lignage pour trace) d'une âme
// cross-agent, alors que `vault_read`/`vault_search`/`vault_list` la masquent déjà.
//
// - `vault_trace` : le seed résolu (par `title_lookup` sur `query="identity/<agent>"`, par
//   ULID nu, ou par FTS) passe désormais `enforce_identity_read_guard` AVANT le lignage →
//   403 fail-closed sur âme cross-agent (parité `vault_history_impl`).
// - `vault_graph`/`vault_links` : les nœuds/arêtes de section `identity` sont filtrés pour
//   le non-privilégié (parité `identity_section_hidden`), no-op pour le privilégié.
//
// Différentiel priv/non-priv (ACL `["**"]`) : ces tests ÉCHOUENT sans le guard/filtre.

/// Seed une âme `identity/<agent>` dans l'index in-memory (section + titre résolvable).
///
/// Retourne l'ULID. Contrairement à `seed_soul` (vault disque réel), suffit pour les
/// surfaces trace/graph/links qui n'interrogent que l'index (`state.search`).
async fn seed_soul_index(idx: &Arc<SqliteIndex>, state: &AppState, agent: &str) -> String {
    use gradatum_core::identity::NoteId;
    let note_id = NoteId::new();
    let ulid = note_id.to_string();
    idx.seed_note_with_fts(&ulid, "identity", "ame agent souveraine f1 lignage")
        .await
        .expect("seed identity F-1");
    state
        .search
        .upsert_note_title(&note_id, &format!("identity/{agent}"))
        .await
        .expect("upsert_note_title F-1");
    ulid
}

/// `vault_trace` : non-privilégié, seed = âme cross-agent (résolu par titre) → 403.
///
/// Vecteur d'énumération : `query="identity/main"` → `title_lookup` résout l'ULID de l'âme
/// de `main`. Sans le guard, `frontend` recevait 200 + le lignage (existence confirmée).
#[tokio::test]
async fn vault_trace_identity_seed_foreign_denied_403() {
    let (app, idx, state) = build_search_env().await;
    let _ulid = seed_soul_index(&idx, &state, "main").await;

    let token = sign_for(&state, "frontend");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_trace",
        &token,
        serde_json::json!({ "query": "identity/main", "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "frontend ne doit pas tracer le lignage de l'âme identity/main — attendu 403: {json}"
    );
    assert!(
        json["entries"].is_null(),
        "aucune entrée de lignage ne doit fuiter sur un refus 403: {json}"
    );
}

/// `vault_trace` : privilégié `main-agent`, même seed identity → 200 (pas de blackout).
#[tokio::test]
async fn vault_trace_identity_seed_privileged_ok() {
    let (app, idx, state) = build_search_env().await;
    let _ulid = seed_soul_index(&idx, &state, "main").await;

    let token = sign_for(&state, "main-agent");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_trace",
        &token,
        serde_json::json!({ "query": "identity/main", "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "main-agent (privilégié) doit tracer identity/main — attendu 200: {json}"
    );
}

/// Seed une arête publique → âme : `id_public` (section `decisions`) pointe vers
/// `id_identity` (âme de `main`). Retourne `(id_public, id_identity)`.
async fn seed_public_to_soul_edge(idx: &Arc<SqliteIndex>, state: &AppState) -> (String, String) {
    let id_identity = seed_soul_index(idx, state, "main").await;
    let id_public = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &id_public,
        "decisions",
        "note publique pointant vers ame f1",
    )
    .await
    .expect("seed public F-1");
    state
        .search
        .upsert_link("main", &id_public, &id_identity)
        .await
        .expect("upsert_link public->identity F-1");
    (id_public, id_identity)
}

/// `vault_graph` : non-privilégié → le nœud/arête `identity` est retiré du graphe.
///
/// Root = note publique liée à l'âme. Sans le filtre, le nœud `id_identity` et l'arête
/// `public → identity` révélaient l'existence + l'ULID de l'âme.
#[tokio::test]
async fn vault_graph_excludes_identity_node_for_non_privileged() {
    let (app, idx, state) = build_search_env().await;
    let (id_public, id_identity) = seed_public_to_soul_edge(&idx, &state).await;

    let token = sign_for(&state, "frontend");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_graph",
        &token,
        serde_json::json!({ "root": id_public, "depth": 2, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_graph doit retourner 200: {json}"
    );
    let nodes = json["nodes"].as_array().expect("nodes array graph np");
    assert!(
        !nodes
            .iter()
            .any(|n| n.as_str() == Some(id_identity.as_str())),
        "frontend ne doit PAS voir le nœud de l'âme identity dans le graphe: {json}"
    );
    // Contrôle : le nœud public (root, section decisions) reste présent (filtre no-op).
    assert!(
        nodes.iter().any(|n| n.as_str() == Some(id_public.as_str())),
        "le nœud public (root) doit rester dans le graphe (guard no-op hors identity): {json}"
    );
    // Défense en profondeur : aucune arête ne référence l'ULID de l'âme.
    assert!(
        !json.to_string().contains(&id_identity),
        "l'ULID de l'âme ne doit apparaître dans aucun nœud/arête: {json}"
    );
}

/// `vault_graph` : privilégié `main-agent` → le nœud `identity` reste visible (pas de blackout).
#[tokio::test]
async fn vault_graph_allows_identity_node_for_privileged() {
    let (app, idx, state) = build_search_env().await;
    let (id_public, id_identity) = seed_public_to_soul_edge(&idx, &state).await;

    let token = sign_for(&state, "main-agent");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_graph",
        &token,
        serde_json::json!({ "root": id_public, "depth": 2, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_graph doit retourner 200: {json}"
    );
    let nodes = json["nodes"].as_array().expect("nodes array graph priv");
    assert!(
        nodes
            .iter()
            .any(|n| n.as_str() == Some(id_identity.as_str())),
        "main-agent (privilégié) DOIT voir le nœud de l'âme dans le graphe: {json}"
    );
}

/// `vault_links` : non-privilégié → le nœud/arête `identity` (backlink) est retiré.
///
/// `path` = âme ; ses backlinks incluent la note publique. Un non-privilégié ne doit pas
/// voir le nœud de l'âme elle-même (path poussé en nœud) ni confirmer son existence.
#[tokio::test]
async fn vault_links_excludes_identity_node_for_non_privileged() {
    let (app, idx, state) = build_search_env().await;
    let (id_public, id_identity) = seed_public_to_soul_edge(&idx, &state).await;

    let token = sign_for(&state, "frontend");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_links",
        &token,
        serde_json::json!({ "path": id_identity, "include_backlinks": true, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_links doit retourner 200: {json}"
    );
    let nodes = json["nodes"].as_array().expect("nodes array links np");
    assert!(
        !nodes
            .iter()
            .any(|n| n.as_str() == Some(id_identity.as_str())),
        "frontend ne doit PAS voir le nœud de l'âme identity dans les liens: {json}"
    );
    // Le backlink public existe mais l'arête le reliant à l'âme est retirée avec le nœud âme.
    assert!(
        !json.to_string().contains(&id_identity),
        "l'ULID de l'âme ne doit apparaître dans aucun nœud/arête: {json}"
    );
    // Contrôle : la note publique (section decisions) reste un nœud légitime si présente.
    let _ = &id_public;
}

/// `vault_links` : privilégié `main-agent` → le nœud `identity` reste visible.
#[tokio::test]
async fn vault_links_allows_identity_node_for_privileged() {
    let (app, idx, state) = build_search_env().await;
    let (_id_public, id_identity) = seed_public_to_soul_edge(&idx, &state).await;

    let token = sign_for(&state, "main-agent");
    let (status, json) = post_json_auth(
        &app,
        "/api/v1/vault_links",
        &token,
        serde_json::json!({ "path": id_identity, "include_backlinks": true, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "vault_links doit retourner 200: {json}"
    );
    let nodes = json["nodes"].as_array().expect("nodes array links priv");
    assert!(
        nodes
            .iter()
            .any(|n| n.as_str() == Some(id_identity.as_str())),
        "main-agent (privilégié) DOIT voir le nœud de l'âme dans les liens: {json}"
    );
}
