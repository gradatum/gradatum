//! Tests d'intégration — serveur MCP natif in-process (B2-Phase2 v0.6.0, mode stateless v0.6.5+).
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
//!    (a) autant d'outils que de noms canoniques, (b) noms identiques à
//!    [`CANONICAL_TOOL_NAMES`], (c) inputSchema présent pour tous les outils paramétrés.
//!    Aucun effectif n'est écrit en dur : la population grandit avec le produit, et un
//!    nombre gravé se contente de mentir un release plus tard.
//!
//! 3. **R2 — Protection DNS-rebinding** : un POST `/mcp` avec un `Host` non autorisé
//!    (`evil.example.com`) est rejeté par rmcp AVANT que la requête n'atteigne
//!    l'`auth_middleware` ou le handler MCP.
//!
//! # Mode STATELESS (v0.6.5+)
//!
//! Depuis `build_mcp_service` avec `.with_stateful_mode(false)` :
//! - rmcp n'émet plus le header `Mcp-Session-Id` dans aucune réponse.
//! - Chaque POST est autonome (OneshotTransport par requête) — pas de handshake
//!   `initialize` requis avant `tools/list` ou `tools/call`.
//! - GET et DELETE ne sont plus supportés (405 Method Not Allowed).
//!
//! Conséquence pour les tests : les séquences `initialize → session_id → request`
//! sont remplacées par des POST directs sans session. L'invariant R1 est préservé
//! via le `TrustContext` injecté par `auth_middleware` dans chaque requête.
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
//! # Format des requêtes MCP (Streamable HTTP, mode stateless)
//!
//! - `Content-Type: application/json`
//! - `Accept: application/json, text/event-stream`
//! - Body : JSON-RPC 2.0. En stateless, `tools/list` et `tools/call` peuvent être
//!   envoyés directement sans `initialize` préalable ni `Mcp-Session-Id`.
//! - Réponse : flux SSE (`data: <json>\n\n`) ou JSON direct selon configuration.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use gradatum_auth::jwt::TokenScope;
use gradatum_server::{api_v1, middleware::auth_middleware, state::AppState};
use reqwest::StatusCode;

// ── Liste canonique des outils MCP servis par gradatum-server ─────────────────

/// Noms canoniques des outils MCP exposés par **le serveur**.
///
/// Cette liste est la référence unique du test d'équivalence (R3), et le seul effectif
/// auquel les tests se comparent — d'où l'absence de cardinal écrit en dur ailleurs.
///
/// Elle doit être maintenue en sync avec `gradatum-server/src/api_v1/mcp.rs::list_tools`.
///
/// ⚠️ Elle n'est **pas** en parité avec `gradatum-mcp-stub/src/main.rs` : le stub n'expose
/// pas `job_status`. Cette liste ne mesure donc que la surface serveur ; aucun test ne
/// compare aujourd'hui les deux surfaces, et prétendre le contraire ferait passer un écart
/// réel pour une propriété vérifiée.
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
    // feature card creation — 1 (numéro F-XX attribué serveur)
    "create_feature_card",
    // history F-40 — 4
    "vault_history",
    "vault_history_get",
    "vault_restore",
    "vault_diff",
    // forget F-44 — 1
    "vault_forget",
    // archives listing F-100 1.6 — 1 (LECTURE SEULE ; delete/restore/purge = interne uniquement)
    "vault_archives_list",
    // lesson recall F-60 — 1
    "vault_lessons_recall",
    // code scope F-61 — 1
    "code_scope",
    // proactive recall F-46 — 2
    "vault_proactive_recall",
    "vault_proactive_recall_feedback",
    // job introspection F-63 — 1 (état terminal d'un job async, « tout MCP natif »)
    "job_status",
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

