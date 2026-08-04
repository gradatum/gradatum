//! Test skew C4-1e Slice B3 (MIGRATE) — `InternalClient::get_trust` passe `vault_id`.
//!
//! Vérifie, sur le chemin HTTP réel (`InternalPersistClient`), les propriétés
//! de la migration :
//!   * MIGRATE nominal — le client neuf émet `?vault_id=<v>` ; un serveur neuf
//!     route le trust par vault → preuve que le param est transmis ET honoré.
//!   * SKEW inverse — le même client neuf contre un serveur d'ORIGINE qui IGNORE
//!     le param répond identiquement, sans 500 : passer `vault_id` ne casse pas
//!     un serveur qui ne le lit pas (query params inconnus = ignorés côté HTTP).
//!   * OFF — `get_trust("main", ulid)` == comportement mono-vault pré-B3
//!     (le client envoie `?vault_id=main`, le serveur applique son défaut `main`).
//!
//! Le serveur de test renvoie une `String` JSON brute : `reqwest::Response::json`
//! désérialise sans exiger de Content-Type, ce qui évite la feature `json` d'axum
//! (le worker déclare `axum { features = [] }`).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::routing::get;
use gradatum_worker::internal_client::{InternalClient, InternalPersistClient};

/// Capture partagée du dernier `vault_id` observé par le serveur de test.
type VaultCapture = Arc<Mutex<Option<String>>>;

/// ULID valide arbitraire (le serveur de test ne le résout pas réellement).
const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Extrait `vault_id` de la query string brute (`a=1&vault_id=x&b=2`).
fn parse_vault_id(raw: Option<String>) -> Option<String> {
    raw?.split('&')
        .find_map(|pair| pair.strip_prefix("vault_id=").map(str::to_string))
}

/// Serveur NEUF : lit `?vault_id=` et route le trust par vault (preuve transmission).
async fn trust_neuf(
    State(cap): State<VaultCapture>,
    Path(_ulid): Path<String>,
    RawQuery(query): RawQuery,
) -> String {
    let vault = parse_vault_id(query);
    *cap.lock().expect("VaultCapture mutex empoisonné") = vault.clone();
    let trust: f32 = match vault.as_deref() {
        Some("vault-b") => 0.90,
        _ => 0.50, // défaut serveur (inclut "main")
    };
    format!("{{\"trust\":{trust}}}")
}

/// Serveur d'ORIGINE : ignore tout query param (comportement pré-B2).
async fn trust_origine(Path(_ulid): Path<String>) -> String {
    "{\"trust\":0.42}".to_string()
}

/// Bind éphémère `127.0.0.1:0`, sert `app` en tâche détachée, renvoie la base URL.
async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind éphémère 127.0.0.1:0");
    let addr = listener
        .local_addr()
        .expect("local_addr du listener de test");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            panic!("serveur de test axum interrompu : {e}");
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn migrate_nominal_transmet_vault_id_et_route_le_trust() {
    let cap: VaultCapture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/internal/v1/note/{ulid}/trust", get(trust_neuf))
        .with_state(cap.clone());
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let t = client
        .get_trust("vault-b", ULID)
        .await
        .expect("get_trust(vault-b) doit réussir");

    assert!(
        (t - 0.90).abs() < 1e-6,
        "trust routé par vault-b attendu 0.90, obtenu {t}"
    );
    assert_eq!(
        cap.lock().expect("lecture capture").as_deref(),
        Some("vault-b"),
        "le serveur neuf doit avoir reçu ?vault_id=vault-b"
    );
}

#[tokio::test]
async fn skew_inverse_serveur_origine_ignore_le_param_sans_500() {
    // Client NEUF (envoie ?vault_id=main) contre serveur d'ORIGINE (ignore le param).
    // La réponse doit être identique et surtout ne pas produire de 500.
    let app = Router::new().route("/internal/v1/note/{ulid}/trust", get(trust_origine));
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let t = client
        .get_trust("main", ULID)
        .await
        .expect("get_trust ne doit pas 500 face à un serveur qui ignore vault_id");

    assert!(
        (t - 0.42).abs() < 1e-6,
        "réponse du serveur d'origine attendue 0.42, obtenu {t}"
    );
}

#[tokio::test]
async fn off_main_est_byte_identical() {
    // OFF (mono-vault) : get_trust("main", ulid) → défaut serveur "main".
    let cap: VaultCapture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/internal/v1/note/{ulid}/trust", get(trust_neuf))
        .with_state(cap.clone());
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let t = client
        .get_trust("main", ULID)
        .await
        .expect("get_trust(main) doit réussir");

    assert!(
        (t - 0.50).abs() < 1e-6,
        "trust défaut main attendu 0.50, obtenu {t}"
    );
    assert_eq!(
        cap.lock().expect("lecture capture").as_deref(),
        Some("main"),
        "OFF : le client émet explicitement ?vault_id=main"
    );
}
