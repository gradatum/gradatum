//! Provenance des résultats `vault_search` — le champ `vault_id` de `SearchHit`.
//!
//! ## Ce que ces tests prouvent
//!
//! Chaque hit retourné par `vault_search` porte le vault **effectivement lu**. Les deux
//! premiers tests sont discriminants par construction : ils comparent le vault du JWT
//! (`main`) au vault CIBLE (`research`) sur la même fixture, si bien qu'une implémentation
//! qui se contenterait de recopier `tenant_id` — ou une constante `"main"` — échouerait sur
//! `hit_reports_target_vault_on_cross_vault_read` tout en passant les autres.
//!
//! ## Tolérance du client MCP (volet 2)
//!
//! Un champ additionnel dans un item de résultat ne peut casser un client MCP que si le
//! serveur lui donne de quoi valider : un `outputSchema` déclaré sur l'outil, ou un
//! `structuredContent` dans le résultat (MCP 2025-06-18 — le client ne valide QUE ces
//! deux surfaces ; `content[]` est du contenu libre). Les deux derniers tests interrogent
//! un vrai serveur MCP (`tools/list` puis `tools/call`) et constatent l'absence des deux :
//! il n'existe aucune surface contre laquelle un client pourrait rejeter `vault_id`.
//!
//! Ce sont des tests d'ABSENCE : ils ne valent que pour la surface énumérée
//! (`tools/list` + `tools/call` du serveur MCP natif). Ils ne disent rien d'un client qui
//! validerait contre un schéma qu'il détient par ailleurs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Preset ACL ────────────────────────────────────────────────────────────────
//
// `reader-main` a l'ACL Read sur `main/*` ET `research/*` : le cross-read n'est donc
// gouverné que par le GRANT, et le test isole bien la provenance, pas l'autorisation.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "reader-main"
read_patterns  = ["main/*", "main/main", "research/*", "research/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// ULID de la note seedée dans le vault `research` (corpus cible du cross-read).
const RESEARCH_NOTE_ID: &str = "01HRESEARCHAAAAAAAAAAAAAAA";
/// ULID de la note seedée dans le vault `main` (corpus propre de l'appelant).
const MAIN_NOTE_ID: &str = "01HMANVAAAAAAAAAAAAAAAAAAA";
/// Requête FTS commune aux deux corpus — les deux notes matchent, seul le vault diffère.
const PROBE_QUERY: &str = "gravity probe";

// ── Fixture HTTP ──────────────────────────────────────────────────────────────

struct Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// `AppState` avec index SQLite réel (migrations, seed `main`↔`main`) et flag
/// `multi_tenant` paramétrable. Le régime `enabled = true` est LOCAL au harnais.
async fn build_env(multi_tenant_enabled: bool) -> Env {
    let dir = TempDir::new().expect("tempdir provenance");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL provenance — statique");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig {
            enabled: multi_tenant_enabled,
        },
        ..ServerConfig::default()
    };

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations")
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);

    Env {
        state,
        index_path,
        _dir: dir,
    }
}

/// Sème une note FTS dans `vault` — le corpus des deux vaults répond à `PROBE_QUERY`.
fn seed_note(index_path: &std::path::Path, id: &str, vault: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
           VALUES ('{id}', '{vault}', NULL, 'reference', 'live', 1, {now}, X'00', 'isolation gravity probe corpus', 'Gravity Probe');
         INSERT INTO notes_fts (rowid, body_text)
           SELECT rowid, body_text FROM notes WHERE id = '{id}';"
    ))
    .expect("seed note FTS");
}

/// Enregistre le vault `research` : tenant actif + self-grant write.
fn register_research_vault(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db register");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('research', 'research', 'write');"
    ))
    .expect("register research vault");
}

/// Ajoute le grant cross-vault `main → research` en lecture.
fn grant_main_read_on_research(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db grant");
    conn.execute(
        "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES ('main', 'research', 'read')",
        [],
    )
    .expect("grant main→research read");
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

fn sign_jwt(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "reader-main",
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT — clé éphémère")
}

/// `POST /api/v1/vault_search` authentifié → réponse JSON désérialisée (200 exigé).
async fn search(router: axum::Router, jwt: &str, vault_id: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "query": PROBE_QUERY, "tenant_id": "main" });
    if let Some(v) = vault_id {
        body["vault_id"] = serde_json::Value::String(v.to_owned());
    }
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_search")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    assert_eq!(
        status,
        StatusCode::OK,
        "vault_search doit répondre 200. body={}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("réponse vault_search JSON")
}