/// Démarre un serveur Axum de test avec ACL permissive pour `"mcp-native-tester"` + store proactive_recall in-memory.
///
/// Différence vs `start_mcp_test_server()` :
/// - ACL configurée avec un preset qui autorise `"mcp-native-tester"` à lire/écrire partout.
/// - Store `proactive_recall` in-memory injecté → les sessions sont persistées en RAM.
///
/// Utilisée par les tests F-46 qui doivent atteindre la logique métier au-delà de l'ACL.
async fn start_permissive_test_server() -> (SocketAddr, String) {
    use axum::{Router, middleware};
    use gradatum_acl_policy::AclEngine;
    use gradatum_auth::jwt::JwtService;
    use gradatum_server::api_v1::mcp::build_mcp_service;
    use gradatum_server::proactive_recall_store::ProactiveRecallStore;

    // Preset ACL : "mcp-native-tester" peut lire et écrire dans tous les loci.
    // Les champs TOML sont `read_patterns` et `write_patterns` (ConsumerEntry).
    const PERMISSIVE_PRESET: &str = r#"
[[consumer]]
identity = "mcp-native-tester"
read_patterns = ["**"]
write_patterns = ["**"]
"#;

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(PERMISSIVE_PRESET)
        .expect("preset ACL permissif valide — invariant de test");
    let recall_store = ProactiveRecallStore::open_in_memory()
        .await
        .expect("ProactiveRecallStore in-memory valide — invariant de test");
    let state = AppState::with_jwt_and_acl(jwt, acl).with_proactive_recall(recall_store);

    let token = state
        .jwt
        .sign(
            "mcp-native-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test — clé éphémère AppState permissif");

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
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test permissif arrêté proprement");
    });

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

/// Corps JSON-RPC MCP `tools/list`.
///
/// En mode stateless, cet appel peut être envoyé directement sans initialize préalable
/// ni `Mcp-Session-Id`. rmcp dispatche chaque POST de manière autonome (OneshotTransport).
fn list_tools_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    })
}

/// Corps JSON-RPC MCP `tools/call` pour `vault_search`.
fn vault_search_call_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "vault_search",
            "arguments": { "query": "test" }
        }
    })
}

/// Envoie une requête MCP POST (Streamable HTTP) avec les headers requis.
///
/// En mode stateless, `session_id` est ignoré (le paramètre est conservé pour la
/// lisibilité des appels mais n'est jamais envoyé — rmcp stateless n'en tient pas compte).
///
/// Retourne la réponse HTTP brute.
async fn post_mcp(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
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

/// **STATELESS — Test pivot : POST direct sans initialize ni Mcp-Session-Id.**
///
/// Encode la correction du bug de décrochage MCP (session in-memory perdue après
/// redémarrage serveur). En mode stateless, chaque POST `tools/list` est autonome :
/// aucun handshake `initialize` préalable n'est requis.
///
/// Invariant : avec un Bearer valide, `tools/list` en POST direct retourne le catalogue
/// complet sans aucun `Mcp-Session-Id` dans la requête ni dans la réponse.
#[tokio::test]
async fn stateless_tools_list_sans_initialize_retourne_le_catalogue_complet() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST direct tools/list SANS initialize préalable, SANS Mcp-Session-Id.
    let resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
    )
    .await;

    // Vérifier qu'aucun Mcp-Session-Id n'est émis (invariant stateless).
    assert!(
        resp.headers().get("Mcp-Session-Id").is_none(),
        "mode stateless ne doit PAS émettre Mcp-Session-Id"
    );

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST direct tools/list sans initialize doit retourner 200"
    );

    let body_text = resp.text().await.expect("body tools/list stateless");
    let json = parse_sse_json(&body_text);

    assert!(
        json.get("error").is_none(),
        "tools/list stateless ne doit pas retourner d'erreur, got: {json}"
    );

    let tools = json["result"]["tools"]
        .as_array()
        .expect("result.tools doit être un tableau");

    assert_eq!(
        tools.len(),
        CANONICAL_TOOL_NAMES.len(),
        "POST direct tools/list doit retourner tout le catalogue canonique, got: {}",
        tools.len()
    );

    // Vérifier la parité des noms avec CANONICAL_TOOL_NAMES.
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in CANONICAL_TOOL_NAMES {
        assert!(
            tool_names.contains(expected),
            "outil '{expected}' manquant. Reçus: {tool_names:?}"
        );
    }
}

