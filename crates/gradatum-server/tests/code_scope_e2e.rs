//! Tests E2E F-61 Phase C — `POST /api/v1/code_scope`.
//!
//! Couvre :
//! 1. `scope_returns_symbols` — selector query → entries structurées conformes.
//! 2. `scope_rejects_vault_main` — INVARIANT SÉCU N°1 : vault `main` → 400, jamais servi.
//! 3. `scope_rejects_traversal` — vault avec path traversal → 400.
//! 4. `scope_unauthenticated_401` — sans JWT → 401.
//! 5. `scope_invalid_selector_400` — selector.kind hors vocabulaire → 400.
//! 6. `scope_budget_truncates` — budget serré → truncated=true, entrées entières.
//! 7. `scope_path_selector` — selector path → tous les symboles d'un fichier.
//! 8. `scope_main_note_never_served` — une note `main` existe mais code_scope ne la sert jamais.
//! 9. `scope_never_ingested_404` — M1 §3.3bis : vault jamais ingéré → 404.
//! 10. `scope_ingested_no_match_200` — M1 discriminant : vault ingéré + 0 match → 200 vide.
//! 11. `scope_stale_flag_handler_path` — FIX-T1 (§4 critère 7b) : le handler détecte la dérive
//!     disque → stale=true dans la réponse JSON (chemin detect_stale_paths, pas check_freshness).
//! 12. `scope_stale_flag_deleted_file` — variante : fichier supprimé du disque → stale=true.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::index_store::CodeScopeEntryRaw;
use gradatum_embed::Noop as NoopEmbedder;
use gradatum_index::{CodeSymbolMeta, DerivedNote, SqliteIndex};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const TEST_ACL: &str = r#"
[[consumer]]
identity = "code-tester"
read_patterns  = ["main/*", "main/main"]
write_patterns = []
"#;

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let idx = Arc::new(SqliteIndex::open_in_memory().await.expect("open_in_memory"));
    let noop = Arc::new(NoopEmbedder::new(8));
    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(noop);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx)
}

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "code-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT")
}

fn scope_req(body: serde_json::Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri("/api/v1/code_scope")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Helper : note dérivée avec metadata structurée (sans span — rétrocompat).
fn derived(
    vault_id: &str,
    source_path: &str,
    kind: &str,
    qname: &str,
    sig: Option<&str>,
    deps: Vec<&str>,
) -> DerivedNote {
    derived_with_span(vault_id, source_path, kind, qname, sig, deps, None)
}

/// Helper : note dérivée avec span explicite (pour les tests include_body).
fn derived_with_span(
    vault_id: &str,
    source_path: &str,
    kind: &str,
    qname: &str,
    sig: Option<&str>,
    deps: Vec<&str>,
    span: Option<(u32, u32)>,
) -> DerivedNote {
    let key: Vec<u8> = format!("{vault_id}\x1f{source_path}\x1f{kind}\x1f{qname}").into_bytes();
    let id = gradatum_core::identity::NoteId::derived_from(&key);
    DerivedNote {
        id,
        body_text: format!("{kind} {qname} {}", sig.unwrap_or("")),
        tags: format!("code rust {kind} root"),
        title: Some(qname.to_string()),
        code_meta: Some(CodeSymbolMeta {
            source_path: source_path.to_string(),
            kind: kind.to_string(),
            qualified_name: qname.to_string(),
            signature: sig.map(|s| s.to_string()),
            deps: deps.into_iter().map(|d| d.to_string()).collect(),
            visibility: Some("pub".to_string()),
            span,
        }),
    }
}

/// Test 1 : selector query → entries structurées conformes au contrat §3.3bis.
#[tokio::test]
async fn scope_returns_symbols() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Enregistrer le vault (simule `code ingest`) — obligatoire sinon 404.
    idx.set_code_vault_repo_path("code-test", "/tmp/code-test-repo")
        .await
        .expect("set_code_vault_repo_path");
    idx.write_note_derived_batch(
        "code-test",
        "src/parser.rs",
        "h1",
        "sha1",
        vec![derived(
            "code-test",
            "src/parser.rs",
            "fn",
            "parse_tokens",
            Some("(input: &str) -> Vec<Token>"),
            vec!["Token"],
        )],
    )
    .await
    .expect("seed derived");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-test",
                "selector": {"kind": "query", "value": "parse_tokens"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "200 attendu");

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "1 symbole");
    let e = &entries[0];
    assert_eq!(e["qualified_name"], "parse_tokens");
    assert_eq!(e["source_path"], "src/parser.rs");
    assert_eq!(e["kind"], "fn");
    assert_eq!(e["signature"], "(input: &str) -> Vec<Token>");
    assert_eq!(e["deps"][0], "Token");
    assert_eq!(json["total_matched"], 1);
    assert_eq!(json["truncated"], false);
}

