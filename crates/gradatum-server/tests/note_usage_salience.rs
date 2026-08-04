//! Tests d'intégration E2E — F-110 : salience 4ᵉ facteur de scoring.
//!
//! Couvre les invariants du design :
//! 1. Flag OFF (défaut) : la présence de compteurs `note_usage` ne change NI l'ordre
//!    NI la sérialisation — aucun champ `salience_*` émis (byte-identical).
//! 2. Flag ON : un usage asymétrique fait remonter la note sur-utilisée dans le top-K.
//! 3. Flag ON sans store `note_usage` : facteur neutre, aucune erreur, ordre inchangé.
//!
//! Fixtures FTS calquées sur `tests/vault_search_anchor_recency.rs` : embedder Noop
//! (chemin FTS pur, déterministe), notes seedées via `seed_note_with_created` pour
//! contrôler le `recency_factor` (donc l'ordre de base indépendant de la salience).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::scope::VaultId;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_server::config::SalienceConfig;
use gradatum_server::note_usage_store::{KIND_READ, NoteUsageStore};
use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "salience-tester"
read_patterns  = ["main/*", "decisions/*", "main/decisions"]
write_patterns = []
"#;

// IDs Crockford base32 valides (26 chars, pas I/L/O/U).
const ID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV"; // note fraîche → base-first
const ID_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ"; // note plus ancienne + sur-utilisée

const QUERY_TOKEN: &str = "zzsaliencetoken";
const SECTION: &str = "decisions";

/// Embedder Noop (dim 8) — force le chemin FTS pur (semantic branch désactivée).
struct NoopBackend;

#[async_trait::async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-salience"
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

/// Construit un environnement de test : deux notes FTS (A fraîche, B ancienne + usage),
/// salience `enabled`, store `note_usage` câblé ssi `with_store`.
///
/// Retourne `(router, token, tmp, vault)` — `tmp`/`vault` conservés vivants par l'appelant.
async fn setup(
    enabled: bool,
    with_store: bool,
    b_read_count: u64,
) -> (axum::Router, String, TempDir, Arc<Vault>) {
    use axum::{Router, middleware};

    let tmp = TempDir::new().expect("TempDir");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index = vault.index().clone();

    // Notes identiques côté FTS ; seul le recency diffère (A fraîche, B -20 j) → ordre de
    // base déterministe [A, B], indépendant de la salience.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let body = format!("{QUERY_TOKEN} corpus salience test");
    index
        .seed_note_with_created(ID_A, SECTION, &body, now_ms)
        .await
        .expect("seed A");
    index
        .seed_note_with_created(ID_B, SECTION, &body, now_ms - 20 * 86_400_000i64)
        .await
        .expect("seed B");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        .with_salience(
            SalienceConfig {
                enabled,
                ..Default::default()
            }
            .resolve(),
        );
    state.search = Arc::clone(&index) as Arc<dyn Index>;

    if with_store {
        let store = NoteUsageStore::open_or_create(&tmp.path().join("note_usage.db"))
            .await
            .expect("NoteUsageStore::open_or_create");
        let mut batch: HashMap<(String, String, String), (u64, i64)> = HashMap::new();
        batch.insert(
            ("main".to_string(), ID_B.to_string(), KIND_READ.to_string()),
            (b_read_count, now_ms),
        );
        store.flush_batch(batch).await.expect("flush B usage");
        state.note_usage = Some(store);
    }

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    let token = state
        .jwt
        .sign(
            "salience-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT");

    (app, token, tmp, vault)
}

/// `POST /api/v1/vault_search` avec `include_scores=true`. Retourne le body JSON brut (string)
/// + la valeur désérialisée.
async fn search(app: axum::Router, token: &str) -> (String, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "query": QUERY_TOKEN,
                "tenant_id": "main",
                "limit": 10,
                "include_scores": true
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_search doit répondre 200"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8(bytes.to_vec()).expect("body utf8");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (raw, json)
}