/// **Invariant fondateur F-100 (`decisions/01KXAP7Z61`) — surface MCP.**
///
/// Test structurel de la contrainte #3 (arbitrage Tech Lead) : les mutations du cycle
/// delete/archive (`delete`, `restore`, `purge`) ne sont JAMAIS exposées en MCP. Seul le
/// listing **lecture seule** `vault_archives_list` est présent. Prouve mécaniquement que la
/// « main des agents » (canal MCP) ne peut ni supprimer, ni restaurer, ni détruire une
/// archive — uniquement les VOIR.
#[tokio::test]
async fn mcp_surface_excludes_archive_mutations() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tools/list doit répondre 200"
    );
    let body_text = resp.text().await.expect("body tools/list");
    let json = parse_sse_json(&body_text);
    let tools = json["result"]["tools"]
        .as_array()
        .expect("result.tools tableau");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Aucune mutation du cycle delete/archive n'est exposée (assertion par nom exact —
    // `vault_restore` = history CoW légitime, `vault_archives_list` = lecture seule légitime,
    // ne pas les confondre avec les mutations d'archive interdites ci-dessous).
    for forbidden in [
        "vault_delete",
        "vault_archives_delete",
        "vault_archives_restore",
        "vault_archives_purge",
    ] {
        assert!(
            !tool_names.contains(&forbidden),
            "outil interdit '{forbidden}' NE DOIT PAS apparaître en MCP. Reçus: {tool_names:?}"
        );
    }

    // Le listing lecture seule reste présent (les agents VOIENT les archives).
    assert!(
        tool_names.contains(&"vault_archives_list"),
        "le listing lecture seule vault_archives_list doit rester exposé. Reçus: {tool_names:?}"
    );
}

/// **R1 — Test pivot : SANS Bearer → TrustContext::Unauthenticated → erreur auth MCP.**
///
/// Prouve que :
/// 1. `auth_middleware` injecte `TrustContext::Unauthenticated` quand pas de Bearer.
/// 2. Le `StreamableHttpService` propage les extensions HTTP aux `RequestContext.extensions`.
/// 3. `call_tool` lit le `TrustContext` depuis les extensions et refuse les appels
///    non authentifiés avec une erreur MCP INVALID_REQUEST.
///
/// En mode stateless, chaque POST est autonome — on envoie `tools/call` directement.
#[tokio::test]
async fn r1_sans_bearer_trustcontext_unauthenticated_call_tool_refuses() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST tools/call SANS Bearer — directement, sans initialize préalable.
    // auth_middleware injecte Unauthenticated → Parts injectées → call_tool reçoit Unauthenticated.
    let call_resp = post_mcp(
        &client,
        &mcp_url,
        &vault_search_call_body(),
        None, // PAS de Bearer
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
        error_msg, "not authenticated",
        "message d'erreur MCP doit être 'not authenticated', got: {error_msg}"
    );
}

/// **R1 — Test pivot : AVEC Bearer valide → TrustContext authentifié arrive dans call_tool.**
///
/// Prouve que :
/// 1. `auth_middleware` injecte `TrustContext::BearerToken` avec un Bearer valide.
/// 2. Les `Parts` HTTP (avec le `TrustContext`) traversent `StreamableHttpService`.
/// 3. `call_tool` reçoit un contexte authentifié — pas d'erreur "non authentifié".
///    (L'ACL peut ensuite refuser l'accès vault — c'est une erreur différente, pas auth.)
///
/// En mode stateless, le POST est autonome — pas d'initialize requis.
#[tokio::test]
async fn r1_avec_bearer_valide_trustcontext_authentifie_traverse_call_tool() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST tools/call AVEC Bearer valide — directement, sans initialize.
    // auth_middleware injecte BearerToken → TrustContext authentifié → call_tool ne refuse pas auth.
    let call_resp = post_mcp(
        &client,
        &mcp_url,
        &vault_search_call_body(),
        Some(valid_token.as_str()),
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
            msg, "not authenticated",
            "avec Bearer valide, call_tool NE DOIT PAS retourner 'not authenticated'. \
             Erreur reçue : {msg}"
        );
    }
    // Si pas d'erreur → succès complet (résultat d'outil ou erreur ACL tolérée).
}

