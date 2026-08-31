//! F-215 / F-216 — robustesse des formes de référence de note et bornage de `vault_tags`.
//!
//! ## F-215 — `vault_links` / `vault_graph` acceptent `ULID` nu ET `section/ULID`
//!
//! `note_links` porte des ULID **nus**, mais le reste de l'API expose `section/ULID`
//! (`vault_read` l'accepte, `vault_search` le renvoie). Avant le fix, passer la forme
//! préfixée à `vault_links`/`vault_graph` rendait `edges: []` **en silence** — un
//! appelant enchaînant recherche→exploration concluait à tort « pas de liens ». Le fix
//! résout la référence exactement comme `vault_read` (ULID / titre / slug) : les deux
//! formes d'une référence **résoluble** rendent le même graphe. Une référence
//! **inconnue** reste un 200 + graphe vide (contrat v1, couvert par `v1-parity-tests`).
//!
//! ## F-216 — `vault_tags` renvoie une réponse bornée par défaut
//!
//! Sans borne, `vault_tags` renvoyait la liste complète (~135 Ko observés). La réponse
//! par défaut est désormais plafonnée (`DEFAULT_TAGS_LIMIT`, tri fréquence décroissante),
//! `total` expose le cardinal complet, et `?limit=` lève la borne.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use http_body_util::BodyExt;
use tower::ServiceExt;

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, seed_backlink_to, sign_token};

/// POST JSON authentifié → (statut, corps décodé si 200).
async fn post_json(
    app: axum::Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, Option<serde_json::Value>) {
    let req = Request::builder()
        .uri(uri)
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("serialize body"),
        ))
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    if status != StatusCode::OK {
        return (status, None);
    }
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (
        status,
        Some(serde_json::from_slice(&bytes).expect("decode JSON")),
    )
}

/// GET authentifié → (statut, corps décodé si 200).
async fn get_json(
    app: axum::Router,
    token: &str,
    uri: &str,
) -> (StatusCode, Option<serde_json::Value>) {
    let req = Request::builder()
        .uri(uri)
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    if status != StatusCode::OK {
        return (status, None);
    }
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (
        status,
        Some(serde_json::from_slice(&bytes).expect("decode JSON")),
    )
}

/// Ensemble trié des arêtes `(from,to,kind)` d'une réponse graphe, pour comparaison
/// indépendante de l'ordre.
fn edge_set(resp: &serde_json::Value) -> Vec<(String, String, String)> {
    let mut edges: Vec<(String, String, String)> = resp["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .map(|e| {
            (
                e["from"].as_str().unwrap_or_default().to_string(),
                e["to"].as_str().unwrap_or_default().to_string(),
                e["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    edges.sort();
    edges
}

// ── F-215 ───────────────────────────────────────────────────────────────────────

/// `vault_links` : la forme préfixée `section/ULID` rend le MÊME résultat non vide que
/// l'ULID nu. Échoue si l'une des deux rend un ensemble d'arêtes vide (le bug F-215).
#[tokio::test]
async fn vault_links_prefixed_and_bare_forms_are_equivalent() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Note cible (section `reference`) + une source qui la référence (backlink).
    let target = env
        .write_note_in_section("reference", "Cible F215 links", "corps cible")
        .await;
    let bare = target.to_string();
    let prefixed = format!("reference/{bare}");
    seed_backlink_to(&env, &bare).await;

    let (s_bare, bare_body) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_links",
        serde_json::json!({ "path": bare }),
    )
    .await;
    let (s_pref, pref_body) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_links",
        serde_json::json!({ "path": prefixed }),
    )
    .await;

    assert_eq!(s_bare, StatusCode::OK, "forme nue → 200");
    assert_eq!(s_pref, StatusCode::OK, "forme préfixée → 200");
    let bare_edges = edge_set(&bare_body.expect("corps nu"));
    let pref_edges = edge_set(&pref_body.expect("corps préfixé"));

    assert!(
        !bare_edges.is_empty(),
        "la forme nue doit rendre au moins l'arête du backlink"
    );
    assert!(
        !pref_edges.is_empty(),
        "F-215 : la forme préfixée NE doit PLUS rendre un ensemble vide muet"
    );
    assert_eq!(
        bare_edges, pref_edges,
        "F-215 : les deux formes doivent rendre exactement le même graphe"
    );
}

