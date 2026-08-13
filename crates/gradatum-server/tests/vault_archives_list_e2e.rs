//! Tests E2E F-100 1.6 — `POST /api/v1/vault_archives_list` (listing archives LECTURE SEULE).
//!
//! Cycle complet réaliste : on archive une note via l'endpoint admin interne
//! (`POST /internal/v1/admin/delete`), puis on la RETROUVE via l'endpoint public de
//! listing — prouvant que l'agent/opérateur peut VOIR les archives (pour préparer ses
//! commandes CLI) sans qu'aucune mutation du cycle delete/restore/purge ne soit exposée
//! publiquement ni en MCP.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::Index;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_vault::Vault;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

use gradatum_server::{api_v1, internal, state::AppState};

const ADMIN_TOKEN: &str = "test-admin-token-0123456789abcdef";

const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-admin"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// Environnement : routeur public (listing) + routeur interne (admin delete) partageant l'état.
struct Env {
    public: Router,
    internal: Router,
    vault: Arc<Vault>,
    _tmp: TempDir,
}

async fn build_env() -> Env {
    use gradatum_auth::jwt::JwtService;

    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let idx = vault.index().clone();
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_PRESET).expect("preset ACL");

    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_admin_api_token(secrecy::SecretString::from(ADMIN_TOKEN.to_string()));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let public = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_all))
        .with_state(state.clone());
    let internal = internal::build_internal_router(state);

    Env {
        public,
        internal,
        vault,
        _tmp: tmp,
    }
}

async fn trust_all(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "test-admin".into(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".into(),
        jti: None,
    });
    next.run(req).await
}

fn live_frontmatter(section: Section) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: chrono::Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: Some("test".to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Archive une note réelle via l'endpoint admin interne. Retourne son ULID.
async fn archive_note(env: &Env, section: Section, body: &str) -> String {
    let note = env
        .vault
        .write_note(live_frontmatter(section), body.to_string())
        .await
        .expect("write_note");
    let id = note.id.to_string();
    let req = Request::builder()
        .uri("/internal/v1/admin/delete")
        .method("POST")
        .header("content-type", "application/json")
        .header("X-Gradatum-Admin", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "note_id": id, "dry_run": false, "confirm_ulids": [id]
            }))
            .unwrap(),
        ))
        .unwrap();
    let mut req = req;
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let resp = env.internal.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "archivage admin doit réussir"
    );
    id
}

async fn list_archives(public: Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/vault_archives_list")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = public.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Le cycle : archiver deux notes → le listing public les retrouve (métadonnées + count).
#[tokio::test]
async fn archived_notes_appear_in_public_listing() {
    let env = build_env().await;
    let id1 = archive_note(&env, Section::Feedback, "# a\nnote une").await;
    let id2 = archive_note(&env, Section::LessonsLearned, "# b\nnote deux").await;

    let (status, json) = list_archives(env.public.clone(), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "listing 200: {json}");

    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(json["count"], 2, "deux archives actives: {json}");
    let ids: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["note_id"].as_str())
        .collect();
    assert!(ids.contains(&id1.as_str()) && ids.contains(&id2.as_str()));

    // Les métadonnées de récupération sont exposées (pour préparer la CLI).
    let e1 = entries
        .iter()
        .find(|e| e["note_id"] == id1)
        .expect("entrée id1");
    assert_eq!(e1["section"], "feedback");
    assert_eq!(e1["vault_id"], "main", "dimension vault exposée: {e1}");
    assert_eq!(e1["archived_by"], "operator-admin");
    assert!(
        e1["archive_path"]
            .as_str()
            .unwrap_or("")
            .starts_with(".archive/main/"),
        "archive_path exposé: {e1}"
    );
    // Une archive active omet gc_at/restored_at.
    assert!(e1.get("gc_at").is_none());
    assert!(e1.get("restored_at").is_none());
}