/// **R3 — Équivalence list_tools golden runtime.**
///
/// Construit un vrai serveur MCP (avec `build_mcp_service`), envoie `tools/list`
/// en POST direct (mode stateless) et vérifie :
/// (a) Autant d'outils que `CANONICAL_TOOL_NAMES` en contient.
/// (b) Les noms correspondent exactement à `CANONICAL_TOOL_NAMES` (double inclusion).
/// (c) Chaque outil a un `inputSchema` (objet JSON, éventuellement vide pour
///     les outils sans paramètres).
///
/// Ce test ne dit **rien** de `gradatum-mcp-stub` : il ne mesure que la surface serveur.
#[tokio::test]
async fn r3_list_tools_golden_runtime_egale_la_liste_canonique() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST direct tools/list — mode stateless, pas d'initialize requis.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
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

    // (a) Effectif == celui de la liste canonique (attrape aussi un doublon de nom, qu'une
    // simple double inclusion laisserait passer).
    assert_eq!(
        tools.len(),
        CANONICAL_TOOL_NAMES.len(),
        "list_tools doit retourner exactement la liste canonique, got: {}",
        tools.len()
    );

    // (b) Noms identiques à CANONICAL_TOOL_NAMES (double inclusion, surface serveur seule).
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
/// Ce comportement est indépendant du mode stateful/stateless : la vérification
/// du header `Host` est effectuée avant le dispatch de la requête.
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
        .json(&list_tools_body())
        .send()
        .await
        .expect("requête avec Host non autorisé");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Host non autorisé doit être rejeté 403 par rmcp (DNS-rebinding protection)"
    );
}

/// **R3-snapshot — Golden inputSchema de chaque outil MCP exposé (DT-MCP-SCHEMA-1 + durcissement R3).**
///
/// Fige la sérialisation COMPLÈTE de tous les `inputSchema` via `insta::assert_json_snapshot!`.
///
/// Cette garde résout la faiblesse de R3 existant (`inputSchema.is_object()` faible :
/// un Map vide `{}` passait au vert et aurait laissé passer la régression `34e70eb`).
///
/// Le snapshot acté est la preuve de **parité wire octet-pour-octet** avant et après
/// le refactor DT-MCP-SCHEMA-1. Toute divergence de schéma (ajout/retrait de propriété,
/// changement de type) fait échouer ce test. L'ensemble des clés du snapshot étant
/// l'ensemble des noms d'outils, l'ajout ou le retrait d'un outil le fait rougir aussi :
/// c'est le snapshot qui porte l'effectif, jamais le nom de ce test.
///
/// La map est triée par nom d'outil (`BTreeMap`) pour déterminisme entre runs.
#[tokio::test]
async fn r3_snapshot_input_schema_de_chaque_outil_expose_golden() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST direct tools/list — mode stateless.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
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
        CANONICAL_TOOL_NAMES.len(),
        "snapshot golden attend le catalogue canonique complet"
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

    // Snapshot golden - fige tous les inputSchema octet-pour-octet.
    // Pour regénérer : INSTA_UPDATE=always cargo nextest run -p gradatum-server r3_snapshot
    insta::assert_json_snapshot!("mcp_tools_input_schema_golden", schema_map);
}

// ── F-01 — Authentification de `list_tools` (durcissement /mcp) ────────────────

/// **F-01 — `tools/list` SANS Bearer → erreur "non authentifié".**
///
/// Avant le fix, `list_tools` ignorait le `RequestContext` et divulguait le catalogue
/// complet des outils (noms + schémas JSON complets) à tout client LAN non authentifié.
///
/// Après le fix, `list_tools` applique la même garde que `call_tool` :
/// `TrustContext::Unauthenticated` → `ErrorData(INVALID_REQUEST, "non authentifié")`.
///
/// En mode stateless, le POST direct sans Bearer doit retourner l'erreur auth.
#[tokio::test]
async fn f01_list_tools_sans_bearer_refuse_non_authentifie() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // POST direct tools/list SANS Bearer.
    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        None, // PAS de Bearer
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
        error_msg, "not authenticated",
        "message d'erreur MCP doit être 'not authenticated', got: {error_msg}"
    );
    // Le catalogue ne doit PAS avoir fuité.
    assert!(
        json.get("result").is_none(),
        "tools/list sans Bearer ne doit divulguer aucun result, got: {json}"
    );
}

