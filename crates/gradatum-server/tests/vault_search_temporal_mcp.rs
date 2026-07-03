//! Tests d'intégration MCP — filtre temporel F-65 (`from_ms`/`to_ms` + `anchor_ms`).
//!
//! Couvre :
//! 1. `f65_schema_mcp_from_ms_to_ms_in_inputschema` — l'inputSchema MCP de `vault_search`
//!    expose les propriétés `from_ms` et `to_ms` (schemars génère le schéma depuis
//!    `VaultSearchRequest`).
//! 2. `f65_golden_anchor_ms_via_mcp` — appel `vault_search` via MCP avec bornes temporelles
//!    retourne des items dont `anchor_ms` est valorisé et correspond à la valeur seedée.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::{AnchorSrc, Index, TemporalEntry};
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::api_v1::mcp::build_mcp_service;
use gradatum_server::state::AppState;
use gradatum_server::{api_v1, middleware::auth_middleware};
use reqwest::StatusCode;

// ── Noop embedder ─────────────────────────────────────────────────────────────

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-temporal-mcp"
    }
    fn dim(&self) -> u16 {
        8
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0f32; 8])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── ACL permissive pour "temporal-mcp-tester" ────────────────────────────────

const TEST_ACL: &str = r#"
[[consumer]]
identity = "temporal-mcp-tester"
read_patterns  = ["main/*", "main/main", "*/decisions", "decisions/*"]
write_patterns = []
"#;

// ── Helper — serveur MCP de test avec SqliteIndex exposé ──────────────────────

/// Démarre un serveur MCP de test avec un SqliteIndex in-memory exposé pour le seed.
///
/// Retourne `(adresse, token_jwt, arc_index)`.
async fn start_temporal_mcp_server() -> (SocketAddr, String, Arc<SqliteIndex>) {
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("ACL temporelle valide");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() temporal mcp"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let token = state
        .jwt
        .sign(
            "temporal-mcp-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT temporal mcp");

    let (mcp_service, _cancel) = build_mcp_service(state.clone());

    let mcp_router = Router::new().route_service("/mcp", mcp_service).layer(
        tower_http::limit::RequestBodyLimitLayer::new(gradatum_server::api_v1::mcp::MCP_BODY_LIMIT),
    );

    let authed = Router::new()
        .nest("/api/v1", api_v1::router())
        .merge(mcp_router)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let app = Router::new().merge(authed).with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère temporal mcp");
    let addr = listener
        .local_addr()
        .expect("adresse locale listener temporal mcp");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur temporal mcp arrêté proprement");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    (addr, token, idx)
}

// ── Helper — seed note + temporal_index ──────────────────────────────────────

/// Note IDs conformes Crockford base32 (0-9, A-Z sans I, L, O, U — 26 chars).
const ID_MCP_T6A: &str = "01HX000000000000000F65MCPA"; // anchor_ms = 5_000_000

async fn seed_with_temporal(idx: &Arc<SqliteIndex>, id: &str, anchor_ms: i64) {
    idx.seed_note_with_created(id, "decisions", "temporal mcp test token", anchor_ms)
        .await
        .expect("seed_note_with_created temporal mcp");
    let entry = TemporalEntry {
        note_id: id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry temporal mcp");
}

// ── Client HTTP minimal ───────────────────────────────────────────────────────

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client HTTP temporal mcp")
}

/// Envoie un POST MCP (Streamable HTTP) et retourne la réponse brute.
async fn post_mcp(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    bearer: &str,
) -> reqwest::Response {
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {bearer}"))
        .json(body)
        .send()
        .await
        .expect("POST MCP temporal")
}