/// Extrait les couples `(path, vault_id)` de tous les items, en exigeant `vault_id`
/// présent sur CHAQUE item (un item sans provenance est une régression, pas un `None`).
fn provenances(resp: &serde_json::Value) -> Vec<(String, String)> {
    resp["items"]
        .as_array()
        .expect("items doit être un tableau")
        .iter()
        .map(|hit| {
            let vault = hit["vault_id"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "hit sans champ `vault_id` de type string — provenance perdue. hit={hit}"
                    )
                })
                .to_owned();
            let path = hit["path"].as_str().unwrap_or_default().to_owned();
            (path, vault)
        })
        .collect()
}

// ── Provenance — sémantique ──────────────────────────────────────────────────

/// Cross-read : la provenance est le vault CIBLE, jamais celui de l'appelant.
///
/// Le JWT porte `tenant = main`, la requête cible `research`. Une implémentation qui
/// recopierait `tenant_id` (ou une constante `"main"`) échoue ici et nulle part ailleurs
/// — c'est le test discriminant de la provenance.
#[tokio::test]
async fn hit_reports_target_vault_on_cross_vault_read() {
    let env = build_env(true).await;
    register_research_vault(&env.index_path);
    grant_main_read_on_research(&env.index_path);
    seed_note(&env.index_path, RESEARCH_NOTE_ID, "research");
    seed_note(&env.index_path, MAIN_NOTE_ID, "main");

    let jwt = sign_jwt(&env.state);
    let resp = search(build_router(env.state.clone()), &jwt, Some("research")).await;
    let provenances = provenances(&resp);

    assert_eq!(
        provenances,
        vec![(
            format!("reference/{RESEARCH_NOTE_ID}"),
            "research".to_owned()
        )],
        "le cross-read doit remonter la seule note de `research`, estampillée `research`"
    );
}

/// Sans `vault_id`, la provenance est le vault propre de l'appelant.
///
/// Contre-épreuve du test précédent, même fixture : prouve que le champ suit le vault lu
/// et n'est pas figé sur la cible du cross-read.
#[tokio::test]
async fn hit_reports_own_vault_when_request_omits_vault_id() {
    let env = build_env(true).await;
    register_research_vault(&env.index_path);
    grant_main_read_on_research(&env.index_path);
    seed_note(&env.index_path, RESEARCH_NOTE_ID, "research");
    seed_note(&env.index_path, MAIN_NOTE_ID, "main");

    let jwt = sign_jwt(&env.state);
    let resp = search(build_router(env.state.clone()), &jwt, None).await;
    let provenances = provenances(&resp);

    assert_eq!(
        provenances,
        vec![(format!("reference/{MAIN_NOTE_ID}"), "main".to_owned())],
        "sans vault_id, seule la note de `main` remonte, estampillée `main`"
    );
}

/// Le champ existe aussi sur le chemin legacy (`multi_tenant` OFF — régime LIVE).
///
/// La provenance n'est pas une propriété du seul mode multi-vault : le champ doit être
/// là avec la même valeur, sinon un client devrait le traiter comme optionnel.
#[tokio::test]
async fn hit_carries_vault_id_at_multi_tenant_off() {
    let env = build_env(false).await;
    seed_note(&env.index_path, MAIN_NOTE_ID, "main");

    let jwt = sign_jwt(&env.state);
    let resp = search(build_router(env.state.clone()), &jwt, None).await;
    let provenances = provenances(&resp);

    assert_eq!(
        provenances,
        vec![(format!("reference/{MAIN_NOTE_ID}"), "main".to_owned())],
        "flag OFF : la note de `main` doit être estampillée `main`"
    );
}

// ── Surface MCP — tolérance du client ────────────────────────────────────────

