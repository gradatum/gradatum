//! Preuves salience per-vault à flag GLOBAL ON (L6, post-mortem — F-124).
//!
//! LOCALES au harnais (aucune activation LIVE) : câblent `with_salience(Some(..))` (salience
//! globale ON) + une map per-vault résolue par la VRAIE voie de prod
//! ([`ServerConfig::resolve_salience_per_vault`]).
//!
//! - **C1 (footgun)** `per_vault_disable_neutralizes_salience` : un override
//!   `[per_vault.main.salience] enabled=false` DOIT neutraliser la salience pour `main`
//!   (`salience_factor` ABSENT). AVANT le fix l'override désactivé était droppé ⇒ retombée sur
//!   le global ACTIF ⇒ salience appliquée quand même. Rouge pré-fix, vert post-fix.
//! - **C2(a)** `per_vault_active_override_changes_composite` : un override ACTIF (params ≠
//!   global) modifie effectivement le composite vs le global seul.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::config::{PerVaultOverride, SalienceConfig, ServerConfig};
use gradatum_server::note_usage_store::NoteUsageStore;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "search-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

const FIXED_NOTE_ULID: &str = "01J000000000000000000000AA";

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-salience-on"
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

/// Construit l'état depuis une `ServerConfig` : salience globale = `cfg.salience.resolve()`,
/// map per-vault = `cfg.resolve_salience_per_vault()`. Usage `read` de la note figée seedé
/// (ws>0). Écrit SANS nommer le type de valeur de la map ⇒ compile à l'identique pré/post-fix.
async fn build_app(cfg: ServerConfig) -> (axum::Router, AppState, Arc<SqliteIndex>, TempDir) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory"),
    );

    // `NoteUsageStore::open_in_memory` est `#[cfg(test)]`-gaté (garde-fou anti-prod) donc
    // invisible depuis une crate de test d'intégration. On réutilise le constructeur public
    // `open_or_create` sur un fichier tempdir — pattern des autres tests d'intégration
    // (`note_usage_salience.rs`). Le `TempDir` est remonté à l'appelant pour rester vivant
    // toute la durée du test (sinon suppression du fichier avant la requête de recherche).
    let tmp = TempDir::new().expect("TempDir");
    let usage = NoteUsageStore::open_or_create(&tmp.path().join("note_usage.db"))
        .await
        .expect("NoteUsageStore::open_or_create");
    let mut batch: HashMap<
        gradatum_server::note_usage_store::UsageKey,
        gradatum_server::note_usage_store::UsageValue,
    > = HashMap::new();
    batch.insert(
        (
            "main".to_string(),
            FIXED_NOTE_ULID.to_string(),
            "read".to_string(),
        ),
        (20u64, chrono::Utc::now().timestamp_millis()),
    );
    usage.flush_batch(batch).await.expect("seed usage read x20");

    let global = cfg.salience.resolve();
    let per_vault = cfg.resolve_salience_per_vault();

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        .with_scoring(gradatum_search::TrustDecayConfig {
            enabled: false,
            half_life_days: HashMap::new(),
        })
        .with_salience(global)
        .with_salience_per_vault(per_vault);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.note_usage = Some(usage);

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx, tmp)
}

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "search-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

async fn seed_fixed_fresh(idx: &Arc<SqliteIndex>) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_note_with_created(
        FIXED_NOTE_ULID,
        "reference",
        "content gradatum salience per vault query token alpha",
        now_ms,
    )
    .await
    .expect("seed note figee");
}

fn search_req(token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": "salience per vault query token",
        "limit": 10,
        "tenant_id": "main",
        "include_scores": true,
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn r6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Récupère `(composite, salience_factor_present)` du PREMIER hit.
async fn first_hit(app: axum::Router, token: &str) -> (f64, bool) {
    let resp = app.oneshot(search_req(token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items").clone();
    assert!(!items.is_empty(), "au moins un hit attendu. body={json}");
    let scores = items[0]
        .get("scores")
        .unwrap_or_else(|| panic!("`scores` attendu. hit={}", items[0]));
    (
        r6(scores["composite"].as_f64().expect("composite")),
        scores.get("salience_factor").is_some(),
    )
}

/// C1 (footgun) : override `[per_vault.main.salience] enabled=false` ⇒ salience NEUTRALISÉE
/// pour `main` (champ absent). Rouge sur le code pré-fix (retombée sur le global actif), vert
/// après le fix (map porte `None` pour `main` ⇒ `effective_salience == None`).
#[tokio::test]
async fn per_vault_disable_neutralizes_salience() {
    let cfg = ServerConfig {
        salience: SalienceConfig {
            enabled: true,
            ..SalienceConfig::default()
        },
        per_vault: [(
            "main".to_string(),
            PerVaultOverride {
                salience: Some(SalienceConfig {
                    enabled: false,
                    ..SalienceConfig::default()
                }),
                review_promote: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let (app, state, idx, _tmp) = build_app(cfg).await;
    let token = sign(&state);
    seed_fixed_fresh(&idx).await;

    let (_composite, salience_present) = first_hit(app, &token).await;
    assert!(
        !salience_present,
        "C1 : override main enabled=false => salience neutralisee (salience_factor ABSENT). \
         AVANT fix : override droppe => retombee sur le global actif => champ present (footgun)."
    );
}

/// C2(a) : un override salience per-vault ACTIF (params ≠ global) modifie le composite vs le
/// global seul. Même corpus, même usage, même comp_base ⇒ l'écart provient de la salience.
#[tokio::test]
async fn per_vault_active_override_changes_composite() {
    // (i) Global seul (aucun override main) => salience = params globaux.
    let cfg_global = ServerConfig {
        salience: SalienceConfig {
            enabled: true,
            gamma: 0.10,
            k_norm: 10.0,
            ..SalienceConfig::default()
        },
        ..Default::default()
    };
    let (app_g, state_g, idx_g, _tmp_g) = build_app(cfg_global).await;
    let tok_g = sign(&state_g);
    seed_fixed_fresh(&idx_g).await;
    let (comp_global, present_g) = first_hit(app_g, &tok_g).await;
    assert!(
        present_g,
        "global ON + usage seede => salience appliquee (champ present)"
    );

    // (ii) Override ACTIF pour main, params tres differents => composite different.
    let cfg_override = ServerConfig {
        salience: SalienceConfig {
            enabled: true,
            gamma: 0.10,
            k_norm: 10.0,
            ..SalienceConfig::default()
        },
        per_vault: [(
            "main".to_string(),
            PerVaultOverride {
                salience: Some(SalienceConfig {
                    enabled: true,
                    gamma: 0.90,
                    k_norm: 2.0,
                    ..SalienceConfig::default()
                }),
                review_promote: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let (app_o, state_o, idx_o, _tmp_o) = build_app(cfg_override).await;
    let tok_o = sign(&state_o);
    seed_fixed_fresh(&idx_o).await;
    let (comp_override, present_o) = first_hit(app_o, &tok_o).await;
    assert!(
        present_o,
        "override actif => salience appliquee (champ present)"
    );

    assert!(
        (comp_global - comp_override).abs() > 1e-9,
        "C2(a) : override salience per-vault ACTIF doit modifier le composite \
         (global={comp_global}, override={comp_override})"
    );
}