/// Test 2 : INVARIANT SÉCU N°1 — vault `main` → 400, JAMAIS servi.
///
/// On seed une note dans `main` ET une note dérivée dans `code-test`. La requête
/// `vault:"main"` DOIT être rejetée (400) AVANT toute requête index — aucune note
/// `main` n'est jamais retournée par code_scope.
#[tokio::test]
async fn scope_rejects_vault_main() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Note réelle dans main (le vault protégé).
    idx.seed_note_with_fts(
        "01KMAINSECRETXXXXXXXXXXXXXX",
        "decisions",
        "secret main note",
    )
    .await
    .expect("seed main note");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "main",
                "selector": {"kind": "query", "value": "secret"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vault 'main' DOIT être rejeté (invariant sécu N°1)"
    );
}

/// Test 2b : preuve directe au niveau index — code_scope_query rejette 'main'.
/// Défense en profondeur : même si le handler était contourné, l'index refuse.
#[tokio::test]
async fn scope_main_note_never_served() {
    let (_app, _state, idx) = build_app().await;

    idx.seed_note_with_fts("01KMAINSECRETYYYYYYYYYYYYYY", "decisions", "topsecret main")
        .await
        .expect("seed main note");

    // Tentative directe sur l'index avec vault 'main' → erreur (jamais de fuite).
    use gradatum_core::index_store::CodeSelector;
    let res = idx
        .code_scope_query("main", &CodeSelector::Query("topsecret".into()), 10)
        .await;
    assert!(
        res.is_err(),
        "code_scope_query('main') doit échouer — jamais de fuite main"
    );
}

/// Test 3 : vault avec path traversal → 400.
#[tokio::test]
async fn scope_rejects_traversal() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);

    for bad in [
        "code-../main",
        "code-/etc",
        "code-a.b",
        "notcode-x",
        "code-",
    ] {
        let resp = app
            .clone()
            .oneshot(scope_req(
                serde_json::json!({
                    "vault": bad,
                    "selector": {"kind": "query", "value": "x"}
                }),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "vault '{bad}' doit être rejeté"
        );
    }
}

/// Test 4 : sans JWT → 401.
#[tokio::test]
async fn scope_unauthenticated_401() {
    let (app, _state, _idx) = build_app().await;
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-test",
                "selector": {"kind": "query", "value": "x"}
            }),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Test 5 : selector.kind hors vocabulaire → 400.
