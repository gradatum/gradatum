//! Tests adversariaux C4 (caveat C1 HAUTE — council `01KXTRART`) : isolation cross-vault
//! des MUTATIONS **couche Vault** par ULID, direction **tiers → main**.
//!
//! ## Menace fermée
//!
//! Les chemins `move` / `PATCH.status` / `add_tags` / `vault_restore` passent par
//! `state.vault.<method>` = une instance `Vault` figée au boot, liée au vault `main`. Avant
//! ce correctif, ces méthodes résolvaient la note sous `self.tenant_id` (= `main`) SANS tenir
//! compte du vault du tenant requête : un tenant **tiers** légitime (JWT + scope write +
//! self-grant sur SON vault, provisionné dans `tenant_vault_grants`) pouvait muter une note
//! de `main` en la ciblant par son seul ULID. C'est le vecteur write cross-vault
//! **tiers → main**, non couvert par `cross_vault_write_c3a.rs` (qui teste main → research,
//! déjà bloqué car le Vault est lié à `main`).
//!
//! ## Modèle de test
//!
//! La note-victime est une **VRAIE note de `main`** (frontmatter + `.md` sur disque, écrite via
//! le Vault) : sans le correctif, `read_note(main, victim)` la trouve → la mutation ABOUTIT. Le
//! `.md` réel est donc indispensable pour que le test DISCRIMINE le fix (une note index-only
//! donnerait un 404 trompeur — `.md` absent — même sans gate).
//!
//! L'attaquant est le tenant tiers **`research`** (actif + self-grant write, ACL Read/Write sur
//! `research/*`). Il cible la victime par son ULID en déclarant son PROPRE `tenant_id = "research"`
//! : il PASSE le middleware (allow-list), l'ACL Write sur `research/main`, le scope write et le
//! self-grant. Post-fix, le témoin [`AclCheckedVaultId`] porte `research` ≠ `main` (tenant du
//! Vault) → `NoteNotFound` (404) AVANT toute mutation. Un `403` signifierait un refus ACL/grant
//! (l'attaquant n'aurait pas atteint le gate) ; on exige donc **404** ET l'intégrité de la victime.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::scope::{LocusId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

// L'attaquant porte AUSSI l'ACL `main/*` : ça ne l'aide pas à flag ON (le tenant est dérivé
// du JWT = `research`, le handler évalue `research/main` puis le gate témoin bloque), mais ça
// permet au même consumer de jouer le tenant légitime `main` dans le test byte-identical OFF.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "attacker-research"
read_patterns  = ["research/*", "research/main", "research/timeline", "main/*", "main/main", "main/timeline"]
write_patterns = ["research/*", "research/main", "main/*", "main/main"]
"#;

/// Locus initial de la victime — sert de témoin d'intégrité pour le test `move`.
const VICTIM_LOCUS: &str = "origin";

struct Env {
    state: AppState,
    index_path: std::path::PathBuf,
    /// VRAIE note (frontmatter + `.md`) du vault `main`, cible des attaques cross-vault.
    victim: Ulid,
    /// sha256 hex du contenu de la victime — pour les tentatives d'overwrite avec sha CONNU.
    victim_sha: String,
    _dir: TempDir,
}

/// Frontmatter minimal d'une note `main` `live`, locus [`VICTIM_LOCUS`].
fn victim_frontmatter() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: Some(LocusId::parse(VICTIM_LOCUS).expect("locus valide")),
        section: Section::Reference,
        status: NoteStatus::Live,
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