/// Le filtre `section` restreint le listing.
#[tokio::test]
async fn listing_section_filter() {
    let env = build_env().await;
    archive_note(&env, Section::Feedback, "# a\nfeedback").await;
    let lesson_id = archive_note(&env, Section::LessonsLearned, "# b\nlesson").await;

    let (status, json) = list_archives(
        env.public.clone(),
        serde_json::json!({ "section": "lessons-learned" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["count"], 1,
        "une seule archive lessons-learned: {json}"
    );
    assert_eq!(json["entries"][0]["note_id"], lesson_id);
}

/// Pagination : `limit` borne le nombre d'entrées retournées et se reflète dans la réponse.
#[tokio::test]
async fn listing_pagination_limit() {
    let env = build_env().await;
    for i in 0..3 {
        archive_note(&env, Section::Feedback, &format!("# n{i}\ncorps {i}")).await;
    }

    let (status, json) = list_archives(env.public.clone(), serde_json::json!({ "limit": 2 })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["limit"], 2, "limit reflété: {json}");
    assert_eq!(json["count"], 2, "deux entrées ramenées: {json}");
    assert_eq!(json["entries"].as_array().unwrap().len(), 2);
}

// ── Endpoints admin internes archives (list + purge) ────────────────────────────

/// POST vers le routeur interne admin (loopback + token admin).
async fn admin_post(
    internal: Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .uri(path)
        .method("POST")
        .header("content-type", "application/json")
        .header("X-Gradatum-Admin", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let resp = internal.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// L'endpoint interne admin archives/list retrouve les archives (parité avec le public).
#[tokio::test]
async fn admin_internal_archives_list() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nnote").await;

    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["count"], 1, "{json}");
    assert_eq!(json["entries"][0]["note_id"], id);
}

/// Cycle purge : dry-run (aucune destruction) → réel (destruction + gc_at) → l'archive
/// quitte la liste active et apparaît en include_gc.
#[tokio::test]
async fn admin_internal_archives_purge_cycle() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nà purger").await;

    // Dry-run : montre l'archive cible, ne détruit rien.
    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/purge",
        serde_json::json!({ "note_id": id, "dry_run": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["purged"], false);
    assert_eq!(json["archive"]["note_id"], id, "preview archive: {json}");

    // L'archive est toujours active.
    let (_, active) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(active["count"], 1, "archive active avant purge réelle");

    // Réel : détruit + marque gc_at.
    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/purge",
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["purged"], true, "purge réelle: {json}");

    // La liste active est vide ; include_gc retrouve l'archive détruite avec gc_at.
    let (_, active) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(active["count"], 0, "plus d'archive active après purge");

    let (_, with_gc) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({ "include_gc": true }),
    )
    .await;
    assert_eq!(
        with_gc["count"], 1,
        "archive détruite visible en include_gc"
    );
    assert!(
        with_gc["entries"][0]["gc_at"].is_i64(),
        "gc_at posé: {with_gc}"
    );
}

/// Purge réelle avec confirm_ulids ≠ [note_id] → 400.
#[tokio::test]
async fn admin_internal_archives_purge_confirm_mismatch_400() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nnote").await;

    let (status, _json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/purge",
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "confirm vide → 400");
}

/// Purge d'une note sans archive active → no-op idempotent (purged=false).
#[tokio::test]
async fn admin_internal_archives_purge_absent_is_noop() {
    let env = build_env().await;
    let unknown = ulid::Ulid::generate().to_string();

    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/purge",
        serde_json::json!({ "note_id": unknown, "dry_run": false, "confirm_ulids": [unknown] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["purged"], false, "aucune archive → no-op: {json}");
    assert!(json["archive"].is_null(), "pas d'archive: {json}");
}

// ── Endpoint admin interne archives/restore (quarantaine) ───────────────────────

/// Cycle restore : dry-run (aucune mutation) → réel (note ré-indexée `pending-review`) →
/// l'archive quitte la liste active et apparaît en include_restored avec `restored_at`.
#[tokio::test]
async fn admin_internal_archives_restore_cycle() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nà restaurer").await;

    // Dry-run : montre l'archive cible, ne restaure rien.
    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/restore",
        serde_json::json!({ "note_id": id, "dry_run": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["restored"], false);
    assert_eq!(json["archive"]["note_id"], id, "preview archive: {json}");

    // L'archive est toujours active.
    let (_, active) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(active["count"], 1, "archive active avant restore réel");

    // Réel : restaure en quarantaine (pending-review).
    let (status, json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/restore",
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["restored"], true, "restore réel: {json}");
    assert_eq!(json["status"], "pending-review", "quarantaine: {json}");
    assert!(
        json["restored_path"]
            .as_str()
            .unwrap_or("")
            .ends_with(&format!("{id}.md")),
        "chemin restauré: {json}"
    );

    // La note est de retour dans le vault en statut PendingReview (preuve de ré-ingestion).
    let nid = NoteId(ulid::Ulid::from_string(&id).expect("ULID valide"));
    let note = env
        .vault
        .read_note(nid)
        .await
        .expect("note restaurée lisible");
    assert_eq!(
        note.frontmatter.status,
        NoteStatus::PendingReview,
        "note restaurée en quarantaine"
    );

    // L'archive quitte la liste active ; include_restored la retrouve avec restored_at.
    let (_, active) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(active["count"], 0, "plus d'archive active après restore");

    let (_, restored) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/list",
        serde_json::json!({ "include_restored": true }),
    )
    .await;
    assert_eq!(
        restored["count"], 1,
        "archive restaurée visible: {restored}"
    );
    assert!(
        restored["entries"][0]["restored_at"].is_i64(),
        "restored_at posé: {restored}"
    );
}