/// **F-01 — `tools/list` AVEC Bearer valide → catalogue complet (non-régression client).**
///
/// Le client légitime (Claude Code) envoie l'api-key Bearer sur chaque requête.
/// Le gating F-01 ne casse donc PAS la découverte authentifiée : le catalogue entier est
/// renvoyé.
#[tokio::test]
async fn f01_list_tools_avec_bearer_retourne_le_catalogue_complet() {
    let (addr, valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let list_resp = post_mcp(
        &client,
        &mcp_url,
        &list_tools_body(),
        Some(valid_token.as_str()),
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
        CANONICAL_TOOL_NAMES.len(),
        "tools/list authentifié doit retourner tout le catalogue canonique, got: {}",
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
/// La limite est vérifiée AVANT toute logique MCP — pas d'initialize requis.
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
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "vault_search",
            "arguments": { "query": oversized_payload }
        }
    });

    let resp = post_mcp(&client, &mcp_url, &body, Some(valid_token.as_str())).await;

    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "POST /mcp avec body > 512 KiB doit être rejeté 413 (DefaultBodyLimit effectif)"
    );
}

/// **F-02 — POST `/mcp` avec body normal → PAS de 413 (limite non sur-restrictive).**
///
/// Garantit que la limite de 512 KiB ne rejette pas un payload légitime de taille
/// normale (ici un `tools/list` minimal). La réponse est un 200 SSE rmcp.
#[tokio::test]
async fn f02_body_normal_pas_de_413() {
    let (addr, _valid_token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let resp = post_mcp(&client, &mcp_url, &list_tools_body(), None).await;

    assert_ne!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "un body normal ne doit jamais être rejeté 413"
    );
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tools/list avec body normal doit retourner 200"
    );
}

// ── F-46 — Proactive Recall endpoints (e2e HTTP) ─────────────────────────────

/// **F-46 — POST `/api/v1/proactive_recall` mode proactive → 200 surface vide.**
///
/// Le serveur de test démarre avec SQLite in-memory et aucune donnée.
/// En mode proactive (sans `context`), le store est absent (`None`) →
/// la surface est vide (`items: []`). Ce n'est pas une erreur — 200 attendu.
///
/// Prouve que l'endpoint est câblé et que le handler traite correctement
/// l'absence de store proactif.
#[tokio::test]
async fn f46_proactive_recall_mode_proactive_surface_vide_retourne_200() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let api_url = format!("http://127.0.0.1:{}/api/v1/proactive_recall", addr.port());

    let resp = client
        .post(&api_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "proactive_recall mode proactive (surface vide) doit retourner 200"
    );

    let body: serde_json::Value = resp.json().await.expect("body JSON proactive_recall");

    // recall_id présent (ULID généré par le serveur).
    assert!(
        body["recall_id"].as_str().is_some(),
        "recall_id doit être présent dans la réponse, got: {body}"
    );
    // mode = "proactive" (context absent).
    assert_eq!(
        body["mode"].as_str(),
        Some("proactive"),
        "mode doit être 'proactive' quand context absent"
    );
    // items vides (corpus vide, store absent).
    let items = body["items"]
        .as_array()
        .expect("items doit être un tableau");
    assert!(
        items.is_empty(),
        "items doit être vide (corpus vide), got: {items:?}"
    );
}

