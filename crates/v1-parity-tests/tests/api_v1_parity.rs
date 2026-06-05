//! Tests de parité de forme (shape parity) — API v1 gradatum-server vs DTOs.
//!
//! # Phase 2.0a — Option α post-arbitrage the maintainer 2026-05-05
//!
//! Le but de Phase 2.0a n'est PAS de reproduire iso le contenu du legacy vault v1.6.2,
//! mais de vérifier que :
//! - les 10 endpoints sont joignables avec un bearer JWT valide,
//! - chaque réponse est conforme aux DTOs déclarés (champs présents, types corrects),
//! - l'authentification fonctionne de bout en bout (JwtService Ed25519 réel).
//!
//! # Phase 2.1 — Full content parity
//!
//! La comparaison de contenu (diff JSON nul via [`diff_json_strip_tenant`]) est différée
//! à Phase 2.1 avec `migrate-from-v0` (import legacy vault v1.6.2 → gradatum-storage).
//!
//! # Preset ACL de test
//!
//! Le consumer `"test-bearer"` dispose de `read_patterns = ["**"]` — il peut lire
//! tout locus (y compris `"main/main"` utilisé par les handlers GET sans body).
//! En production, le preset est plus restrictif (preset TOML chargé depuis config).
//!
//! Arbitrage T12 Option α : design spec P2.0 — 2026-05-04.

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_server::middleware::auth_middleware;
use gradatum_server::{api_v1, state::AppState};
use serde_json::Value;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL TOML qui autorise `"test-bearer"` à lire tous les loci.
///
/// En production, le preset est chargé depuis config (caveat C10).
/// Ici le consumer matche le `sub` JWT signé par la `JwtService` de test.
const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-bearer"
read_patterns = ["**"]
write_patterns = []
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Démarre une instance gradatum-server de test sur un port OS-assigné.
///
/// Retourne l'adresse de bind et un bearer JWT valide pour `"test-bearer"`
/// signé par la même `JwtService` que celle injectée dans `AppState`.
///
/// Le serveur tourne dans une tâche tokio détachée — arrêté en fin de processus.
async fn spawn_test_server() -> (SocketAddr, String) {
    use axum::{middleware, Router};

    // Clé éphémère locale au test — pas de fichier PEM requis.
    let jwt = JwtService::new_ephemeral();
    // Signer un bearer valide AVANT de déplacer jwt dans AppState.
    let bearer = jwt
        .sign(
            "test-bearer",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signer un bearer de test — clé éphémère valide");

    // Construire l'AppState avec le preset ACL de test.
    let acl = AclEngine::from_preset_str(TEST_ACL_PRESET)
        .expect("preset ACL de test toujours valide — invariant statique");
    let state = AppState::with_jwt_and_acl(jwt, acl);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test arrêté proprement");
    });
    // Laisser le serveur démarrer (liaison async → accept loop actif).
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, bearer)
}

/// Client reqwest sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

/// Compare deux réponses JSON en ignorant les champs propres à gradatum
/// absents du legacy vault v1.6.2.
///
/// # Usage Phase 2.1
///
/// Cette fonction sera activée en Phase 2.1 avec `migrate-from-v0` pour la
/// parité contenu stricte (diff JSON nul sur les 10 méthodes).
///
/// # Champs ignorés
///
/// - `tenant_id` (gradatum-only)
/// - `created_at_ms` / `updated_at_ms` (précision timestamp différente)
/// - `_gradatum_*` (méta-champs internes)
///
/// Retourne les chemins JSON (notation pointée) où les valeurs diffèrent.
/// Une liste vide = parité stricte sur les champs comparés.
#[allow(dead_code)]
fn diff_json_strip_tenant(_a: &Value, _b: &Value) -> Vec<String> {
    // Implémentation différée à Phase 2.1 (migrate-from-v0 requis).
    // Le scaffold compile et valide la signature pour usage futur.
    vec![]
}

// ── Constantes scaffold Phase 2.1 (conservées #[allow(dead_code)]) ────────────

