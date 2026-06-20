//! Helpers tests partagés — `vault_trace`, `vault_read`, `vault_context`.
//!
//! Pattern TDD : `#[path = "helpers/mod.rs"] mod helpers;` au début de chaque fichier
//! de test d'intégration correspondant.
//!
//! ## Conventions de typage
//!
//! - `vault_id` runtime = `&str` (généralement `"main"` dans les helpers)
//! - `tenant_id` DTO = `String` (sérialisation JSON inchangée)
//! - `seed_note_with_fts` hard-code `vault_id='main'` côté SQLite — paramètre `vault`
//!   du helper conservé pour signature lisible (ignoré côté SQL pour l'instant).
//!
//! ## API réelle vs spec rev2.1
//!
//! La spec rev2.1 a documenté des signatures indicatives. L'API réelle SqliteIndex
//! impose :
//! - `seed_note_with_fts(id, section, body)` — vault toujours "main"
//! - `upsert_note_title(NoteId, title)` — pas de vault arg
//! - `downgrade_note(NoteId, reason, replaced_by)` — pas de `set_status`
//!
//! Les helpers ci-dessous adaptent à cette API tout en gardant l'intention de la spec.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::identity::NoteId;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

/// Preset ACL minimal autorisant `read_patterns` larges sur le tenant `main`.
///
/// Les patterns autorisent à la fois `main/*` (locus avec section explicite),
/// `main/main` (locus tenant-only) et `*/reference` (cross-tenant section
/// `reference`) pour couvrir tous les chemins de section utilisés dans les
/// tests Tasks 14/15/16.
pub const TEST_ACL: &str = r#"
[[consumer]]
identity = "alpha13-tester"
read_patterns  = ["main/*", "main/main", "*/reference", "reference/*", "decisions/*", "main/decisions"]
write_patterns = []
"#;

/// Embedder Noop dimension 8 — utilisé pour tous les tests Tasks 14/15/16 qui ne
/// dépendent pas de la sémantique embedding (BM25 + title_lookup + FTS pur).
pub struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-alpha13"
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

/// Bundle test : `(Router, AppState, TempDir)` — TempDir DOIT être conservé en vie
/// pour la durée du test (sinon le vault disque disparaît et `read_note_by_id` échoue).
///
/// Utiliser le pattern :
/// ```ignore
/// let env = build_app().await;
/// let token = sign_token(&env.state);
/// let resp = call_vault_read(env.app.clone(), &token, ...).await;
/// // env._tmp est drop à la fin du test → vault tempdir nettoyé
/// ```
pub struct TestEnv {
    pub app: axum::Router,
    pub state: AppState,
    /// Handle typé vers le vault concret — nécessaire pour `vault.write_note`
    /// (méthode inhérente, pas dans le trait Registry).
    pub _vault_typed: Arc<gradatum_vault::Vault>,
    /// Conservé pour empêcher le drop du vault disque pendant le test.
    pub _tmp: TempDir,
}

/// Construit un environnement de test E2E avec un Vault réel sur TempDir.
///
/// - `state.search` = index `vault.index()` partagé avec le vault (cohérence write/read).
/// - `state.vault` = `Vault::create(TempDir/.gradatum/...)` (lecture par ULID OK).
/// - Embedder Noop dim=8.
/// - ACL `alpha13-tester` (read scope).
///
/// **Important** : `vault.write_note(frontmatter, body)` est requis pour que
/// `state.vault.read_note_by_id(ulid)` réussisse (fichier .md sur disque + index SQLite).
/// Les helpers `seed_note_with_h1` / `seed_note_downgraded` utilisent ce chemin.
pub async fn build_app() -> TestEnv {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir tests/helpers");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create test fixture"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL alpha13 valide");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        .with_vault_arc(vault_registry);
    // Aligner state.search sur le SqliteIndex utilisé par le vault — sinon
    // les tests qui font `state.search.title_lookup` seraient déconnectés du
    // vault-disk-write. Pas de `with_search_arc` public — assignation directe
    // (le field est `pub` dans AppState).
    state.search = index;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    TestEnv {
        app,
        state,
        _vault_typed: vault,
        _tmp: tmp,
    }
}

