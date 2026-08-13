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

/// Embedder de test déterministe : retourne un vecteur basé sur le contenu textuel.
///
/// `backend_kind()` = `Http` → non-Noop → active le chemin sémantique (RRF + embed)
/// dans `vault_search` et `vault_context`. Textes lexicalement proches → cosine élevé.
///
/// # Déterminisme
///
/// Même texte → même vecteur (pas de tirage aléatoire). Utilise un hash FNV-1a simplifié
/// sur les bytes du texte pour distribuer les composantes, puis normalise le vecteur (L2).
///
/// # Usage
///
/// ```ignore
/// let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
/// assert_ne!(env.state.embedder.backend_kind(), EmbedBackend::Noop);
/// ```
pub struct FakeEmbedder {
    /// Dimension du vecteur retourné (ex: 1024 pour bge-m3, 8 pour tests rapides).
    pub dim: u16,
}

/// Produit un vecteur déterministe de dimension `dim` à partir du texte.
///
/// Stratégie : chaque byte du texte incrémente la composante `(i*31 + b) % dim`.
/// Le résultat est normalisé L2 (cosine défini). Vecteur nul (texte vide) → `v[0] = 1.0`.
fn deterministic_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    for (i, &b) in text.as_bytes().iter().enumerate() {
        let idx = i.wrapping_mul(31).wrapping_add(b as usize) % dim;
        v[idx] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    } else if dim > 0 {
        // Texte vide : vecteur unitaire [1, 0, 0, ...] — cosine bien défini.
        v[0] = 1.0;
    }
    v
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn embedder_id(&self) -> &str {
        "fake-embedder"
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(deterministic_embed(text, self.dim as usize))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| deterministic_embed(t, self.dim as usize))
            .collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        // Http = non-Noop → active le chemin sémantique dans vault_search/vault_context.
        EmbedBackend::Http
    }
}

/// Embedder de test lent — simule un backend embed avec latence configurable.
///
/// `backend_kind()` = `Http` → non-Noop → active le chemin sémantique.
/// Utiliser pour tester la dégradation gracieuse sur timeout embed.
///
/// # Usage
///
/// ```ignore
/// let env = build_app_with_context_config(
///     Arc::new(SlowEmbedder { delay_ms: 200 }),
///     ContextConfig { embed_timeout_ms: 1, ..ContextConfig::default() },
/// ).await;
/// ```
pub struct SlowEmbedder {
    /// Délai en millisecondes simulant la latence d'un backend embed lent.
    pub delay_ms: u64,
}

#[async_trait]
impl Embedder for SlowEmbedder {
    fn embedder_id(&self) -> &str {
        "slow-embedder"
    }

    fn dim(&self) -> u16 {
        8
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(vec![0.0f32; 8])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        // Http = non-Noop → active le chemin sémantique dans vault_search/vault_context.
        EmbedBackend::Http
    }
}

/// Embedder de test à latence différenciée — rapide pour `embed`, lent pour `embed_batch`.
///
/// Permet de tester le timeout de `build_skill_index` indépendamment du timeout retrieval :
/// - `embed` (requête principale dans retrieval) → réponse immédiate → `query_embedding != None`.
/// - `embed_batch` (index skills dans `build_skill_index`) → attend `batch_delay_ms` →
///   déclenche le timeout si `embed_timeout_ms < batch_delay_ms`.
///
/// `backend_kind()` = `Http` → active le chemin sémantique.
pub struct SlowBatchEmbedder {
    /// Délai en ms appliqué uniquement à `embed_batch` (simule index skills lent).
    pub batch_delay_ms: u64,
}

#[async_trait]
impl Embedder for SlowBatchEmbedder {
    fn embedder_id(&self) -> &str {
        "slow-batch-embedder"
    }