/// Port d'écoute de l'instance legacy vault v1.6.2 de test (Phase 2.1).
#[allow(dead_code)]
const LEGACY_VAULT_TEST_PORT: u16 = 18462;

/// Port d'écoute de l'instance gradatum-server de test (Phase 2.1).
#[allow(dead_code)]
const GRADATUM_TEST_PORT: u16 = 19190;

/// Chemin absolu vers le snapshot DB legacy vault v1.6.2 (Phase 2.1).
#[allow(dead_code)]
const SNAPSHOT_DB_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/legacy-vault-snapshot.db"
);

// ── Tests shape parity — 10 méthodes read ─────────────────────────────────────
//
// Chaque test :
// 1. Démarre gradatum-server avec JwtService éphémère + ACL test-bearer/**
// 2. POST/GET avec bearer JWT valide
// 3. Asserte le code HTTP attendu (200 ou 404 pour vault_read stub)
// 4. Parse le JSON et vérifie la présence/type des champs DTO

/// Shape parity : vault_search — `{ items: [...] }` avec tableau présent.
///
/// Stub T8 retourne `items: []`. Shape : champ `items` présent, type array.
#[tokio::test]
async fn shape_vault_search() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_search"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "query": "test", "tenant_id": "main" }))
        .send()
        .await
        .expect("requête vault_search avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_search doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_search : body JSON parseable");
    assert!(
        body.get("items").is_some(),
        "vault_search : champ 'items' absent — DTO VaultSearchResponse"
    );
    assert!(
        body["items"].is_array(),
        "vault_search : 'items' doit être un array"
    );
}

/// Shape parity : vault_read — stub T8 retourne 404.
///
/// vault_read retourne 404 tant que `gradatum-vault` n'est pas câblé (T12 content).
/// Phase 2.0a documente ce comportement attendu.
#[tokio::test]
async fn shape_vault_read() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_read"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "path": "decisions/test-note",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_read avec bearer valide");

    // Stub T8 : toujours 404 (aucun vault réel câblé).
    // Documenté dans handlers.rs : "Stub T8 — toujours 404 (aucun vault réel câblé). T12 câblera state.vault."
    assert_eq!(
        resp.status(),
        404,
        "vault_read stub T8 doit retourner 404 (pas de vault câblé)"
    );
}

/// Shape parity : vault_list — `{ entries: [], next_cursor: null, total: 0 }`.
///
/// Shape : `entries` (array), `total` (number), `next_cursor` (null ou string).
#[tokio::test]
async fn shape_vault_list() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_list"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "tenant_id": "main" }))
        .send()
        .await
        .expect("requête vault_list avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_list doit retourner 200");

    let body: Value = resp.json().await.expect("vault_list : body JSON parseable");
    assert!(
        body.get("entries").is_some(),
        "vault_list : champ 'entries' absent — DTO VaultListResponse"
    );
    assert!(
        body["entries"].is_array(),
        "vault_list : 'entries' doit être un array"
    );
    assert!(
        body.get("total").is_some(),
        "vault_list : champ 'total' absent — DTO VaultListResponse"
    );
    assert!(
        body["total"].is_number(),
        "vault_list : 'total' doit être un number"
    );
}

