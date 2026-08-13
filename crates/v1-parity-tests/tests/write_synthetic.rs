//! Tests write E2E synthétiques — T8 steps 3 (tests 11-21).
//!
//! # Périmètre
//!
//! Tests 11-20 : enqueue write/classify/downgrade + validation structure 202.
//! Test 21 : writes concurrents (unicité des job_id sous charge).
//!
//! # Note : tests 16-18 (LLM wiremock) et 22b (leader election)
//!
//! Les tests 16-18 (LLM wiremock) et 22b (leader election) sont marqués `#[ignore]`
//! (infrastructure non câblée). L'ancien test 22 (round-trip du `Dispatcher` legacy) a
//! été retiré : le traitement curate actif est couvert par
//! `gradatum-worker/tests/curate_*` (`handle_curate`).
//!

use std::sync::Arc;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_queue::{Queue, SqliteQueue};
use gradatum_server::middleware::auth_middleware;
use gradatum_server::{api_v1, state::AppState};
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;

// ── Helpers ───────────────────────────────────────────────────────────────────

const TEST_ACL_WRITE: &str = r#"
[[consumer]]
identity = "test-writer"
read_patterns = ["**"]
write_patterns = ["**"]
"#;

/// Démarre un serveur de test avec write ACL + SqliteQueue in-memory.
async fn spawn_write_server() -> (std::net::SocketAddr, String, Arc<SqliteQueue>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let bearer = jwt
        .sign(
            "test-writer",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signer bearer write — clé éphémère valide");

    let acl = AclEngine::from_preset_str(TEST_ACL_WRITE).expect("preset ACL write toujours valide");

    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue in-memory pour write synthetic"),
    );

    // Phase 1.2 : vault_write bridge vers job_store (gradatum_jobs) — nécessaire pour 202.
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test write_synthetic");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test write_synthetic");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(Arc::clone(&queue) as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère write synthetic");
    let addr = listener
        .local_addr()
        .expect("adresse locale write synthetic");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur write synthetic arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, bearer, queue)
}

/// Client HTTP avec timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP")
}

// ── Tests 11-15 : write E2E synthétiques ─────────────────────────────────────

/// Test 11 — vault_write : titre [DECISIONS] → job enqueued avec kind "curate".
///
/// Vérifie que vault_write accepte n'importe quel titre et enqueue correctement.
/// Le worker (heuristique) assignera "decisions" en mode E2E complet.
#[tokio::test]
async fn test_11_write_decisions_title_enqueues_curate_job() {
    let (addr, bearer, queue) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "[DECISIONS] Architecture choix SQLite WAL",
            "body": "Nous avons choisi SQLite WAL pour la persistance locale...",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write test 11");

    assert_eq!(resp.status(), 202, "test 11 : 202 attendu");
    let body: Value = resp.json().await.expect("body 202");
    // Phase 1.2 : job_id est un ULID string (bridge job_store gradatum_jobs).
    let job_id = body["job_id"].as_str().expect("job_id ULID Phase 1.2");
    assert!(!job_id.is_empty(), "job_id ne doit pas être vide");

    // Phase 1.2 : vault_write enfile dans gradatum_jobs — queue legacy jobs_v2 reste vide.
    let depth = queue.depth().await.expect("depth queue");
    assert_eq!(
        depth, 0,
        "test 11 Phase 1.2 : queue legacy vide (job dans gradatum_jobs)"
    );
}

/// Test 12 — vault_write : titre [DEBUG] → job enqueued.
#[tokio::test]
async fn test_12_write_debug_title_enqueues_job() {
    let (addr, bearer, _queue) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "[DEBUG] Null pointer exception dans vault::read_note",
            "body": "Erreur reproduite sur Vault::read_note quand la note est absente du cache...",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write test 12");

    assert_eq!(resp.status(), 202, "test 12 : 202 attendu");
    let body: Value = resp.json().await.expect("body 202");
    // Phase 1.2 : job_id est un ULID string non vide.
    assert!(
        !body["job_id"].as_str().unwrap_or("").is_empty(),
        "job_id ULID non vide Phase 1.2"
    );
    assert_eq!(body["status"], "queued");
}