/// Restore réel avec `confirm_ulids` ≠ [note_id] → 400 (protection côté serveur).
#[tokio::test]
async fn admin_internal_archives_restore_confirm_mismatch_400() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nnote").await;

    let (status, _json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/restore",
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "confirm vide → 400");
}

/// Restore d'une note sans archive active → 404.
#[tokio::test]
async fn admin_internal_archives_restore_absent_404() {
    let env = build_env().await;
    let unknown = ulid::Ulid::generate().to_string();

    let (status, _json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/restore",
        serde_json::json!({ "note_id": unknown, "dry_run": false, "confirm_ulids": [unknown] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "aucune archive active → 404");
}

/// Restore alors qu'une note LIVE porte déjà cet ULID → 409 (anti-écrasement).
#[tokio::test]
async fn admin_internal_archives_restore_conflict_409() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nnote").await;

    // Ré-occuper l'ULID par une note vivante (l'archivage avait dé-indexé l'original).
    let nid = NoteId(ulid::Ulid::from_string(&id).expect("ULID valide"));
    env.vault
        .write_note_with_id(
            live_frontmatter(Section::Feedback),
            "# a\nré-occupée".to_string(),
            nid,
        )
        .await
        .expect("ré-écriture note vivante");

    let (status, _json) = admin_post(
        env.internal.clone(),
        "/internal/v1/admin/archives/restore",
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "ULID occupé → 409");
}

// ── Invariant fondateur F-100 : test structurel de la surface (constraint #3) ────

/// POST vers le routeur PUBLIC, retourne uniquement le statut HTTP.
async fn public_status(public: Router, uri: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    public.oneshot(req).await.unwrap().status()
}

/// Invariant fondateur F-100 (`decisions/01KXAP7Z61`) — surface HTTP.
///
/// Test structurel de la contrainte #3 : les mutations `delete`/`restore`/`purge` sont
/// **absentes du routeur public** (aucune route `/api/v1/...` ni exposition du namespace
/// interne), tandis que le listing **lecture seule** y est monté. Les mutations n'existent
/// QUE dans le namespace interne loopback. Prouve mécaniquement que « la main des agents »
/// (surface publique/gateway) ne peut ni archiver, ni restaurer, ni purger.
#[tokio::test]
async fn mutations_absent_from_public_router_present_internal() {
    let env = build_env().await;
    let id = archive_note(&env, Section::Feedback, "# a\nnote").await;

    // 1. Aucune route publique de mutation (routes /api/v1 dédiées ABSENTES, et le
    //    namespace interne loopback n'est PAS monté sur le routeur public).
    for uri in [
        "/api/v1/vault_delete",
        "/api/v1/vault_archives_restore",
        "/api/v1/vault_archives_purge",
        "/internal/v1/admin/delete",
        "/internal/v1/admin/archives/restore",
        "/internal/v1/admin/archives/purge",
    ] {
        let st = public_status(
            env.public.clone(),
            uri,
            serde_json::json!({ "note_id": id }),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "{uri} ne doit PAS exister sur le routeur public (got {st})"
        );
    }

    // 2. Le listing lecture seule EST monté publiquement (les agents VOIENT les archives).
    let (st, _) = list_archives(env.public.clone(), serde_json::json!({})).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "vault_archives_list doit rester public (lecture seule)"
    );

    // 3. Les mutations EXISTENT dans le namespace interne (dry-run → 200 = route présente).
    for path in [
        "/internal/v1/admin/archives/restore",
        "/internal/v1/admin/archives/purge",
    ] {
        let (st, _) = admin_post(
            env.internal.clone(),
            path,
            serde_json::json!({ "note_id": id, "dry_run": true }),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::OK,
            "{path} doit exister dans le namespace interne (dry-run)"
        );
    }
}
