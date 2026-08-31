//! Isolation des jobs au niveau handler + 404 anti-disclosure.
//!
//! - **Flag OFF** (défaut LIVE) : filtre `None` → byte-identical, le job reste
//!   visible (200).
//! - **Flag ON** (LOCAL au harnais, flip INTERDIT LIVE) : la queue est scopée au
//!   tenant du Bearer — le job d'un autre tenant renvoie **404** (pas 403 : le
//!   grant/ACL passe, c'est le filtre store qui cache l'existence), own → 200.
//!
//! Le job servant `bob` porte `CurateSpec.tenant_id = "bob"` → `spec_tenant` = "bob"
//! → colonne `gradatum_jobs.tenant_id = "bob"`. À ON, `alice` filtre `Some("alice")`
//! → `get` renvoie `None` → 404.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::{
    CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, RetryBackoff, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

// À ON, `resolve_read_vault` cible le vault PROPRE du tenant (`alice/jobs`,
// `bob/jobs`) — chaque identité a donc l'ACL Read/Write sur SON vault. À OFF, le
// chemin legacy vise `main/jobs`. Ainsi un refus provient du filtre tenant (store),
// jamais de l'ACL/grant — on isole le scoping L1.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "main"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
[[consumer]]
identity = "alice"
read_patterns  = ["alice/*", "alice/alice", "main/jobs"]
write_patterns = ["alice/*", "alice/alice", "main/jobs"]
[[consumer]]
identity = "bob"
read_patterns  = ["bob/*", "bob/bob", "main/jobs"]
write_patterns = ["bob/*", "bob/bob", "main/jobs"]
"#;

struct Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

async fn build_env(enabled: bool) -> Env {
    let dir = TempDir::new().expect("tempdir jobs isolation");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL valide");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig { enabled },
        ..ServerConfig::default()
    };

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations")
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool)
        .with_server_config(cfg);

    // B7 : seed agent_grants pour les trois identités du preset ACL de test.
    // Le middleware vérifie tenant_grants ∩ agent_grants quand multi_tenant = ON.
    for agent in &["main", "alice", "bob"] {
        seed_agent_grants(&index_path, agent);
    }

    Env {
        state,
        index_path,
        _dir: dir,
    }
}

fn make_curate(tenant: &str) -> JobRecord {
    let now = chrono::Utc::now();
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                tenant_id: tenant.to_string(),
                ..Default::default()
            }),
            class: JobClass::Api,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Variante DÉTERMINISTE de [`make_curate`] : `id` (ULID) et `created_at` imposés.
///
/// `Ulid::generate()` n'est **pas** monotone à l'intérieur d'une même milliseconde : seuls
/// les 48 bits de poids fort portent l'horodatage (résolution ms), les 80 bits
/// restants sont tirés aléatoirement. Deux ULID générés dans la même ms ont donc un
/// ordre lexicographique aléatoire (~50 % d'inversions, mesuré sur 200 000 paires).
///
/// `SqliteQueueStore::latest_job()` tranchant par `ORDER BY id DESC`, tout test qui
/// dépend de « quel job est le plus récent » DOIT imposer des ULID strictement
/// ordonnés — jamais s'en remettre à `Ulid::generate()` ni à un `sleep`. Même parti pris
/// que le test unitaire `latest_job_returns_most_recent_not_oldest`.
///
/// `created_at` est aligné sur le même instant que l'ULID : l'assertion reste vraie
/// quel que soit la clé de tri retenue côté store (`id` ou `created_at`).
fn make_curate_ordered(tenant: &str, at_secs: u64) -> JobRecord {
    let mut record = make_curate(tenant);
    record.id =
        Ulid::from_datetime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(at_secs));
    let at = chrono::DateTime::from_timestamp(
        i64::try_from(at_secs).expect("at_secs de test tient dans un i64"),
        0,
    )
    .expect("timestamp de test valide");
    record.lifecycle.created_at = at;
    record.scheduling.scheduled_at = at;
    record
}