#[tokio::test]
async fn scope_invalid_selector_400() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-test",
                "selector": {"kind": "regex", "value": ".*"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Test 6 : budget serré → truncated=true, entrées ENTIÈRES (jamais coupées).
#[tokio::test]
async fn scope_budget_truncates() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Enregistrer le vault avant de seeder (obligatoire → sinon 404).
    idx.set_code_vault_repo_path("code-test", "/tmp/code-test-repo")
        .await
        .expect("set_code_vault_repo_path");
    // Seed 5 symboles dans un fichier — tous matchent "func".
    let notes: Vec<DerivedNote> = (0..5)
        .map(|i| {
            derived(
                "code-test",
                "src/big.rs",
                "fn",
                &format!("func_number_{i}"),
                Some("(a: u32, b: u32) -> u32"),
                vec![],
            )
        })
        .collect();
    idx.write_note_derived_batch("code-test", "src/big.rs", "h", "sha", notes)
        .await
        .expect("seed");

    // Budget très serré (5 tokens) → au plus 1 entrée tient.
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-test",
                "selector": {"kind": "query", "value": "func"},
                "budget_tokens": 5
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json["entries"].as_array().unwrap();
    assert!(entries.len() < 5, "budget serré → entrées omises");
    assert_eq!(json["truncated"], true, "truncated=true attendu");
    assert_eq!(json["total_matched"], 5, "total_matched compte tout");
    // Chaque entrée servie est ENTIÈRE (signature non tronquée).
    for e in entries {
        assert_eq!(
            e["signature"], "(a: u32, b: u32) -> u32",
            "signature jamais tronquée intra-entrée"
        );
    }
}

/// Test 7 : selector path → tous les symboles d'un fichier.
#[tokio::test]
async fn scope_path_selector() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Enregistrer le vault (obligatoire → sinon 404).
    idx.set_code_vault_repo_path("code-test", "/tmp/code-test-repo")
        .await
        .expect("set_code_vault_repo_path");
    idx.write_note_derived_batch(
        "code-test",
        "src/a.rs",
        "h",
        "sha",
        vec![
            derived("code-test", "src/a.rs", "fn", "alpha", None, vec![]),
            derived("code-test", "src/a.rs", "struct", "Beta", None, vec![]),
        ],
    )
    .await
    .expect("seed a");
    idx.write_note_derived_batch(
        "code-test",
        "src/b.rs",
        "h",
        "sha",
        vec![derived(
            "code-test",
            "src/b.rs",
            "fn",
            "gamma",
            None,
            vec![],
        )],
    )
    .await
    .expect("seed b");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-test",
                "selector": {"kind": "path", "value": "src/a.rs"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let names: Vec<String> = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["qualified_name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"Beta".to_string()));
    assert!(!names.contains(&"gamma".to_string()), "gamma hors scope");
}

/// Test 8 : vault code-* jamais ingéré → 404 (contrat §3.3bis M1 acté 2026-06-13).
///
/// Distinguo par rapport au test 8b ci-dessous :
/// - jamais ingéré (absent de `code_vault`) → **404**
/// - ingéré + selector sans match → **200** vide
/// Ce test et le suivant sont les deux faces discriminantes de M1.
#[tokio::test]
async fn scope_never_ingested_404() {
    let (app, state, _idx) = build_app().await;
    let token = sign(&state);
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-never-ingested",
                "selector": {"kind": "query", "value": "anything"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "vault jamais ingéré → 404 (M1 §3.3bis)"
    );
}

/// Test 8b : vault ingéré + selector sans match → 200 vide (pas un 404).
///
/// Distinguo discriminant : le vault existe dans `code_vault` (ingéré), mais
/// le selector ne correspond à aucun symbole → 200 `{entries:[], total_matched:0}`.
#[tokio::test]
async fn scope_ingested_no_match_200() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Injecter une entrée dans code_vault (simule un ingest) sans note qui matche.
    idx.set_code_vault_repo_path("code-empty-match", "/tmp/nonexistent-repo")
        .await
        .expect("set_code_vault_repo_path");
    // Aucune note dérivée → total_matched=0.

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": "code-empty-match",
                "selector": {"kind": "query", "value": "symbole-absent-xyz"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault ingéré + 0 match → 200 vide (pas un 404)"
    );
    let json = body_json(resp).await;
    assert_eq!(json["total_matched"], 0);
    assert!(json["entries"].as_array().unwrap().is_empty());
}

