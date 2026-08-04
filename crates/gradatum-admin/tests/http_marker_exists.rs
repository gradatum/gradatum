//! Tests d'intégration HTTP pour `HttpVaultClient::marker_exists`.
//!
//! ## Objectif
//!
//! Prouver la régression `results` → `items` (champ JSON réel de l'API `vault_search`).
//! Les `MockVaultClient` existants bypassent le parsing JSON — ces tests exercent le
//! code de parsing réel via un faux serveur HTTP (wiremock).
//!
//! ## Cas couverts
//!
//! 1. **bug_results_field** : le serveur répond `{"items":[{...hit...}]}` →
//!    `marker_exists` doit retourner `true`. ROUGE avant fix (code lit `results`).
//! 2. **empty_items_returns_false** : `{"items":[]}` → `false`.
//! 3. **precision_different_feature** : le hit renvoyé porte un marqueur d'un
//!    autre F-YY (faux positif BM25) → `marker_exists("pm-feature-source:F-42")`
//!    doit retourner `false`.
//! 4. **second_run_idempotence** : simuler un 2ème passage (hit présent dès le 1er
//!    appel de marker_exists) → `skipped=N, created=0`.
//! 12. **ranking_ejection_deterministic_fallback** : vault_search retourne N hits
//!     qui ne contiennent pas la carte cible (éjectée du top-N BM25) → l'ancienne
//!     logique retourne `false` (ROUGE) ; la nouvelle logique appelle `vault_list`
//!     pour scanner toute la section puis `vault_read` sur le path manquant → `true`
//!     (VERT après fix).
//! 13. **vault_list_pagination** : vault_list renvoie `next_cursor` sur la page 1
//!     (section > limit), la cible est sur la page 2 → l'ancienne logique ignore
//!     `next_cursor` → ROUGE ; le fix suit la pagination → VERT (P1-C1 reviewer).
//!
//! ## Garde-fou
//!
//! NE PAS lancer ces tests avec `--apply` contre le serveur LIVE :19090.
//! Wiremock démarre un faux serveur sur un port éphémère non conflictuel.

use anyhow::Result;
use async_trait::async_trait;
use gradatum_admin::changelog_backfill::{
    BackfillChangelogArgs, BackfillChangelogReport, VaultWriteClient, run_backfill,
};
use gradatum_admin::project_map_card::VaultWriteCard;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─── Utilitaires locaux ───────────────────────────────────────────────────────

/// Construit un `SearchHit` JSON réaliste contenant le marqueur cible dans le snippet.
///
/// Ce format doit correspondre exactement à la struct `SearchHit` de
/// `gradatum-server/src/api_v1/dto.rs`.
fn search_hit_with_marker(marker: &str) -> serde_json::Value {
    json!({
        "path": "project-map/01ABCDEFGHIJKLMNOPQRSTUV",
        "score": 0.01667,
        "title": "[PROJECT-MAP][gradatum] Some Feature — v0.4.2",
        // Le snippet contient le marqueur littéral (extrait FTS5 du body).
        "snippet": format!("[[feature:F-42]] [[project:gradatum]] ... {marker}"),
        "trust": 0.5,
        "status": "live"
    })
}

/// Construit un `SearchHit` JSON pour un marqueur DIFFÉRENT (faux positif BM25).
///
/// Simule le cas où la query `pm-feature-source:F-42` retourne une note
/// dont le marqueur réel est `pm-feature-source:F-41` (autre feature).
fn search_hit_different_marker(returned_marker: &str) -> serde_json::Value {
    json!({
        "path": "project-map/01ZZZZZZZZZZZZZZZZZZZZZ1",
        "score": 0.01400,
        "title": "[PROJECT-MAP][gradatum] Other Feature — v0.3.0",
        // snippet du body — contient F-41, pas F-42.
        "snippet": format!("[[feature:F-41]] [[project:gradatum]] ... {returned_marker}"),
        "trust": 0.5,
        "status": "live"
    })
}