/// Enregistre un tenant actif + self-grant write sur SON propre vault (ON only).
///
/// À ON, `resolve_read_vault` cible le vault propre du tenant : il faut donc
/// `tenants(tenant, active)` (require_active_target) + grant `(tenant, tenant)`
/// (require_read_grant). `write` est superset de read → couvre get ET cancel.
fn seed_tenant_grants(index_path: &std::path::Path, tenant: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed grants");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT OR IGNORE INTO tenants (id, status, created_at) VALUES ('{tenant}', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES ('{tenant}', '{tenant}', 'write');"
    ))
    .expect("seed tenant grants");
}

/// Enregistre un grant agent→vault pour `agent_id` (B7, plan v1.0.0).
///
/// Le middleware vérifie l'intersection `tenant_grants ∩ agent_grants` quand
/// `multi_tenant.enabled = true`. Sans cette ligne, un BearerToken valide (tenant
/// seedé) est refusé au niveau agent-grants (fail-closed).
fn seed_agent_grants(index_path: &std::path::Path, agent_id: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed agent grants");
    conn.execute(
        "INSERT OR IGNORE INTO agent_vault_grants (agent_id, vault_id, access) VALUES (?1, ?1, 'write')",
        rusqlite::params![agent_id],
    )
    .expect("seed agent grants");
}

fn build_router(state: AppState) -> axum::Router {
    use axum::{Router, middleware};
    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

fn sign(state: &AppState, tenant: &str) -> String {
    state
        .jwt
        .sign(
            tenant,
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign JWT test")
}

async fn get_status(router: axum::Router, uri: &str, jwt: &str) -> StatusCode {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::empty())
        .expect("build request");
    router.oneshot(req).await.expect("service").status()
}

/// POST sans body (ex : cancel) → StatusCode.
async fn post_status(router: axum::Router, uri: &str, jwt: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::empty())
        .expect("build POST");
    router.oneshot(req).await.expect("service").status()
}

/// GET → (StatusCode, body string).
async fn get_body(router: axum::Router, uri: &str, jwt: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::empty())
        .expect("build GET");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// POST /api/v1/jobs — enqueue un `Curate` dont le spec sert `spec_tenant`.