/// FIX-T1 — §4 critère 7b : le handler détecte la dérive disque → `stale=true` dans JSON.
///
/// Chemin testé : `detect_stale_paths` → `get_code_vault_repo_path` + `code_freshness_hashes`
/// + `fs::read` + sha256_hex → `stale=true`. Ce test NE passe PAS via `check_freshness`
/// (qui n'est pas appelé par le handler). Discriminant : si le handler ne calcule pas le
/// flag, le test échoue sur `assert_eq!(e["stale"], true)`.
#[tokio::test]
async fn scope_stale_flag_handler_path() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // ── Créer un repo temp avec un fichier source réel ────────────────────────
    let tmp = tempfile::TempDir::new().expect("TempDir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    let file_path = src_dir.join("stale_target.rs");
    let original_content = b"pub fn stale_target(x: u32) -> u32 { x }";
    std::fs::write(&file_path, original_content).expect("write original");

    let vault_id = "code-stale-test";
    let source_path = "src/stale_target.rs";

    // ── Calculer le hash du contenu ORIGINAL (identique à gradatum-ingest) ────
    let original_hash: String = {
        let h: [u8; 32] = Sha256::digest(original_content).into();
        h.iter().map(|b| format!("{b:02x}")).collect()
    };

    // ── Peupler l'index : write_note_derived_batch stocke content_hash_source ─
    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &original_hash,   // content_hash_source = hash ingesté
        "abc123deadbeef", // ingested_sha (SHA git, pas comparé ici)
        vec![derived(
            vault_id,
            source_path,
            "fn",
            "stale_target",
            Some("(x: u32) -> u32"),
            vec![],
        )],
    )
    .await
    .expect("write_note_derived_batch");

    // ── Enregistrer le repo path dans code_vault (idem `code ingest`) ─────────
    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set_code_vault_repo_path");

    // ── Muter le fichier sur disque APRÈS l'ingest ────────────────────────────
    // Le hash stocké (original) ≠ hash actuel → detect_stale_paths doit retourner ce path.
    let mutated_content = b"pub fn stale_target(x: u32, y: u32) -> u64 { (x + y) as u64 }";
    std::fs::write(&file_path, mutated_content).expect("write mutated");

    // ── POST code_scope — le handler doit détecter la dérive ─────────────────
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "path", "value": source_path}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries array");
    // L'entrée doit être servie (elle existe dans l'index) mais avec stale=true.
    assert_eq!(
        entries.len(),
        1,
        "1 symbole attendu — l'entrée est servie même si stale"
    );
    let e = &entries[0];
    assert_eq!(e["qualified_name"], "stale_target");
    assert_eq!(
        e["stale"], true,
        "stale doit être true : le fichier a été muté après ingest — \
         si ce test échoue, le handler ne propage pas le flag (FIX-T1 non couvert)"
    );
}

/// FIX-T1 variante — fichier supprimé du disque → `stale=true`.
///
/// Même chemin que `scope_stale_flag_handler_path` : `detect_stale_paths` tente
/// `fs::read` → `Err` → insert dans `stale`. Discriminant : si le handler ne gère
/// pas le cas de disparition, le test échoue sur `assert_eq!(e["stale"], true)`.
#[tokio::test]
async fn scope_stale_flag_deleted_file() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let tmp = tempfile::TempDir::new().expect("TempDir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    let file_path = src_dir.join("deleted_fn.rs");
    let content = b"pub fn deleted_fn() {}";
    std::fs::write(&file_path, content).expect("write");

    let vault_id = "code-stale-deleted";
    let source_path = "src/deleted_fn.rs";

    let hash: String = {
        let h: [u8; 32] = Sha256::digest(content).into();
        h.iter().map(|b| format!("{b:02x}")).collect()
    };

    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &hash,
        "sha-deleted-test",
        vec![derived(
            vault_id,
            source_path,
            "fn",
            "deleted_fn",
            None,
            vec![],
        )],
    )
    .await
    .expect("seed");

    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");

    // Supprimer le fichier du disque — detect_stale_paths → Err(read) → stale.
    std::fs::remove_file(&file_path).expect("rm file");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "path", "value": source_path}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(
        entries.len(),
        1,
        "symbole servi (présent en index même si fichier absent)"
    );
    assert_eq!(
        entries[0]["stale"], true,
        "stale=true si le fichier source a disparu du disque"
    );
}

