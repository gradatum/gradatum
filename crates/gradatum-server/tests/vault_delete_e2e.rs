//! Tests E2E F-100 1.6 — `POST /internal/v1/admin/delete` (delete on-demand = archivage).
//!
//! Le delete on-demand n'est plus une route publique ni un outil MCP (bascule surface
//! F-100 1.6, arbitrage Tech Lead Option A) : sa seule porte d'entrée est l'endpoint
//! admin interne (namespace loopback + token admin dédié), appelé par la CLI opérateur.
//! Ces tests pilotent le **vrai** routeur interne (`build_internal_router`) — donc à la
//! fois l'orchestration (dry-run/confirm/protégé/tombstone/archivage) ET l'auth admin.
//!
//! # Cas de test
//!
//! - Auth : token absent / invalide / adresse non-loopback → 401.
//! - `dry_run_returns_preview_with_backlinks` — dry-run 200 + backlinks entrants.
//! - `dry_run_absent_note_is_noop` — note inexistante → 200 exists=false.
//! - `protected_sections_refused_403` — les 6 sections PROTECTED_DELETE → 403 (même admin).
//! - `confirm_mismatch_returns_400` / `confirm_multiple_ulids_returns_400` — borne mono-note.
//! - `real_delete_archives_note_and_records_registry` — archivage + registre + `archived_path`.
//! - `idempotent_delete_absent_note` — mode réel note absente → 200 deleted=false.
//! - `real_delete_emits_durable_tombstone` — tombstone durable (deleted_by=operator-admin) [P1-2].
//! - `tombstone_failure_aborts_delete` — échec sink → 500, note préservée [P1-2 crash-safety].
//! - `real_delete_absent_note_with_malformed_confirm_returns_400` — confirm avant idempotence [P2-1].

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::index::Index;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

use gradatum_server::{internal, state::AppState};

/// Token admin de test (≥ 32 caractères, cohérent avec la longueur publique-par-design).
const ADMIN_TOKEN: &str = "test-admin-token-0123456789abcdef";

/// Adresse loopback synthétique injectée dans les extensions (ConnectInfo).
fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40000))
}

/// Sink d'audit capturant en mémoire — assert du tombstone durable (P1-2).
#[derive(Clone, Default)]
struct CapturingSink {
    events: Arc<Mutex<Vec<HttpAuditEvent>>>,
}

#[async_trait]
impl AuditSink for CapturingSink {
    async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        self.events.lock().expect("lock CapturingSink").push(event);
        Ok(())
    }
}

/// Sink d'audit qui échoue toujours — prouve que l'échec du tombstone abandonne le delete.
struct FailingSink;

#[async_trait]
impl AuditSink for FailingSink {
    async fn record(&self, _event: HttpAuditEvent) -> Result<(), std::io::Error> {
        Err(std::io::Error::other("sink audit indisponible (test)"))
    }
}

/// Environnement de test : routeur interne (admin) + vrai Vault (tempdir) partageant l'index.
struct DeleteEnv {
    app: Router,
    vault: Arc<Vault>,
    idx: Arc<SqliteIndex>,
    audit_events: Arc<Mutex<Vec<HttpAuditEvent>>>,
    _tmp: TempDir,
}

async fn build_app() -> DeleteEnv {
    let sink = CapturingSink::default();
    let audit_events = Arc::clone(&sink.events);
    build_app_with_audit(Arc::new(sink), audit_events).await
}

/// Variante avec un sink d'audit explicite (capturant ou échouant).
async fn build_app_with_audit(
    sink: Arc<dyn AuditSink>,
    audit_events: Arc<Mutex<Vec<HttpAuditEvent>>>,
) -> DeleteEnv {
    use gradatum_auth::jwt::JwtService;

    let tmp = TempDir::new().expect("TempDir vault_delete_e2e");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create — vault_delete_e2e"),
    );
    let idx = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    // L'admin bypasse l'ACL par-tenant (pleine autorité) — preset vide suffit.
    let acl = AclEngine::from_preset_str("").expect("preset ACL vault_delete_e2e");

    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_admin_api_token(secrecy::SecretString::from(ADMIN_TOKEN.to_string()));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.audit = sink;

    let app = internal::build_internal_router(state);

    DeleteEnv {
        app,
        vault,
        idx,
        audit_events,
        _tmp: tmp,
    }
}