/// Séquence ordonnée des ULID présents dans `items[].path`.
fn order(json: &serde_json::Value) -> Vec<String> {
    json["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|it| it["path"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn position_of(order: &[String], id: &str) -> Option<usize> {
    order.iter().position(|p| p.contains(id))
}

// Test 1 — flag OFF (défaut) : la présence de compteurs note_usage ne change NI l'ordre
// NI la sérialisation. On compare deux réponses OFF (avec store peuplé vs sans store) :
// mêmes ULID dans le même ordre, et aucun champ `salience_*` émis (byte-identical).
#[tokio::test]
async fn search_response_byte_identical_when_salience_disabled() {
    let (app_with, tok_with, _t1, _v1) = setup(false, true, 1000).await;
    let (app_without, tok_without, _t2, _v2) = setup(false, false, 0).await;

    let (raw_with, json_with) = search(app_with, &tok_with).await;
    let (raw_without, json_without) = search(app_without, &tok_without).await;

    // Ordre identique malgré B sur-utilisé (flag OFF ⇒ usage ignoré).
    assert_eq!(
        order(&json_with),
        order(&json_without),
        "OFF : l'usage note_usage ne doit pas réordonner les résultats"
    );

    // Aucun champ salience émis (skip_serializing_if sur None) dans les deux réponses.
    assert!(
        !raw_with.contains("salience_"),
        "OFF : aucun champ salience_* ne doit être sérialisé (avec store)"
    );
    assert!(
        !raw_without.contains("salience_"),
        "OFF : aucun champ salience_* ne doit être sérialisé (sans store)"
    );
}

// Test 2 — flag ON : usage asymétrique ⇒ la note B (sur-utilisée) remonte devant A.
// A est base-first (plus fraîche) ; B (base-second) capte un boost salience qui inverse
// l'ordre. include_scores expose salience_factor > 0 pour B, == 0.0 pour A.
#[tokio::test]
async fn salience_reorders_hits_when_enabled() {
    let (app, token, _tmp, _vault) = setup(true, true, 1000).await;
    let (_raw, json) = search(app, &token).await;

    let ord = order(&json);
    let pos_a = position_of(&ord, ID_A).expect("A présent");
    let pos_b = position_of(&ord, ID_B).expect("B présent");
    assert!(
        pos_b < pos_a,
        "ON : B (sur-utilisée) doit remonter devant A. ordre={ord:?}"
    );

    let items = json["items"].as_array().unwrap();
    let sf = |id: &str| -> f64 {
        items
            .iter()
            .find(|it| it["path"].as_str().unwrap_or_default().contains(id))
            .and_then(|it| it["scores"]["salience_factor"].as_f64())
            .unwrap_or_else(|| panic!("salience_factor absent pour {id}"))
    };
    assert!(sf(ID_B) > 0.0, "ON : salience_factor(B) doit être > 0");
    assert_eq!(
        sf(ID_A),
        0.0,
        "ON : salience_factor(A) doit être 0.0 (aucun usage)"
    );
}

// Test 3 — flag ON mais store note_usage absent : facteur neutre partout, aucune erreur,
// ordre inchangé (A base-first), salience_factor présent mais 0.0 pour tous.
#[tokio::test]
async fn salience_enabled_without_store_is_neutral_and_never_fails() {
    let (app, token, _tmp, _vault) = setup(true, false, 0).await;
    let (_raw, json) = search(app, &token).await;

    let ord = order(&json);
    let pos_a = position_of(&ord, ID_A).expect("A présent");
    let pos_b = position_of(&ord, ID_B).expect("B présent");
    assert!(
        pos_a < pos_b,
        "ON sans store : ordre de base inchangé (A devant B). ordre={ord:?}"
    );

    let items = json["items"].as_array().unwrap();
    for id in [ID_A, ID_B] {
        let sf = items
            .iter()
            .find(|it| it["path"].as_str().unwrap_or_default().contains(id))
            .and_then(|it| it["scores"]["salience_factor"].as_f64())
            .unwrap_or_else(|| panic!("salience_factor absent pour {id}"));
        assert_eq!(
            sf, 0.0,
            "ON sans store : salience_factor doit être 0.0 pour {id}"
        );
    }
}
