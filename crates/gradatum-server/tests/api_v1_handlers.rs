//! Tests d'intégration — 10 handlers MCP read (T8).
//!
//! Vérifie pour chaque handler :
//! - **401 UNAUTHORIZED** si pas de header `Authorization` (pas de TrustContext authentifié).
//!
//! Les tests 200/403 sont couverts dans la suite T12 (parity tests) qui câble un vrai preset ACL.
//! T8 vérifie uniquement le routing + auth gate.
//!
//! # Démarrage du serveur de test
//!
//! Un serveur Axum est démarré sur un port aléatoire (bind `127.0.0.1:0`) pour chaque
//! test. Le serveur utilise `AppState::default()` avec ACL vide (default deny) et
//! middleware TrustContext stub.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use reqwest::StatusCode;
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Démarre un serveur Axum de test sur un port éphémère et retourne son adresse.
///
/// Le serveur tourne dans une tâche tokio détachée — il sera arrêté à la fin
/// du processus de test.
async fn start_test_server() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::api_v1;

    // Middleware trust stub identique à main.rs (extraction bearer → BearerToken ou Unauthenticated).
    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        let trust = if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
            if let Ok(val) = auth.to_str() {
                if let Some(token) = val.strip_prefix("Bearer ") {
                    if !token.is_empty() {
                        TrustContext::BearerToken {
                            kid: "test-kid".to_string(),
                            aud: "gradatum".to_string(),
                            sub: token.into(),
                            scopes: vec!["read".to_string()],
                            tenant_id: "main".into(),
                            jti: None,
                        }
                    } else {
                        TrustContext::Unauthenticated
                    }
                } else {
                    TrustContext::Unauthenticated
                }
            } else {
                TrustContext::Unauthenticated
            }
        } else {
            TrustContext::Unauthenticated
        };
        req.extensions_mut().insert(trust);
        next.run(req).await
    }

    let state = AppState::default();
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test arrêté proprement");
    });
    // Laisser le serveur démarrer.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Client reqwest sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Tests 401 unauthenticated ────────────────────────────────────────────────

/// vault_search — POST sans bearer → 401.
#[tokio::test]
async fn vault_search_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_search", addr))
        .json(&serde_json::json!({ "query": "test" }))
        .send()
        .await
        .expect("requête vault_search sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_search sans bearer doit retourner 401"
    );
}

/// vault_read — POST sans bearer → 401.
#[tokio::test]
async fn vault_read_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_read", addr))
        .json(&serde_json::json!({ "path": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_read sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_read sans bearer doit retourner 401"
    );
}

/// vault_list — POST sans bearer → 401.
#[tokio::test]
async fn vault_list_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_list", addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("requête vault_list sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_list sans bearer doit retourner 401"
    );
}

/// vault_status — GET sans bearer → 401.
#[tokio::test]
async fn vault_status_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_status", addr))
        .send()
        .await
        .expect("requête vault_status sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_status sans bearer doit retourner 401"
    );
}

/// vault_graph — POST sans bearer → 401.
#[tokio::test]
async fn vault_graph_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_graph", addr))
        .json(&serde_json::json!({ "root": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_graph sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_graph sans bearer doit retourner 401"
    );
}

/// vault_links — POST sans bearer → 401.
#[tokio::test]
async fn vault_links_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_links", addr))
        .json(&serde_json::json!({ "path": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_links sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_links sans bearer doit retourner 401"
    );
}

/// vault_trace — POST sans bearer → 401.
#[tokio::test]
async fn vault_trace_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_trace", addr))
        .json(&serde_json::json!({ "query": "architecture" }))
        .send()
        .await
        .expect("requête vault_trace sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_trace sans bearer doit retourner 401"
    );
}

/// vault_context — POST sans bearer → 401.
#[tokio::test]
async fn vault_context_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_context", addr))
        .json(&serde_json::json!({ "query": "architecture rust" }))
        .send()
        .await
        .expect("requête vault_context sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_context sans bearer doit retourner 401"
    );
}

/// vault_authors — GET sans bearer → 401.
#[tokio::test]
async fn vault_authors_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_authors", addr))
        .send()
        .await
        .expect("requête vault_authors sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_authors sans bearer doit retourner 401"
    );
}

/// vault_tags — GET sans bearer → 401.
#[tokio::test]
async fn vault_tags_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_tags", addr))
        .send()
        .await
        .expect("requête vault_tags sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_tags sans bearer doit retourner 401"
    );
}

// ── Tests 403 FORBIDDEN (bearer présent mais ACL default deny) ───────────────
// Ces tests vérifient que le bearer stub est bien extrait, mais que l'ACL vide
// retourne FORBIDDEN pour tout consumer inconnu (default deny).