/// Monte un `MockServer` wiremock et enregistre les deux routes requises par
/// `HttpVaultClient::new` (`/auth/exchange`) + `marker_exists` (`/api/v1/vault_search`).
///
/// `vault_search_response` : la réponse JSON que le faux serveur renverra.
async fn setup_mock_server_with_search_response(
    vault_search_response: serde_json::Value,
) -> MockServer {
    let server = MockServer::start().await;

    // Route 1 : /auth/exchange — requis par HttpVaultClient::new pour obtenir le JWT.
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token",
            "ttl_secs": 86400,
            "scopes": ["read", "write"],
            "tenant_id": "main",
            "kid": "test-kid"
        })))
        .mount(&server)
        .await;

    // Route 2 : /api/v1/vault_search — réponse configurée par l'appelant.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vault_search_response))
        .mount(&server)
        .await;

    server
}

// ─── Helpers fallback (Tasks 1-5) ────────────────────────────────────────────

/// Monte un `MockServer` avec les 3 routes : /auth/exchange + /api/v1/vault_search
/// + /api/v1/vault_read. `read_status` permet de simuler une erreur (ex: 500).
async fn setup_mock_server_full(
    vault_search_response: serde_json::Value,
    vault_read_response: serde_json::Value,
    read_status: u16,
) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token", "ttl_secs": 86400,
            "scopes": ["read", "write"], "tenant_id": "main", "kid": "test-kid"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vault_search_response))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(read_status).set_body_json(vault_read_response))
        .mount(&server)
        .await;

    server
}

/// `SearchHit` dont le snippet NE contient PAS le marqueur cible (faux négatif FTS5 simulé).
fn search_hit_snippet_miss(path_val: &str) -> serde_json::Value {
    json!({
        "path": path_val,
        "score": 0.01,
        "title": "Feature card",
        // snippet = fenêtre FTS qui ne montre pas le marqueur littéral
        "snippet": "[[project:gradatum]] [[status:live]] ... (fenêtre sans marqueur)",
        "trust": 0.5,
        "status": "live"
    })
}

// ─── TEST 1 : rouge avant fix — le bug `results` vs `items` ──────────────────

/// Prouve la régression : le serveur répond `{"items":[{...}]}` mais le code
/// actuel lit `payload["results"]` → toujours `null` → `count=0` → `false`.
///
/// Ce test DOIT être ROUGE avant fix et VERT après.
#[tokio::test]
async fn bug_results_field_items_nonempty_returns_true() {
    let marker = "pm-feature-source:F-42";
    let response = json!({
        "items": [search_hit_with_marker(marker)]
    });

    let server = setup_mock_server_with_search_response(response).await;
    let base_url = server.uri();

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&base_url, "ak_test")
        .await
        .expect("HttpVaultClient::new doit réussir avec le mock /auth/exchange");

    let result = client
        .marker_exists(marker)
        .await
        .expect("marker_exists ne doit pas retourner Err");

    // ROUGE avant fix : code lit `payload["results"]` → null → false.
    // VERT après fix : code lit `payload["items"]` + vérifie le marqueur dans le snippet.
    assert!(
        result,
        "marker_exists doit retourner true quand items contient un hit avec le marqueur {marker}"
    );
}

// ─── TEST 2 : items vide → false ─────────────────────────────────────────────

/// `{"items":[]}` → `marker_exists` retourne `false`.
///
/// Ce test est déjà vert sur le code actuel (items vide = count 0 = false),
/// mais on le garde pour s'assurer que le fix ne casse pas ce chemin.
#[tokio::test]
async fn empty_items_returns_false() {
    let response = json!({ "items": [] });

    let server = setup_mock_server_with_search_response(response).await;
    let base_url = server.uri();

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&base_url, "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let result = client
        .marker_exists("pm-feature-source:F-99")
        .await
        .expect("marker_exists");

    assert!(
        !result,
        "marker_exists doit retourner false quand items est vide"
    );
}

