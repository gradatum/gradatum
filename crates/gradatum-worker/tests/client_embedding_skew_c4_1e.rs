//! Test skew C4-1e Slice E (MIGRATE) — `InternalClient::get_note_embedding` passe `vault_id`.
//!
//! Vérifie, sur le chemin HTTP réel (`InternalPersistClient`), les propriétés
//! de la migration :
//!   * MIGRATE nominal — le client neuf émet `?embedder_id=<e>&vault_id=<v>` ; un serveur
//!     neuf route le vecteur par vault → preuve que le param est transmis ET honoré.
//!   * SKEW inverse — le même client neuf contre un serveur d'ORIGINE qui IGNORE le param
//!     répond identiquement, sans 500 : passer `vault_id` ne casse pas un serveur qui ne le
//!     lit pas (query params inconnus = ignorés côté HTTP).
//!   * OFF — `get_note_embedding("main", ulid, e)` == comportement mono-vault pré-Slice E
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
const EMBEDDER: &str = "noop-internal";

/// Extrait `vault_id` de la query string brute (`embedder_id=e&vault_id=x`).
fn parse_vault_id(raw: Option<String>) -> Option<String> {
    raw?.split('&')
        .find_map(|pair| pair.strip_prefix("vault_id=").map(str::to_string))
}

/// Rend un `EmbeddingReadDto` JSON dont le premier élément du vecteur = `head`.
fn dto_json(head: f32) -> String {
    format!(
        "{{\"note_id\":\"{ULID}\",\"embedder_id\":\"{EMBEDDER}\",\"dim\":4,\"vector\":[{head},0.0,0.0,0.0]}}"
    )
}

/// Serveur NEUF : lit `?vault_id=` et route le vecteur par vault (preuve transmission).
async fn emb_neuf(
    State(cap): State<VaultCapture>,
    Path(_ulid): Path<String>,
    RawQuery(query): RawQuery,
) -> String {
    let vault = parse_vault_id(query);
    *cap.lock().expect("VaultCapture mutex empoisonné") = vault.clone();
    let head: f32 = match vault.as_deref() {
        Some("vault-b") => 0.50,
        _ => 0.10, // défaut serveur (inclut "main")
    };
    dto_json(head)
}

/// Serveur d'ORIGINE : ignore tout query param (comportement pré-Slice E).
async fn emb_origine(Path(_ulid): Path<String>) -> String {
    dto_json(0.42)
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
async fn migrate_nominal_transmet_vault_id_et_route_le_vecteur() {
    let cap: VaultCapture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/internal/v1/note/{ulid}/embedding", get(emb_neuf))
        .with_state(cap.clone());
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let dto = client
        .get_note_embedding("vault-b", ULID, EMBEDDER)
        .await
        .expect("get_note_embedding(vault-b) doit réussir");

    assert!(
        (dto.vector[0] - 0.50).abs() < 1e-6,
        "vecteur routé par vault-b attendu head 0.50, obtenu {:?}",
        dto.vector
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
    let app = Router::new().route("/internal/v1/note/{ulid}/embedding", get(emb_origine));
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let dto = client
        .get_note_embedding("main", ULID, EMBEDDER)
        .await
        .expect("get_note_embedding ne doit pas 500 face à un serveur qui ignore vault_id");

    assert!(
        (dto.vector[0] - 0.42).abs() < 1e-6,
        "réponse du serveur d'origine attendue head 0.42, obtenu {:?}",
        dto.vector
    );
}

#[tokio::test]
async fn off_main_est_byte_identical() {
    // OFF (mono-vault) : get_note_embedding("main", ulid, e) → défaut serveur "main".
    let cap: VaultCapture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/internal/v1/note/{ulid}/embedding", get(emb_neuf))
        .with_state(cap.clone());
    let base = spawn_server(app).await;

    let client = InternalPersistClient::new(base, "test-token").expect("client HTTP");
    let dto = client
        .get_note_embedding("main", ULID, EMBEDDER)
        .await
        .expect("get_note_embedding(main) doit réussir");

    assert!(
        (dto.vector[0] - 0.10).abs() < 1e-6,
        "vecteur défaut main attendu head 0.10, obtenu {:?}",
        dto.vector
    );
    assert_eq!(
        cap.lock().expect("lecture capture").as_deref(),
        Some("main"),
        "OFF : le client émet explicitement ?vault_id=main"
    );
}