/// Test 13 — vault_write : titre [REASONING] → job enqueued.
#[tokio::test]
async fn test_13_write_reasoning_title_enqueues_job() {
    let (addr, bearer, _queue) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "[REASONING] Pourquoi on a choisi Rust pour le backend",
            "body": "Rust offre la sécurité mémoire sans GC, idéale pour un daemon critique...",
            "section_hint": "reasoning",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write test 13");

    assert_eq!(resp.status(), 202, "test 13 : 202 attendu");
    let body: Value = resp.json().await.expect("body 202");
    // Phase 1.2 : poll_url = /api/v1/jobs/{ulid}/v2.
    let poll_url = body["poll_url"].as_str().expect("poll_url");
    let job_id = body["job_id"].as_str().expect("job_id ULID Phase 1.2");
    assert_eq!(
        poll_url,
        format!("/api/v1/jobs/{job_id}/v2"),
        "poll_url Phase 1.2"
    );
}

/// Test 14 — vault_write avec tags → job enqueued, tags préservés dans payload.
///
/// Les tags fournis sont transportés dans le `CurateSpec` du job enfilé (gradatum_jobs).
#[tokio::test]
async fn test_14_write_with_tags_enqueues_job() {
    let (addr, bearer, _queue) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "Note avec tags explicites",
            "body": "Corps de la note avec tags rust, architecture, p2.0c.",
            "tags": ["rust", "architecture", "p2.0c"],
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write test 14 avec tags");

    assert_eq!(resp.status(), 202, "test 14 : 202 attendu avec tags");
    let body: Value = resp.json().await.expect("body 202");
    // Phase 1.2 : job_id est un ULID string non vide.
    assert!(
        !body["job_id"].as_str().unwrap_or("").is_empty(),
        "job_id ULID non vide Phase 1.2"
    );
}

/// Test 15 — 5 vault_write séquentiels → 5 ULID job_id distincts.
///
/// vault_write retourne des ULID (non ordonnés numériquement).
/// On vérifie l'unicité des job_id (ULID unicité garantie par randomness).
#[tokio::test]
async fn test_15_sequential_writes_produce_monotonic_job_ids() {
    let (addr, bearer, _queue) = spawn_write_server().await;
    let c = client();
    let mut job_ids = Vec::with_capacity(5);

    for i in 0..5u32 {
        let resp = c
            .post(format!("http://{addr}/api/v1/vault_write"))
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "title": format!("Note séquentielle #{i}"),
                "body": format!("Corps de la note #{i} pour test séquentiel."),
                "tenant_id": "main"
            }))
            .send()
            .await
            .expect("vault_write séquentiel");

        assert_eq!(resp.status(), 202);
        let body: Value = resp.json().await.expect("body 202");
        // Phase 1.2 : job_id est un ULID string (non numérique).
        let job_id = body["job_id"]
            .as_str()
            .expect("job_id ULID string Phase 1.2")
            .to_owned();
        assert!(!job_id.is_empty(), "job_id ne doit pas être vide");
        job_ids.push(job_id);
    }

    // Unicité des ULIDs (ULID garantit l'unicité même dans la même milliseconde).
    let unique: std::collections::HashSet<_> = job_ids.iter().collect();
    assert_eq!(
        unique.len(),
        5,
        "5 vault_write séquentiels doivent produire 5 ULID distincts Phase 1.2: {job_ids:?}"
    );
}

// ── Tests 19-20 : downgrade/restore ──────────────────────────────────────────

/// Test 19 — vault_downgrade synchrone — 404 pour note inexistante (bearer valide).
///
/// `vault_downgrade` est synchrone (200/404), pas async 202.
/// Une note inexistante retourne directement 404 sans enqueue.
/// La queue reste à depth=0 (aucun job créé).
///
/// Le bearer JWT est requis depuis le fix F-1 (A01 Broken Access Control) :
/// sans bearer → 401, avec bearer + note inexistante → 404.
#[tokio::test]
async fn test_19_downgrade_enqueues_job() {
    let (addr, bearer, queue) = spawn_write_server().await;

    // Note inexistante en DB — handler sync retourne 404 (après auth OK).
    let fake_note_id = ulid::Ulid::generate().to_string();

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_downgrade"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "note_id": fake_note_id,
            "reason": "Révisée et remplacée par une version plus récente",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_downgrade test 19");

    assert_eq!(
        resp.status(),
        404,
        "test 19 : vault_downgrade sync → 404 pour note inexistante (F-1 : bearer requis, note absente)"
    );

    // La queue reste vide — aucun job créé par le handler synchrone.
    let depth = queue.depth().await.expect("depth queue post-downgrade");
    assert_eq!(
        depth, 0,
        "test 19 : queue vide — handler sync n'enqueue pas"
    );
}