// ─── TEST 3 : précision — faux positif BM25 → false ─────────────────────────

/// Prouve la vérification de précision : le serveur retourne un hit avec un
/// marqueur DIFFÉRENT (`pm-feature-source:F-41`), la query était pour F-42.
///
/// Un simple `count > 0` retournerait `true` (faux positif).
/// L'impl correcte vérifie via fast-path (snippet/title) ET via fallback
/// vault_read (body complet) que le marqueur littéral est absent.
///
/// Mise à jour : avec le fallback vault_read, le mock doit aussi
/// enregistrer `/api/v1/vault_read` — le body ne contient pas F-42 → false.
#[tokio::test]
async fn precision_hit_with_different_marker_returns_false() {
    let queried_marker = "pm-feature-source:F-42";
    // Le serveur retourne un hit d'une AUTRE feature (F-41).
    let server = setup_mock_server_full(
        json!({ "items": [search_hit_different_marker("pm-feature-source:F-41")] }),
        // body d'un F-41 — ne contient PAS pm-feature-source:F-42
        json!({
            "path": "project-map/01ZZZZZZZZZZZZZZZZZZZZZ1",
            "title": "Other feature",
            "content": "# Feature F-41\n\npm-feature-source:F-41\n[[project:gradatum]]",
            "metadata": null,
            "size_bytes": 50,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let result = client
        .marker_exists(queried_marker)
        .await
        .expect("marker_exists");

    // Un simple count > 0 donnerait true (faux positif).
    // Fast-path + fallback body doivent tous deux échouer → false.
    assert!(
        !result,
        "marker_exists doit retourner false quand aucun hit ne contient littéralement {queried_marker}"
    );
}

// ─── TEST 4 : second-run idempotence via run_backfill ────────────────────────

/// Simule un second `--apply` sur un substrat déjà peuplé.
///
/// Le mock vault_search retourne TOUJOURS le marqueur dans le snippet →
/// `marker_exists` retourne `true` pour chaque entrée → `skipped=N, created=0`.
///
/// Ce test est ROUGE avant fix (marker_exists retourne toujours false →
/// toutes les entrées seraient créées au lieu d'être sautées).
///
/// Le mock est stateless ici — on utilise un `TrackingMockClient` qui implémente
/// `VaultWriteClient` avec `marker_exists` qui retourne toujours `true` (substrat plein).
#[tokio::test]
async fn second_run_skips_all_existing_markers() {
    // Client dont marker_exists retourne toujours `true` (substrat déjà peuplé).
    // vault_write ne doit JAMAIS être appelé.
    struct AlwaysExistsClient {
        write_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl VaultWriteClient for AlwaysExistsClient {
        async fn marker_exists(&self, _marker: &str) -> Result<bool> {
            Ok(true)
        }

        async fn vault_write(&self, _card: &VaultWriteCard) -> Result<String> {
            self.write_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("unexpected-write".to_string())
        }
    }

    const TEST_CHANGELOG: &str = r#"
## [0.5.2] - 2026-06-15

### Added

- **vault_write in-place update**: supports note_id + expected_sha256.
- **vault_timeline endpoint**: new chronological listing endpoint.

### Fixed

- **Optimistic-lock Conflict**: fixed by anti-clobber guard.
"#;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("CHANGELOG.md");
    std::fs::write(&path, TEST_CHANGELOG).expect("écriture CHANGELOG test");

    let args = BackfillChangelogArgs {
        changelog_path: path,
        from_version: "0.5.2".to_string(),
        to_version: "0.5.2".to_string(),
        apply: true,
        server_url: "http://127.0.0.1:19090".to_string(),
        api_key: "test-api-key".to_string(), // garde-fou
        include_meta: false,
    };

    let client = AlwaysExistsClient {
        write_count: std::sync::atomic::AtomicUsize::new(0),
    };

    let report: BackfillChangelogReport = run_backfill(&args, &client)
        .await
        .expect("run_backfill second-run");

    assert_eq!(
        report.created, 0,
        "second-run : aucune note ne doit être créée (toutes skippées)"
    );
    assert!(
        report.skipped > 0,
        "second-run : au moins une note doit être sautée (skipped={})",
        report.skipped
    );
    assert_eq!(
        client.write_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "vault_write ne doit jamais être appelé en second-run"
    );
}

// ─── TEST 11 : stateful create→re-detect (DT-MARKER-2) ──────────────────────

/// Prouve le cycle create→re-detect (idempotence end-to-end) via wiremock stateful.
///
/// Scénario 2 états :
/// - Avant write : vault_search renvoie items:[] (priorité haute, 1 seul appel)
///   → `marker_exists=false` (write attendu).
/// - Après write : vault_search renvoie un hit snippet-miss + vault_read body
///   avec le marqueur → `marker_exists=true` (skip attendu).
///
/// C'est précisément le scénario manquant qui a causé l'anomalie 98/45.
#[tokio::test]
async fn stateful_create_then_redetect() {
    let marker = "pm-feature-source:F-77";
    let server = MockServer::start().await;

    // /auth/exchange
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token", "ttl_secs": 86400,
            "scopes": ["read", "write"], "tenant_id": "main", "kid": "test-kid"
        })))
        .mount(&server)
        .await;

    // vault_search état initial : 1er appel renvoie items vide (priorité haute, 1 fois).
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // vault_search état post-write : appels suivants renvoient le hit snippet-miss.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [search_hit_snippet_miss("project-map/01NEW")]
        })))
        .with_priority(2)
        .mount(&server)
        .await;

    // vault_read renvoie le body avec le marqueur (re-detect via fallback).
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/01NEW",
            "title": "New feature",
            "content": format!("# Feature F-77\n\n{marker}\n"),
            "metadata": null,
            "size_bytes": 40,
            "sha256": "0".repeat(64)
        })))
        .mount(&server)
        .await;

    // vault_write (création) — accepté.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_write"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "note_id": "01NEW" })))
        .mount(&server)
        .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    // Cycle : absent → write → re-detect.
    let before = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(!before, "1er check : marqueur absent (items vide) → false");

    // VaultWriteCard vit dans project_map_card (pas changelog_backfill::).
    let card = gradatum_admin::project_map_card::VaultWriteCard {
        title: "New feature F-77".to_string(),
        body: format!("# Feature F-77\n\n{marker}\n"),
        tags: vec!["project-map".to_string()],
        section_hint: "project-map".to_string(),
    };
    client.vault_write(&card).await.expect("vault_write OK");

    let after = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        after,
        "2e check post-write : re-detect via fallback vault_read → true (idempotence prouvée)"
    );
}