/// Shape parity : vault_status — GET, `{ tenant_id, note_count, total_size_bytes, index_version, health, ... }`.
///
/// Shape : tous les champs obligatoires du DTO VaultStatusResponse présents.
#[tokio::test]
async fn shape_vault_status() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .get(format!("http://{addr}/api/v1/vault_status"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("requête vault_status avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_status doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_status : body JSON parseable");
    for field in &[
        "tenant_id",
        "note_count",
        "total_size_bytes",
        "index_version",
        "health",
    ] {
        assert!(
            body.get(*field).is_some(),
            "vault_status : champ '{field}' absent — DTO VaultStatusResponse"
        );
    }
    assert!(
        body["note_count"].is_number(),
        "vault_status : 'note_count' doit être un number"
    );
    assert!(
        body["health"].is_string(),
        "vault_status : 'health' doit être une string"
    );
}

/// Shape parity : vault_graph — `{ nodes: [], edges: [] }`.
///
/// Shape : `nodes` (array de strings), `edges` (array d'objets).
#[tokio::test]
async fn shape_vault_graph() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_graph"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "root": "decisions/test",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_graph avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_graph doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_graph : body JSON parseable");
    assert!(
        body.get("nodes").is_some(),
        "vault_graph : champ 'nodes' absent — DTO VaultGraphResponse"
    );
    assert!(
        body["nodes"].is_array(),
        "vault_graph : 'nodes' doit être un array"
    );
    assert!(
        body.get("edges").is_some(),
        "vault_graph : champ 'edges' absent — DTO VaultGraphResponse"
    );
    assert!(
        body["edges"].is_array(),
        "vault_graph : 'edges' doit être un array"
    );
}

/// Shape parity : vault_links — alias thin vault_graph depth=1, même DTO `{ nodes: [], edges: [] }`.
#[tokio::test]
async fn shape_vault_links() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_links"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "path": "decisions/test",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_links avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_links doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_links : body JSON parseable");
    assert!(
        body.get("nodes").is_some(),
        "vault_links : champ 'nodes' absent — DTO VaultLinksResponse (alias VaultGraphResponse)"
    );
    assert!(
        body["nodes"].is_array(),
        "vault_links : 'nodes' doit être un array"
    );
    assert!(
        body.get("edges").is_some(),
        "vault_links : champ 'edges' absent — DTO VaultLinksResponse"
    );
    assert!(
        body["edges"].is_array(),
        "vault_links : 'edges' doit être un array"
    );
}

/// Shape parity : vault_trace — `{ entries: [] }`.
///
/// Shape : `entries` (array d'objets TraceEntry).
#[tokio::test]
async fn shape_vault_trace() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_trace"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "query": "architecture rust",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_trace avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_trace doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_trace : body JSON parseable");
    assert!(
        body.get("entries").is_some(),
        "vault_trace : champ 'entries' absent — DTO VaultTraceResponse"
    );
    assert!(
        body["entries"].is_array(),
        "vault_trace : 'entries' doit être un array"
    );
}

/// Shape parity : vault_context — `{ context: "", estimated_tokens: 0, sources: [] }`.
///
/// Shape : `context` (string), `estimated_tokens` (number), `sources` (array).
#[tokio::test]
async fn shape_vault_context() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_context"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "query": "architecture rust async",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_context avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_context doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_context : body JSON parseable");
    assert!(
        body.get("context").is_some(),
        "vault_context : champ 'context' absent — DTO VaultContextResponse"
    );
    assert!(
        body["context"].is_string(),
        "vault_context : 'context' doit être une string"
    );
    assert!(
        body.get("estimated_tokens").is_some(),
        "vault_context : champ 'estimated_tokens' absent — DTO VaultContextResponse"
    );
    assert!(
        body["estimated_tokens"].is_number(),
        "vault_context : 'estimated_tokens' doit être un number"
    );
    assert!(
        body.get("sources").is_some(),
        "vault_context : champ 'sources' absent — DTO VaultContextResponse"
    );
    assert!(
        body["sources"].is_array(),
        "vault_context : 'sources' doit être un array"
    );
}