/// `vault_graph` : même équivalence des deux formes (paramètre `root`).
#[tokio::test]
async fn vault_graph_prefixed_and_bare_forms_are_equivalent() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let target = env
        .write_note_in_section("reference", "Cible F215 graph", "corps cible")
        .await;
    let bare = target.to_string();
    let prefixed = format!("reference/{bare}");
    seed_backlink_to(&env, &bare).await;

    let body = |root: &str| serde_json::json!({ "root": root, "include_backlinks": true });
    let (s_bare, bare_body) =
        post_json(env.app.clone(), &token, "/api/v1/vault_graph", body(&bare)).await;
    let (s_pref, pref_body) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_graph",
        body(&prefixed),
    )
    .await;

    assert_eq!(s_bare, StatusCode::OK);
    assert_eq!(s_pref, StatusCode::OK);
    let bare_edges = edge_set(&bare_body.expect("corps nu"));
    let pref_edges = edge_set(&pref_body.expect("corps préfixé"));

    assert!(!bare_edges.is_empty(), "forme nue non vide");
    assert!(
        !pref_edges.is_empty(),
        "F-215 : forme préfixée non vide (plus de graphe muet)"
    );
    assert_eq!(bare_edges, pref_edges, "F-215 : graphes identiques");
}

// ── F-216 ───────────────────────────────────────────────────────────────────────

/// Nombre de tags distincts semés dans une seule note dérivée, choisi strictement au-delà
/// de la borne serveur par défaut (50) pour prouver le plafonnement.
const SEEDED_TAGS: usize = 60;

/// Sème `SEEDED_TAGS` tags distincts dans le vault `main` via une seule note portant
/// tous les tags dans son frontmatter (`distinct_tags` éclate `notes.tags` par espaces).
async fn seed_many_tags(env: &helpers::TestEnv) {
    let tags = (0..SEEDED_TAGS)
        .map(|i| Tag::new(format!("f216tag{i:03}")).expect("tag de test valide"))
        .collect();
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Feedback,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags,
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    env._vault_typed
        .write_note_with_id(frontmatter, "# F216\n\nCorps.".to_string(), NoteId::new())
        .await
        .expect("seed_many_tags: write_note_with_id — invariant test");
}

/// La réponse par défaut de `vault_tags` est BORNÉE : elle ne dépasse jamais la borne
/// serveur, même si le vault contient bien plus de tags — et `total` révèle le reste.
/// Échoue si la réponse par défaut renvoie tout (le bug F-216).
#[tokio::test]
async fn vault_tags_default_response_is_bounded() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    seed_many_tags(&env).await;

    let (status, body) = get_json(env.app.clone(), &token, "/api/v1/vault_tags").await;
    assert_eq!(status, StatusCode::OK);
    let body = body.expect("corps vault_tags");

    let returned = body["tags"].as_array().expect("tags array").len();
    let total = body["total"].as_u64().expect("total u64");

    assert!(
        total >= SEEDED_TAGS as u64,
        "total doit refléter le cardinal complet ({total} < {SEEDED_TAGS})"
    );
    // Borne serveur = DEFAULT_TAGS_LIMIT (50) < SEEDED_TAGS (60) : la réponse DOIT tronquer.
    assert!(
        returned < SEEDED_TAGS,
        "F-216 : la réponse par défaut ({returned}) NE doit PAS renvoyer tous les {SEEDED_TAGS} tags"
    );
    assert!(
        (total as usize) > returned,
        "total ({total}) > tags renvoyés ({returned}) : la troncature est détectable"
    );
}

