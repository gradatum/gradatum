//! E2E — immuabilité de l'identité `[[feature:F-XX]]` d'une carte `project-map`.
//!
//! **Vrai chemin, aucun mock** : HTTP → `auth_middleware` réel → handler → `vault_write_impl`
//! / `create_feature_card_impl`, avec un **vrai `Vault`** (TempDir) — la carte existante est
//! réellement écrite sur disque puis relue par la garde d'immuabilité via `read_note_by_id` —,
//! un **vrai `SqliteQueueStore`** (file LIVE `gradatum_jobs`) pour l'enqueue.
//!
//! Le piège évité : un harnais sans vault réel lirait `NoteNotFound` sur toute cible, ce qui
//! ferait passer une mise à jour préservant le rôle pour une création (identité vide) et la
//! rejetterait à tort — un faux rouge branché sur un placeholder. Ici la carte existe vraiment.
//!
//! Contrat vérifié (durcissement HEAD d554d39f) :
//! 1. création externe portant un rôle `feature` → **refus 400** ;
//! 2. création via `create_feature_card` (allocation serveur) → **passe 202** (le serveur ne
//!    se bloque pas lui-même) ;
//! 3. mise à jour **conservant** le rôle → **passe 202** ;
//! 4. mise à jour **changeant** le rôle → **refus 400** ;
//! 5. mise à jour **omettant** le rôle (carte redevenue changelog, schéma valide) → **refus 400** ;
//! 6. autre section (`decisions`) portant un rôle `feature` → **non concernée**, passe 202.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::TokenScope;
use gradatum_core::QueueStore;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_vault::{Registry, Vault};
use reqwest::StatusCode;
use tempfile::TempDir;

const TESTER_SUB: &str = "feature-identity-tester";

/// Preset ACL : read+write sur `main/*`.
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "feature-identity-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// SHA-256 hex bien formé (64 chars). La garde d'identité s'exécute AVANT le contrôle de
/// forme du sha et AVANT la garde overwrite ; le serveur ne vérifie pas le sha lui-même
/// (l'optimistic-lock est délégué au worker, hors scope de ce test). Un hex valide suffit
/// donc à franchir la garde overwrite d'une note vivante sur le chemin serveur.
const WELL_FORMED_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Corps de carte-feature complet (6 rôles) pour un identifiant donné.
fn feature_card_body(feature: &str) -> String {
    format!(
        "[[feature:{feature}]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
         [[release:planned]] [[version:gradatum/backlog]]\n\nCorps de carte."
    )
}

/// Démarre un serveur de test complet (vault réel + queue + job_store) et rend
/// `(adresse, vault, token)`. Le `TempDir` est gardé vivant via le `Vault` (Arc) et le
/// binding renvoyé.
async fn start_server() -> (SocketAddr, Arc<Vault>, String, TempDir) {
    use axum::{Router, middleware, routing::get};
    use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
    use gradatum_server::api_v1;
    use gradatum_server::state::AppState;

    let dir = TempDir::new().expect("TempDir project_map_feature_identity_e2e");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let mut state = AppState::new()
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>)
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool);
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            TESTER_SUB,
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test");

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind éphémère");
    let addr = listener.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serveur de test");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, vault, token, dir)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client HTTP")
}

/// Écrit une carte-feature `project-map` RÉELLE sur disque (rôle `[[feature:{feature}]]`) et
/// rend son ULID. Écriture directe par la couche vault (pas de validation de schéma HTTP) —
/// simule une carte préexistante que la garde relira.
async fn seed_feature_card(vault: &Vault, feature: &str) -> String {
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::ProjectMap,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: chrono::Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let note = vault
        .write_note(fm, feature_card_body(feature))
        .await
        .expect("write_note — seed carte-feature");
    note.id.to_string()
}

async fn post_vault_write(
    addr: SocketAddr,
    token: &str,
    payload: &serde_json::Value,
) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .expect("requête vault_write")
}

// ── 1. Création externe portant un rôle feature → refus 400 ────────────────────