/// vault_search — bearer présent, ACL default deny → 403.
#[tokio::test]
async fn vault_search_403_acl_default_deny() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_search", addr))
        .bearer_auth("test-token-stub")
        .json(&serde_json::json!({ "query": "test" }))
        .send()
        .await
        .expect("requête vault_search avec bearer stub");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_search avec bearer non autorisé doit retourner 403 (ACL default deny)"
    );
}

/// vault_list — bearer présent, ACL default deny → 403.
#[tokio::test]
async fn vault_list_403_acl_default_deny() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_list", addr))
        .bearer_auth("test-token-stub")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("requête vault_list avec bearer stub");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_list avec bearer non autorisé doit retourner 403 (ACL default deny)"
    );
}

// ── F-171 : filtre role_kind / role_status (chemin trait list_notes_filtered) ──
//
// Contrairement aux tests 401/403 ci-dessus (`AppState::default()`, index placeholder, ACL
// vide), ce test câble un **vrai** index SQLite — celui du `Vault` réel — dans `state.search`,
// un preset ACL read+write, et un JWT signé. Le seed passe par la couche vault (`write_note`),
// qui upsert l'index **synchronement** via le MÊME `upsert_note` que le chemin HTTP et peuple
// donc `role_kind`/`role_status` (dérivation F-171). Le listing HTTP filtré traverse
// `IndexStore::list_notes_filtered` en dispatch dynamique sur `Arc<dyn Index>` — la méthode
// inhérente `SqliteIndex::list_notes_filtered` est inatteignable via le trait-objet, donc ce
// test exerce bien le chemin du trait, jamais un court-circuit.

/// Preset ACL : read+write sur `main/*` pour le sujet de test.
const ROLE_FILTER_ACL: &str = r#"
[[consumer]]
identity = "role-filter-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// Démarre un serveur de test dont `state.search` pointe sur l'index réel du vault.
///
/// Rend `(adresse, vault, token, tempdir)`. Le `TempDir` doit rester vivant le temps du test.
async fn start_role_filter_server() -> (SocketAddr, Arc<Vault>, String, TempDir) {
    use axum::{Router, middleware, routing::get};
    use gradatum_acl_policy::AclEngine;
    use gradatum_auth::jwt::TokenScope;
    use gradatum_core::index::Index;
    use gradatum_core::scope::VaultId;
    use gradatum_server::api_v1;

    let dir = TempDir::new().expect("TempDir role filter — doit réussir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );

    let mut state = AppState::new();
    // L'index interrogé par vault_list == celui écrit par les seeds.
    state.search = Arc::clone(vault.index()) as Arc<dyn Index>;
    state.acl = Arc::new(AclEngine::from_preset_str(ROLE_FILTER_ACL).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            "role-filter-tester",
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

/// Écrit une carte `project-map` RÉELLE (`[[kind:…]] [[status:…]]`) via la couche vault et rend
/// son ULID alloué. `write_note` upsert l'index synchronement (peuple `role_kind`/`role_status`).
async fn seed_project_map_card(vault: &Vault, kind: &str, status: &str) -> String {
    seed_project_map_card_with_prose(vault, kind, status, "Corps de carte.").await
}

/// Comme [`seed_project_map_card`], mais ajoute `prose` après les rôles bien formés.
///
/// `prose` peut porter un **token nu** (`FIX` en toutes lettres, cas A/B/C) ou un **lien
/// malformé** (`[[kind:bugfix]]`, cas E) : dans les deux cas la carte ne porte qu'UN `kind`
/// valide (le malformé est écarté par `parse_link` à la validation), donc `write_note`
/// l'accepte et la colonne `role_kind` reste ancrée sur le rôle réel.
async fn seed_project_map_card_with_prose(
    vault: &Vault,
    kind: &str,
    status: &str,
    prose: &str,
) -> String {
    use gradatum_core::frontmatter::Frontmatter;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

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
    let body = format!("[[project:gradatum]] [[status:{status}]] [[kind:{kind}]]\n\n{prose}");
    let note = vault
        .write_note(fm, body)
        .await
        .expect("write_note — seed carte project-map");
    note.id.to_string()
}

/// POST `/api/v1/vault_list` filtré sur `(role_kind, role_status)` en `project-map` ; rend le
/// corps JSON après avoir exigé un 200.
async fn list_roles(addr: SocketAddr, token: &str, kind: &str, status: &str) -> serde_json::Value {
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_list"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "section": "project-map",
            "role_kind": kind,
            "role_status": status,
        }))
        .send()
        .await
        .expect("requête vault_list filtrée par rôles");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_list filtré doit répondre 200"
    );
    resp.json().await.expect("corps JSON vault_list")
}

