//! Tests d'intégration — serveur MCP natif in-process (B2-Phase2 v0.6.0).
//!
//! # Objectifs
//!
//! 1. **R1 — Traversée TrustContext** (test pivot) : prouve que la chaîne
//!    `auth_middleware → StreamableHttpService → call_tool` propage le
//!    [`TrustContext`](gradatum_core::trust::TrustContext) correctement par-requête.
//!    - POST `/mcp` SANS `Authorization: Bearer` → `TrustContext::Unauthenticated` →
//!      le handler MCP retourne une erreur auth (INVALID_REQUEST "non authentifié").
//!      L'outil n'est PAS exécuté.
//!    - POST `/mcp` AVEC un Bearer JWT valide → `TrustContext::BearerToken` → le
//!      handler MCP reçoit un contexte authentifié (ACL peut accepter ou refuser selon
//!      la politique — dans ce test on vérifie uniquement que l'auth traverse, pas le
//!      résultat métier de l'outil).
//!
//! 2. **R3 — Équivalence `list_tools` (golden runtime)** : construit un vrai
//!    `GradatumMcpHandler`, appelle la chaîne HTTP `tools/list` et vérifie :
//!    (a) 21 outils, (b) noms identiques à ceux du stub, (c) inputSchema présent
//!    pour tous les outils paramétrés.
//!
//! 3. **R2 — Protection DNS-rebinding** : un POST `/mcp` avec un `Host` non autorisé
//!    (`evil.example.com`) est rejeté par rmcp AVANT que la requête n'atteigne
//!    l'`auth_middleware` ou le handler MCP.
//!
//! # Architecture du serveur de test
//!
//! Chaque test lance un serveur Axum sur un port éphémère (`127.0.0.1:0`) avec :
//! - Le vrai [`auth_middleware`](gradatum_server::middleware::auth_middleware) (Ed25519
//!   JWT, fail-closed).
//! - Le vrai [`build_mcp_service`](gradatum_server::api_v1::mcp::build_mcp_service).
//! - Un [`AppState`](gradatum_server::state::AppState) de test (clé Ed25519 éphémère,
//!   index SQLite in-memory, ACL vide = default-deny).
//!
//! # Format des requêtes MCP (Streamable HTTP)
//!
//! Le protocole MCP Streamable HTTP exige :
//! - `Content-Type: application/json`
//! - `Accept: application/json, text/event-stream`
//! - Body : JSON-RPC 2.0 avec `"method": "initialize"` pour la première requête
//!   (sans `Mcp-Session-Id`), puis `"method": "tools/list"` ou `"method": "tools/call"`
//!   avec le header `Mcp-Session-Id` retourné par `initialize`.
//!
//! La réponse à `initialize` est un flux SSE contenant la réponse JSON-RPC.
//! Les requêtes suivantes (avec session-id) retournent également un SSE.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use gradatum_auth::jwt::TokenScope;
use gradatum_server::{api_v1, middleware::auth_middleware, state::AppState};
use reqwest::StatusCode;

// ── Liste canonique des 21 outils (parité avec gradatum-mcp-stub) ─────────────