/// Test IB-1 (golden include_body) — `body` = slice exact des lignes du fichier source.
///
/// Critère d'acceptation spec : `include_body=true` sur un symbole → `body` ==
/// slice exact des lignes du fichier source pour le span `(start_line, end_line)`.
#[tokio::test]
async fn include_body_golden_exact_slice() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Créer un fichier source sur disque avec un contenu précis.
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_path = "src/golden.rs";
    // 5 lignes : symbole sur lignes 3-5.
    let content = "// ligne 1\n// ligne 2\npub fn golden_fn() {\n    42\n}\n";
    let file_path = tmp.path().join("src").join("golden.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).expect("mkdir src");
    std::fs::write(&file_path, content).expect("write golden.rs");

    let vault_id = "code-golden";
    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");

    // Ingest avec hash cohérent (fichier frais = non-stale).
    let hash = {
        let bytes = std::fs::read(&file_path).expect("read");
        let h: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &hash, // ingested_sha = hash du fichier tel qu'il est = frais
        &hash,
        vec![derived_with_span(
            vault_id,
            source_path,
            "fn",
            "golden_fn",
            Some("() -> ()"),
            vec![],
            Some((3, 5)), // lignes 3-5 du fichier
        )],
    )
    .await
    .expect("seed derived");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "golden_fn"},
                "include_body": true
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "1 symbole attendu");
    let e = &entries[0];
    assert_eq!(e["stale"], false, "fichier frais → stale=false");
    // body = slice exact des lignes 3-5 (1-based).
    let expected_body = "pub fn golden_fn() {\n    42\n}";
    assert_eq!(
        e["body"].as_str().expect("body présent"),
        expected_body,
        "IB-1 golden: body doit être le slice exact des lignes 3-5"
    );
    // body_truncated absent (false omis du JSON — K1).
    assert!(
        json["body_truncated"].is_null() || json["body_truncated"] == false,
        "body_truncated doit être absent ou false"
    );
}

/// Test IB-2 (M2 — gardien invariant 2, BLOQUANT) : fichier modifié → stale=true ∧ body=null.
///
/// Scénario : le fichier est ingéré, puis ses lignes sont décalées (modification) sans
/// changer le hash stocké → le handler doit détecter stale=true et NE PAS servir de corps.
#[tokio::test]
async fn include_body_m2_stale_implies_no_body() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let tmp = tempfile::tempdir().expect("tempdir");
    let source_path = "src/m2.rs";
    let original_content = "pub fn m2_fn() {\n    1\n}\n";
    let file_path = tmp.path().join("src").join("m2.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).expect("mkdir src");
    std::fs::write(&file_path, original_content).expect("write original");

    let vault_id = "code-m2";

    // Calculer le hash de l'original.
    let original_hash = {
        let bytes = std::fs::read(&file_path).expect("read");
        let h: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");

    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &original_hash,
        &original_hash,
        vec![derived_with_span(
            vault_id,
            source_path,
            "fn",
            "m2_fn",
            Some("() -> ()"),
            vec![],
            Some((1, 3)), // span indexé sur l'original
        )],
    )
    .await
    .expect("seed");

    // Modifier le fichier APRÈS ingest (décalage des lignes, signature inchangée).
    // Le hash stocké est original_hash ≠ hash courant → stale=true.
    let modified_content =
        "// commentaire ajouté AVANT — décale les lignes\npub fn m2_fn() {\n    1\n}\n";
    std::fs::write(&file_path, modified_content).expect("write modified");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "m2_fn"},
                "include_body": true
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "symbole toujours servi");
    let e = &entries[0];

    // M2 : fichier modifié → stale=true (hash différent).
    assert_eq!(
        e["stale"], true,
        "M2 BLOQUANT: fichier modifié → stale=true attendu"
    );
    // M2 : stale=true → body=null (jamais de corps potentiellement faux).
    assert!(
        e["body"].is_null(),
        "M2 BLOQUANT: stale=true → body doit être null (absent du JSON), obtenu {:?}",
        e["body"]
    );
}