/// `AppState` : Vault réel lié à `main` (seed migration 0030 `main`↔`main`) + une VRAIE
/// note-victime écrite via le Vault (`.md` présent) ; index SQLite PARTAGÉ avec `state.search`,
/// flag `multi_tenant` paramétrable.
async fn build_env(multi_tenant_enabled: bool) -> Env {
    let dir = TempDir::new().expect("tempdir");
    let vault_dir = dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );
    let index_path = gradatum_core::paths::vault_dir_index_path(&vault_dir);

    // Écrit une VRAIE note dans `main` (frontmatter + `.md` + index) — pas un seed index-only.
    let victim_note = vault
        .write_note(victim_frontmatter(), "main secret corpus".into())
        .await
        .expect("seed victim note via Vault");
    let victim = victim_note.id.0;
    let victim_sha = victim_note.content_hash.hex();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL valide — invariant statique");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig {
            enabled: multi_tenant_enabled,
        },
        ..ServerConfig::default()
    };

    let idx = vault.index().clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault as Arc<dyn gradatum_vault::Registry>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);
    state.search = idx as Arc<dyn gradatum_core::index::Index>;

    Env {
        state,
        index_path,
        victim,
        victim_sha,
        _dir: dir,
    }
}

/// Insère un grant agent→vault (B7) pour chaque identité listée — le middleware
/// vérifie `tenant_grants ∩ agent_grants` quand `multi_tenant.enabled = true`.
fn seed_agent_grants(index_path: &std::path::Path, agents: &[&str]) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed agent grants");
    for agent in agents {
        conn.execute(
            "INSERT OR IGNORE INTO agent_vault_grants (agent_id, vault_id, access) VALUES (?1, 'main', 'write')",
            rusqlite::params![agent],
        )
        .expect("seed agent grant");
    }
}

/// Provisionne le tenant tiers `research` : actif + self-grant write (allow-list). C'est ce
/// provisioning (sans 2e vault physique) qui rend le vecteur tiers→main joignable à flag ON.
fn seed_research_tenant(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed tenant");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES ('research', 'research', 'write');
         -- B9 : agent grant pour attacker-research sur son propre vault
         INSERT INTO agent_vault_grants (agent_id, vault_id, access) VALUES ('attacker-research', 'research', 'write');",
    ))
    .expect("seed research tenant + self-grant + agent grant");
}