/// Ensemble des ULID (dernier segment de `section/<ULID>`) des entrées paginées d'une réponse
/// `vault_list`.
fn entry_ids(corps: &serde_json::Value) -> std::collections::BTreeSet<String> {
    corps["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["path"].as_str())
        .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
        .collect()
}

/// `vault_list` filtré sur `(role_kind, role_status)` ne rend que la carte au rôle exact,
/// via le chemin du trait `list_notes_filtered`.
#[tokio::test]
async fn vault_list_filters_on_roles() {
    let (addr, vault, token, _dir) = start_role_filter_server().await;
    let fix_id = seed_project_map_card(&vault, "FIX", "OPEN").await;
    let _feature_id = seed_project_map_card(&vault, "FEATURE", "OPEN").await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_list"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "section": "project-map",
            "role_kind": "FIX",
            "role_status": "OPEN",
        }))
        .send()
        .await
        .expect("requête vault_list filtrée par rôles");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_list filtré doit répondre 200"
    );
    let corps: serde_json::Value = resp.json().await.expect("corps JSON vault_list");
    assert_eq!(
        corps["total"], 1,
        "seule la carte FIX/OPEN doit être comptée (FEATURE/OPEN exclue par role_kind)"
    );
    // Cardinalité de l'ENSEMBLE paginé : `total` vient de la requête de comptage, `entries`
    // de la requête paginée. Sans cette assertion, une désynchronisation du prédicat paginé
    // vis-à-vis du comptage (ex : filtre de rôle neutralisé côté pagination seule) passerait
    // en silence — les deux cartes remonteraient dans `entries` alors que `total` resterait 1.
    assert_eq!(
        corps["entries"].as_array().map(|a| a.len()),
        Some(1),
        "l'unique entrée paginée — pas seulement le compte"
    );
    let path = corps["entries"][0]["path"]
        .as_str()
        .expect("entries[0].path présent");
    assert!(
        path.ends_with(&fix_id),
        "l'unique entrée doit être la carte FIX seedée (path={path}, fix_id={fix_id})"
    );
}

// ── F-171 Task 4 : Layer B (oracle par identifiant, E2E HTTP) + gardes croisées ──
//
// Layer B sème par le VRAI chemin d'écriture (`vault.write_note`, upsert synchrone) et
// vérifie le filtre à travers le VRAI chemin HTTP `vault_list`. Assertions **par
// identifiant** (ensemble exact des ULID alloués par le vault), jamais par cardinal seul —
// et cardinalité de la page vérifiée EN PLUS du `total` (leçon Task 3 : un `total` correct
// avec une page divergente a laissé passer un mutant). Les ULID sont alloués par le vault :
// impossibles à coder en dur (les identifiants illustratifs du plan, `01TESTFIX…`, ne sont
// ni des ULID valides ni imposables) — on assère sur les ids RETOURNÉS par le seed.

/// Le filtre `(role_kind, role_status)` rend l'ENSEMBLE EXACT des cartes au rôle
/// ancré, prose exclue ; et la carte à prose adverse retombe bien dans son vrai bucket.
#[tokio::test]
async fn filter_returns_exact_ids_prose_excluded() {
    let (addr, vault, token, _dir) = start_role_filter_server().await;

    // Bucket cible FIX/OPEN : deux cartes, assertion PAR IDENTIFIANT.
    let fix1 = seed_project_map_card(&vault, "FIX", "OPEN").await;
    let fix2 = seed_project_map_card(&vault, "FIX", "OPEN").await;
    let attendus: std::collections::BTreeSet<String> = [fix1, fix2].into_iter().collect();

    // Cas A : FEATURE dont la PROSE dit « FIX » — ne doit JAMAIS tomber dans le bucket FIX.
    let prose_a = seed_project_map_card_with_prose(
        &vault,
        "FEATURE",
        "OPEN",
        "Cette carte parle d'un FIX mais n'en est pas un.",
    )
    .await;
    // Cas H : jumelle propre FEATURE/OPEN — garde anti-vacuité du bucket FEATURE.
    let _h = seed_project_map_card(&vault, "FEATURE", "OPEN").await;

    // FIX/OPEN : exactement les deux cartes FIX semées.
    let fix_open = list_roles(addr, &token, "FIX", "OPEN").await;
    let ids = entry_ids(&fix_open);
    assert_eq!(
        ids, attendus,
        "exactement les IDs FIX/OPEN semés (ensemble, pas seulement le cardinal)"
    );
    assert!(
        !ids.contains(&prose_a),
        "cas A : la prose « FIX » sur une carte FEATURE ne compte pas"
    );
    assert_eq!(fix_open["total"], 2, "total FIX/OPEN == 2");
    // Cardinalité de la PAGE en plus du total (mutation « filtre neutralisé côté pagination »).
    assert_eq!(
        fix_open["entries"].as_array().map(|a| a.len()),
        Some(2),
        "la page paginée porte exactement 2 entrées"
    );

    // Contrôle négatif comportemental : la carte A EST dans le bucket FEATURE.
    let feat_open = list_roles(addr, &token, "FEATURE", "OPEN").await;
    let feat_ids = entry_ids(&feat_open);
    assert!(
        feat_ids.contains(&prose_a),
        "la carte A est FEATURE, pas FIX — filtre ancré, pas sous-chaîne"
    );
    assert!(
        !feat_ids.is_empty(),
        "garde anti-vacuité (H) : le harnais compte bien quelque chose"
    );
}