/// Noms canoniques des 21 outils MCP gradatum.
///
/// Cette liste est la référence unique pour le test d'équivalence (R3).
/// Elle doit être maintenue en sync avec :
/// - `gradatum-mcp-stub/src/main.rs::EXPECTED_TOOL_NAMES`
/// - `gradatum-server/src/api_v1/mcp.rs::list_tools`
const CANONICAL_TOOL_NAMES: &[&str] = &[
    // read — 11
    "vault_search",
    "vault_read",
    "vault_list",
    "vault_status",
    "vault_graph",
    "vault_links",
    "vault_trace",
    "vault_context",
    "vault_timeline",
    "vault_authors",
    "vault_tags",
    // write — 3
    "vault_write",
    "vault_classify",
    "vault_downgrade",
    // history F-40 — 4
    "vault_history",
    "vault_history_get",
    "vault_restore",
    "vault_diff",
    // forget F-44 — 1
    "vault_forget",
    // lesson recall F-60 — 1
    "vault_lessons_recall",
    // code scope F-61 — 1
    "code_scope",
];

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Démarre un serveur Axum de test avec le vrai `auth_middleware` + `build_mcp_service`.
///
/// Retourne `(adresse, token_jwt_valide)`.
///
/// L'`AppState` de test utilise :
/// - Clé Ed25519 éphémère (générée à chaque appel — non partagée entre tests).
/// - SQLite in-memory (pas de données persistantes).
/// - ACL vide (default-deny) : les outils qui accèdent à l'index retourneront
///   une erreur ACL, mais l'essentiel est que le `TrustContext` traverse correctement.
async fn start_mcp_test_server() -> (SocketAddr, String) {
    use axum::{Router, middleware};
    use gradatum_server::api_v1::mcp::build_mcp_service;

    let state = AppState::new();

    // Signer un JWT valide avec la clé éphémère de cet AppState.
    let token = state
        .jwt
        .sign(
            "mcp-native-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test — clé éphémère AppState::new()");

    let (mcp_service, _cancel) = build_mcp_service(state.clone());

    // Routeur avec auth_middleware identique à `main.rs::build_router`.
    // `/mcp` est DANS le sous-routeur authed → auth_middleware s'exécute avant.
    //
    // F-02 : on réplique EXACTEMENT le layering de prod — `/mcp` isolé dans son propre
    // `Router` portant la limite de body (512 KiB) via `RequestBodyLimitLayer`. C'est ce
    // qui rend le test 413 (`f02_body_au_dessus_limite_rejete_413`) une preuve valide de
    // l'effectivité de la limite en production (et non un artefact de test).
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
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur MCP de test arrêté proprement");
    });

    // Laisser le serveur démarrer avant d'envoyer des requêtes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    (addr, token)
}

/// Client reqwest sans retry, timeout 5s, sans suivi de redirections.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

/// Corps JSON-RPC MCP `initialize` (première requête obligatoire).
///
/// Envoyer `initialize` d'abord est requis par le protocole Streamable HTTP :
/// rmcp retourne un `Mcp-Session-Id` dans la réponse qui doit être utilisé
/// pour les appels suivants (`tools/list`, `tools/call`, etc.).
fn initialize_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "mcp-native-test", "version": "0.0.1" }
        }
    })
}

/// Corps JSON-RPC MCP `tools/list`.
fn list_tools_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    })
}

/// Corps JSON-RPC MCP `tools/call` pour `vault_search`.
fn vault_search_call_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "vault_search",
            "arguments": { "query": "test" }
        }
    })
}

/// Envoie une requête MCP POST (Streamable HTTP) avec les headers requis.
///
/// Retourne la réponse HTTP brute. Le body SSE peut contenir plusieurs événements ;
/// pour les tests, on lit le premier événement de données.
async fn post_mcp(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
    session_id: Option<&str>,
) -> reqwest::Response {
    let mut req = client
        .post(url)
        // Headers MCP Streamable HTTP obligatoires.
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(body);

    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }

    req.send().await.expect("requête MCP envoyée")
}