/// Extrait le premier objet JSON depuis un body SSE (`data: {...}\n\n`).
fn parse_sse_json(text: &str) -> serde_json::Value {
    for line in text.lines() {
        if let Some(json_str) = line.strip_prefix("data: ")
            && let Ok(val) = serde_json::from_str(json_str)
        {
            return val;
        }
    }
    serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// **F-65 T6.1 — inputSchema MCP de `vault_search` expose `from_ms` et `to_ms`.**
///
/// Vérifie que `tools/list` retourne un schema pour `vault_search` dont les
/// propriétés incluent `from_ms` et `to_ms` — preuve que schemars génère le
/// JSON Schema depuis `VaultSearchRequest` incluant les champs F-65.
///
/// Invariant : MCP client (Claude) peut utiliser `from_ms`/`to_ms` dans ses appels.
#[tokio::test]
async fn f65_schema_mcp_from_ms_to_ms_in_inputschema() {
    let (addr, token, _idx) = start_temporal_mcp_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let resp = post_mcp(
        &client,
        &mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }),
        &token,
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tools/list doit retourner 200"
    );

    let body_text = resp.text().await.expect("body tools/list temporal");
    let json = parse_sse_json(&body_text);

    assert!(
        json.get("error").is_none(),
        "tools/list ne doit pas retourner d'erreur, got: {json}"
    );

    let tools = json["result"]["tools"]
        .as_array()
        .expect("result.tools doit être un tableau");

    // Trouver l'outil vault_search.
    let vault_search_tool = tools
        .iter()
        .find(|t| t["name"].as_str() == Some("vault_search"))
        .expect("outil 'vault_search' doit être présent dans tools/list");

    let properties = &vault_search_tool["inputSchema"]["properties"];

    assert!(
        !properties["from_ms"].is_null(),
        "inputSchema.properties.from_ms doit exister pour vault_search. schema={vault_search_tool}"
    );
    assert!(
        !properties["to_ms"].is_null(),
        "inputSchema.properties.to_ms doit exister pour vault_search. schema={vault_search_tool}"
    );
}

/// **F-65 T6.2 — Golden `anchor_ms` via MCP : appel `vault_search` avec bornes temporelles.**
///
/// Vérifie end-to-end via le chemin MCP (JSON-RPC → SSE → contenu texte sérialisé) que :
/// 1. L'appel `vault_search` avec `from_ms`/`to_ms` réussit (pas d'erreur MCP).
/// 2. Le hit pour la note seedée expose `anchor_ms` avec la valeur exacte seedée.
///
/// Le contenu du hit est dans `result.content[0].text` — JSON sérialisé de
/// `VaultSearchResponse` (via `to_mcp_content`).
#[tokio::test]
async fn f65_golden_anchor_ms_via_mcp() {
    const ANCHOR: i64 = 5_000_000;

    let (addr, token, idx) = start_temporal_mcp_server().await;
    seed_with_temporal(&idx, ID_MCP_T6A, ANCHOR).await;

    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Appel vault_search via MCP avec bornes temporelles encadrant ANCHOR.
    let resp = post_mcp(
        &client,
        &mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "vault_search",
                "arguments": {
                    "query": "temporal mcp test token",
                    "tenant_id": "main",
                    "from_ms": ANCHOR - 1_000_000i64,
                    "to_ms":   ANCHOR + 1_000_000i64
                }
            }
        }),
        &token,
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_search MCP doit retourner 200"
    );

    let body_text = resp.text().await.expect("body vault_search MCP");
    let json = parse_sse_json(&body_text);

    assert!(
        json.get("error").is_none(),
        "vault_search MCP ne doit pas retourner d'erreur. got: {json}"
    );

    // result.content[0].text contient le JSON sérialisé de VaultSearchResponse.
    let content_text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("result.content[0].text doit être une chaîne JSON");

    let search_resp: serde_json::Value =
        serde_json::from_str(content_text).expect("content text doit être du JSON valide");

    let items = search_resp["items"]
        .as_array()
        .expect("VaultSearchResponse.items doit être un tableau");

    // La note seedée doit être présente.
    let hit = items
        .iter()
        .find(|it| it["path"].as_str().is_some_and(|p| p.contains(ID_MCP_T6A)))
        .expect("hit pour la note seedée doit être présent dans les résultats MCP");

    assert_eq!(
        hit["anchor_ms"].as_i64(),
        Some(ANCHOR),
        "anchor_ms doit valoir {ANCHOR} dans le hit MCP. hit={hit}"
    );
}