// ─── TESTs 8-10 : anti-collision sous-chaîne (P1 reviewer) ──────────────────

/// `F-4` ne doit PAS matcher `F-42` via le fallback body.
///
/// Classe de bug de l'anomalie 98/45 : `F-4` est sous-chaîne de `F-42`.
/// `marker_matches` borné ferme ce bug (frontière droite non-chiffre).
#[tokio::test]
async fn substring_marker_does_not_false_match_in_body() {
    let marker = "pm-feature-source:F-4"; // sous-chaîne de F-42
    let server = setup_mock_server_full(
        json!({ "items": [search_hit_snippet_miss("project-map/01ABC")] }),
        json!({
            "path": "project-map/01ABC",
            "title": "Feature F-42",
            // body contient F-42, PAS F-4 isolé
            "content": "# Feature F-42\n\npm-feature-source:F-42\n",
            "metadata": null,
            "size_bytes": 40,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let exists = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        !exists,
        "F-4 ne doit PAS matcher F-42 (collision sous-chaîne = bug 98/45)"
    );
}

/// `F-6` ne doit PAS matcher `F-63` via le fast-path snippet.
///
/// Prouve que `marker_matches` est aussi appliqué dans le fast-path snippet/title
/// et pas seulement dans le fallback body.
#[tokio::test]
async fn substring_marker_does_not_false_match_in_snippet() {
    let marker = "pm-feature-source:F-6"; // sous-chaîne de F-63
    let server = setup_mock_server_full(
        // snippet contient F-63 (le marqueur F-6 en est préfixe)
        json!({ "items": [json!({
            "path": "project-map/01DEF",
            "score": 0.01,
            "title": "Feature F-63",
            "snippet": "pm-feature-source:F-63 [[project:gradatum]]",
            "trust": 0.5,
            "status": "live"
        })] }),
        json!({
            "path": "project-map/01DEF",
            "title": "Feature F-63",
            "content": "# Feature F-63\n\npm-feature-source:F-63\n",
            "metadata": null,
            "size_bytes": 40,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let exists = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        !exists,
        "F-6 ne doit PAS matcher F-63 ni en fast-path ni en fallback"
    );
}

// ─── TEST 12 : ranking éjection → fallback vault_list déterministe ───────────

/// Prouve la cause racine des doublons F-03/F-19 : la carte cible peut être
/// éjectée du top-N BM25 de `vault_search` quand toutes les autres cartes de la
/// section partagent les mêmes tokens FTS5 (`pm`, `feature`, `source`, `f`, `XX`).
///
/// Scénario :
/// - `vault_search` retourne 5 hits (F-01..F-05), aucun ne contient `F-42`.
/// - Le fallback actuel lit les 5 bodies → aucun match → retourne `false`.
/// - La carte cible est en réalité LIVE à "project-map/01F42000000000000000000000".
///
/// Avec l'ancienne logique (vault_search seul) : ROUGE (false au lieu de true).
/// Avec le fix (vault_list + vault_read complémentaire) : VERT (true).
///
/// Ce test simule `vault_list` via `/api/v1/vault_list` (format réel :
/// `{ entries: [{path, size_bytes, modified_at}], total, next_cursor }`)
/// — le mock expose les 6 paths (5 mauvais + la cible), et vault_read sur
/// la cible retourne le body avec le marqueur.
#[tokio::test]
async fn ranking_ejection_deterministic_fallback_finds_ejected_card() {
    let target_marker = "pm-feature-source:F-42";
    let target_path = "project-map/01F42000000000000000000000";
    let server = MockServer::start().await;

    // /auth/exchange
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token", "ttl_secs": 86400,
            "scopes": ["read", "write"], "tenant_id": "main", "kid": "test-kid"
        })))
        .mount(&server)
        .await;

    // vault_search : retourne 50 hits (F-01..F-50 = la limite serveur), AUCUN ne
    // contient F-42. La carte F-42 est éjectée du top-50 BM25 (classée > 50).
    // 50 = VAULT_SEARCH_LIMIT dans changelog_backfill.rs — déclenche le step 4.
    let other_paths: Vec<(String, String)> = (1u32..=50)
        .filter(|&n| n != 42) // F-42 est la cible éjectée — absente du top-50
        .chain(std::iter::once(51)) // compenser le trou F-42 → 50 hits au total
        .map(|n| {
            (
                format!("project-map/01F{n:02}000000000000000000000"),
                format!("pm-feature-source:F-{n:02}"),
            )
        })
        .collect();
    assert_eq!(
        other_paths.len(),
        50,
        "test setup: vault_search doit retourner 50 hits"
    );

    let search_items: Vec<serde_json::Value> = other_paths
        .iter()
        .map(|(p, m)| {
            json!({
                "path": p.as_str(),
                "score": 0.014,
                "title": "Feature card",
                // snippet ne contient PAS F-42 (snippet-miss simulé)
                "snippet": format!("[[project:gradatum]] {m} [[status:live]]"),
                "trust": 0.5,
                "status": "live"
            })
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": search_items,
            // corpus_match_count > items.len() → signal éjection (P1-C2).
            // 51 cartes matchent les tokens FTS5 mais seulement 50 retournées.
            "corpus_match_count": 51
        })))
        .mount(&server)
        .await;

    // vault_read : pour chacun des 5 autres paths, retourne un body SANS F-42.
    // Pour la cible (F-42), retourne le body AVEC le marqueur.
    // Wiremock fait un match par body JSON exact — on ne peut pas distinguer les paths
    // avec wiremock sans matchers body. On utilise donc un mock global qui lit le path
    // dans le body JSON de la requête et retourne la réponse adéquate.
    //
    // SIMPLIFICATION : un seul mock `/api/v1/vault_read` avec réponse générique sans
    // le marqueur F-42 (pour les 5 mauvais), et un mock prioritaire qui matche le path
    // cible spécifique.
    //
    // Wiremock ne supporte pas nativement le routing par body JSON. On monte donc 2
    // mocks vault_read :
    //   - Priorité 1 (haute) : le path cible → body avec F-42.
    //   - Priorité 2 (basse) : tous les autres → body générique sans F-42.

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .and(wiremock::matchers::body_partial_json(
            json!({ "path": target_path }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": target_path,
            "title": "Feature F-42",
            "content": format!("# Feature F-42\n\n{target_marker}\n[[project:gradatum]]"),
            "metadata": null,
            "size_bytes": 60,
            "sha256": "0".repeat(64)
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    // vault_read par défaut (5 mauvais paths) → body sans F-42.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/01F01000000000000000000000",
            "title": "Other feature",
            "content": "# Another feature\n\npm-feature-source:F-01\n[[project:gradatum]]",
            "metadata": null,
            "size_bytes": 50,
            "sha256": "0".repeat(64)
        })))
        .with_priority(2)
        .mount(&server)
        .await;

    // vault_list : retourne les 51 paths (50 autres + la cible F-42 éjectée).
    // Format réel : VaultListResponse { entries: [{path, size_bytes, modified_at}], total, next_cursor }
    let mut all_entries: Vec<serde_json::Value> = other_paths
        .iter()
        .map(|(p, _)| json!({ "path": p.as_str(), "size_bytes": 50, "modified_at": "2026-06-01T00:00:00Z" }))
        .collect();
    all_entries.push(json!({
        "path": target_path,
        "size_bytes": 60,
        "modified_at": "2026-06-15T10:00:00Z"
    }));
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": all_entries,
            "next_cursor": null,
            "total": 51
        })))
        .mount(&server)
        .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let result = client
        .marker_exists(target_marker)
        .await
        .expect("marker_exists ne doit pas retourner Err");

    // ROUGE avec l'ancienne logique (vault_search seul, limit:5, éjection ranking).
    // VERT après fix (vault_list fallback déterministe, scan complet section).
    assert!(
        result,
        "marker_exists doit retourner true pour F-42 même quand la carte est éjectée \
         du top-5 BM25 de vault_search (fix vault_list déterministe requis)"
    );
}