    fn dim(&self) -> u16 {
        8
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Rapide : pas de sleep → retrieval embed réussit dans n'importe quel timeout.
        Ok(vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Lent : simule un backend embed surchargé pour le build de l'index skills.
        tokio::time::sleep(std::time::Duration::from_millis(self.batch_delay_ms)).await;
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http
    }
}

/// Variante de [`build_app_inner`] avec embedder + ContextConfig paramétriques.
///
/// Permet d'override `embed_timeout_ms` ou tout autre champ de [`ContextConfig`]
/// pour tester le comportement de dégradation gracieuse (timeout embed, budget, etc.).
///
/// # Exemple
///
/// ```ignore
/// let env = build_app_with_context_config(
///     Arc::new(SlowEmbedder { delay_ms: 200 }),
///     gradatum_server::config::ContextConfig { embed_timeout_ms: 1, ..Default::default() },
/// ).await;
/// ```
pub async fn build_app_with_context_config(
    embedder: Arc<dyn Embedder>,
    context: gradatum_server::config::ContextConfig,
) -> TestEnv {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir tests/helpers");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test fixture"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let jwt = gradatum_auth::jwt::JwtService::new_ephemeral();
    let acl = gradatum_acl_policy::AclEngine::from_preset_str(TEST_ACL)
        .expect("preset ACL alpha13 valide");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(embedder)
        .with_vault_arc(vault_registry)
        .with_context(context);
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

/// Noyau commun : construit un `TestEnv` E2E en injectant l'embedder fourni.
///
/// Évite la duplication entre `build_app()` (NoopBackend) et
/// `build_app_with_embedder()` (embedder paramétrique).
async fn build_app_inner(embedder: Arc<dyn Embedder>) -> TestEnv {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir tests/helpers");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test fixture"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL alpha13 valide");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(embedder)
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
///
/// Pour activer le chemin sémantique (RRF), utiliser [`build_app_with_embedder`].
pub async fn build_app() -> TestEnv {
    build_app_inner(Arc::new(NoopBackend)).await
}

/// Variante de [`build_app`] injectant un embedder paramétrique.
///
/// Permet d'activer le chemin sémantique/RRF dans `vault_search` et `vault_context`
/// en passant un `FakeEmbedder { dim }` (non-Noop). Requise pour tous les tests
/// sémantiques (Tasks 5/9/10) — sous `NoopBackend`, `backend_kind()==Noop` désactive RRF.
///
/// # Exemple
///
/// ```ignore
/// let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
/// assert_ne!(env.state.embedder.backend_kind(), EmbedBackend::Noop);
/// ```
pub async fn build_app_with_embedder(embedder: Arc<dyn Embedder>) -> TestEnv {
    build_app_inner(embedder).await
}

/// Construit un `AppState` **2-vaults réels** (`main` + `vault-b`), adossés au MÊME pool
/// `SqliteIndex` (un seul `index.db`, partition par la colonne `vault_id`) — modèle
/// multi-vault cible (council `01KXWMCR0N`) : md par-vault sous `<root>/<vault_id>/`.
///
/// Choke-point de routage peuplé : `state.vaults.resolve(&vault_id)` sert le handle du
/// vault demandé (fail-closed `VaultNotFound` sur absence). Réutilisé par tous les tests
/// **ON** des vagues W1-W3 qui exercent l'isolation cross-vault.
///
/// ## Régime flag
///
/// Ce harnais est PUREMENT local aux tests : il matérialise un 2e vault que le LIVE
/// n'aura JAMAIS tant que `multi_tenant` est OFF (byte-identical LIVE = registre `{main}`
/// singleton via `with_vault_path`). Il ne modifie AUCUN flag.
///
/// ## Cycle de vie disque
///
/// Le contrat plan (T1) impose la signature `-> AppState` : le helper ne peut donc pas
/// porter le guard `TempDir`. La racine est volontairement **fuitée** (`TempDir::keep()`)
/// pour que le vault disque survive à toute la durée du test — sans quoi le drop du guard
/// supprimerait `index.db`/les `.md` et casserait les reads des consommateurs. Fuite
/// test-only, bornée à la durée du binaire de test (process court).
pub async fn spawn_two_vault_state() -> AppState {
    use gradatum_core::scope::VaultId;
    use gradatum_server::state::VaultRegistry;
    use gradatum_vault::Vault;

    let root = TempDir::new()
        .expect("TempDir spawn_two_vault_state")
        .keep();

    // `main` ouvre le pool `index.db` ; `vault-b` le RÉUTILISE (handle partagé, un seul pool).
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main — invariant harnais 2-vaults"),
    );
    let shared_index = Arc::clone(vault_main.index());
    let vault_b = Arc::new(
        Vault::with_shared_index(&root, VaultId::new("vault-b"), Arc::clone(&shared_index))
            .await
            .expect("Vault::with_shared_index vault-b — invariant harnais 2-vaults"),
    );

    // Registre 2-vaults via `insert` (fail-closed vault_id — réutilise state.rs:565).
    let registry = VaultRegistry::new();
    registry
        .insert(VaultId::new("main"), Arc::clone(&vault_main))
        .expect("insert main — vault_id cohérent par construction");
    registry
        .insert(VaultId::new("vault-b"), Arc::clone(&vault_b))
        .expect("insert vault-b — vault_id cohérent par construction");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL alpha13 valide");
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault_main.clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::new(NoopBackend))
        .with_vault_arc(vault_registry);
    // Aligner `state.search` sur le pool partagé (cohérence write/read côté SQLite).
    state.search = shared_index;
    // Peupler le registre de handles — `with_vault_arc` ne set que le singleton `state.vault`.
    state.vaults = Arc::new(registry);
    state
}

// ── Helpers Task 6 — Filtre incrémental session (F-30) ──────────────────────

/// Variante de [`build_app_with_embedder`] avec un [`SessionTraceStore`] in-memory câblé.
///
/// Nécessaire pour les tests F-30 (filtre incrémental session) : `get_sent`
/// et `mark_sent` sont opérationnels (store présent), contrairement à
/// [`build_app_with_embedder`] où `state.session_trace` est `None`.
///
/// Le store in-memory est partagé via `Arc` avec le state injecté dans le router :
/// les appels `mark_sent` via le handler sont visibles via `env.state.session_trace`.
///
/// # Usage
///
/// ```ignore
/// let env = build_app_with_session_trace_and_embedder(
///     Arc::new(FakeEmbedder { dim: 1024 }),
/// ).await;
/// assert!(env.state.session_trace.is_some());
/// ```
pub async fn build_app_with_session_trace_and_embedder(embedder: Arc<dyn Embedder>) -> TestEnv {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let tmp = TempDir::new().expect("TempDir tests/helpers (session_trace)");
    let vault_path = tmp.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test fixture"),
    );
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let index = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL alpha13 valide");

