//! Preuves byte-identical L6 — le raffinement salience per-vault (overrides A6) est câblé dans
//! le read hot-path (`api_v1::logic::vault_search_impl`) UNIQUEMENT à l'intérieur du bras
//! `state.salience.is_some()`. À flag salience OFF (`state.salience == None`, défaut de
//! production) le `ScoreBreakdown` doit rester inchangé.
//!
//! Couvre :
//! - **Preuve A** (`score_breakdown_at_salience_off_snapshot`) : snapshot `insta` du
//!   `ScoreBreakdown` retourné par `vault_search` (`include_scores = true`) sur un corpus + une
//!   requête FIGÉS, à salience OFF. Les champs `salience_weighted_sum` / `salience_factor` sont
//!   ABSENTS (skip_serializing_if None), et le composite provient du chemin sans salience. Ce
//!   snapshot est le garde-fou de régression : toute altération future du chemin OFF le casse.
//! - **Preuve C** (`per_vault_map_not_consulted_at_salience_off`) : même setup, mais l'état
//!   porte une map `salience_per_vault` EMPOISONNÉE (override pour `main` avec des params
//!   extrêmes). À salience OFF, la map n'est JAMAIS consultée (gate `None` court-circuite) ⇒ les
//!   champs salience restent absents et le composite est identique au chemin sans map.
//!
//! Déterminisme du snapshot : trust-decay désactivé (`with_scoring(enabled=false)`) + note seedée
//! « fraîche » (recency ≈ 1.0) + arrondi 6 décimales. Un seul hit ⇒ aucun tie-break de rang.

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
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "search-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*"]
write_patterns = []
"#;

/// ULID FIGÉ pour un corpus reproductible (le snapshot n'inclut pas l'id, mais la stabilité de
/// l'insertion garantit des rangs déterministes).
const FIXED_NOTE_ULID: &str = "01J000000000000000000000AA";

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-salience-off"
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

/// Construit un état à salience OFF (défaut) + trust-decay OFF (déterminisme), avec une map
/// `salience_per_vault` fournie (vide pour la preuve A, empoisonnée pour la preuve C).
async fn build_app(
    salience_per_vault: std::collections::HashMap<
        String,
        Option<Arc<gradatum_search::SalienceParams>>,
    >,
) -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test salience-off"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        // Trust-decay OFF ⇒ trust_raw/trust_decayed = None, composite = f(rrf, recency, pagerank)
        // — retire la variance temporelle liée à l'âge de la note.
        .with_scoring(gradatum_search::TrustDecayConfig {
            enabled: false,
            half_life_days: std::collections::HashMap::new(),
        })
        // salience GLOBALE laissée à None (défaut OFF) — on NE câble PAS `with_salience`.
        .with_salience_per_vault(salience_per_vault);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx)
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

/// Seed une unique note FRAÎCHE (recency ≈ 1.0) sur un ULID figé.
async fn seed_fixed_fresh(idx: &Arc<SqliteIndex>) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_note_with_created(
        FIXED_NOTE_ULID,
        "reference",
        "content gradatum salience byte identical query token alpha",
        now_ms,
    )
    .await
    .expect("seed note figée");
}

fn search_req(token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": "salience byte identical query token",
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

/// Arrondi 6 décimales — neutralise le bruit f64 sur recency/composite (age ~ms ⇒ recency
/// stable à 1.000000, composite stable à params fixes).
fn r6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Projection déterministe d'un objet `scores` pour snapshot / comparaison.
fn project_scores(scores: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "rrf_score": r6(scores["rrf_score"].as_f64().expect("rrf_score")),
        "recency_factor": r6(scores["recency_factor"].as_f64().expect("recency_factor")),
        "pagerank_factor": r6(scores["pagerank_factor"].as_f64().expect("pagerank_factor")),
        "in_degree": scores["in_degree"].as_u64().expect("in_degree"),
        "composite": r6(scores["composite"].as_f64().expect("composite")),
        // Présence explicite : à OFF, ces deux clés DOIVENT être absentes.
        "salience_weighted_sum_present": scores.get("salience_weighted_sum").is_some(),
        "salience_factor_present": scores.get("salience_factor").is_some(),
    })
}