/// Shape parity : vault_authors — GET, `{ authors: [] }`.
///
/// Shape : `authors` (array d'objets AuthorEntry).
#[tokio::test]
async fn shape_vault_authors() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .get(format!("http://{addr}/api/v1/vault_authors"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("requête vault_authors avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_authors doit retourner 200");

    let body: Value = resp
        .json()
        .await
        .expect("vault_authors : body JSON parseable");
    assert!(
        body.get("authors").is_some(),
        "vault_authors : champ 'authors' absent — DTO VaultAuthorsResponse"
    );
    assert!(
        body["authors"].is_array(),
        "vault_authors : 'authors' doit être un array"
    );
}

/// Shape parity : vault_tags — GET, `{ tags: [] }`.
///
/// Shape : `tags` (array d'objets TagEntry).
#[tokio::test]
async fn shape_vault_tags() {
    let (addr, bearer) = spawn_test_server().await;
    let resp = client()
        .get(format!("http://{addr}/api/v1/vault_tags"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("requête vault_tags avec bearer valide");

    assert_eq!(resp.status(), 200, "vault_tags doit retourner 200");

    let body: Value = resp.json().await.expect("vault_tags : body JSON parseable");
    assert!(
        body.get("tags").is_some(),
        "vault_tags : champ 'tags' absent — DTO VaultTagsResponse"
    );
    assert!(
        body["tags"].is_array(),
        "vault_tags : 'tags' doit être un array"
    );
}

// ── Test smoke — 10 méthodes joignables ──────────────────────────────────────

/// Smoke test : toutes les 10 méthodes répondent avec un body JSON parseable.
///
/// Un seul serveur, une seule passe — vérifie que chaque méthode est joignable
/// avec un bearer valide et retourne un body JSON (même si content vide ou 404).
///
/// Les assertions de shape fine sont dans les tests individuels ci-dessus.
#[tokio::test]
async fn smoke_all_10_methods_reachable() {
    let (addr, bearer) = spawn_test_server().await;
    let c = client();

    // vault_search
    let r = c
        .post(format!("http://{addr}/api/v1/vault_search"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "query": "smoke" }))
        .send()
        .await
        .expect("smoke vault_search");
    assert_eq!(r.status(), 200, "smoke : vault_search doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_search : body JSON");

    // vault_read (404 attendu — stub T8)
    let r = c
        .post(format!("http://{addr}/api/v1/vault_read"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "path": "smoke/note" }))
        .send()
        .await
        .expect("smoke vault_read");
    assert_eq!(r.status(), 404, "smoke : vault_read stub T8 → 404");
    // 404 Axum retourne un body vide — ne pas tenter de parser JSON

    // vault_list
    let r = c
        .post(format!("http://{addr}/api/v1/vault_list"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("smoke vault_list");
    assert_eq!(r.status(), 200, "smoke : vault_list doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_list : body JSON");

    // vault_status
    let r = c
        .get(format!("http://{addr}/api/v1/vault_status"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("smoke vault_status");
    assert_eq!(r.status(), 200, "smoke : vault_status doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_status : body JSON");

    // vault_graph
    let r = c
        .post(format!("http://{addr}/api/v1/vault_graph"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "root": "smoke/root" }))
        .send()
        .await
        .expect("smoke vault_graph");
    assert_eq!(r.status(), 200, "smoke : vault_graph doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_graph : body JSON");

    // vault_links
    let r = c
        .post(format!("http://{addr}/api/v1/vault_links"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "path": "smoke/note" }))
        .send()
        .await
        .expect("smoke vault_links");
    assert_eq!(r.status(), 200, "smoke : vault_links doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_links : body JSON");

    // vault_trace
    let r = c
        .post(format!("http://{addr}/api/v1/vault_trace"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "query": "smoke" }))
        .send()
        .await
        .expect("smoke vault_trace");
    assert_eq!(r.status(), 200, "smoke : vault_trace doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_trace : body JSON");

    // vault_context
    let r = c
        .post(format!("http://{addr}/api/v1/vault_context"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({ "query": "smoke" }))
        .send()
        .await
        .expect("smoke vault_context");
    assert_eq!(r.status(), 200, "smoke : vault_context doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_context : body JSON");

    // vault_authors
    let r = c
        .get(format!("http://{addr}/api/v1/vault_authors"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("smoke vault_authors");
    assert_eq!(r.status(), 200, "smoke : vault_authors doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_authors : body JSON");

    // vault_tags
    let r = c
        .get(format!("http://{addr}/api/v1/vault_tags"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("smoke vault_tags");
    assert_eq!(r.status(), 200, "smoke : vault_tags doit être joignable");
    let _: Value = r.json().await.expect("smoke vault_tags : body JSON");
}
