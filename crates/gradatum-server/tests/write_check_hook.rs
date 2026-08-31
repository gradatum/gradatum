//! Tests F-36 v0.7.3 — câblage drift detection non-bloquant dans vault_write_impl.
//!
//! Vérifie le comportement warn-only absolu :
//! - Écriture incohérente (catégorie vs section) → **202 Accepted** + métrique +1
//! - Écriture cohérente → **202 Accepted** + métrique inchangée (0)
//!
//! Pattern : tower::oneshot (pas de spawn réseau) — permet d'accéder directement
//! à `state.metrics` pour vérifier l'incrémentation du compteur.
//!
//! Trust middleware : stub Bearer→sub (pas de validation JWT) — identique au pattern
//! de `vault_write_note_id_overwrite.rs` (trust_stub local à chaque module).

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Router, body::Body, http::Request, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::api_v1;
use gradatum_server::metrics::DriftRuleLabel;
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use prometheus_client::encoding::text::encode;
use tempfile::TempDir;
use tower::util::ServiceExt as _;

/// ACL write-capable pour les tests F-36.
const WRITE_ACL: &str = r#"
[[consumer]]
identity = "main-agent"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Trust middleware stub ────────────────────────────────────────────────────

/// Extrait la valeur brute du Bearer header comme `sub` (pas de validation JWT).
///
/// Pattern identique à `vault_write_note_id_overwrite.rs` : les tests passent
/// directement l'identité agent comme valeur Bearer (ex. `"main-agent"`).
async fn trust_stub(
    mut req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    let trust = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .map(|t| TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: t.into(),
            scopes: vec!["read".into(), "write".into()],
            tenant_id: "main".into(),
            jti: None,
        })
        .unwrap_or(TrustContext::Unauthenticated);
    req.extensions_mut().insert(trust);
    next.run(req).await
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Construit un environnement de test avec vault + job_store SQLite en mémoire.
///
/// Retourne `(Router, AppState, TempDir)` — le `TempDir` DOIT être conservé en vie
/// (sinon le vault disque est supprimé et les lectures échouent).
///
/// Pattern identique à `vault_write_note_id_overwrite.rs::spawn()` mais sans
/// spawn réseau : le `Router` est utilisé via `tower::ServiceExt::oneshot`,
/// permettant d'accéder directement à `state.metrics` après chaque requête.
async fn build_write_env() -> (Router, AppState, TempDir) {
    let tmp = TempDir::new().expect("TempDir write_check_hook");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test fixture"),
    );

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("sqlite::memory: pool — invariant test fixture");
    run_migrations(&jobs_pool)
        .await
        .expect("run_migrations — invariant test fixture");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(WRITE_ACL).expect("preset ACL write_check_hook valide");

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_vault_arc(vault as Arc<dyn Registry>);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state.clone());

    (app, state, tmp)
}

/// Effectue POST `/api/v1/vault_write` via tower oneshot.
///
/// `sub` : identité agent passée directement comme Bearer (ex. `"main-agent"`).
/// Retourne `StatusCode`.
async fn post_vault_write(app: Router, sub: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .uri("/api/v1/vault_write")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {sub}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("sérialisation body — invariant"),
        ))
        .expect("construction requête — invariant");

    app.oneshot(req)
        .await
        .expect("vault_write oneshot")
        .status()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// F-36 warn-only absolu : écriture incohérente ([COUNCIL] dans section reference)
/// → 202 Accepted (write non-bloqué) + métrique `gradatum_write_check_total{rule}` = 1.
#[tokio::test]
async fn vault_write_drift_warns_but_succeeds() {
    let (app, state, _tmp) = build_write_env().await;

    let status = post_vault_write(
        app,
        "main-agent",
        serde_json::json!({
            "title": "[COUNCIL][x] T — d",
            "body": "corps test drift warn-only",
            "section_hint": "reference",  // incohérent : COUNCIL attendu dans "council"
            "tenant_id": "main",
            "tags": []
        }),
    )
    .await;

    // WARN-ONLY ABSOLU : le write doit réussir même si drift détectée.
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "write incohérent doit retourner 202"
    );

    // Vérifier que la métrique a été incrémentée via encodage Prometheus.
    let mut buf = String::new();
    encode(&mut buf, state.metrics.registry.as_ref()).unwrap();
    assert!(
        buf.contains("gradatum_write_check_total"),
        "gradatum_write_check_total doit apparaître dans l'encodage après drift"
    );
    assert!(
        buf.contains("rule=\"category_section_coherence\""),
        "label rule=\"category_section_coherence\" doit apparaître"
    );

    // Vérifier la valeur = 1 via get_or_create (Family partagé via Arc interne).
    let count = state
        .metrics
        .write_check
        .get_or_create(&DriftRuleLabel {
            rule: "category_section_coherence",
        })
        .get();
    assert_eq!(count, 1, "métrique doit valoir 1 après 1 drift détectée");
}

/// F-36 warn-only absolu : écriture cohérente ([COUNCIL] dans section council)
/// → 202 Accepted + métrique inchangée (0 — jamais incrémentée).
#[tokio::test]
async fn vault_write_coherent_no_drift() {
    let (app, state, _tmp) = build_write_env().await;

    let status = post_vault_write(
        app,
        "main-agent",
        serde_json::json!({
            "title": "[COUNCIL][x] T — d",
            "body": "corps test cohérent",
            "section_hint": "council",  // cohérent : COUNCIL attendu dans "council"
            "tenant_id": "main",
            "tags": []
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "write cohérent doit retourner 202"
    );

    // Métrique ne doit PAS être incrémentée pour une écriture cohérente.
    let count = state
        .metrics
        .write_check
        .get_or_create(&DriftRuleLabel {
            rule: "category_section_coherence",
        })
        .get();
    assert_eq!(
        count, 0,
        "métrique doit valoir 0 pour une écriture cohérente"
    );
}