/// Variante « câblage PROD » (P1-1) : construit l'état via le VRAI `with_audit_dir`
/// (JsonlFileSink sur disque), comme `main.rs` — aucun sink injecté. Retourne aussi le
/// répertoire d'audit pour vérifier la trace durable écrite sur disque.
async fn build_app_with_audit_dir() -> (DeleteEnv, std::path::PathBuf) {
    use gradatum_auth::jwt::JwtService;

    let tmp = TempDir::new().expect("TempDir vault_delete_e2e audit_dir");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create — audit_dir"),
    );
    let idx = vault.index().clone();
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str("").expect("preset ACL audit_dir");
    let audit_dir = tmp.path().join("audit");

    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_admin_api_token(secrecy::SecretString::from(ADMIN_TOKEN.to_string()))
        .with_audit_dir(&audit_dir)
        .await
        .expect("with_audit_dir (câblage prod JsonlFileSink)");
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = internal::build_internal_router(state);
    (
        DeleteEnv {
            app,
            vault,
            idx,
            audit_events: Arc::new(Mutex::new(Vec::new())),
            _tmp: tmp,
        },
        audit_dir,
    )
}

/// Frontmatter minimal `status=live` pour écrire une vraie note via le vault.
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

/// POST admin avec token valide + adresse loopback (chemin nominal).
async fn post_delete(app: Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    post_delete_with(app, body, Some(ADMIN_TOKEN), loopback()).await
}

/// POST admin paramétrable (token optionnel + adresse) — pour les cas d'auth.
async fn post_delete_with(
    app: Router,
    body: serde_json::Value,
    token: Option<&str>,
    addr: SocketAddr,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .uri("/internal/v1/admin/delete")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("X-Gradatum-Admin", format!("Bearer {t}"));
    }
    let mut req = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    // ConnectInfo n'est pas fourni par `oneshot` — on l'injecte manuellement (le
    // middleware admin lit `ConnectInfo<SocketAddr>` pour la garde loopback).
    req.extensions_mut().insert(ConnectInfo(addr));
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ── Auth admin (loopback + token dédié) ─────────────────────────────────────────