/// Extrait le premier objet JSON depuis un body SSE (`data: {...}\n\n`).
///
/// Le format SSE de rmcp est `data: <json>\n\n`. Cette fonction parse le premier
/// événement et retourne le JSON désérialisé.
fn parse_sse_json(text: &str) -> serde_json::Value {
    for line in text.lines() {
        if let Some(json_str) = line.strip_prefix("data: ")
            && let Ok(val) = serde_json::from_str(json_str)
        {
            return val;
        }
    }
    // Si pas de SSE, tenter de parser le body entier comme JSON (JSON direct).
    serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// **R1 — Test pivot : SANS Bearer → TrustContext::Unauthenticated → erreur auth MCP.**
///
/// Prouve que :
/// 1. `auth_middleware` injecte `TrustContext::Unauthenticated` quand pas de Bearer.
/// 2. Le `StreamableHttpService` propage les extensions HTTP aux `RequestContext.extensions`.
/// 3. `call_tool` lit le `TrustContext` depuis les extensions et refuse les appels
///    non authentifiés avec une erreur MCP INVALID_REQUEST.
///
/// Séquence :
/// 1. POST `/mcp` avec `initialize` sans Bearer → session créée (MCP accepte l'initialize
///    sans auth — le TrustContext est Unauthenticated, mais initialize ne vérifie pas l'auth).
/// 2. POST `/mcp` avec `tools/call` sur le session_id obtenu, SANS Bearer → la requête
///    atteint `call_tool` avec `TrustContext::Unauthenticated` → erreur MCP "non authentifié".
#[tokio::test]
async fn r1_sans_bearer_trustcontext_unauthenticated_call_tool_refuses() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Étape 1 : initialize sans Bearer → obtenir le session_id.
    // L'initialize est accepté (rmcp gère l'init avant les vérifs métier).
    let init_resp = post_mcp(&client, &mcp_url, &initialize_body(), None, None).await;
    // rmcp retourne 200 avec SSE pour l'initialize.
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize doit retourner 200 même sans Bearer"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent dans la réponse initialize");
    // Consommer le body SSE pour libérer la connexion.
    let _init_body = init_resp.text().await.expect("body initialize");

    // Étape 2 : tools/call vault_search SANS Bearer sur le session établi.
    // auth_middleware injecte Unauthenticated → Parts injectées → call_tool reçoit Unauthenticated.
    let call_resp = post_mcp(
        &client,
        &mcp_url,
        &vault_search_call_body(),
        None, // PAS de Bearer
        Some(session_id.as_str()),
    )
    .await;

    assert_eq!(
        call_resp.status(),
        StatusCode::OK,
        "rmcp retourne toujours 200 sur un call_tool (l'erreur est dans le body JSON-RPC)"
    );

    let body_text = call_resp.text().await.expect("body call_tool sans bearer");
    let json = parse_sse_json(&body_text);

    // Le résultat MCP doit contenir une erreur (pas un résultat d'outil).
    assert!(
        json.get("error").is_some(),
        "appel sans Bearer doit retourner une erreur MCP, got: {json}"
    );
    let error_msg = json["error"]["message"].as_str().unwrap_or("");
    assert_eq!(
        error_msg, "non authentifié",
        "message d'erreur MCP doit être 'non authentifié', got: {error_msg}"
    );
}

/// **R1 — Test pivot : AVEC Bearer valide → TrustContext authentifié arrive dans call_tool.**
///
/// Prouve que :
/// 1. `auth_middleware` injecte `TrustContext::BearerToken` avec un Bearer valide.
/// 2. Les `Parts` HTTP (avec le `TrustContext`) traversent `StreamableHttpService`.
/// 3. `call_tool` reçoit un contexte authentifié — pas d'erreur "non authentifié".
///    (L'ACL peut ensuite refuser l'accès vault — c'est une erreur différente, pas auth.)
#[tokio::test]
async fn r1_avec_bearer_valide_trustcontext_authentifie_traverse_call_tool() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Étape 1 : initialize AVEC Bearer valide.
    let init_resp = post_mcp(
        &client,
        &mcp_url,
        &initialize_body(),
        Some(valid_token.as_str()),
        None,
    )
    .await;
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize avec Bearer valide doit retourner 200"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent");
    let _init_body = init_resp.text().await.expect("body initialize");

    // Étape 2 : tools/call vault_search AVEC Bearer valide.
    // auth_middleware injecte BearerToken → TrustContext authentifié → call_tool ne refuse pas auth.
    let call_resp = post_mcp(
        &client,
        &mcp_url,
        &vault_search_call_body(),
        Some(valid_token.as_str()),
        Some(session_id.as_str()),
    )
    .await;

    assert_eq!(
        call_resp.status(),
        StatusCode::OK,
        "call_tool avec Bearer valide doit retourner 200"
    );

    let body_text = call_resp.text().await.expect("body call_tool avec bearer");
    let json = parse_sse_json(&body_text);

    // Le résultat NE doit PAS être une erreur d'auth.
    // Il peut y avoir une erreur ACL (vault vide, ACL deny) — ce qui est OK :
    // l'important est que le TrustContext a traversé (pas d'erreur "non authentifié").
    if let Some(error) = json.get("error") {
        let msg = error["message"].as_str().unwrap_or("");
        assert_ne!(
            msg, "non authentifié",
            "avec Bearer valide, call_tool NE DOIT PAS retourner 'non authentifié'. \
             Erreur reçue : {msg}"
        );
    }
    // Si pas d'erreur → succès complet (résultat d'outil ou erreur ACL tolérée).
}