/// Démarre un vrai serveur MCP (Streamable HTTP) sur un port éphémère.
///
/// Retourne `(adresse, jwt, chemin de l'index)` — le seed se fait par `index_path`.
async fn start_mcp_server() -> (SocketAddr, String, std::path::PathBuf, TempDir) {
    use axum::{Router, middleware};

    let env = build_env(false).await;
    let jwt = sign_jwt(&env.state);
    let index_path = env.index_path.clone();
    let dir = env._dir;

    let (mcp_service, _cancel) = gradatum_server::api_v1::mcp::build_mcp_service(env.state.clone());
    let mcp_router = Router::new().route_service("/mcp", mcp_service).layer(
        tower_http::limit::RequestBodyLimitLayer::new(gradatum_server::api_v1::mcp::MCP_BODY_LIMIT),
    );

    let authed = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .merge(mcp_router)
        .layer(middleware::from_fn_with_state(
            env.state.clone(),
            gradatum_server::middleware::auth_middleware,
        ));
    let app = Router::new().merge(authed).with_state(env.state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère MCP");
    let addr = listener.local_addr().expect("adresse locale MCP");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serveur MCP");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    (addr, jwt, index_path, dir)
}

/// Envoie une requête JSON-RPC MCP et retourne l'enveloppe JSON (body SSE ou JSON brut).
async fn call_mcp(addr: SocketAddr, jwt: &str, body: serde_json::Value) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client HTTP MCP");
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", addr.port()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&body)
        .send()
        .await
        .expect("POST MCP");
    assert_eq!(resp.status().as_u16(), 200, "MCP doit répondre 200");
    let text = resp.text().await.expect("body MCP");
    for line in text.lines() {
        if let Some(raw) = line.strip_prefix("data: ")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw)
        {
            return v;
        }
    }
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

/// Le résultat MCP transporte `vault_id` — le champ franchit la frontière outil.
///
/// Le contenu utile est dans `result.content[0].text` (JSON sérialisé de
/// `VaultSearchResponse` par `to_mcp_content`).
#[tokio::test]
async fn mcp_tool_result_carries_vault_id_in_text_content() {
    let (addr, jwt, index_path, _dir) = start_mcp_server().await;
    seed_note(&index_path, MAIN_NOTE_ID, "main");

    let env = call_mcp(
        addr,
        &jwt,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "vault_search",
                        "arguments": { "query": PROBE_QUERY, "tenant_id": "main" } }
        }),
    )
    .await;

    assert!(
        env.get("error").is_none(),
        "tools/call vault_search ne doit pas retourner d'erreur. got={env}"
    );
    let text = env["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("result.content[0].text attendu. got={env}"));
    let payload: serde_json::Value =
        serde_json::from_str(text).expect("content text doit être du JSON");

    assert_eq!(
        provenances(&payload),
        vec![(format!("reference/{MAIN_NOTE_ID}"), "main".to_owned())],
        "le hit MCP doit porter sa provenance"
    );
}

/// Le client MCP n'a AUCUNE surface pour rejeter un champ additionnel de résultat.
///
/// Preuve d'absence, bornée à la surface interrogée (`tools/list` + `tools/call` du
/// serveur MCP natif) :
/// 1. l'outil `vault_search` ne déclare pas d'`outputSchema` — un client n'a donc rien
///    contre quoi valider le résultat (MCP 2025-06-18 §Tools : la validation du résultat
///    est conditionnée à la déclaration d'un `outputSchema`) ;
/// 2. le résultat ne porte pas de `structuredContent` — le seul champ que la spec
///    soumette à cette validation. Le payload voyage en `content[].text`, contenu libre.
#[tokio::test]
async fn mcp_vault_search_exposes_no_validatable_result_surface() {
    let (addr, jwt, index_path, _dir) = start_mcp_server().await;
    seed_note(&index_path, MAIN_NOTE_ID, "main");

    let listed = call_mcp(
        addr,
        &jwt,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let tool = listed["result"]["tools"]
        .as_array()
        .expect("result.tools tableau")
        .iter()
        .find(|t| t["name"].as_str() == Some("vault_search"))
        .expect("outil vault_search présent")
        .clone();

    assert!(
        tool.get("outputSchema")
            .is_none_or(serde_json::Value::is_null),
        "vault_search ne doit déclarer aucun outputSchema — sinon un champ additionnel \
         devient rejetable côté client. tool={tool}"
    );

    let called = call_mcp(
        addr,
        &jwt,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "vault_search",
                        "arguments": { "query": PROBE_QUERY, "tenant_id": "main" } }
        }),
    )
    .await;

    assert!(
        called["result"]
            .get("structuredContent")
            .is_none_or(serde_json::Value::is_null),
        "le résultat ne doit pas porter de structuredContent — c'est le seul champ que la \
         spec MCP soumette à la validation par outputSchema. result={}",
        called["result"]
    );
}