/// Token admin absent → 401 (fail-closed).
#[tokio::test]
async fn admin_delete_without_token_is_401() {
    let env = build_app().await;
    let (status, _json) = post_delete_with(
        env.app,
        serde_json::json!({ "note_id": Ulid::new().to_string(), "dry_run": true }),
        None,
        loopback(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "token absent → 401");
}

/// Token admin invalide → 401.
#[tokio::test]
async fn admin_delete_wrong_token_is_401() {
    let env = build_app().await;
    let (status, _json) = post_delete_with(
        env.app,
        serde_json::json!({ "note_id": Ulid::new().to_string(), "dry_run": true }),
        Some("mauvais-token-completement-different"),
        loopback(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "token invalide → 401");
}

/// Adresse source non-loopback → 401 (même avec le bon token).
#[tokio::test]
async fn admin_delete_non_loopback_is_401() {
    let env = build_app().await;
    let (status, _json) = post_delete_with(
        env.app,
        serde_json::json!({ "note_id": Ulid::new().to_string(), "dry_run": true }),
        Some(ADMIN_TOKEN),
        SocketAddr::from(([10, 0, 0, 5], 40000)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "non-loopback → 401");
}

// ── Orchestration ───────────────────────────────────────────────────────────────

/// Dry-run sur une note existante avec un backlink entrant → 200, exists=true,
/// backlinks rapporte la source (aucune mutation).
#[tokio::test]
async fn dry_run_returns_preview_with_backlinks() {
    let env = build_app().await;

    let target = Ulid::new().to_string();
    env.idx
        .seed_note_with_fts(&target, "feedback", "note cible")
        .await
        .expect("seed cible");
    let src = Ulid::new().to_string();
    env.idx
        .seed_note_with_fts(&src, "feedback", "note source")
        .await
        .expect("seed source");
    // src → target (backlink entrant sur target).
    env.idx
        .upsert_link("main", &src, &target)
        .await
        .expect("upsert_link");

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": target, "dry_run": true }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "dry-run 200: {json}");
    assert_eq!(json["exists"], true, "note existe: {json}");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["section"], "feedback");
    let backlinks = json["backlinks"].as_array().expect("backlinks array");
    assert!(
        backlinks.iter().any(|b| b.as_str() == Some(&src)),
        "backlink src {src} doit être rapporté: {json}"
    );

    // Aucune mutation : la note existe toujours dans l'index.
    assert!(
        env.idx.get_note("main", &target).await.unwrap().is_some(),
        "dry-run ne doit pas supprimer la note"
    );
}

/// Dry-run sur une note inexistante → 200, exists=false (no-op idempotent).
#[tokio::test]
async fn dry_run_absent_note_is_noop() {
    let env = build_app().await;
    let unknown = Ulid::new().to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": unknown, "dry_run": true }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "dry-run absent 200: {json}");
    assert_eq!(json["exists"], false, "note absente: {json}");
}

/// Les 6 sections de PROTECTED_DELETE → refus dur 403, même pour l'admin (pleine
/// autorité mais garde de gouvernance inviolable via cette API).
#[tokio::test]
async fn protected_sections_refused_403() {
    let env = build_app().await;

    for section in [
        "agent-issues",
        "council",
        "project-map",
        "identity",
        "decisions",
        "reasoning",
    ] {
        let id = Ulid::new().to_string();
        env.idx
            .seed_note_with_fts(&id, section, "note gouvernance")
            .await
            .unwrap_or_else(|e| panic!("seed {section}: {e}"));

        // Dry-run ET mode réel doivent tous deux refuser en 403.
        let (status_dry, json_dry) = post_delete(
            env.app.clone(),
            serde_json::json!({ "note_id": id, "dry_run": true }),
        )
        .await;
        assert_eq!(
            status_dry,
            StatusCode::FORBIDDEN,
            "section protégée {section} dry-run doit être 403: {json_dry}"
        );

        let (status_real, json_real) = post_delete(
            env.app.clone(),
            serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
        )
        .await;
        assert_eq!(
            status_real,
            StatusCode::FORBIDDEN,
            "section protégée {section} mode réel doit être 403: {json_real}"
        );

        // La note protégée est toujours présente (jamais supprimée).
        assert!(
            env.idx.get_note("main", &id).await.unwrap().is_some(),
            "note {section} protégée ne doit jamais être supprimée"
        );
    }
}

/// Mode réel avec confirm_ulids ≠ [note_id] → 400.
#[tokio::test]
async fn confirm_mismatch_returns_400() {
    let env = build_app().await;
    let id = Ulid::new().to_string();
    env.idx
        .seed_note_with_fts(&id, "feedback", "note à confirmer")
        .await
        .expect("seed");

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({
            "note_id": id,
            "dry_run": false,
            "confirm_ulids": ["01JFAKE00000000000000000XX"]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "confirm mismatch 400: {json}"
    );
}

/// Mode réel avec confirm_ulids > 1 ULID → 400 (borne mono-note).
#[tokio::test]
async fn confirm_multiple_ulids_returns_400() {
    let env = build_app().await;
    let id = Ulid::new().to_string();
    env.idx
        .seed_note_with_fts(&id, "feedback", "note mono")
        .await
        .expect("seed");
    let other = Ulid::new().to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({
            "note_id": id,
            "dry_run": false,
            "confirm_ulids": [id, other]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "confirm > 1 ULID doit être 400 (mono-note): {json}"
    );
}

/// F-100 incrément 1.6 — le delete ARCHIVE : `archived_path` en réponse + entrée active
/// au registre `archive_index`, tout en retirant totalement la note des index.
#[tokio::test]
async fn real_delete_archives_note_and_records_registry() {
    let env = build_app().await;

    let note = env
        .vault
        .write_note(
            live_frontmatter(Section::Feedback),
            "# titre\ncorps à archiver".to_string(),
        )
        .await
        .expect("write_note réelle");
    let id = note.id.to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete réel 200 : {json}");
    assert_eq!(json["deleted"], true, "deleted=true: {json}");
    assert!(json["backup"].is_object(), "backup présent: {json}");

    // La réponse porte le chemin d'archive.
    let archived_path = json["archived_path"]
        .as_str()
        .expect("archived_path présent dans la réponse");
    assert!(
        archived_path.starts_with(".archive/main/"),
        "archived_path sous .archive/main/ : {archived_path}"
    );

    // Le registre porte une archive ACTIVE avec les métadonnées de récupération.
    let entry = env
        .idx
        .get_active_archive("main", &id)
        .await
        .expect("get_active_archive")
        .expect("archive active enregistrée au registre");
    assert_eq!(entry.section, "feedback");
    assert_eq!(entry.archive_path, archived_path);
    assert_eq!(
        entry.archived_by.as_deref(),
        Some("operator-admin"),
        "archived_by = sub de l'identité admin synthétique"
    );
    assert!(entry.gc_due > entry.archived_at, "gc_due dans le futur");
    assert!(entry.gc_at.is_none() && entry.restored_at.is_none());

    // La note est totalement absente des index (cascade inchangée).
    assert!(
        env.idx.get_note("main", &id).await.unwrap().is_none(),
        "note absente de l'index après archivage"
    );
    // Le `.md` d'origine a disparu (déplacé sous .archive/).
    assert!(
        env.vault.read_note_by_id(&id).await.is_err(),
        "le `.md` d'origine doit avoir été déplacé (archivé)"
    );
}

/// Mode réel sur une note inexistante → 200, deleted=false (idempotent, backup absent).
#[tokio::test]
async fn idempotent_delete_absent_note() {
    let env = build_app().await;
    let unknown = Ulid::new().to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({
            "note_id": unknown,
            "dry_run": false,
            "confirm_ulids": [unknown]
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "delete absent 200 idempotent: {json}"
    );
    assert_eq!(json["deleted"], false, "deleted=false: {json}");
    assert!(json.get("backup").is_none() || json["backup"].is_null());
}

/// P1-2 — un delete réel émet un tombstone durable AVANT la cascade, portant
/// deleted_by (= sub de l'identité admin = operator-admin) + section + body + timestamp.
#[tokio::test]
async fn real_delete_emits_durable_tombstone() {
    let env = build_app().await;

    let note = env
        .vault
        .write_note(
            live_frontmatter(Section::Feedback),
            "# titre tombstone\ncorps à tracer".to_string(),
        )
        .await
        .expect("write_note réelle");
    let id = note.id.to_string();

    let (status, _json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = env.audit_events.lock().unwrap();
    let tomb = events
        .iter()
        .find(|e| e.event == "vault_delete")
        .expect("un événement d'audit vault_delete doit être émis");

    // deleted_by = sub de l'identité admin synthétique (operator-admin).
    assert_eq!(tomb.actor.sub, "operator-admin", "actor.sub = deleted_by");
    assert_eq!(tomb.outcome, "deleted");
    assert_eq!(tomb.note_id.as_deref(), Some(id.as_str()));
    // Contenu de récupération complet dans le tombstone durable.
    let curator = tomb.curator.as_ref().expect("curator/tombstone présent");
    let t = &curator["tombstone"];
    assert_eq!(t["section"], "feedback", "tombstone.section: {curator}");
    assert_eq!(
        t["deleted_by"], "operator-admin",
        "tombstone.deleted_by: {curator}"
    );
    assert!(
        t["body"].as_str().unwrap_or("").contains("corps à tracer"),
        "tombstone.body capture le corps: {curator}"
    );
    assert!(
        t.get("title").is_some(),
        "tombstone.title présent (même null): {curator}"
    );
}

/// P1-2 — si l'écriture du tombstone durable échoue, la cascade N'EST PAS exécutée :
/// 500 + note toujours présente (jamais de suppression irréversible sans trace).
#[tokio::test]
async fn tombstone_failure_aborts_delete() {
    let env = build_app_with_audit(Arc::new(FailingSink), Arc::new(Mutex::new(Vec::new()))).await;

    let note = env
        .vault
        .write_note(
            live_frontmatter(Section::Feedback),
            "# critique\nne doit pas être supprimée".to_string(),
        )
        .await
        .expect("write_note réelle");
    let id = note.id.to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "échec tombstone → 500: {json}"
    );
    // La note DOIT toujours exister (cascade non exécutée).
    assert!(
        env.idx.get_note("main", &id).await.unwrap().is_some(),
        "note ne doit PAS être supprimée si le tombstone échoue"
    );
    assert!(
        env.vault.read_note_by_id(&id).await.is_ok(),
        "le .md ne doit PAS être supprimé si le tombstone échoue"
    );
}

/// P2-1 — mode réel sur une note inexistante avec confirm_ulids MALFORMÉ → 400,
/// PAS le court-circuit idempotent 200 (validation avant idempotence).
#[tokio::test]
async fn real_delete_absent_note_with_malformed_confirm_returns_400() {
    let env = build_app().await;
    let unknown = Ulid::new().to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({
            "note_id": unknown,
            "dry_run": false,
            "confirm_ulids": ["01JFAKE00000000000000000ZZ"]
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "confirm malformé sur note absente doit être 400 (pas le no-op 200): {json}"
    );
}

// ── P1-1 — câblage du sink d'audit prod (JsonlFileSink durable) ──────────────────

/// Câblage PROD (P1-1) : via le VRAI `with_audit_dir` (JsonlFileSink), un delete réel
/// écrit un tombstone DURABLE sur disque (`audit.YYYY-MM-DD.jsonl`) — pas seulement en
/// mémoire. Prouve que la précondition dure du tombstone est réellement armée en prod
/// (le no-op sink ne subsiste que si aucun sink n'est câblé).
#[tokio::test]
async fn real_delete_writes_durable_jsonl_via_prod_wiring() {
    let (env, audit_dir) = build_app_with_audit_dir().await;

    let note = env
        .vault
        .write_note(
            live_frontmatter(Section::Feedback),
            "# titre\ncorps à archiver (audit durable)".to_string(),
        )
        .await
        .expect("write_note réelle");
    let id = note.id.to_string();

    let (status, json) = post_delete(
        env.app,
        serde_json::json!({ "note_id": id, "dry_run": false, "confirm_ulids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete réel 200 : {json}");
    assert_eq!(json["deleted"], true, "deleted=true : {json}");

    // Un fichier JSONL d'audit a été flush sur disque par le JsonlFileSink câblé.
    let audit_file = std::fs::read_dir(&audit_dir)
        .expect("répertoire d'audit lisible")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .next()
        .expect("un fichier .jsonl d'audit écrit sur disque (câblage prod)");
    let content = std::fs::read_to_string(&audit_file).expect("fichier d'audit lisible");

    // Le tombstone durable porte l'événement, l'acteur admin et l'ULID de la note.
    assert!(
        content.contains("vault_delete"),
        "événement vault_delete dans le tombstone durable : {content}"
    );
    assert!(
        content.contains("operator-admin"),
        "deleted_by=operator-admin dans le tombstone durable : {content}"
    );
    assert!(
        content.contains(&id),
        "ULID de la note dans le tombstone durable : {content}"
    );
}

/// Verrou de câblage (P1-1) : `main.rs` DOIT invoquer `with_audit_dir` au boot. Sans lui,
/// `state.audit` retombe sur `NoopAuditSink` et la précondition dure du tombstone redevient
/// vacante en prod (cause racine du finding P1-1). Ce test échoue si le câblage disparaît.
#[test]
fn main_rs_wires_prod_audit_sink() {
    let src = include_str!("../src/main.rs");
    assert!(
        src.contains(".with_audit_dir("),
        "main.rs doit câbler le sink d'audit durable via with_audit_dir (anti-régression P1-1)"
    );
}