fn build_router(state: AppState) -> axum::Router {
    use axum::{Router, middleware};
    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

/// Signe un JWT read+write pour `(sub="attacker-research", tenant)`.
fn sign(state: &AppState, tenant: &str) -> String {
    state
        .jwt
        .sign(
            "attacker-research",
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign JWT test")
}

/// `(status, locus)` de la note-victime dans `main` — prouve l'absence de mutation.
fn main_victim_state(index_path: &std::path::Path, victim: &Ulid) -> (String, String) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db check");
    conn.query_row(
        "SELECT status, COALESCE(locus, '') FROM notes WHERE id = ?1 AND vault_id = 'main'",
        rusqlite::params![victim.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("note main présente")
}

/// Tags (colonne `notes.tags`, espace-séparés) de la note-victime dans `main` — prouve que
/// le champ RÉELLEMENT ciblé par une attaque `add_tags` n'a pas été muté.
fn main_victim_tags(index_path: &std::path::Path, victim: &Ulid) -> String {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db tags check");
    conn.query_row(
        "SELECT COALESCE(tags, '') FROM notes WHERE id = ?1 AND vault_id = 'main'",
        rusqlite::params![victim.to_string()],
        |row| row.get(0),
    )
    .expect("note main présente")
}

/// Corps indexé (`notes.body_text`) de la victime dans `main` — prouve l'intégrité du CONTENU
/// après une tentative d'overwrite (le contenu original ne doit pas avoir été altéré).
fn main_victim_body(index_path: &std::path::Path, victim: &Ulid) -> String {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db body check");
    conn.query_row(
        "SELECT COALESCE(body_text, '') FROM notes WHERE id = ?1 AND vault_id = 'main'",
        rusqlite::params![victim.to_string()],
        |row| row.get(0),
    )
    .expect("note main présente")
}

async fn request(
    router: axum::Router,
    method: &str,
    uri: &str,
    jwt: &str,
    body: serde_json::Value,
) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    router.oneshot(req).await.expect("service").status()
}

// ── move — chemin `state.vault.move_locus` (gate témoin C4) ──────────────────────

/// ON : move cross-vault tiers→main → 404 `NoteNotFound`, note `main` intacte (statut + locus).
/// Discriminant : sans le gate, la VRAIE note serait déplacée `origin → knowledge` (204).
#[tokio::test]
async fn flag_on_move_tiers_to_main_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");

    let status = request(
        build_router(env.state.clone()),
        "POST",
        &format!("/api/v1/notes/{}/move", env.victim),
        &jwt,
        serde_json::json!({ "locus": "knowledge" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "move tiers→main doit être 404 (gate témoin, PAS 403 ACL/grant)"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim),
        ("live".to_string(), VICTIM_LOCUS.to_string()),
        "la note main NE DOIT PAS avoir été déplacée ni altérée"
    );
}

/// ON : oracle — un ULID inexistant donne le MÊME 404 que la victime cross-vault (pas d'oracle
/// d'existence divulgué par le gate).
#[tokio::test]
async fn flag_on_move_nonexistent_matches_tiers_to_main_oracle() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");
    let ghost = Ulid::generate();

    let status = request(
        build_router(env.state.clone()),
        "POST",
        &format!("/api/v1/notes/{ghost}/move"),
        &jwt,
        serde_json::json!({ "locus": "knowledge" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "ULID inexistant → 404, indistinguable du cross-vault tiers→main"
    );
}

// ── PATCH.status — chemin `state.vault.update_note_status` (gate témoin C4) ───────

/// ON : PATCH status cross-vault tiers→main → 404, note `main` toujours `live`.
/// Discriminant : sans le gate, la transition `live → deprecated` aboutirait (204).
#[tokio::test]
async fn flag_on_patch_status_tiers_to_main_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");

    let status = request(
        build_router(env.state.clone()),
        "PATCH",
        &format!("/api/v1/notes/{}", env.victim),
        &jwt,
        serde_json::json!({ "status": "deprecated" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "PATCH status tiers→main doit être 404 (gate témoin couche-Vault)"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).0,
        "live",
        "la note main NE DOIT PAS avoir changé de statut"
    );
}

// ── add_tags — chemin `state.vault.add_tags` (gate témoin C4, DÉFENSE EN PROFONDEUR) ──
//
// NB discrimination : le gate `add_tags` est volontairement redondant. À flag ON, un PATCH
// add_tags-seul traverse d'abord `patch_note_impl`, dont le témoin index (`patch_note_status`
// scopé `AND vault_id = ?`, C3a) refuse déjà la cible cross-vault (404) AVANT d'atteindre
// `state.vault.add_tags`. Le gate couche-Vault est donc une 2e barrière (parité de traitement
// des 4 chemins, robustesse si un futur appelant court-circuite `patch_note_impl`). Seuls
// `move` et `PATCH.status` sont GÉNUINEMENT discriminants (sans gate → mutation aboutit, 204,
// cf. tests dédiés). On vérifie néanmoins ici l'intégrité du champ RÉELLEMENT ciblé (tags).

/// ON : PATCH add_tags cross-vault tiers→main → 404, note `main` intacte, tag `pwned` absent.
#[tokio::test]
async fn flag_on_add_tags_tiers_to_main_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");

    let status = request(
        build_router(env.state.clone()),
        "PATCH",
        &format!("/api/v1/notes/{}", env.victim),
        &jwt,
        serde_json::json!({ "add_tags": ["pwned"] }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "PATCH add_tags tiers→main doit être 404 (défense en profondeur : témoin index + gate Vault)"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).0,
        "live",
        "la note main NE DOIT PAS avoir été modifiée"
    );
    assert!(
        !main_victim_tags(&env.index_path, &env.victim).contains("pwned"),
        "le champ RÉELLEMENT ciblé (tags) NE DOIT PAS contenir 'pwned' — intégrité du champ muté"
    );
}

// ── vault_restore — chemin `state.vault.history_restore` (gate témoin C4) ─────────

/// ON : restore cross-vault tiers→main → statut d'erreur (non-2xx), note `main` intacte.
/// DÉFENSE EN PROFONDEUR (non discriminant) : sans le gate, `history_restore` échouerait de
/// toute façon sur le snapshot d'historique absent (ts_ms=1) — le gate rejette simplement plus
/// tôt (parité oracle avec les 3 autres chemins). Seuls `move`/`PATCH.status` sont discriminants.
#[tokio::test]
async fn flag_on_restore_tiers_to_main_does_not_mutate() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_restore",
        &jwt,
        serde_json::json!({ "note_id": env.victim.to_string(), "ts_ms": 1, "tenant_id": "research" }),
    )
    .await;

    assert!(
        !status.is_success(),
        "restore tiers→main ne doit PAS aboutir (got {status})"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).0,
        "live",
        "la note main NE DOIT PAS avoir été restaurée/écrasée"
    );
}

