//! Tests Fix B — garde overwrite vault_write (409 sans sha + enqueue valide avec sha).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_server::state::AppState;
use gradatum_vault::{Registry, Vault};
use reqwest::StatusCode;
use tempfile::TempDir;
use ulid::Ulid;
// Imports corrigés (chemins réels, alignés sur minimal_fm de
// crates/gradatum-vault/src/write.rs:120).
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

const SUB: &str = "test-note-id-writer";
const ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-note-id-writer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// Frontmatter minimal — modèle copié de minimal_fm() (vault/src/write.rs:120).
fn seed_fm() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Spawn serveur avec un vault réel ; seede une note (note_id, body) si `seed` = Some.
///
/// Retourne aussi le `Arc<Vault>` typé : permet de fabriquer des notes **fantômes**
/// (entrée d'index sans `.md`) via `vault.index().seed_note_with_fts(...)` pour les
/// tests de la garde hybride phantom-write.
async fn spawn(seed: Option<(&str, &str)>) -> (SocketAddr, Arc<Vault>) {
    use axum::{Router, middleware};
    use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
    use gradatum_queue::SqliteQueue;
    use gradatum_server::api_v1;
    use sqlx::sqlite::SqlitePoolOptions;

    let tmp = TempDir::new().unwrap();
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .unwrap(),
    );
    if let Some((nid, body)) = seed {
        let id = NoteId(Ulid::from_string(nid).unwrap());
        vault
            .write_note_with_id(seed_fm(), body.to_string(), id)
            .await
            .unwrap();
    }
    std::mem::forget(tmp); // garder le TempDir vivant pour la durée du serveur

    let queue = Arc::new(
        SqliteQueue::new(&std::env::temp_dir().join(format!("fixb-q-{}.db", Ulid::new())))
            .await
            .unwrap(),
    );
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations(&jobs_pool).await.unwrap();
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(ACL_PRESET).unwrap();
    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(queue as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_vault_arc(vault.clone() as Arc<dyn Registry>);

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
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

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, vault)
}

#[tokio::test]
async fn overwrite_existing_without_sha_returns_409() {
    let nid = "01KTYB000000000000000000DD";
    let (addr, _vault) = spawn(Some((nid, "corps original"))).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "modifié", "section_hint": "decisions",
            "tenant_id": "main", "note_id": nid
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn note_id_with_malformed_sha_returns_400() {
    // C1 P0 — un expected_sha256 syntaxiquement invalide ne doit PAS fail-open.
    let (addr, _vault) = spawn(None).await; // pas besoin de seed : le 400 précède le lookup
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "x", "section_hint": "decisions", "tenant_id": "main",
            "note_id": "01KTYB000000000000000000FF", "expected_sha256": "not-a-valid-sha"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn overwrite_existing_with_sha_is_accepted_and_preserves_note_id() {
    let nid = "01KTYB000000000000000000EE";
    let (addr, _vault) = spawn(Some((nid, "corps original"))).await;
    // La garde serveur ne vérifie QUE la présence d'expected_sha256.
    // La validation du hash réel est faite par le worker (write_if_match) — hors scope serveur.
    let sha = "a".repeat(64);
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "modifié", "section_hint": "decisions",
            "tenant_id": "main", "note_id": nid, "expected_sha256": sha
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let b: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        b["note_id"].as_str().unwrap(),
        nid,
        "ULID préservé dans la réponse"
    );
}

// ── Garde hybride phantom-write (P1) — décision opérateur self-heal hybride ──────
//
// Note fantôme = entrée d'index présente MAIS `.md` absent du disque. `seed_note_with_fts`
// insère dans l'index sans écrire de `.md` → fabrique exactement ce cas.

/// Cas 1 — fantôme + expected_sha256 = Some → 409 Conflict.
///
/// L'`expected_sha256` ne peut être confronté à aucun contenu (`.md` absent) : la garde
/// serveur refuse AVANT d'enqueuer, plutôt que de laisser le worker traiter le fantôme
/// comme une note neuve et bypasser silencieusement l'optimistic-lock.
#[tokio::test]
async fn overwrite_phantom_with_sha_returns_409() {
    let nid = Ulid::new().to_string();
    let (addr, vault) = spawn(None).await;
    // Fantôme : index présent, `.md` absent.
    vault
        .index()
        .seed_note_with_fts(&nid, "decisions", "# phantom\ncorps")
        .await
        .unwrap();
    let sha = "a".repeat(64);
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "modifié", "section_hint": "decisions",
            "tenant_id": "main", "note_id": nid, "expected_sha256": sha
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "fantôme + expected_sha256 = Some doit ressortir 409 (sha invérifiable)"
    );
}

/// Cas 2 — fantôme + expected_sha256 = None → self-heal autorisé (202 Accepted).
///
/// La garde ne doit PAS bloquer : le job est enqueué, le worker ressuscitera le `.md`
/// (auto-réparation). Au niveau serveur, on fige le 202 (≠ 409).
#[tokio::test]
async fn overwrite_phantom_without_sha_is_accepted() {
    let nid = Ulid::new().to_string();
    let (addr, vault) = spawn(None).await;
    vault
        .index()
        .seed_note_with_fts(&nid, "decisions", "# phantom\ncorps")
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "ressuscite", "section_hint": "decisions",
            "tenant_id": "main", "note_id": nid
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "fantôme + expected_sha256 = None doit passer (self-heal), jamais 409"
    );
}

/// Cas 3 — note réellement neuve (jamais indexée) + expected_sha256 = Some → 202
/// (comportement inchangé : aucune entrée d'index, donc pas un fantôme).
#[tokio::test]
async fn new_note_with_sha_is_accepted() {
    let nid = Ulid::new().to_string(); // jamais seedé → absent de l'index
    let (addr, _vault) = spawn(None).await;
    let sha = "a".repeat(64);
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(SUB)
        .json(&serde_json::json!({
            "title": "o", "body": "neuf", "section_hint": "decisions",
            "tenant_id": "main", "note_id": nid, "expected_sha256": sha
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "note neuve (non indexée) + sha doit passer (inchangé)"
    );
}