    // Ouvrir le SessionTraceStore sur le même index.db que le vault.
    // Vault::create a déjà exécuté la migration 0015 (session_trace table) — connexion sûre.
    // L'Arc<Mutex<Connection>> interne est partagé entre le clone injecté dans le router
    // et env.state : mark_sent depuis le handler est visible via env.state.session_trace.
    let db_path = vault_path.join(".gradatum").join("index.db");
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(embedder)
        .with_vault_arc(vault_registry)
        .with_session_trace_path(&db_path)
        .await
        .expect("SessionTraceStore::open — invariant test fixture");
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

/// Effectue `POST /api/v1/vault_context` avec un body JSON libre et retourne
/// `(StatusCode, serde_json::Value)` sans paniquer sur erreur HTTP.
///
/// Contrairement à [`call_vault_context_json`] qui panique sur non-200, ce helper
/// permet de tester les cas d'erreur (ex : `session_id` invalide → 400 BAD_REQUEST).
///
/// Le body JSON est `serde_json::Value::Null` si la réponse est vide ou non-JSON.
///
/// # Usage
///
/// ```ignore
/// let (status, body) = call_vault_context_json_status(
///     env.app.clone(), &token,
///     serde_json::json!({"query": "test", "session_id": "invalide"}),
/// ).await;
/// assert_eq!(status, StatusCode::BAD_REQUEST);
/// ```
pub async fn call_vault_context_json_status(
    app: axum::Router,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/vault_context")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("sérialisation body — invariant"),
        ))
        .expect("construction requête — invariant");

    let resp = app.oneshot(req).await.expect("vault_context oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body — invariant")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
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
    idx.upsert_note_title("main", &nid, title)
        .await
        .expect("upsert_note_title");
    nid
}

/// Variante SQL-only avec downgrade (status='downgraded').
pub async fn seed_note_downgraded_sql(idx: &SqliteIndex, ulid: &str, title: &str) -> NoteId {
    let nid = seed_note_sql_only(idx, ulid, "reference", title, "downgraded body").await;
    idx.downgrade_note(
        &gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new("main"),
        ),
        &nid,
        "test fixture downgrade",
        None,
    )
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
            .upsert_note_title(note.frontmatter.vault_id.as_str(), &note.id, title)
            .await
            .expect("upsert_note_title seed");

        note.id
    }

    /// Seed une note + applique downgrade.
    pub async fn write_note_downgraded(&self, title: &str) -> NoteId {
        let nid = self.write_note_with_h1(title, "downgraded body").await;
        self.state
            .search
            .downgrade_note(
                &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                    gradatum_core::scope::VaultId::new("main"),
                ),
                &nid,
                "test fixture downgrade",
                None,
            )
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