async fn run_and_project(app: axum::Router, token: &str) -> Vec<serde_json::Value> {
    let resp = app.oneshot(search_req(token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let items = json["items"].as_array().expect("items").clone();
    assert!(!items.is_empty(), "au moins un hit attendu. body={json}");
    items
        .iter()
        .map(|it| {
            let scores = it
                .get("scores")
                .unwrap_or_else(|| panic!("`scores` attendu avec include_scores. hit={it}"));
            project_scores(scores)
        })
        .collect()
}

/// Params salience EXTRÊMES — s'ils étaient consultés à OFF, ils modifieraient massivement le
/// composite et feraient apparaître les champs salience. Ils NE doivent jamais l'être.
fn poison_params() -> Arc<gradatum_search::SalienceParams> {
    Arc::new(gradatum_search::SalienceParams {
        gamma: 9.9,
        k_norm: 1.0,
        kind_weights: [
            ("read".to_string(), 999.0),
            ("search-hit".to_string(), 999.0),
        ]
        .into_iter()
        .collect(),
    })
}

/// Preuve A — snapshot du `ScoreBreakdown` à salience OFF, map per-vault VIDE.
#[tokio::test]
async fn score_breakdown_at_salience_off_snapshot() {
    let (app, state, idx) = build_app(std::collections::HashMap::new()).await;
    let token = sign(&state);
    seed_fixed_fresh(&idx).await;

    let projected = run_and_project(app, &token).await;

    insta::assert_json_snapshot!(projected);
}

/// Preuve C — map per-vault EMPOISONNÉE, salience toujours OFF : la map n'est pas consultée.
///
/// À OFF le gate `state.salience` (None) court-circuite ⇒ `effective_salience == None` ⇒ la map
/// `salience_per_vault` (pourtant garnie d'un override extrême pour `main`) est ignorée. On
/// vérifie : (1) champs salience ABSENTS, (2) le composite est IDENTIQUE à la preuve A (map vide).
#[tokio::test]
async fn per_vault_map_not_consulted_at_salience_off() {
    // Référence : map vide.
    let (app_ref, state_ref, idx_ref) = build_app(std::collections::HashMap::new()).await;
    let token_ref = sign(&state_ref);
    seed_fixed_fresh(&idx_ref).await;
    let projected_ref = run_and_project(app_ref, &token_ref).await;

    // Empoisonné : override extrême pour `main`.
    let mut poison = std::collections::HashMap::new();
    // Override ACTIF (Some) empoisonné : prouve qu'à salience globale OFF, même un override
    // per-vault présent-et-actif n'est pas consulté (gate `state.salience == None`).
    poison.insert("main".to_string(), Some(poison_params()));
    let (app_poison, state_poison, idx_poison) = build_app(poison).await;
    let token_poison = sign(&state_poison);
    seed_fixed_fresh(&idx_poison).await;
    let projected_poison = run_and_project(app_poison, &token_poison).await;

    // Champs salience absents malgré la map garnie (gate None court-circuite).
    for row in &projected_poison {
        assert_eq!(
            row["salience_weighted_sum_present"],
            serde_json::Value::Bool(false),
            "à OFF, salience_weighted_sum reste absent même avec map empoisonnée : {row}"
        );
        assert_eq!(
            row["salience_factor_present"],
            serde_json::Value::Bool(false),
            "à OFF, salience_factor reste absent même avec map empoisonnée : {row}"
        );
    }

    // Composite (et le reste) identiques à la map vide ⇒ map jamais consultée pour le scoring.
    assert_eq!(
        projected_poison, projected_ref,
        "map per-vault empoisonnée == map vide à salience OFF (non consultée)"
    );
}