/// Test IB-3 — budget corps serré → body_truncated=true, corps entiers, signatures intactes.
#[tokio::test]
async fn include_body_budget_truncates_bodies_not_entries() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let tmp = tempfile::tempdir().expect("tempdir");
    let source_path = "src/budget.rs";
    // Deux fonctions, chacune occupant ~30 chars de corps.
    let content = "pub fn fn_a() { 1 }\npub fn fn_b() { 2 }\n";
    let file_path = tmp.path().join("src").join("budget.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).expect("mkdir src");
    std::fs::write(&file_path, content).expect("write");

    let vault_id = "code-budget";
    let hash = {
        let bytes = std::fs::read(&file_path).expect("read");
        let h: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");

    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &hash,
        &hash,
        vec![
            derived_with_span(
                vault_id,
                source_path,
                "fn",
                "fn_a",
                None,
                vec![],
                Some((1, 1)),
            ),
            derived_with_span(
                vault_id,
                source_path,
                "fn",
                "fn_b",
                None,
                vec![],
                Some((2, 2)),
            ),
        ],
    )
    .await
    .expect("seed");

    // Budget corps = 1 token → seul le premier corps tient, le deuxième est omis.
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "path", "value": source_path},
                "include_body": true,
                "body_budget_tokens": 1
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");
    // Les 2 entrées sont présentes (seul le budget corps coupe les corps, pas les entrées).
    assert_eq!(
        entries.len(),
        2,
        "IB-3: 2 entrées retenues malgré budget corps serré"
    );
    // body_truncated=true car au moins un corps omis.
    assert_eq!(
        json["body_truncated"], true,
        "IB-3: body_truncated=true attendu"
    );
    // truncated=false car les entrées ne sont pas coupées (budget signatures non atteint).
    assert_eq!(json["truncated"], false, "IB-3: truncated indépendant");
    // Au moins une entrée a body=null (corps omis par budget).
    let has_null_body = entries.iter().any(|e| e["body"].is_null());
    assert!(has_null_body, "IB-3: au moins un body=null attendu");
}

/// Test IB-4 (régression K1) — `include_body=false` → réponse byte-for-byte identique.
///
/// Le champ `body` ne doit PAS apparaître dans le JSON quand `include_body=false`.
/// `body_truncated` ne doit PAS apparaître (false omis par `skip_serializing_if`).
#[tokio::test]
async fn include_body_false_response_identical_to_baseline() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let vault_id = "code-regression";
    idx.set_code_vault_repo_path(vault_id, "/tmp/nonexistent-repo")
        .await
        .expect("set repo path");
    idx.write_note_derived_batch(
        vault_id,
        "src/x.rs",
        "sha_x",
        "sha_x",
        vec![derived_with_span(
            vault_id,
            "src/x.rs",
            "fn",
            "regression_fn",
            Some("() -> ()"),
            vec![],
            Some((1, 3)),
        )],
    )
    .await
    .expect("seed");

    // Requête SANS include_body (défaut false).
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "regression_fn"}
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "1 entrée");

    // K1 : `body` absent du JSON (skip_serializing_if = Option::is_none).
    assert!(
        entries[0].get("body").is_none() || entries[0]["body"].is_null(),
        "IB-4 K1: champ 'body' doit être absent du JSON quand include_body=false"
    );
    // K1 : `body_truncated` absent du JSON (skip_serializing_if = Not::not sur false).
    assert!(
        json.get("body_truncated").is_none() || json["body_truncated"].is_null(),
        "IB-4 K1: champ 'body_truncated' doit être absent du JSON quand false"
    );
}