/// La borne se lève sur demande explicite : `?limit=` élevé renvoie la liste complète.
#[tokio::test]
async fn vault_tags_limit_lifts_the_bound() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    seed_many_tags(&env).await;

    let (status, body) = get_json(env.app.clone(), &token, "/api/v1/vault_tags?limit=1000").await;
    assert_eq!(status, StatusCode::OK);
    let body = body.expect("corps vault_tags");

    let returned = body["tags"].as_array().expect("tags array").len();
    let total = body["total"].as_u64().expect("total u64") as usize;

    assert_eq!(
        returned, total,
        "avec un limit élevé, la réponse rend l'intégralité des tags"
    );
    assert!(
        returned >= SEEDED_TAGS,
        "au moins les {SEEDED_TAGS} tags semés"
    );
}

// ── F-215 critère 4 — le reste de la famille ────────────────────────────────────
//
// `resolve_note_ref` n'avait que deux appelants (`vault_graph`, `vault_links`). Les
// quatre outils du sous-système d'historique CoW partagent le champ `note_id` et le
// passaient BRUT à la couche Vault, dont `parse_note_id` rend un
// `GradatumError::Storage("invalid ULID …")` → **-32603 / 500 opaque** : un refus
// d'entrée déguisé en panne interne, classé erreur de *stockage*.
//
// Deux issues, tranchées par outil (cf. les rustdoc `logic::*_impl`) :
//   - PARITÉ  — `vault_history`, `vault_history_get`, `vault_restore`, `vault_diff` ;
//   - REFUS   — `vault_classify`, `vault_downgrade`, `vault_write` (déjà 400 nommés :
//               le message cite désormais la valeur reçue et la forme attendue).
//
// Les tests de refus appellent `logic::*_impl` DIRECTEMENT : la couche HTTP ne rend
// qu'un `StatusCode` (corps vide), le message n'atteint le client que par la couche MCP
// (`mcp::gradatum_error_to_mcp` : `InvalidInput`/`Validation` → -32602 **avec** message,
// `Storage` → -32603 « internal error » **sans**). Assertion sur l'erreur typée =
// assertion sur ce que les deux transports rendront.

/// Preset ACL read **+ write** sur `main` — les surfaces mutantes (`vault_restore`,
/// `vault_downgrade`, `vault_write`) sont refusées par `helpers::TEST_ACL`
/// (`write_patterns = []`) avant même d'atteindre la résolution de référence.
const RW_ACL: &str = r#"
[[consumer]]
identity = "f215-rw"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// `TrustContext` porteur des scopes read+write sur le tenant `main`.
fn trust_rw() -> gradatum_core::trust::TrustContext {
    gradatum_core::trust::TrustContext::BearerToken {
        kid: "f215-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "f215-rw".into(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".into(),
        jti: None,
    }
}

/// Remplace l'ACL de `env.state` par [`RW_ACL`]. N'affecte QUE les appels directs à
/// `logic::*_impl` — `env.app` porte un clone antérieur de l'état.
fn grant_write(env: &mut helpers::TestEnv) {
    env.state.acl = std::sync::Arc::new(
        gradatum_acl_policy::AclEngine::from_preset_str(RW_ACL).expect("preset ACL rw valide"),
    );
}

/// Sème une note puis la réécrit sous le **même** identifiant : la seconde écriture
/// déclenche le copy-on-write, donc crée exactement un instantané d'historique.
///
/// Rend `(ULID nu, forme préfixée `section/ULID`)`.
async fn seed_note_with_snapshot(
    env: &helpers::TestEnv,
    section: &str,
    title: &str,
) -> (String, String) {
    let id = env.write_note_in_section(section, title, "corps v1").await;
    let v1 = env
        ._vault_typed
        .read_note(id)
        .await
        .expect("relecture de la version 1");
    env._vault_typed
        .write_note_with_id(v1.frontmatter, format!("# {title}\ncorps v2"), id)
        .await
        .expect("réécriture v2 — déclenche le CoW");
    let bare = id.to_string();
    let prefixed = format!("{section}/{bare}");
    (bare, prefixed)
}