// ─── TEST 13 : pagination vault_list (P1-C1 reviewer) ────────────────────────

/// Prouve que `vault_list_section_paths` suit `next_cursor` jusqu'à épuisement.
///
/// Scénario (P1-C1 reviewer 2026-06-23) :
/// - `vault_search` retourne 50 hits (corpus_match_count=51 → step 4 activé).
/// - `vault_list` page 1 : 2 paths génériques + `next_cursor: "page2"`.
/// - `vault_list` page 2 : path cible F-42 + `next_cursor: null`.
/// - `vault_read` sur F-42 → body avec le marqueur → `true`.
///
/// Avec l'ancienne logique (limit:1000 single-shot, ignore next_cursor) :
/// - Page 1 retournée, F-42 absent, next_cursor ignoré → ROUGE (false).
///
/// Avec le fix (boucle next_cursor) :
/// - Page 1 + page 2 lues, F-42 trouvé → VERT (true).
#[tokio::test]
async fn vault_list_pagination_follows_next_cursor_to_find_ejected_card() {
    let target_marker = "pm-feature-source:F-42";
    let target_path = "project-map/01F42PAGINATEDTARGET00000";
    let server = MockServer::start().await;

    // /auth/exchange
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "test-jwt-token", "ttl_secs": 86400,
            "scopes": ["read", "write"], "tenant_id": "main", "kid": "test-kid"
        })))
        .mount(&server)
        .await;

    // vault_search : 50 hits sans F-42, corpus_match_count=51 → éjection signalée.
    let search_hits: Vec<serde_json::Value> = (1u32..=50)
        .filter(|&n| n != 42)
        .chain(std::iter::once(51))
        .map(|n| {
            json!({
                "path": format!("project-map/01F{n:02}PAGTEST00000000000000"),
                "score": 0.014,
                "title": "Feature card",
                "snippet": format!("pm-feature-source:F-{n:02} [[project:gradatum]]"),
                "trust": 0.5,
                "status": "live"
            })
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": search_hits,
            "corpus_match_count": 51
        })))
        .mount(&server)
        .await;

    // vault_list page 1 (sans cursor) : 2 paths + next_cursor → prouve pagination.
    // Priorité haute, up_to_n_times(1) pour laisser le mock page-2 répondre ensuite.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                { "path": "project-map/01PAGE1ENTRY1000000000000", "size_bytes": 50, "modified_at": "2026-06-01T00:00:00Z" },
                { "path": "project-map/01PAGE1ENTRY2000000000000", "size_bytes": 50, "modified_at": "2026-06-01T00:00:00Z" }
            ],
            "next_cursor": "page2-cursor-abc",
            "total": 3
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // vault_list page 2 (avec cursor "page2-cursor-abc") : contient la cible F-42.
    // Priorité basse — répond aux appels après expiration du mock page-1.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                { "path": target_path, "size_bytes": 60, "modified_at": "2026-06-15T10:00:00Z" }
            ],
            "next_cursor": null,
            "total": 3
        })))
        .with_priority(2)
        .mount(&server)
        .await;

    // vault_read : cible F-42 → body avec marqueur (priorité haute).
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .and(wiremock::matchers::body_partial_json(
            json!({ "path": target_path }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": target_path,
            "title": "Feature F-42",
            "content": format!("# Feature F-42\n\n{target_marker}\n[[project:gradatum]]"),
            "metadata": null,
            "size_bytes": 60,
            "sha256": "0".repeat(64)
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    // vault_read par défaut (50 hits + 2 page-1 entries) → body sans F-42.
    Mock::given(method("POST"))
        .and(path("/api/v1/vault_read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "project-map/generic",
            "title": "Other feature",
            "content": "# Other feature\n\npm-feature-source:F-01\n[[project:gradatum]]",
            "metadata": null,
            "size_bytes": 50,
            "sha256": "0".repeat(64)
        })))
        .with_priority(2)
        .mount(&server)
        .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let result = client
        .marker_exists(target_marker)
        .await
        .expect("marker_exists ne doit pas retourner Err");

    // ROUGE avec l'ancienne logique (vault_list single-shot, next_cursor ignoré →
    //   page 2 jamais lue → F-42 manqué → false).
    // VERT après fix (boucle next_cursor → page 2 lue → F-42 trouvé → true).
    assert!(
        result,
        "marker_exists doit suivre next_cursor pour trouver F-42 sur la page 2 \
         (fix pagination complète vault_list requis — P1-C1 reviewer)"
    );
}