/// Cas E par le chemin HTTP réel : une carte FEATURE portant EN PLUS un
/// `[[kind:bugfix]]` malformé est acceptée à l'écriture, listée sous FEATURE, et ne crée aucun
/// bucket fantôme `bugfix`.
#[tokio::test]
async fn filter_feature_includes_card_with_malformed_kind_link() {
    let (addr, vault, token, _dir) = start_role_filter_server().await;

    let e_id = seed_project_map_card_with_prose(
        &vault,
        "FEATURE",
        "OPEN",
        "[[kind:bugfix]] — rôle malformé, écarté par la taxonomie.",
    )
    .await;

    // FEATURE/OPEN contient la carte E.
    let feat = list_roles(addr, &token, "FEATURE", "OPEN").await;
    assert!(
        entry_ids(&feat).contains(&e_id),
        "la carte E (malformé écarté) est listée sous FEATURE"
    );

    // Aucun bucket « bugfix » : le malformé n'a pas créé de rôle fantôme.
    let bug = list_roles(addr, &token, "bugfix", "OPEN").await;
    assert_eq!(bug["total"], 0, "aucun rôle fantôme « bugfix »");
    assert!(
        entry_ids(&bug).is_empty(),
        "le bucket bugfix est vide (page)"
    );
}

/// Preuve côté serveur que le VRAI chemin d'écriture REFUSE la corruption non
/// représentable (F rôle double, G aucun rôle, D rôle dans un bloc de code). Utilise le scanner
/// du chemin d'écriture (`gradatum_curator::wikilinks::extract_wikilinks`, aveugle aux fences)
/// puis le validateur de schéma. Confirme : F/G/D = corruption absente de la production.
#[test]
fn write_path_validator_rejects_uncardinal_role_bodies() {
    let corrupt = [
        "[[project:gradatum]] [[status:OPEN]] [[kind:FIX]] [[kind:ENHANCEMENT]]", // F : 2 kinds
        "[[project:gradatum]] [[status:OPEN]]",                                   // G : 0 kind
        "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]]\n```\n[[kind:FIX]]\n```", // D : fence
    ];
    for body in corrupt {
        let targets = gradatum_curator::wikilinks::extract_wikilinks(body);
        assert!(
            gradatum_core::project_map::validate_links_from_targets(&targets).is_err(),
            "le validateur d'écriture refuse ce corps (corruption absente en prod) : {body}"
        );
    }
}

/// Accord entre les DEUX scanners `[[…]]` pré-existants sur toute carte VALIDE :
/// core (`extract_wikilink_targets` via `roles_of_body`) et curator (`extract_wikilinks`), tous
/// deux alimentant `parse_link`. Unique garde de la dette core↔curator (surveillée
/// ici) : le jour où ils divergeraient sur une carte valide, ce test tombe.
#[test]
fn core_and_curator_scanners_agree_on_valid_cards() {
    use gradatum_core::project_map::{ProjectMapLink, parse_link, roles_of_body};

    let valid = [
        "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]]\n\nprose",
        "[[project:gradatum]] [[status:DONE]] [[kind:FIX]]",
        "[[project:gradatum]] [[status:BLOCKED]] [[kind:TASK]]",
        "[[project:gradatum]] [[status:IN_PROGRESS]] [[kind:ENHANCEMENT]]",
    ];
    for body in valid {
        let core = roles_of_body(body);
        // Reproduit la sémantique « première occurrence gagne » de roles_of_body, mais sur le
        // scanner CURATOR — pour prouver l'accord des deux extracteurs sur une carte valide.
        let mut cur_kind: Option<&str> = None;
        let mut cur_status: Option<&str> = None;
        for t in gradatum_curator::wikilinks::extract_wikilinks(body) {
            match parse_link(&t) {
                Ok(ProjectMapLink::Kind(k)) if cur_kind.is_none() => cur_kind = Some(k.as_wire()),
                Ok(ProjectMapLink::Status(s)) if cur_status.is_none() => {
                    cur_status = Some(s.as_wire());
                }
                _ => {}
            }
        }
        assert_eq!(core.kind, cur_kind, "accord core/curator sur kind — {body}");
        assert_eq!(
            core.status, cur_status,
            "accord core/curator sur status — {body}"
        );
    }
}