/// **R3 — Équivalence list_tools golden runtime.**
///
/// Construit un vrai serveur MCP (avec `build_mcp_service`), envoie `tools/list`
/// via HTTP et vérifie :
/// (a) 21 outils retournés.
/// (b) Les noms correspondent exactement à `CANONICAL_TOOL_NAMES` (== stub).
/// (c) Chaque outil a un `inputSchema` (objet JSON, éventuellement vide pour
///     les outils sans paramètres).
#[tokio::test]
async fn r3_list_tools_golden_runtime_21_outils_parité_stub() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Étape 1 : initialize pour obtenir le session_id.
    let init_resp = post_mcp(
        &client,
        &mcp_url,
        &initialize_body(),
        Some(valid_token.as_str()),
        None,
    )
    .await;
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize doit réussir"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent");
    let _init_body = init_resp.text().await.expect("body initialize");

    // Étape 2 : tools/list.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
        Some(session_id.as_str()),
    )
    .await;
    assert_eq!(
        list_resp.status(),
        StatusCode::OK,
        "tools/list doit retourner 200"
    );

    let body_text = list_resp.text().await.expect("body tools/list");
    let json = parse_sse_json(&body_text);

    // Vérifier la structure JSON-RPC.
    assert!(
        json.get("error").is_none(),
        "tools/list ne doit pas retourner d'erreur, got: {json}"
    );
    let result = &json["result"];
    let tools = result["tools"]
        .as_array()
        .expect("result.tools doit être un tableau");

    // (a) 21 outils.
    assert_eq!(
        tools.len(),
        21,
        "list_tools doit retourner exactement 21 outils, got: {}",
        tools.len()
    );

    // (b) Noms identiques à CANONICAL_TOOL_NAMES (parité avec le stub).
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in CANONICAL_TOOL_NAMES {
        assert!(
            tool_names.contains(expected),
            "outil '{expected}' manquant dans list_tools. Outils reçus: {tool_names:?}"
        );
    }
    // Vérifier qu'il n'y a pas d'outil supplémentaire inattendu.
    for actual in &tool_names {
        assert!(
            CANONICAL_TOOL_NAMES.contains(actual),
            "outil inattendu '{actual}' dans list_tools (pas dans CANONICAL_TOOL_NAMES)"
        );
    }

    // (c) inputSchema présent pour tous les outils.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<inconnu>");
        assert!(
            tool.get("inputSchema").is_some(),
            "outil '{name}' doit avoir un inputSchema"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "outil '{name}' inputSchema doit être un objet JSON"
        );
    }
}

/// **R2 — Protection DNS-rebinding : Host non autorisé → 403.**
///
/// Vérifie que rmcp rejette les requêtes avec un `Host` header différent de la
/// whitelist `["localhost", "127.0.0.1", "::1"]`. C'est la protection contre les
/// attaques DNS-rebinding (spec MCP Streamable HTTP 2025-06-18).
///
/// Note : `reqwest` envoie le `Host` header basé sur l'URL. Pour simuler un Host
/// non autorisé, on utilise `reqwest` avec un override manuel du header `Host`.
#[tokio::test]
async fn r2_host_non_autorise_rejete_403() {
    let (addr, _token) = start_mcp_test_server().await;
    let client = http_client();

    // Envoyer la requête à 127.0.0.1:<port> mais avec un Host: evil.example.com.
    // reqwest envoie l'URL réelle mais on override le header Host.
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", addr.port()))
        .header("Host", "evil.example.com")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize_body())
        .send()
        .await
        .expect("requête avec Host non autorisé");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Host non autorisé doit être rejeté 403 par rmcp (DNS-rebinding protection)"
    );
}