/// Seed une note dans la section `skills` (colonne SQL string directe — hors enum Section).
///
/// Écrit via `seed_note_with_fts` sur l'index concret du vault (accessible via
/// `env._vault_typed.index()`). Utilisé pour l'injection de skills opt-in `vault_context`.
///
/// # Note sur la section
///
/// La section `"skills"` est passée comme string libre au SQL (`notes.section`).
/// L'enum [`gradatum_core::section::Section`] ne comporte pas de variant `Skills` —
/// toute requête SQL peut filtrer sur `section = 'skills'`; les handlers Axum qui
/// passent par l'enum ignorent ces notes dans les chemins enum-gated.
pub async fn seed_skill(env: &TestEnv, title: &str, body: &str) {
    let idx = env._vault_typed.index();
    let ulid = Ulid::generate().to_string();
    let full_body = format!("# {title}\n{body}");
    idx.seed_note_with_fts(&ulid, "skills", &full_body)
        .await
        .expect("seed_skill: seed_note_with_fts — invariant test");
    let note_id =
        NoteId(ulid::Ulid::from_string(&ulid).expect("seed_skill: ULID parse — invariant"));
    idx.upsert_note_title("main", &note_id, title)
        .await
        .expect("seed_skill: upsert_note_title — invariant test");
}

// ── Helpers Task 5 — Retrieval RRF ───────────────────────────────────────────

/// Effectue `POST /api/v1/vault_context` avec un body JSON libre.
///
/// Contourne la signature fixe de [`call_vault_context`] (qui n'expose pas `mode`).
/// Panique si la réponse n'est pas HTTP 200 (erreur de test, pas erreur métier).
///
/// # Usage
///
/// ```ignore
/// let resp = call_vault_context_json(
///     env.app.clone(), &token,
///     serde_json::json!({"query": "alpha", "mode": "assembled"}),
/// ).await;
/// assert!(resp["assembled_text"].as_str().is_some());
/// ```
pub async fn call_vault_context_json(
    app: axum::Router,
    token: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = Request::builder()
        .uri("/api/v1/vault_context")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("sérialisation body — invariant"),
        ))
        .expect("construction requête — invariant");

    let resp = app.oneshot(req).await.expect("vault_context oneshot");
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "vault_context doit retourner 200, body = {:?}",
        body,
    );
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body — invariant")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("decode vault_context JSON — invariant")
}

/// Seed `count` notes avec du contenu FTS-searchable (mots « alpha », « beta », etc.).
///
/// Les notes contiennent « alpha » + un numéro → matchent la requête `"alpha beta"` via FTS.
/// Utilisé par [`retrieval_semantic_path_active_with_fake_embedder`].
///
/// # Note sur `env.state.search`
///
/// `env.state.search` est un `Arc<dyn Index>` — pas dowcastable vers `SqliteIndex`.
/// On passe par `env._vault_typed.index()` pour accéder aux méthodes `seed_*` inhérentes.
pub async fn seed_notes(env: &TestEnv, count: usize) {
    let idx = env._vault_typed.index();
    for i in 0..count {
        let ulid = Ulid::generate().to_string();
        // Contenu contenant « alpha » et « beta » + numéro pour variété lexicale.
        let body = format!(
            "# Note alpha beta {i}\nalpha beta contenu test retrieval rrf note numéro {i}."
        );
        idx.seed_note_with_fts(&ulid, "reference", &body)
            .await
            .expect("seed_notes: seed_note_with_fts — invariant test");
        let nid = NoteId(Ulid::from_string(&ulid).expect("seed_notes: ULID parse"));
        idx.upsert_note_title("main", &nid, &format!("Note alpha beta {i}"))
            .await
            .expect("seed_notes: upsert_note_title — invariant test");
    }
}

/// Seed une note et retourne son ULID sous forme de `String`.
///
/// Écrit via `Vault::write_note` (fichier .md + index SQLite) — `read_note_by_id`
/// et `get_note` fonctionneront ensuite. Utilisé par le test ULID-direct.
pub async fn seed_note_return_ulid(env: &TestEnv, title: &str, body: &str) -> String {
    let nid = env.write_note_in_section("reference", title, body).await;
    nid.to_string()
}

/// Seed une note source qui contient un lien wikilink vers `target_ulid`.
///
/// Crée la note source (SQL + fichier .md) puis insère l'entrée dans `note_links`.
/// Après cet appel, `state.search.backlinks("main", target_ulid)` retournera la source.
pub async fn seed_backlink_to(env: &TestEnv, target_ulid: &str) {
    // Créer la note source via Vault::write_note (cohérence disque + index).
    let src_nid = env
        .write_note_in_section(
            "reference",
            &format!("Lien vers {target_ulid}"),
            &format!("Contenu qui link vers [[{target_ulid}]]."),
        )
        .await;
    let src_ulid = src_nid.to_string();

    // Insérer le lien dans note_links via l'index SqliteIndex inherent.
    let idx = env._vault_typed.index();
    idx.upsert_link("main", &src_ulid, target_ulid)
        .await
        .expect("seed_backlink_to: upsert_link — invariant test");
}