/// Référence qui ne résout vers AUCUNE note : ni ULID (dernier segment non-Crockford),
/// ni titre semé, ni slug de redirection.
const UNRESOLVABLE_REF: &str = "reference/pas-du-tout-un-ulid";

/// Vérifie qu'une erreur est bien un refus d'entrée **nommé** citant la valeur reçue —
/// et surtout PAS un `Storage` (qui deviendrait un -32603 muet).
fn assert_named_input_refusal(err: &gradatum_core::error::GradatumError, quoted_value: &str) {
    use gradatum_core::error::GradatumError;
    let msg = match err {
        GradatumError::InvalidInput(m) => m.clone(),
        GradatumError::Validation(v) => v.to_string(),
        other => panic!(
            "F-215 critère 4 : refus attendu typé InvalidInput/Validation, obtenu {other:?} \
             (un Storage devient un -32603 « internal error » muet côté MCP)"
        ),
    };
    assert!(
        msg.contains(quoted_value),
        "le message doit citer la valeur reçue ({quoted_value}) — obtenu : {msg}"
    );
    assert!(
        msg.contains("ULID"),
        "le message doit énoncer la forme attendue — obtenu : {msg}"
    );
}

/// `vault_history` : la forme préfixée rend le MÊME historique non vide que l'ULID nu.
/// Échoue si la forme préfixée retombe sur le 500 opaque d'avant le fix.
#[tokio::test]
async fn vault_history_accepts_prefixed_reference_at_parity_with_bare() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let (bare, prefixed) = seed_note_with_snapshot(&env, "reference", "F215c4 history").await;

    let (s_bare, b_bare) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history",
        serde_json::json!({ "note_id": bare }),
    )
    .await;
    let (s_pref, b_pref) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history",
        serde_json::json!({ "note_id": prefixed }),
    )
    .await;

    assert_eq!(s_bare, StatusCode::OK, "forme nue → 200");
    assert_eq!(
        s_pref,
        StatusCode::OK,
        "F-215 critère 4 : la forme préfixée NE doit plus rendre 500"
    );
    let b_bare = b_bare.expect("corps nu");
    assert!(
        b_bare["count"].as_u64().expect("count") >= 1,
        "le CoW semé doit produire au moins un instantané"
    );
    assert_eq!(
        b_bare,
        b_pref.expect("corps préfixé"),
        "historiques identiques"
    );
}

/// `vault_history_get` : les deux formes rendent le même instantané, et la réponse
/// écho l'identifiant **résolu** (forme canonique nue), pas la référence saisie.
#[tokio::test]
async fn vault_history_get_accepts_prefixed_reference_at_parity_with_bare() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let (bare, prefixed) = seed_note_with_snapshot(&env, "reference", "F215c4 history_get").await;

    let (_, hist) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history",
        serde_json::json!({ "note_id": bare }),
    )
    .await;
    let ts = hist.expect("corps history")["versions"][0]
        .as_i64()
        .expect("un instantané au moins");

    let (s_bare, b_bare) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history_get",
        serde_json::json!({ "note_id": bare, "ts_ms": ts }),
    )
    .await;
    let (s_pref, b_pref) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history_get",
        serde_json::json!({ "note_id": prefixed, "ts_ms": ts }),
    )
    .await;

    assert_eq!(s_bare, StatusCode::OK, "forme nue → 200");
    assert_eq!(
        s_pref,
        StatusCode::OK,
        "F-215 critère 4 : la forme préfixée NE doit plus rendre 500"
    );
    let b_pref = b_pref.expect("corps préfixé");
    assert_eq!(b_bare.expect("corps nu"), b_pref, "instantanés identiques");
    assert_eq!(
        b_pref["note_id"].as_str().expect("note_id"),
        bare,
        "la réponse écho l'identifiant RÉSOLU, pas la référence préfixée saisie"
    );
}