/// Signe un JWT pour le consumer `alpha13-tester` (read scope, tenant `main`).
pub fn sign_token(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "alpha13-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT alpha13-tester")
}

/// Seed une note via SQL only (`seed_note_with_fts` + `upsert_note_title`), SANS
/// passer par `Vault::write_note`.
///
/// **Limitation** : le fichier `.md` n'est PAS créé sur disque — `state.vault.read_note_by_id`
/// renverra une erreur Storage. Utilisable uniquement pour les tests qui consomment
/// l'index SQLite directement (ex. tests `state.search.title_lookup` ou tests qui
/// appellent les handlers utilisant le SQLite — Tasks 15/16 vault_trace/vault_context
/// utilisent `state.search.get_note()` directement).
pub async fn seed_note_sql_only(
    idx: &SqliteIndex,
    ulid: &str,
    section: &str,
    title: &str,
    body: &str,
) -> NoteId {
    let full = format!("# {title}\n{body}");
    idx.seed_note_with_fts(ulid, section, &full)
        .await
        .expect("seed_note_with_fts");
    let nid = NoteId(Ulid::from_string(ulid).expect("ULID parse seed_note_sql_only"));
    idx.upsert_note_title(&nid, title)
        .await
        .expect("upsert_note_title");
    nid
}

/// Variante SQL-only avec downgrade (status='downgraded').
pub async fn seed_note_downgraded_sql(idx: &SqliteIndex, ulid: &str, title: &str) -> NoteId {
    let nid = seed_note_sql_only(idx, ulid, "reference", title, "downgraded body").await;
    idx.downgrade_note(&nid, "test fixture downgrade", None)
        .await
        .expect("downgrade_note");
    nid
}

/// Seed une note dans une section spécifique (SQL only — pour Tasks 15/16 FTS).
pub async fn seed_note_in_section(
    idx: &SqliteIndex,
    ulid: &str,
    section: &str,
    title: &str,
    body: &str,
) -> NoteId {
    seed_note_sql_only(idx, ulid, section, title, body).await
}

// ── Variante TestEnv : seed via Vault::write_note (fichier .md + SQL) ──────────

impl TestEnv {
    /// Seed une note via `Vault::write_note` — fichier .md sur disque + index SQLite.
    ///
    /// Permet à `state.vault.read_note_by_id(ulid)` de réussir.
    /// Retourne le `NoteId` réel généré par `Vault::write_note` + applique
    /// `upsert_note_title` pour la résolution `title_lookup`.
    pub async fn write_note_with_h1(&self, title: &str, body: &str) -> NoteId {
        self.write_note_in_section("reference", title, body).await
    }

    /// Variante avec section explicite.
    pub async fn write_note_in_section(&self, section: &str, title: &str, body: &str) -> NoteId {
        use chrono::Utc;
        use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
        use gradatum_core::scope::VaultId;
        use gradatum_core::section::Section;
        use gradatum_core::status::NoteStatus;

        let section_enum = match section {
            "decisions" => Section::Decisions,
            "experiments" => Section::Experiments,
            "debug" => Section::Debug,
            "architecture" => Section::Architecture,
            "retrospectives" => Section::Retrospectives,
            "reasoning" => Section::Reasoning,
            "feedback" => Section::Feedback,
            "lessons-learned" => Section::LessonsLearned,
            "agent-issues" => Section::AgentIssues,
            _ => Section::Reference,
        };
        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: section_enum,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        let body_full = format!("# {title}\n{body}");
        let note = self
            .vault_arc()
            .write_note(frontmatter, body_full)
            .await
            .expect("vault.write_note seed");

        // Upsert title (mimique du curator post-extract — alpha.10 migration 0005).
        self.state
            .search
            .upsert_note_title(&note.id, title)
            .await
            .expect("upsert_note_title seed");

        note.id
    }

    /// Seed une note + applique downgrade.
    pub async fn write_note_downgraded(&self, title: &str) -> NoteId {
        let nid = self.write_note_with_h1(title, "downgraded body").await;
        self.state
            .search
            .downgrade_note(&nid, "test fixture downgrade", None)
            .await
            .expect("downgrade_note");
        nid
    }