// ── vault_write overwrite guard — sonde d'existence cross-vault (C4-1b, P0) ───────

/// ON : vault_write overwrite tiers→main SANS `expected_sha256` → la garde overwrite ne SONDE
/// PAS `main` (scopée au vault du tenant : note absente de `research` → traitée comme neuve).
/// Résultat 202 (job enqueué pour le vault `research`), PAS 409 (oracle d'existence fermé), et
/// la note-victime de `main` reste intacte (job non traité, aucune écriture synchrone).
/// Discriminant : sans le fix, la garde lit le Vault `main` → victime vivante → 409 overwrite.
#[tokio::test]
async fn flag_on_vault_write_overwrite_tiers_to_main_not_probed() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({
            "title": "pwn probe",
            "body": "# pwn\ncorps",
            "tags": [],
            "tenant_id": "research",
            "note_id": env.victim.to_string()
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "overwrite tiers→main sans sha doit être 202 (garde scopée au vault du tenant), PAS 409 (sonde main)"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).0,
        "live",
        "la note main NE DOIT PAS avoir été touchée (job non traité, zéro écriture synchrone)"
    );
}

/// ON : vault_write overwrite tiers→main AVEC le sha256 CONNU de la victime → la tentative
/// est scopée au vault du tenant (garde ne sonde pas `main`) : 202 (job pour `research`), oracle
/// fermé (indistinguable d'un ULID inconnu), et le CONTENU de la note `main` reste inchangé —
/// aucune écriture synchrone, et le job (non traité) viserait `research`, jamais `main`.
/// Ferme le vecteur Tampering/EoP « écraser une note main si on connaît le sha256 ».
#[tokio::test]
async fn flag_on_vault_write_overwrite_tiers_to_main_with_known_sha_does_not_touch_main() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-research"]);
    seed_research_tenant(&env.index_path);
    let jwt = sign(&env.state, "research");
    let sha = env.victim_sha.clone();

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({
            "title": "pwn overwrite",
            "body": "# pwn\ncontenu malveillant",
            "tags": [],
            "tenant_id": "research",
            "note_id": env.victim.to_string(),
            "expected_sha256": sha
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "overwrite tiers→main avec sha connu : scopé au vault du tenant (202), PAS 409/oracle sur main"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).0,
        "live",
        "statut de la note main inchangé"
    );
    assert_eq!(
        main_victim_body(&env.index_path, &env.victim),
        "main secret corpus",
        "le CONTENU de la note main NE DOIT PAS avoir été écrasé (intégrité Tampering)"
    );
}

// ── Byte-identical OFF : le tenant `main` mute toujours sa propre note ────────────

/// OFF : move de la note du VAULT PROPRE (`main`) aboutit — le gate témoin est transparent
/// (témoin `main` == tenant du Vault), aucun changement de comportement du parc.
#[tokio::test]
async fn flag_off_move_own_main_note_still_succeeds() {
    let env = build_env(false).await;
    // À flag OFF seul `main` est autorisé (middleware legacy) — le consumer joue `main`.
    let jwt = sign(&env.state, "main");

    let status = request(
        build_router(env.state.clone()),
        "POST",
        &format!("/api/v1/notes/{}/move", env.victim),
        &jwt,
        serde_json::json!({ "locus": "knowledge" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "move de la note du vault propre `main` doit aboutir (byte-identical)"
    );
    assert_eq!(
        main_victim_state(&env.index_path, &env.victim).1,
        "knowledge",
        "la note propre `main` DOIT avoir été déplacée vers knowledge"
    );
}