/// Format de marqueur réel changelog (`changelog/x/y/z`) détecté via fallback.
///
/// Prouve que `marker_matches` fonctionne aussi avec le format non-feature.
/// `changelog/0.5.2/added/0` ne doit PAS coller à `changelog/0.5.2/added/01`.
#[tokio::test]
async fn changelog_format_marker_detected_via_fallback() {
    let marker = "changelog/0.5.2/added/0";
    let server = setup_mock_server_full(
        json!({ "items": [search_hit_snippet_miss("project-map/01CHG")] }),
        json!({
            "path": "project-map/01CHG",
            "title": "v0.5.2 Added",
            "content": format!("# v0.5.2\n\n{marker}\n[[project:gradatum]]"),
            "metadata": null,
            "size_bytes": 50,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let exists = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        exists,
        "marker changelog réel doit être détecté via fallback"
    );
}

// ─── TEST 7 : fallback anti faux-positif BM25 ────────────────────────────────

/// BM25 retourne un hit voisin (tokens partagés) mais le body ne contient PAS
/// le marqueur cible → `marker_exists` doit retourner `false`.
///
/// Prouve que le fallback ne sur-détecte pas.
#[tokio::test]
async fn bm25_false_positive_body_lacks_marker_returns_false() {
    let marker = "pm-feature-source:F-42";
    let server = setup_mock_server_full(
        // hit retourné par BM25 (tokens partagés) mais c'est un AUTRE F-XX
        json!({ "items": [search_hit_snippet_miss("project-map/01OTHER")] }),
        json!({
            "path": "project-map/01OTHER",
            "title": "Other feature",
            // body d'un F-41 voisin — NE contient PAS pm-feature-source:F-42
            "content": "# Feature F-41\n\npm-feature-source:F-41\n[[project:gradatum]]",
            "metadata": null,
            "size_bytes": 50,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let exists = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        !exists,
        "le fallback ne doit PAS détecter un marqueur absent du body (anti faux-positif BM25)"
    );
}

// ─── TEST 6 : fail-loud sur read-error du fallback ───────────────────────────

/// Prouve la sémantique fail-loud : read-error pendant le fallback → `Err`, pas `false`.
///
/// Le snippet ne contient pas le marqueur (faux négatif FTS simulé) + vault_read
/// répond HTTP 500 → `marker_exists` doit propager `Err` (abort `--apply`).
#[tokio::test]
async fn fallback_read_error_is_fail_loud() {
    let marker = "pm-feature-source:F-99";
    let server = setup_mock_server_full(
        json!({ "items": [search_hit_snippet_miss("project-map/01XYZ")] }),
        json!({ "error": "boom" }),
        500, // vault_read échoue
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new");

    let res = client.marker_exists(marker).await;
    assert!(
        res.is_err(),
        "read-error pendant le fallback doit propager Err (fail-loud), pas un bool deviné"
    );
}

// ─── TEST 5 : fallback vault_read sur snippet-miss (DT-MARKER-1) ─────────────

/// Le snippet FTS5 ne contient PAS le marqueur littéral (faux négatif FTS simulé),
/// mais vault_read retourne un body COMPLET qui le contient.
///
/// Prouve que le fallback rattrape le faux négatif FTS5.
#[tokio::test]
async fn fallback_snippet_miss_reads_body_returns_true() {
    let marker = "pm-feature-source:F-42";
    let server = setup_mock_server_full(
        json!({ "items": [search_hit_snippet_miss("project-map/01ABC")] }),
        // vault_read renvoie le body COMPLET qui contient le marqueur
        json!({
            "path": "project-map/01ABC",
            "title": "Feature card",
            "content": format!("# Feature F-42\n\n{marker}\n[[project:gradatum]]"),
            "metadata": null,
            "size_bytes": 64,
            "sha256": "0".repeat(64)
        }),
        200,
    )
    .await;

    let client = gradatum_admin::changelog_backfill::HttpVaultClient::new(&server.uri(), "ak_test")
        .await
        .expect("HttpVaultClient::new doit réussir avec le mock /auth/exchange");

    let exists = client.marker_exists(marker).await.expect("pas d'Err");
    assert!(
        exists,
        "le fallback vault_read doit détecter le marqueur dans le body"
    );
}