/// **F-46 — POST `/api/v1/proactive_recall` mode contextual → 200 items vides.**
///
/// En mode contextuel (avec `context`), le retrieval RRF s'exécute sur un corpus
/// vide → `items: []`. Ce n'est pas une erreur — 200 attendu.
///
/// Prouve que l'endpoint gère le mode contextuel et que le dispatch `context` → RRF
/// est câblé correctement.
#[tokio::test]
async fn f46_proactive_recall_mode_contextuel_corpus_vide_retourne_200() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let api_url = format!("http://127.0.0.1:{}/api/v1/proactive_recall", addr.port());

    let resp = client
        .post(&api_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "context": "test contextuel recall" }))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall contextual");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "proactive_recall mode contextual (corpus vide) doit retourner 200"
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("body JSON proactive_recall contextual");

    assert_eq!(
        body["mode"].as_str(),
        Some("contextual"),
        "mode doit être 'contextual' quand context présent"
    );
    let items = body["items"]
        .as_array()
        .expect("items doit être un tableau");
    assert!(
        items.is_empty(),
        "items doit être vide (corpus vide), got: {items:?}"
    );
}

/// **F-46 — POST `/api/v1/proactive_recall/feedback` recall_id inconnu → 400.**
///
/// Un `recall_id` qui n'a jamais été créé est inexistant dans le store.
/// L'orchestrateur retourne `GradatumError::InvalidInput` → handler mappe en 400.
///
/// Prouve que l'endpoint feedback est câblé et que la validation `recall_id`
/// fonctionne correctement.
#[tokio::test]
async fn f46_proactive_recall_feedback_recall_id_inconnu_retourne_400() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let feedback_url = format!(
        "http://127.0.0.1:{}/api/v1/proactive_recall/feedback",
        addr.port()
    );

    let resp = client
        .post(&feedback_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "recall_id": "01JXYZ_INCONNU_000000000000",
            "accepted_ulids": []
        }))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall/feedback");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "feedback avec recall_id inconnu doit retourner 400"
    );
}

/// **F-46 — POST `/api/v1/proactive_recall/feedback` session validée → 200.**
///
/// Flux complet : (1) POST /proactive_recall → recall_id.
///               (2) POST /proactive_recall/feedback avec le recall_id → 200.
///
/// Prouve que la corrélation session → feedback fonctionne end-to-end.
#[tokio::test]
async fn f46_proactive_recall_feedback_session_validee_retourne_200() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let recall_url = format!("http://127.0.0.1:{}/api/v1/proactive_recall", addr.port());
    let feedback_url = format!(
        "http://127.0.0.1:{}/api/v1/proactive_recall/feedback",
        addr.port()
    );

    // Étape 1 : obtenir un recall_id valide depuis le mode proactive.
    let recall_resp = client
        .post(&recall_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall étape 1");

    assert_eq!(
        recall_resp.status(),
        StatusCode::OK,
        "étape 1 : proactive_recall doit retourner 200"
    );

    let recall_body: serde_json::Value = recall_resp.json().await.expect("body proactive_recall");
    let recall_id = recall_body["recall_id"]
        .as_str()
        .expect("recall_id présent dans la réponse")
        .to_owned();

    // Étape 2 : feedback avec accepted_ulids vide (valide : [] ⊆ surfaced).
    let feedback_resp = client
        .post(&feedback_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "recall_id": recall_id,
            "accepted_ulids": []
        }))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall/feedback étape 2");

    assert_eq!(
        feedback_resp.status(),
        StatusCode::OK,
        "étape 2 : feedback avec recall_id valide et accepted_ulids [] doit retourner 200"
    );
}

/// **F-46 — POST `/api/v1/proactive_recall` SANS Bearer → 401.**
///
/// Même garde d'authentification que tous les autres endpoints `/api/v1`.
#[tokio::test]
async fn f46_proactive_recall_sans_bearer_retourne_401() {
    let (addr, _token) = start_mcp_test_server().await;
    let client = http_client();
    let api_url = format!("http://127.0.0.1:{}/api/v1/proactive_recall", addr.port());

    let resp = client
        .post(&api_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST /api/v1/proactive_recall sans Bearer");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "proactive_recall sans Bearer doit retourner 401"
    );
}

// ── Task 12 — vault_context e2e HTTP (F-29 reference_mode + F-30 compact) ────