/// **R3-snapshot — Golden inputSchema des 21 outils MCP (DT-MCP-SCHEMA-1 + durcissement R3).**
///
/// Fige la sérialisation COMPLÈTE des 21 `inputSchema` via `insta::assert_json_snapshot!`.
///
/// Cette garde résout la faiblesse de R3 existant (`inputSchema.is_object()` faible :
/// un Map vide `{}` passait au vert et aurait laissé passer la régression `34e70eb`).
///
/// Le snapshot acté est la preuve de **parité wire octet-pour-octet** avant et après
/// le refactor DT-MCP-SCHEMA-1. Toute divergence de schéma (ajout/retrait de propriété,
/// changement de type) fait échouer ce test.
///
/// La map est triée par nom d'outil (`BTreeMap`) pour déterminisme entre runs.
#[tokio::test]
async fn r3_snapshot_input_schema_21_outils_golden() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Étape 1 : initialize pour obtenir le session_id.
    let init_resp = post_mcp(
        &client,
        &mcp_url,
        &initialize_body(),
        Some(valid_token.as_str()),
        None,
    )
    .await;
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize doit réussir"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent");
    let _init_body = init_resp.text().await.expect("body initialize");

    // Étape 2 : tools/list.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
        Some(session_id.as_str()),
    )
    .await;
    assert_eq!(
        list_resp.status(),
        StatusCode::OK,
        "tools/list doit retourner 200"
    );

    let body_text = list_resp.text().await.expect("body tools/list");
    let json = parse_sse_json(&body_text);
    let tools = json["result"]["tools"]
        .as_array()
        .expect("result.tools doit être un tableau");

    assert_eq!(
        tools.len(),
        21,
        "snapshot golden attend exactement 21 outils"
    );

    // Construire la map nom->inputSchema triée par nom (BTreeMap = déterminisme).
    let schema_map: BTreeMap<String, serde_json::Value> = tools
        .iter()
        .map(|t| {
            let name = t["name"]
                .as_str()
                .expect("chaque outil doit avoir un nom")
                .to_owned();
            let schema = t["inputSchema"].clone();
            (name, schema)
        })
        .collect();

    // Snapshot golden - fige les 21 inputSchema octet-pour-octet.
    // Pour regénérer : INSTA_UPDATE=always cargo nextest run -p gradatum-server r3_snapshot
    insta::assert_json_snapshot!("mcp_tools_input_schema_golden", schema_map);
}

// ── F-01 — Authentification de `list_tools` (durcissement /mcp) ────────────────

/// **F-01 — `tools/list` SANS Bearer → erreur "non authentifié" (miroir de R1).**
///
/// Avant le fix, `list_tools` ignorait le `RequestContext` et divulguait le catalogue
/// des 21 outils (noms + schémas JSON complets) à tout client LAN non authentifié.
///
/// Après le fix, `list_tools` applique la même garde que `call_tool` :
/// `TrustContext::Unauthenticated` → `ErrorData(INVALID_REQUEST, "non authentifié")`.
///
/// Séquence (identique à R1, mais sur `tools/list` au lieu de `tools/call`) :
/// 1. `initialize` SANS Bearer → session_id (l'init reste libre — non-goal F-01).
/// 2. `tools/list` SANS Bearer sur ce session → `call_tool`-like garde → erreur auth.
///
/// Critère de réussite mesurable F-01 (spec C3).
#[tokio::test]
async fn f01_list_tools_sans_bearer_refuse_non_authentifie() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Étape 1 : initialize sans Bearer → obtenir le session_id.
    let init_resp = post_mcp(&client, &mcp_url, &initialize_body(), None, None).await;
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize doit retourner 200 même sans Bearer (non-goal F-01)"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent dans la réponse initialize");
    let _init_body = init_resp.text().await.expect("body initialize");

    // Étape 2 : tools/list SANS Bearer sur le session établi.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        None, // PAS de Bearer
        Some(session_id.as_str()),
    )
    .await;

    assert_eq!(
        list_resp.status(),
        StatusCode::OK,
        "rmcp retourne toujours 200 (l'erreur est dans le body JSON-RPC)"
    );

    let body_text = list_resp.text().await.expect("body tools/list sans bearer");
    let json = parse_sse_json(&body_text);

    // La réponse doit être une erreur MCP, PAS le catalogue d'outils.
    assert!(
        json.get("error").is_some(),
        "tools/list sans Bearer doit retourner une erreur MCP (pas le catalogue), got: {json}"
    );
    let error_msg = json["error"]["message"].as_str().unwrap_or("");
    assert_eq!(
        error_msg, "non authentifié",
        "message d'erreur MCP doit être 'non authentifié', got: {error_msg}"
    );
    // Le catalogue ne doit PAS avoir fuité.
    assert!(
        json.get("result").is_none(),
        "tools/list sans Bearer ne doit divulguer aucun result, got: {json}"
    );
}