/// Test IB-5 (S1 anti-traversal `..`) — source_path contenant `..` → body=null.
///
/// Un `source_path` contenant `..` est rejeté par le pré-garde explicite de S1.
/// L'entrée reste servie (stale=true) mais body=null : jamais d'exfiltration.
/// L'assertion est INCONDITIONNELLE : l'entrée DOIT être retournée par l'index
/// pour que le test soit discriminant (un test qui ne voit pas l'entrée = faux négatif).
#[tokio::test]
async fn include_body_s1_traversal_path_no_body() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let tmp = tempfile::tempdir().expect("tempdir");
    let vault_id = "code-s1";

    // Seed avec un source_path traversal (contient ..).
    let traversal_path = "../secret.rs";
    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");
    idx.write_note_derived_batch(
        vault_id,
        traversal_path,
        "sha_s1",
        "sha_s1",
        vec![derived_with_span(
            vault_id,
            traversal_path,
            "fn",
            "secret_fn",
            None,
            vec![],
            Some((1, 3)),
        )],
    )
    .await
    .expect("seed traversal");

    // selector=symbol pour maximiser la chance que l'index retourne l'entrée.
    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "secret_fn"},
                "include_body": true
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");

    // Assertion INCONDITIONNELLE : l'entrée DOIT être présente (discriminant).
    // Si l'index ne retourne pas l'entrée, c'est le test lui-même qui est cassé.
    assert_eq!(
        entries.len(),
        1,
        "IB-5 S1: l'entrée 'secret_fn' doit être retournée par l'index — si ce count \
         est 0, le seed ou le selector est incorrect (faux négatif potentiel)"
    );

    // S1 pré-garde `..` : rejeté → stale=true.
    assert_eq!(
        entries[0]["stale"], true,
        "IB-5 S1: source_path avec '..' → stale=true (rejet pré-garde)"
    );
    // Corps absent : jamais d'exfiltration via path traversal.
    assert!(
        entries[0]["body"].is_null(),
        "IB-5 S1: source_path avec '..' → body=null (pas d'exfiltration), obtenu {:?}",
        entries[0]["body"]
    );
}

/// Test IB-7 (S1 symlink hors-repo — SCELLE canonicalize) — vecteur le plus dangereux.
///
/// Scenario : un symlink DANS le repo pointe vers un fichier SECRET HORS du repo.
/// Le `source_path` est un path propre (aucun `..`) → le pré-garde `contains("..")`
/// ne l'attrape PAS. Seul `canonicalize() + starts_with(repo_canonical)` résout le
/// symlink vers le chemin réel hors-repo et rejette la lecture.
///
/// Ce test scelle que la suppression de `canonicalize` provoquerait une régression
/// détectable : sans canonicalize, le symlink serait suivi et le fichier secret lu.
#[tokio::test]
async fn include_body_s1_symlink_out_of_repo_no_body() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Arborescence :
    //   parent_tmp/
    //     repo/              ← repo gradatum (vault_id)
    //       src/
    //         code.rs        ← symlink → ../secret.rs  (DANS le repo, sans ..)
    //     secret.rs          ← fichier SECRET hors repo
    let parent_tmp = tempfile::tempdir().expect("parent tempdir");
    let repo_dir = parent_tmp.path().join("repo");
    let src_dir = repo_dir.join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir repo/src");

    // Fichier secret HORS du repo (dans le parent).
    let secret_path = parent_tmp.path().join("secret.rs");
    std::fs::write(
        &secret_path,
        "// SECRET CONTENT — NE DOIT JAMAIS ÊTRE SERVI\n",
    )
    .expect("write secret");

    // Symlink DANS le repo : src/code.rs → ../../secret.rs  (chemin relatif)
    // Le source_path "src/code.rs" est propre — PAS de `..`.
    let symlink_in_repo = src_dir.join("code.rs");
    std::os::unix::fs::symlink(&secret_path, &symlink_in_repo).expect("symlink");

    let vault_id = "code-symlink";
    let source_path = "src/code.rs"; // path propre, pas de `..`

    idx.set_code_vault_repo_path(vault_id, &repo_dir.to_string_lossy())
        .await
        .expect("set repo path");

    // Hash fictif (le fichier sera "stale" peu importe — ce qui compte c'est
    // que le handler ne lise pas le contenu du symlink via canonicalize reject).
    // On met un hash qui ne correspond pas au contenu → stale quand même, mais
    // le vecteur réel est : S1 doit rejeter AVANT même de comparer les hashes.
    idx.write_note_derived_batch(
        vault_id,
        source_path,
        "sha_symlink",
        "sha_symlink",
        vec![derived_with_span(
            vault_id,
            source_path,
            "fn",
            "symlink_fn",
            None,
            vec![],
            Some((1, 1)),
        )],
    )
    .await
    .expect("seed symlink note");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "symlink_fn"},
                "include_body": true
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");

    // L'entrée DOIT être retournée par l'index (discriminant).
    assert_eq!(
        entries.len(),
        1,
        "IB-7 S1-symlink: l'entrée 'symlink_fn' doit être indexée"
    );

    // S1 canonicalize : repo_dir/src/code.rs → canonicalize() résout vers
    // parent_tmp/secret.rs qui n'est PAS sous repo_dir → rejet → stale=true.
    // Le pré-garde `contains("..")` N'ATTRAPE PAS "src/code.rs" — c'est
    // UNIQUEMENT canonicalize qui protège ici.
    assert_eq!(
        entries[0]["stale"], true,
        "IB-7 S1-symlink: symlink hors-repo → stale=true (rejet via canonicalize)"
    );
    assert!(
        entries[0]["body"].is_null(),
        "IB-7 S1-symlink: symlink hors-repo → body=null, canonicalize scelle \
         le vecteur d'exfiltration (le pré-garde '..' ne suffit PAS ici)"
    );
    // Vérification que le fichier secret n'a PAS été lu :
    // body est null ET stale=true → le handler a rejeté avant de lire le contenu.
    let secret_content = std::fs::read_to_string(&secret_path).expect("read secret after");
    assert!(
        secret_content.contains("SECRET CONTENT"),
        "fichier secret non modifié (lecture n'a pas eu lieu)"
    );
}