/// **T12-e2e-1 — `POST /api/v1/vault_context` mode=compact SANS session_id → 400.**
///
/// Prouve que l'endpoint `/api/v1/vault_context` valide l'absence de `session_id`
/// en mode compact et retourne 400 BAD_REQUEST (GradatumError::InvalidInput).
///
/// Invariant : `assemble_compact` exige un session_id — le routage HTTP +
/// handler mappent correctement l'InvalidInput en 400.
#[tokio::test]
async fn vault_context_e2e_compact_sans_session_id_retourne_400() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let url = format!("http://127.0.0.1:{}/api/v1/vault_context", addr.port());

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "query": "test compact sans session",
            "mode": "compact"
            // session_id absent intentionnellement
        }))
        .send()
        .await
        .expect("POST /api/v1/vault_context mode=compact sans session_id");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "T12-e2e-1 : mode=compact sans session_id doit retourner 400 BAD_REQUEST"
    );
}

/// **T12-e2e-2 — `POST /api/v1/vault_context` mode=compact AVEC session_id valide → 200.**
///
/// Prouve que l'endpoint `/api/v1/vault_context` accepte un session_id ULID valide
/// en mode compact et retourne 200 (vue foldée — corpus vide → dégradation gracieuse P2-4).
///
/// Dégradation P2-4 : `session_trace=None` (absent du serveur de test) → compact retourne
/// une vue assembled vide sans crasher (HashMap::new() comme sent_map).
#[tokio::test]
async fn vault_context_e2e_compact_avec_session_id_retourne_200() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let url = format!("http://127.0.0.1:{}/api/v1/vault_context", addr.port());

    // ULID Crockford base32 valide — 26 chars ASCII alphanumériques.
    // Hardcodé pour éviter de dépendre de la crate ulid dans ce test.
    let session_id = "01JXTASK12E2ECOMPACT000001";

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "query": "test compact session valide",
            "mode": "compact",
            "session_id": session_id,
        }))
        .send()
        .await
        .expect("POST /api/v1/vault_context mode=compact avec session_id valide");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "T12-e2e-2 : mode=compact avec session_id ULID valide doit retourner 200 \
         (dégradation gracieuse P2-4 — session_trace absent)"
    );

    let body: serde_json::Value = resp.json().await.expect("body JSON vault_context compact");
    // `assembled_text` présent dans toute réponse vault_context réussie.
    assert!(
        body.get("assembled_text").is_some(),
        "T12-e2e-2 : assembled_text doit être présent dans la réponse compact, got: {body}"
    );
}

/// **T12-e2e-3 — `POST /api/v1/vault_context` reference_mode=true → 200 + champs F-29 présents.**
///
/// Prouve que l'endpoint `/api/v1/vault_context` avec `reference_mode=true` :
/// 1. Retourne 200 (champ reconnu, aucun crash).
/// 2. Expose les champs `references` (tableau — vide si corpus vide) et
///    `counts` (objet avec les 3 sous-champs `inline`, `stub`, `dropped`).
///
/// Les invariants de contenu (`references` non vide) sont couverts par les tests unitaires
/// `context_reference_mode_on_emits_references` et `select_split_inline_then_stub_then_drop`
/// dans `context_assembly.rs` (tower oneshot + corpus seedé).
#[tokio::test]
async fn vault_context_e2e_reference_mode_champs_presents() {
    let (addr, valid_token) = start_permissive_test_server().await;
    let client = http_client();
    let url = format!("http://127.0.0.1:{}/api/v1/vault_context", addr.port());

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "query": "test reference_mode",
            "reference_mode": true,
            "budget_tokens": 1,
        }))
        .send()
        .await
        .expect("POST /api/v1/vault_context reference_mode=true");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "T12-e2e-3 : reference_mode=true doit retourner 200"
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("body JSON vault_context reference_mode");

    // `references` toujours présent (même vide si corpus vide).
    assert!(
        body.get("references").and_then(|v| v.as_array()).is_some(),
        "T12-e2e-3 : references doit être présent et être un tableau, got: {body}"
    );

    // `counts` avec les 3 sous-champs invariants F-29.
    let counts = body
        .get("counts")
        .expect("T12-e2e-3 : counts doit être présent dans la réponse");
    for field in ["inline", "stub", "dropped"] {
        assert!(
            counts.get(field).is_some(),
            "T12-e2e-3 : counts.{field} doit être présent, got counts: {counts}"
        );
    }
}