/// **F-01 — `tools/list` AVEC Bearer valide → 21 outils (non-régression client).**
///
/// Le client légitime (Claude Code) envoie l'api-key Bearer sur chaque requête.
/// Le gating F-01 ne casse donc PAS la découverte authentifiée : 21 outils renvoyés.
///
/// (R3 existant couvre déjà ce chemin avec Bearer, mais ce test miroir explicite la
/// paire négatif/positif F-01 dans le même fichier — symétrie avec R1.)
#[tokio::test]
async fn f01_list_tools_avec_bearer_retourne_21_outils() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let init_resp = post_mcp(
        &client,
        &mcp_url,
        &initialize_body(),
        Some(valid_token.as_str()),
        None,
    )
    .await;
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize doit réussir"
    );
    let session_id = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .expect("Mcp-Session-Id doit être présent");
    let _init_body = init_resp.text().await.expect("body initialize");

    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
        Some(session_id.as_str()),
    )
    .await;
    assert_eq!(
        list_resp.status(),
        StatusCode::OK,
        "tools/list doit retourner 200"
    );

    let body_text = list_resp.text().await.expect("body tools/list avec bearer");
    let json = parse_sse_json(&body_text);

    assert!(
        json.get("error").is_none(),
        "tools/list avec Bearer valide ne doit pas retourner d'erreur, got: {json}"
    );
    let tools = json["result"]["tools"]
        .as_array()
        .expect("result.tools doit être un tableau");
    assert_eq!(
        tools.len(),
        21,
        "tools/list authentifié doit retourner exactement 21 outils, got: {}",
        tools.len()
    );
}

// ── F-02 — DefaultBodyLimit sur /mcp (anti-DoS, 512 KiB) ───────────────────────

/// **F-02 — POST `/mcp` avec body > 512 KiB → 413 (preuve que le layer mord).**
///
/// C2 (BLOQUANT) : `DefaultBodyLimit` sur un `route_service` (rmcp `StreamableHttpService`,
/// qui lit le body lui-même au niveau `tower::Service`) est INEFFECTIF — vérifié : il
/// renvoyait 422, pas 413. Le fix utilise `tower_http::limit::RequestBodyLimitLayer`, qui
/// agit au niveau service. Ce test EST la preuve que le layering choisi (réplique exacte de
/// `build_router` prod) rejette bien un body surdimensionné en 413, AVANT que rmcp ne
/// consomme le corps.
///
/// Un 413 ici est un HTTP nu (pas une erreur JSON-RPC encadrée) — comportement attendu (P2-1).
#[tokio::test]
async fn f02_body_au_dessus_limite_rejete_413() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // Body JSON-RPC valide structurellement mais avec une charge utile gonflée
    // au-delà de 512 KiB. La limite doit court-circuiter AVANT le parsing rmcp.
    let oversized_payload = "x".repeat(600 * 1024); // 600 KiB > 512 KiB
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "vault_search",
            "arguments": { "query": oversized_payload }
        }
    });

    // Pas besoin d'initialize : la limite de body est appliquée par le layer HTTP
    // en amont de toute logique MCP (init/session). On envoie directement.
    let resp = post_mcp(&client, &mcp_url, &body, Some(valid_token.as_str()), None).await;

    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "POST /mcp avec body > 512 KiB doit être rejeté 413 (DefaultBodyLimit effectif)"
    );
}

/// **F-02 — POST `/mcp` avec body normal → PAS de 413 (limite non sur-restrictive).**
///
/// Garantit que la limite de 512 KiB ne rejette pas un payload légitime de taille
/// normale (ici un `initialize` minimal). La réponse est un 200 SSE rmcp.
#[tokio::test]
async fn f02_body_normal_pas_de_413() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let resp = post_mcp(&client, &mcp_url, &initialize_body(), None, None).await;

    assert_ne!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "un body normal ne doit jamais être rejeté 413"
    );
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "initialize avec body normal doit retourner 200"
    );
}