async fn post_curate(router: axum::Router, jwt: &str, idem: &str, spec_tenant: &str) -> StatusCode {
    let body = serde_json::json!({
        "spec": { "kind": { "type": "Curate", "data": {
            "note_id": Ulid::generate().to_string(),
            "tenant_id": spec_tenant,
        }}}
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Idempotency-Key", idem)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build POST request");
    router.oneshot(req).await.expect("service").status()
}

/// POST /api/v1/jobs avec un `spec` arbitraire → (status, body brut).
async fn post_job(
    router: axum::Router,
    jwt: &str,
    idem: &str,
    spec: serde_json::Value,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Idempotency-Key", idem)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::json!({ "spec": spec }).to_string()))
        .expect("build POST job");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Extrait le `JobScope` du record réellement mis en file par un POST accepté.
async fn enqueued_scope(state: &AppState, created_body: &str) -> JobScope {
    let id: Ulid = serde_json::from_str::<serde_json::Value>(created_body)
        .expect("réponse JSON")
        .get("id")
        .and_then(|v| v.as_str())
        .expect("champ `id` dans la réponse 202")
        .parse()
        .expect("id ULID");
    state
        .job_store
        .get(id, None)
        .await
        .expect("get job store")
        .expect("job enfilé")
        .spec
        .scope
}

fn assert_vault_scope(scope: &JobScope, expected: &str, ctx: &str) {
    match scope {
        JobScope::Vault(v) => assert_eq!(v, expected, "{ctx}"),
        other => panic!("{ctx} : attendu JobScope::Vault({expected}), obtenu {other:?}"),
    }
}

/// A6' défaut 1 — le job créé porte `JobScope::Vault(<tenant du Bearer>)`.
///
/// RED avant fix : `build_job_record_from_spec` codait `JobScope::VaultWide` en dur,
/// que `resolve_job_vault` refuse terminalement dès `multi_tenant = ON` (A2) — tout
/// job accepté en 202 mourait ensuite en DLQ. Le vault vient du contexte d'auth,
/// jamais du body (invariant A6').
///
/// À OFF, `Vault("main")` et `VaultWide` résolvent tous deux `"main"` côté worker :
/// le comportement observable est inchangé.
#[tokio::test]
async fn create_job_scopes_record_to_bearer_vault() {
    // ── ON : le scope porte le tenant du JWT, pas VaultWide.
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    let (status, body) = post_job(
        build_router(on.state.clone()),
        &sign(&on.state, "alice"),
        "k-scope-on",
        serde_json::json!({ "kind": { "type": "Curate", "data": {
            "note_id": Ulid::generate().to_string(),
            "tenant_id": "alice",
        }}}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "ON : alice enfile son job");
    assert_vault_scope(
        &enqueued_scope(&on.state, &body).await,
        "alice",
        "ON : le job doit être scopé sur le vault du Bearer",
    );

    // ── OFF : vault unique `main` — équivalent à l'ancien VaultWide.
    let off = build_env(false).await;
    let (status, body) = post_job(
        build_router(off.state.clone()),
        &sign(&off.state, "main"),
        "k-scope-off",
        serde_json::json!({ "kind": { "type": "Curate", "data": {
            "note_id": Ulid::generate().to_string(),
            "tenant_id": "main",
        }}}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "OFF : enqueue inchangé");
    assert_vault_scope(
        &enqueued_scope(&off.state, &body).await,
        "main",
        "OFF : vault unique",
    );
}

/// A6' défaut 2 — `Purge` et `Distill` sont atteignables par un tenant ≠ `main`.
///
/// RED avant fix : `spec_tenant` repliait sur le littéral `"main"` pour ces deux
/// kinds (aucun tenant porté par `PurgeSpec` ; `DistillSource.scope = Locus` ne
/// porte pas de vault). L'anti-forge comparait donc `"main"` au JWT `alice` → **403
/// systématique**, quel que soit son grant. Le repli est désormais `JobSpec.scope`,
/// que le handler vient de fixer au vault du Bearer.
#[tokio::test]
async fn create_job_purge_and_distill_reachable_for_non_main_tenant() {
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    let jwt = sign(&on.state, "alice");

    // Purge : PurgeSpec ne porte aucun tenant.
    let (status, body) = post_job(
        build_router(on.state.clone()),
        &jwt,
        "k-purge-alice",
        serde_json::json!({ "kind": { "type": "Purge", "data": { "mode": "Lifecycle" }}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "ON : alice doit pouvoir créer un Purge sur SON vault (403 avant A6')"
    );
    assert_vault_scope(
        &enqueued_scope(&on.state, &body).await,
        "alice",
        "ON : le Purge d'alice doit être scopé sur son vault",
    );

    // Distill : `DistillSource.scope = Locus` décrit le QUOI, jamais le OÙ.
    let (status, body) = post_job(
        build_router(on.state.clone()),
        &jwt,
        "k-distill-alice",
        serde_json::json!({ "kind": { "type": "Distill", "data": {
            "scope": { "Locus": "inbox/" },
        }}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "ON : alice doit pouvoir créer un Distill sur SON vault (403 avant A6')"
    );
    assert_vault_scope(
        &enqueued_scope(&on.state, &body).await,
        "alice",
        "ON : le Distill d'alice doit être scopé sur son vault",
    );
}

/// A6' — l'anti-forge reste discriminante sur un kind dont le spec porte un vault.
///
/// Le repli sur `JobSpec.scope` ne s'applique QUE lorsque le spec n'élit aucun vault.
/// Un `Forget::Locus { vault: "bob" }` en élit un : c'est lui qui est comparé au
/// Bearer → 403. Sans cette garantie, le correctif du défaut 2 aurait ouvert une
/// écriture cross-tenant.
#[tokio::test]
async fn create_job_anti_forge_still_blocks_vaulted_spec_scope() {
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    let jwt = sign(&on.state, "alice");

    let (status, _) = post_job(
        build_router(on.state.clone()),
        &jwt,
        "k-forge-forget",
        serde_json::json!({ "kind": { "type": "Forget", "data": {
            "scope": { "Locus": { "vault": "bob", "locus": "inbox/" } },
        }}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : alice ne doit pas forger un Forget ciblant le vault de bob"
    );

    // Le même Forget sur SON vault passe → le 403 ci-dessus vient bien du vault
    // ciblé, pas d'un refus global du kind.
    let (status, body) = post_job(
        build_router(on.state.clone()),
        &jwt,
        "k-forget-own",
        serde_json::json!({ "kind": { "type": "Forget", "data": {
            "scope": { "Locus": { "vault": "alice", "locus": "inbox/" } },
        }}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "ON : alice peut oublier dans son propre vault"
    );
    assert_vault_scope(
        &enqueued_scope(&on.state, &body).await,
        "alice",
        "ON : le Forget d'alice doit être scopé sur son vault",
    );
}

#[tokio::test]
async fn get_job_off_visible_on_isolated_404() {
    // ── OFF : filtre None → byte-identical, le job reste visible (200).
    let off = build_env(false).await;
    let id_main = off
        .state
        .job_store
        .enqueue(make_curate("main"))
        .await
        .expect("enqueue main");
    let jwt_main = sign(&off.state, "main");
    assert_eq!(
        get_status(
            build_router(off.state.clone()),
            &format!("/api/v1/jobs/{id_main}/v2"),
            &jwt_main,
        )
        .await,
        StatusCode::OK,
        "OFF : le job doit rester visible (byte-identical)"
    );

    // ── ON : la queue est scopée au tenant du Bearer.
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    seed_tenant_grants(&on.index_path, "bob");
    let jb = on
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob");

    let jwt_alice = sign(&on.state, "alice");
    let jwt_bob = sign(&on.state, "bob");

    // alice (autre tenant) → 404 anti-disclosure : le grant/ACL passe, le filtre cache.
    assert_eq!(
        get_status(
            build_router(on.state.clone()),
            &format!("/api/v1/jobs/{jb}/v2"),
            &jwt_alice,
        )
        .await,
        StatusCode::NOT_FOUND,
        "ON : alice ne doit PAS voir le job de bob (404, pas 403)"
    );

    // bob (propriétaire) → 200.
    assert_eq!(
        get_status(
            build_router(on.state.clone()),
            &format!("/api/v1/jobs/{jb}/v2"),
            &jwt_bob,
        )
        .await,
        StatusCode::OK,
        "ON : bob doit voir son propre job (200)"
    );
}

/// P0 (SecAuditor #1) — ANTI-FORGE `create_job` : à ON, un client ne peut pas
/// enqueuer un job servant un autre tenant que le sien. À OFF, aucun check.
#[tokio::test]
async fn create_job_anti_forge_cross_tenant() {
    // ── ON : alice tente de forger un job servant bob.
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    let jwt_alice = sign(&on.state, "alice");

    // Forge : spec.tenant_id = "bob" ≠ JWT "alice" → 403.
    assert_eq!(
        post_curate(build_router(on.state.clone()), &jwt_alice, "k-forge", "bob").await,
        StatusCode::FORBIDDEN,
        "ON : forge d'un job servant bob par alice doit être refusée (403)"
    );
    // Légitime : spec.tenant_id = "alice" = JWT → accepté (202).
    assert_eq!(
        post_curate(
            build_router(on.state.clone()),
            &jwt_alice,
            "k-legit",
            "alice"
        )
        .await,
        StatusCode::ACCEPTED,
        "ON : alice peut enqueuer son propre job (202)"
    );

    // ── OFF : aucun cross-check (byte-identical) — spec.tenant_id ≠ JWT accepté.
    let off = build_env(false).await;
    let jwt_main = sign(&off.state, "main");
    assert_eq!(
        post_curate(build_router(off.state.clone()), &jwt_main, "k-off", "bob").await,
        StatusCode::ACCEPTED,
        "OFF : aucun cross-check tenant (byte-identical)"
    );
}

/// P2 (SecAuditor #3) — `cancel_job` isolé par tenant (404 cross-tenant à ON).
#[tokio::test]
async fn cancel_job_isolation() {
    // ── ON : alice ne peut pas annuler le job de bob (404), bob peut (200).
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    seed_tenant_grants(&on.index_path, "bob");
    let jb = on
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob");

    assert_eq!(
        post_status(
            build_router(on.state.clone()),
            &format!("/api/v1/jobs/{jb}/cancel"),
            &sign(&on.state, "alice"),
        )
        .await,
        StatusCode::NOT_FOUND,
        "ON : alice ne peut pas annuler le job de bob (404)"
    );
    // Le job de bob reste actif (non annulé par alice).
    assert_ne!(
        on.state
            .job_store
            .get(jb, None)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .status,
        JobStatus::Cancelled,
        "job de bob ne doit pas être annulé par alice"
    );
    assert_eq!(
        post_status(
            build_router(on.state.clone()),
            &format!("/api/v1/jobs/{jb}/cancel"),
            &sign(&on.state, "bob"),
        )
        .await,
        StatusCode::OK,
        "ON : bob annule son propre job (200)"
    );

    // ── OFF : main annule un job (byte-identical, 200).
    let off = build_env(false).await;
    let jm = off
        .state
        .job_store
        .enqueue(make_curate("main"))
        .await
        .expect("enqueue main");
    assert_eq!(
        post_status(
            build_router(off.state.clone()),
            &format!("/api/v1/jobs/{jm}/cancel"),
            &sign(&off.state, "main"),
        )
        .await,
        StatusCode::OK,
        "OFF : cancel inchangé (200)"
    );
}

/// P2 (SecAuditor #3) — `list_jobs` isolé par tenant (pas de fuite cross-tenant à ON).
#[tokio::test]
async fn list_jobs_isolation() {
    // ── ON : alice ne voit pas le job de bob dans sa liste.
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    seed_tenant_grants(&on.index_path, "bob");
    let ja = on
        .state
        .job_store
        .enqueue(make_curate("alice"))
        .await
        .expect("enqueue alice");
    let jb = on
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob");

    let (status, body) = get_body(
        build_router(on.state.clone()),
        "/api/v1/jobs",
        &sign(&on.state, "alice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&ja.to_string()),
        "ON : list(alice) doit contenir le job d'alice"
    );
    assert!(
        !body.contains(&jb.to_string()),
        "ON : list(alice) ne doit PAS fuiter le job de bob"
    );

    // ── OFF : main voit tous les jobs (byte-identical, y compris tenant ≠ main).
    let off = build_env(false).await;
    let jbob = off
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob off");
    let (status, body) = get_body(
        build_router(off.state.clone()),
        "/api/v1/jobs",
        &sign(&off.state, "main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&jbob.to_string()),
        "OFF : list global voit tous les jobs (byte-identical)"
    );
}

/// P2 (SecAuditor #3) — `job_events` (SSE) isolé par tenant (404 cross-tenant à ON).
#[tokio::test]
async fn job_events_isolation() {
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    seed_tenant_grants(&on.index_path, "bob");
    let jb = on
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob");

    // alice sur le stream du job de bob → 404 (anti-disclosure existence).
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{jb}/events"))
        .header(
            "Authorization",
            format!("Bearer {}", sign(&on.state, "alice")),
        )
        .body(Body::empty())
        .expect("build GET events");
    let status = build_router(on.state.clone())
        .oneshot(req)
        .await
        .expect("service")
        .status();
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "ON : alice ne peut pas ouvrir le stream du job de bob (404)"
    );

    // bob sur son propre stream → 200 (headers SSE).
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{jb}/events"))
        .header(
            "Authorization",
            format!("Bearer {}", sign(&on.state, "bob")),
        )
        .body(Body::empty())
        .expect("build GET events");
    let status = build_router(on.state.clone())
        .oneshot(req)
        .await
        .expect("service")
        .status();
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : bob ouvre son propre stream (200)"
    );
}

/// P2 (SecAuditor #3 / L2) — `dashboard` scopé par tenant à ON (last_job + count).
#[tokio::test]
async fn dashboard_isolation() {
    // ── ON : le dashboard d'alice ne voit que ses jobs.
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");
    seed_tenant_grants(&on.index_path, "bob");
    let ja = on
        .state
        .job_store
        .enqueue(make_curate("alice"))
        .await
        .expect("enqueue alice");
    // bob enqueue APRÈS alice → job le plus récent global = bob.
    let jb = on
        .state
        .job_store
        .enqueue(make_curate("bob"))
        .await
        .expect("enqueue bob");

    let (status, body) = get_body(
        build_router(on.state.clone()),
        "/api/v1/dashboard",
        &sign(&on.state, "alice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&ja.to_string()),
        "ON : dashboard(alice) last_job doit être le job d'alice"
    );
    assert!(
        !body.contains(&jb.to_string()),
        "ON : dashboard(alice) ne doit PAS fuiter le job de bob (L2)"
    );

    // ── OFF : dashboard voit le job le plus récent global (byte-identical).
    //
    // Déterminisme (flake ~1/40 mesuré sur `main` avant correctif) : les deux
    // `enqueue` tombaient massivement dans la même milliseconde, et `Ulid::generate()`
    // n'est pas monotone à cette résolution → `ORDER BY id DESC` désignait le
    // « dernier job » à pile ou face. On impose ici deux instants distincts (donc
    // deux ULID strictement ordonnés), sans dépendre de l'horloge ni d'un `sleep`.
    // Cf. [`make_curate_ordered`].
    const T_FIRST_SECS: u64 = 1_700_000_000;
    const T_LAST_SECS: u64 = T_FIRST_SECS + 3600;

    let off = build_env(false).await;
    let jfirst = off
        .state
        .job_store
        .enqueue(make_curate_ordered("alice", T_FIRST_SECS))
        .await
        .expect("enqueue alice off");
    let jlast = off
        .state
        .job_store
        .enqueue(make_curate_ordered("bob", T_LAST_SECS))
        .await
        .expect("enqueue bob off");
    assert!(
        jfirst < jlast,
        "pré-condition du test : les deux ULID doivent être strictement ordonnés"
    );
    let (status, body) = get_body(
        build_router(off.state.clone()),
        "/api/v1/dashboard",
        &sign(&off.state, "main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&jlast.to_string()),
        "OFF : dashboard voit le dernier job global (byte-identical)"
    );
}

/// A7 défaut 2 — un `Forget::Agent` visant N > 1 vaults est refusé **400 à l'enqueue**.
///
/// RED avant A7 : `forget_scope_vault` n'élit aucun vault (N > 1) → `spec_tenant`
/// retombe sur `JobSpec.scope`, que le handler vient de fixer au vault du Bearer →
/// l'anti-forge compare `alice` à `alice` et **passe** → **202**. Le worker refusait
/// ensuite le spec terminalement (`ensure_forget_scope_vault`, branche `many`) : un
/// « 202 puis mort en DLQ », le symptôme même qu'A6' a corrigé ailleurs.
///
/// 400 et non 403 : le refus est de FORME (un job = exactement un vault, A2-bis) et ne
/// dépend pas de l'identité — il vaut aussi bien pour un porteur qui couvrirait tous
/// les vaults cités.
#[tokio::test]
async fn create_job_rejects_multi_vault_agent_forget() {
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");

    let (status, body) = post_job(
        build_router(on.state.clone()),
        &sign(&on.state, "alice"),
        "k-forget-multi",
        serde_json::json!({ "kind": { "type": "Forget", "data": {
            "scope": { "Agent": { "agent_id": "alice", "vaults": ["bob", "carol"] } },
            "dry_run": true,
        }}}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "un Forget::Agent multi-vault doit être refusé à l'enqueue, pas accepté puis \
         envoyé en DLQ : {body}"
    );
    assert!(
        body.contains("exactly one vault"),
        "le message doit nommer l'invariant violé et orienter vers le fan-out : {body}"
    );
}

/// A7 — la garde ci-dessus ne sur-bloque pas : un `Agent` mono-vault reste enfilable.
///
/// `vaults = [<vault du Bearer>]` est la forme que produit le fan-out du CLI admin
/// (`fan_out_by_vault`) ; le worker l'accepte (`ensure_forget_scope_vault`, branche
/// `[only] if only == vault_id`). La refuser casserait le seul chemin légitime.
#[tokio::test]
async fn create_job_accepts_single_vault_agent_forget() {
    let on = build_env(true).await;
    seed_tenant_grants(&on.index_path, "alice");

    let (status, body) = post_job(
        build_router(on.state.clone()),
        &sign(&on.state, "alice"),
        "k-forget-single",
        serde_json::json!({ "kind": { "type": "Forget", "data": {
            "scope": { "Agent": { "agent_id": "alice", "vaults": ["alice"] } },
            "dry_run": true,
        }}}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "le fan-out mono-vault doit rester accepté : {body}"
    );
    assert_vault_scope(
        &enqueued_scope(&on.state, &body).await,
        "alice",
        "le job mono-vault reste scopé sur le vault du Bearer",
    );
}