/// `vault_diff` : les deux formes rendent le même diff non vide.
#[tokio::test]
async fn vault_diff_accepts_prefixed_reference_at_parity_with_bare() {
    let env = build_app().await;
    let token = sign_token(&env.state);
    let (bare, prefixed) = seed_note_with_snapshot(&env, "reference", "F215c4 diff").await;

    let (_, hist) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_history",
        serde_json::json!({ "note_id": bare }),
    )
    .await;
    let ts = hist.expect("corps history")["versions"][0]
        .as_i64()
        .expect("un instantané au moins");

    let body = |r: &str| serde_json::json!({ "note_id": r, "a": ts.to_string(), "b": "current" });
    let (s_bare, b_bare) =
        post_json(env.app.clone(), &token, "/api/v1/vault_diff", body(&bare)).await;
    let (s_pref, b_pref) = post_json(
        env.app.clone(),
        &token,
        "/api/v1/vault_diff",
        body(&prefixed),
    )
    .await;

    assert_eq!(s_bare, StatusCode::OK, "forme nue → 200");
    assert_eq!(
        s_pref,
        StatusCode::OK,
        "F-215 critère 4 : la forme préfixée NE doit plus rendre 500"
    );
    let b_bare = b_bare.expect("corps nu");
    assert!(
        b_bare["count"].as_u64().expect("count") >= 1,
        "le diff v1↔v2 ne peut pas être vide"
    );
    assert_eq!(b_bare, b_pref.expect("corps préfixé"), "diffs identiques");
}

/// `vault_restore` (surface MUTANTE) : la forme préfixée est acceptée et la réponse écho
/// l'ULID résolu. Un seul sens testé — la restauration déclenche elle-même un CoW, donc
/// rejouer la forme nue ensuite comparerait deux états différents.
#[tokio::test]
async fn vault_restore_accepts_prefixed_reference_and_echoes_resolved_ulid() {
    use gradatum_server::api_v1::logic;

    let mut env = build_app().await;
    let (bare, prefixed) = seed_note_with_snapshot(&env, "reference", "F215c4 restore").await;
    let ts = env
        ._vault_typed
        .history_versions(
            bare.parse::<ulid::Ulid>()
                .map(gradatum_core::identity::NoteId)
                .expect("ULID semé"),
        )
        .await
        .expect("history_versions")
        .first()
        .copied()
        .expect("un instantané au moins");
    grant_write(&mut env);

    let resp = logic::vault_restore_impl(
        &env.state,
        &trust_rw(),
        gradatum_dto::VaultRestoreRequest::new(prefixed, ts),
    )
    .await
    .expect("F-215 critère 4 : la forme préfixée NE doit plus rendre une erreur de stockage");

    assert_eq!(
        resp.note_id, bare,
        "la réponse écho l'identifiant RÉSOLU, pas la référence préfixée saisie"
    );
}

/// Les QUATRE outils du sous-système CoW refusent une référence irrésoluble par une
/// erreur d'entrée **nommée** citant la valeur — jamais par un `Storage` (→ -32603 muet).
#[tokio::test]
async fn history_family_rejects_unresolvable_reference_with_named_input_error() {
    use gradatum_server::api_v1::logic;

    let mut env = build_app().await;
    grant_write(&mut env);
    let trust = trust_rw();

    let e = logic::vault_history_impl(
        &env.state,
        &trust,
        gradatum_dto::VaultHistoryRequest::new(UNRESOLVABLE_REF.to_string()),
    )
    .await
    .expect_err("vault_history doit refuser une référence irrésoluble");
    assert_named_input_refusal(&e, UNRESOLVABLE_REF);

    let e = logic::vault_history_get_impl(
        &env.state,
        &trust,
        gradatum_dto::VaultHistoryGetRequest::new(UNRESOLVABLE_REF.to_string(), 1),
    )
    .await
    .expect_err("vault_history_get doit refuser une référence irrésoluble");
    assert_named_input_refusal(&e, UNRESOLVABLE_REF);

    let e = logic::vault_restore_impl(
        &env.state,
        &trust,
        gradatum_dto::VaultRestoreRequest::new(UNRESOLVABLE_REF.to_string(), 1),
    )
    .await
    .expect_err("vault_restore doit refuser une référence irrésoluble");
    assert_named_input_refusal(&e, UNRESOLVABLE_REF);

    let e = logic::vault_diff_impl(
        &env.state,
        &trust,
        gradatum_dto::VaultDiffRequest::new(
            UNRESOLVABLE_REF.to_string(),
            "current".to_string(),
            "current".to_string(),
        ),
    )
    .await
    .expect_err("vault_diff doit refuser une référence irrésoluble");
    assert_named_input_refusal(&e, UNRESOLVABLE_REF);
}