    /// Récupère le `Arc<Vault>` typé concret stocké dans le TestEnv.
    fn vault_arc(&self) -> Arc<gradatum_vault::Vault> {
        self._vault_typed.clone()
    }
}

/// Effectue une requête `POST /api/v1/vault_read` et retourne la `Response` complète.
///
/// Permet aux tests d'inspecter le `StatusCode` (404, 200) avant de décoder le body.
pub async fn call_vault_read_raw(
    app: axum::Router,
    token: &str,
    path: &str,
    tenant_id: &str,
) -> axum::http::Response<Body> {
    let body = serde_json::json!({
        "path": path,
        "tenant_id": tenant_id,
    });
    let req = Request::builder()
        .uri("/api/v1/vault_read")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.expect("vault_read oneshot")
}

/// Effectue `POST /api/v1/vault_read` et décode le JSON si statut 200.
///
/// Renvoie `Err(StatusCode)` si non-200.
pub async fn call_vault_read(
    app: axum::Router,
    token: &str,
    path: &str,
    tenant_id: &str,
) -> Result<serde_json::Value, StatusCode> {
    let resp = call_vault_read_raw(app, token, path, tenant_id).await;
    let status = resp.status();
    if status != StatusCode::OK {
        return Err(status);
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Ok(serde_json::from_slice(&bytes).expect("decode vault_read JSON"))
}

/// Effectue `POST /api/v1/vault_trace` et retourne la `Response`.
pub async fn call_vault_trace_raw(
    app: axum::Router,
    token: &str,
    query: &str,
    tenant_id: &str,
    limit: u32,
) -> axum::http::Response<Body> {
    let body = serde_json::json!({
        "query": query,
        "tenant_id": tenant_id,
        "limit": limit,
    });
    let req = Request::builder()
        .uri("/api/v1/vault_trace")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.expect("vault_trace oneshot")
}

/// Effectue `POST /api/v1/vault_trace` et décode le JSON si 200.
pub async fn call_vault_trace(
    app: axum::Router,
    token: &str,
    query: &str,
    tenant_id: &str,
    limit: u32,
) -> Result<serde_json::Value, StatusCode> {
    let resp = call_vault_trace_raw(app, token, query, tenant_id, limit).await;
    let status = resp.status();
    if status != StatusCode::OK {
        return Err(status);
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Ok(serde_json::from_slice(&bytes).expect("decode vault_trace JSON"))
}

/// Effectue `POST /api/v1/vault_context`.
pub async fn call_vault_context_raw(
    app: axum::Router,
    token: &str,
    query: &str,
    tenant_id: &str,
    max_tokens: Option<u32>,
    section: Option<&str>,
) -> axum::http::Response<Body> {
    let mut body = serde_json::Map::new();
    body.insert(
        "query".to_string(),
        serde_json::Value::String(query.to_string()),
    );
    body.insert(
        "tenant_id".to_string(),
        serde_json::Value::String(tenant_id.to_string()),
    );
    if let Some(mt) = max_tokens {
        body.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(mt.into()),
        );
    }
    if let Some(sec) = section {
        body.insert(
            "section".to_string(),
            serde_json::Value::String(sec.to_string()),
        );
    }
    let body = serde_json::Value::Object(body);
    let req = Request::builder()
        .uri("/api/v1/vault_context")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.expect("vault_context oneshot")
}

/// Effectue `POST /api/v1/vault_context` et décode le JSON si 200.
pub async fn call_vault_context(
    app: axum::Router,
    token: &str,
    query: &str,
    tenant_id: &str,
    max_tokens: Option<u32>,
    section: Option<&str>,
) -> Result<serde_json::Value, StatusCode> {
    let resp = call_vault_context_raw(app, token, query, tenant_id, max_tokens, section).await;
    let status = resp.status();
    if status != StatusCode::OK {
        return Err(status);
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Ok(serde_json::from_slice(&bytes).expect("decode vault_context JSON"))
}