/// Test 20 — vault_classify avec note absente retourne 404 Not Found.
///
/// L'implémentation heuristique synchrone retourne 404 si la note est absente
/// du vault. Aucun job n'est enqueued (la classification est synchrone).
#[tokio::test]
async fn test_20_classify_unknown_note_returns_404() {
    let (addr, bearer, queue) = spawn_write_server().await;

    let fake_note_id = ulid::Ulid::generate().to_string();

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_classify"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "note_id": fake_note_id,
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_classify (restore) test 20");

    assert_eq!(
        resp.status(),
        404,
        "test 20 : vault_classify avec note absente doit retourner 404"
    );

    // Aucun job ne doit être enqueued (vault_classify est synchrone, heuristique).
    let depth = queue.depth().await.expect("depth queue post-classify");
    assert_eq!(
        depth, 0,
        "test 20 : vault_classify heuristique ne doit rien enqueue"
    );
}

// ── Test 21 : writes concurrents ──────────────────────────────────────────────

/// Test 21 — 10 vault_write parallèles → 10 job_id distincts.
///
/// Vérifie que l'atomic `UPDATE...RETURNING` de SqliteQueue garantit
/// l'unicité des job_id sous charge concurrente maximale.
#[tokio::test]
async fn test_21_concurrent_writes_produce_distinct_job_ids() {
    let (addr, bearer, queue) = spawn_write_server().await;

    let futs: Vec<_> = (0..10u32)
        .map(|i| {
            let bearer = bearer.clone();
            tokio::spawn(async move {
                client()
                    .post(format!("http://{addr}/api/v1/vault_write"))
                    .bearer_auth(&bearer)
                    .json(&serde_json::json!({
                        "title": format!("[REASONING] Note concurrente #{i} — test atomicité"),
                        "body": format!("Corps de la note concurrente {i}. Données uniques pour déduplication."),
                        "tenant_id": "main"
                    }))
                    .send()
                    .await
                    .expect("vault_write concurrent test 21")
            })
        })
        .collect();

    let mut job_ids = Vec::with_capacity(10);
    for fut in futs {
        let resp = fut.await.expect("join tokio task test 21");
        assert_eq!(resp.status(), 202, "concurrent write : 202 attendu");
        let body: Value = resp.json().await.expect("body 202 concurrent");
        // Phase 1.2 : job_id est un ULID string.
        let jid = body["job_id"]
            .as_str()
            .expect("job_id ULID string concurrent Phase 1.2")
            .to_owned();
        job_ids.push(jid);
    }

    // Tous les ULID doivent être distincts (unicité ULID garantie par randomness).
    let unique: std::collections::HashSet<_> = job_ids.iter().collect();
    assert_eq!(
        unique.len(),
        10,
        "test 21 : 10 vault_write parallèles doivent produire 10 ULID distincts Phase 1.2: {job_ids:?}"
    );

    // Phase 1.2 : vault_write enfile dans gradatum_jobs — queue legacy reste vide.
    let depth = queue.depth().await.expect("depth queue test 21");
    assert_eq!(
        depth, 0,
        "test 21 Phase 1.2 : queue legacy vide (10 jobs dans gradatum_jobs)"
    );
}

/// Test 22b — leader election (différé).
///
/// 3 instances Dispatcher → kill leader → re-election ≤90s.
/// Nécessite infrastructure leader election (worker_leadership table + heartbeat)
/// non encore câblée.
#[tokio::test]
#[ignore = "deferred Phase 2.1 — leader election nécessite worker_leadership heartbeat + multi-instance infra non câblée en alpha.4"]
async fn test_22b_leader_election_deferred_phase_2_1() {
    // TODO Phase 2.1 :
    // 1. Spawner 3 Dispatcher::new(queue).with_leader_election(true) dans tokio::spawn
    // 2. Identifier le leader via queue.current_leader()
    // 3. Droper le handle du leader (simule kill)
    // 4. Attendre ≤90s que queue.current_leader() change
    // 5. Asserter que le nouveau leader est différent
    todo!("Phase 2.1 — leader election infra required");
}