#[tokio::test]
async fn external_create_with_client_feature_role_is_rejected() {
    let (addr, _vault, token, _dir) = start_server().await;

    // note_id absent (création), section project-map, corps portant [[feature:F-99]].
    let resp = post_vault_write(
        addr,
        &token,
        &serde_json::json!({
            "title": "Carte triche",
            "body": feature_card_body("F-99"),
            "section_hint": "project-map",
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "une création externe portant un rôle [[feature:…]] doit être refusée (identité serveur only)"
    );
}

// ── 2. Création via create_feature_card (allocation serveur) → passe 202 ───────
//     Le serveur ne se bloque pas lui-même (ServerAllocated exempté).

#[tokio::test]
async fn server_allocated_create_feature_card_passes() {
    let (addr, _vault, token, _dir) = start_server().await;

    // Corps SANS rôle feature : le serveur alloue et injecte.
    let body = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                [[release:planned]] [[version:gradatum/backlog]]\n\nCorps.";
    let resp = client()
        .post(format!("http://{addr}/api/v1/project-map/create-feature"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "title": "Carte serveur", "body": body }))
        .send()
        .await
        .expect("requête create-feature");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "create_feature_card (ServerAllocated) ne doit PAS être bloqué par la garde d'immuabilité"
    );
    let json: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        json["feature"], "F-01",
        "première allocation vault vide = F-01"
    );
}

// ── 3. Mise à jour conservant le rôle → passe 202 ──────────────────────────────

#[tokio::test]
async fn external_update_preserving_feature_role_passes() {
    let (addr, vault, token, _dir) = start_server().await;
    let note_id = seed_feature_card(&vault, "F-07").await;

    // Même rôle [[feature:F-07]], corps modifié ailleurs (statut).
    let updated = "[[feature:F-07]] [[project:gradatum]] [[status:IN_PROGRESS]] [[kind:FEATURE]] \
                   [[release:planned]] [[version:gradatum/backlog]]\n\nCorps mis à jour.";
    let resp = post_vault_write(
        addr,
        &token,
        &serde_json::json!({
            "title": "MAJ statut",
            "body": updated,
            "section_hint": "project-map",
            "note_id": note_id,
            "expected_sha256": WELL_FORMED_SHA,
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "une mise à jour conservant [[feature:F-07]] doit passer (les 5 verbes gov-todo RMW)"
    );
}

// ── 4. Mise à jour changeant le rôle → refus 400 ───────────────────────────────

#[tokio::test]
async fn external_update_changing_feature_role_is_rejected() {
    let (addr, vault, token, _dir) = start_server().await;
    let note_id = seed_feature_card(&vault, "F-07").await;

    // Rôle muté F-07 → F-08 : carte-feature valide au schéma, mais identité changée.
    let mutated = feature_card_body("F-08");
    let resp = post_vault_write(
        addr,
        &token,
        &serde_json::json!({
            "title": "Renommage interdit",
            "body": mutated,
            "section_hint": "project-map",
            "note_id": note_id,
            "expected_sha256": WELL_FORMED_SHA,
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "changer [[feature:F-07]] en [[feature:F-08]] doit être refusé (identité immuable)"
    );
}

// ── 5. Mise à jour omettant le rôle → refus 400 ────────────────────────────────
//     Corps redevenu carte changelog VALIDE au schéma (0 feature, 0 release) : c'est
//     bien la garde d'IDENTITÉ qui rejette, pas la cardinalité de schéma.

#[tokio::test]
async fn external_update_dropping_feature_role_is_rejected() {
    let (addr, vault, token, _dir) = start_server().await;
    let note_id = seed_feature_card(&vault, "F-07").await;

    // Carte changelog valide (sans feature, sans release) → passe le validateur de schéma…
    let changelog = "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] \
                     [[version:gradatum/backlog]]\n\nDevenu changelog.";
    let resp = post_vault_write(
        addr,
        &token,
        &serde_json::json!({
            "title": "Perte identité",
            "body": changelog,
            "section_hint": "project-map",
            "note_id": note_id,
            "expected_sha256": WELL_FORMED_SHA,
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "…mais omettre [[feature:F-07]] sur une carte qui le portait doit être refusé (identité)"
    );
}

// ── 6. Autre section portant un rôle feature → non concernée, passe 202 ────────

#[tokio::test]
async fn feature_role_in_other_section_is_untouched() {
    let (addr, _vault, token, _dir) = start_server().await;

    // section decisions : ni validateur project-map, ni garde d'identité.
    let resp = post_vault_write(
        addr,
        &token,
        &serde_json::json!({
            "title": "[TODO][gradatum] Note portant un wikilink feature",
            "body": "Texte libre citant [[feature:F-42]] comme référence.\n\nCorps.",
            "section_hint": "decisions",
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "un rôle [[feature:…]] hors project-map ne doit PAS être concerné par la garde"
    );
}