/// `vault_classify` — issue RETENUE : refus explicite (poignée de maintenance, pas un
/// `path` de recherche). Le refus doit citer la valeur reçue et la forme attendue.
#[tokio::test]
async fn vault_classify_rejects_prefixed_reference_with_named_input_error() {
    use gradatum_server::api_v1::logic;

    let mut env = build_app().await;
    let id = env
        .write_note_in_section("reference", "F215c4 classify", "corps")
        .await;
    // `RW_ACL` porte l'identité `f215-rw` de `trust_rw()` ; `helpers::TEST_ACL` ne connaît
    // que `alpha13-tester` → l'ACL Read refuserait avant d'atteindre la validation.
    grant_write(&mut env);
    let prefixed = format!("reference/{id}");

    let e = logic::vault_classify_impl(
        &env.state,
        &trust_rw(),
        gradatum_dto::VaultClassifyRequest::new(prefixed.clone()),
    )
    .await
    .expect_err("vault_classify doit refuser explicitement la forme préfixée");
    assert_named_input_refusal(&e, &prefixed);
}

/// `vault_downgrade` — issue RETENUE : refus explicite (cible d'une mutation, jamais
/// résolue par titre ou slug). Couvre les DEUX champs d'identifiant du DTO.
#[tokio::test]
async fn vault_downgrade_rejects_prefixed_reference_with_named_input_error() {
    use gradatum_server::api_v1::logic;

    let mut env = build_app().await;
    let id = env
        .write_note_in_section("reference", "F215c4 downgrade", "corps")
        .await;
    grant_write(&mut env);
    let prefixed = format!("reference/{id}");

    let e = logic::vault_downgrade_impl(
        &env.state,
        &trust_rw(),
        gradatum_dto::VaultDowngradeRequest::new(prefixed.clone(), "obsolete".to_string()),
    )
    .await
    .expect_err("vault_downgrade doit refuser explicitement la forme préfixée (note_id)");
    assert_named_input_refusal(&e, &prefixed);

    let mut req = gradatum_dto::VaultDowngradeRequest::new(id.to_string(), "obsolete".to_string());
    req.replaced_by = Some(prefixed.clone());
    let e = logic::vault_downgrade_impl(&env.state, &trust_rw(), req)
        .await
        .expect_err("vault_downgrade doit refuser explicitement la forme préfixée (replaced_by)");
    assert_named_input_refusal(&e, &prefixed);
}

/// `vault_write` — issue RETENUE : refus explicite. Ce `note_id` est un ULID
/// **pré-alloué** honoré tel quel ; y résoudre un titre écraserait une note homonyme.
#[tokio::test]
async fn vault_write_rejects_prefixed_note_id_with_named_input_error() {
    use gradatum_server::api_v1::logic;

    let mut env = build_app().await;
    let id = env
        .write_note_in_section("reference", "F215c4 write", "corps")
        .await;
    grant_write(&mut env);
    let prefixed = format!("reference/{id}");

    let mut req = gradatum_dto::VaultWriteRequest::new(
        "F215c4 write".to_string(),
        "# F215c4 write\ncorps".to_string(),
    );
    req.note_id = Some(prefixed.clone());

    let e = logic::vault_write_impl(
        &env.state,
        &trust_rw(),
        req,
        "f215c4-req",
        logic::FeatureWriteAuthority::External,
    )
    .await
    .expect_err("vault_write doit refuser explicitement la forme préfixée");
    assert_named_input_refusal(&e, &prefixed);
}