// ── Négociation de version de protocole MCP (handshake `initialize`) ───────────

/// Corps JSON-RPC MCP `initialize` demandant une `protocolVersion` précise.
///
/// Les trois champs `protocolVersion` / `capabilities` / `clientInfo` sont requis par
/// `InitializeRequestParams` (rmcp). `capabilities` vide et un `clientInfo` minimal
/// suffisent à désérialiser une requête valide.
fn initialize_body(protocol_version: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": "negotiation-test-client", "version": "1.0.0" }
        }
    })
}

/// Extrait `result.protocolVersion` d'une réponse `initialize` (SSE ou JSON direct).
async fn negotiated_version(resp: reqwest::Response) -> String {
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "initialize doit retourner 200 (handshake jamais cassé)"
    );
    let body_text = resp.text().await.expect("body initialize");
    let json = parse_sse_json(&body_text);
    assert!(
        json.get("error").is_none(),
        "initialize ne doit pas retourner d'erreur, got: {json}"
    );
    json["result"]["protocolVersion"]
        .as_str()
        .unwrap_or_else(|| panic!("result.protocolVersion absent de la réponse, got: {json}"))
        .to_owned()
}

/// **Négociation de version — vrai handshake `initialize` par le chemin de production.**
///
/// Passe par la CHAÎNE RÉELLE : POST `/mcp` → `auth_middleware` →
/// `StreamableHttpService` (mode stateless, `build_mcp_service`) →
/// `GradatumMcpHandler::initialize`. Le transport Streamable HTTP renvoie la
/// `protocolVersion` du handler **verbatim** — il ne renégocie pas (à la différence du
/// transport stdio `serve_server` de rmcp, qui ajusterait la version après coup et
/// **masquerait** le bug). Ce test exerce donc bien la négociation du handler, pas un double.
///
/// Red-proof : avant le correctif, `initialize` renvoyait inconditionnellement
/// `ProtocolVersion::default()` = `LATEST` (2025-11-25) ; ce test échouait pour toute
/// version demandée différente.
///
/// - **C1 (non-régression)** : chaque version supportée demandée est renvoyée à l'identique —
///   en particulier `2025-11-25`, celle que parlent les consommateurs actuels.
/// - **C2 (échec bruyant)** : une version inconnue/future produit un repli **observable**
///   (log `WARN` côté serveur) sur `LATEST`, conforme à la spec MCP « Version Negotiation »
///   (« the server MUST respond with another protocol version it supports … SHOULD be the
///   latest »). Le handshake n'est jamais cassé.
///
/// `initialize` ne requiert pas d'authentification (l'injection d'âme est optionnelle et
/// dégradée sans Bearer) — on teste ici uniquement la version négociée, indépendante de l'auth.
#[tokio::test]
async fn initialize_negocie_versions_supportees_et_repli_sur_inconnue() {
    let (addr, _token) = start_mcp_test_server().await;
    let client = http_client();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // C1 — toute version servable par le SDK est renvoyée à l'identique.
    // `2025-11-25` en tête : c'est la non-régression stricte des consommateurs actuels.
    for requested in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let resp = post_mcp(&client, &mcp_url, &initialize_body(requested), None).await;
        let got = negotiated_version(resp).await;
        assert_eq!(
            got, requested,
            "version supportée '{requested}' demandée doit être renvoyée à l'identique (négociation)"
        );
    }

    // C2 — version inconnue/future : repli sur LATEST (2025-11-25), observable via WARN serveur.
    let resp = post_mcp(&client, &mcp_url, &initialize_body("2099-01-01"), None).await;
    let got = negotiated_version(resp).await;
    assert_eq!(
        got, "2025-11-25",
        "version inconnue doit produire un repli sur la dernière version servie (LATEST), spec MCP"
    );
}