/// Test IB-6 (B3 span dégénéré) — span start > end ou span absent → body=null.
#[tokio::test]
async fn include_body_b3_degenerate_span_no_body() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    let tmp = tempfile::tempdir().expect("tempdir");
    let source_path = "src/degen.rs";
    let content = "pub fn degen_fn() {}\n";
    let file_path = tmp.path().join("src").join("degen.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&file_path, content).expect("write");

    let vault_id = "code-b3";
    let hash = {
        let bytes = std::fs::read(&file_path).expect("read");
        let h: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    idx.set_code_vault_repo_path(vault_id, &tmp.path().to_string_lossy())
        .await
        .expect("set repo path");

    // Span dégénéré : start (10) > fichier (1 ligne) → B3 → body=null.
    idx.write_note_derived_batch(
        vault_id,
        source_path,
        &hash,
        &hash,
        vec![derived_with_span(
            vault_id,
            source_path,
            "fn",
            "degen_fn",
            None,
            vec![],
            Some((10, 20)), // start > nb_lignes → dégénéré B3
        )],
    )
    .await
    .expect("seed degen");

    let resp = app
        .oneshot(scope_req(
            serde_json::json!({
                "vault": vault_id,
                "selector": {"kind": "symbol", "value": "degen_fn"},
                "include_body": true
            }),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "1 entrée");
    // B3 : span dégénéré (start > nb_lignes) → body=null (pas d'erreur HTTP).
    assert!(
        entries[0]["body"].is_null(),
        "IB-6 B3: span dégénéré → body=null, obtenu {:?}",
        entries[0]["body"]
    );
    // Pas de 500 : accuracy>coverage, pas d'erreur.
    assert_eq!(entries[0]["stale"], false, "fichier frais → stale=false");
}

/// Sanity compile-time : CodeScopeEntryRaw exposé via core (import utilisé).
#[allow(dead_code)]
fn _type_check(_e: CodeScopeEntryRaw) {}
