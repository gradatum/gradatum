//! Tests d'intégration pour `project_map_export`.
//!
//! Utilise `Connection::open_in_memory()` (rusqlite) — même pattern que
//! `project_map_scope.rs` — pour rester synchrone et sans dépendance HTTP.
//!
//! ## Schéma de test
//!
//! Aligné sur le schéma prod (migration 0001 + 0005) :
//! - `title TEXT` nullable (migration 0005 : `ADD COLUMN title TEXT` sans `NOT NULL`)
//! - `created INTEGER NOT NULL` (migration 0001 : unix epoch ms)
//!
//! La divergence `title TEXT NOT NULL` / `created TEXT` masquait le bug R1 (title NULL
//! → rusqlite error → zéro JSON au lieu de dégradation). Alignement sur prod permet
//! au test `feature_with_null_title_returns_empty_string` de capturer ce bug.
//!
//! ## Cas couverts
//!
//! 1. Export miroir-site par défaut : exclut `dropped` uniquement (Règle A
//!    NOMENCLATURE §10e : `version/backlog` **inclus** avec `version = "vX.Y.Z"`) ;
//!    exclut les cartes sans `[[feature:]]` (changelog) ; tri par F-XX croissant.
//! 2. Flag `--include-dropped` : inclut les cartes dropped (backlog déjà inclus).
//! 3. `title NULL` en base → `title: ""` dans l'export (dégradation, pas d'erreur).
//! 4. Cartes `downgraded`/`garbage` exclues quel que soit le flag.

use rusqlite::Connection;

use gradatum_admin::project_map_export::{ExportOptions, export_features_from_conn};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Crée le schéma aligné sur la prod (migration 0001 + 0005).
///
/// `title TEXT` nullable (pas de NOT NULL — migration 0005).
/// `created INTEGER NOT NULL` (unix epoch ms — migration 0001).
fn create_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            vault_id TEXT NOT NULL,
            section TEXT NOT NULL,
            body_text TEXT NOT NULL,
            title TEXT,
            status TEXT NOT NULL,
            created INTEGER NOT NULL DEFAULT 0
        )",
    )
    .expect("création table test");
}

/// Insère une note avec un titre explicite (cas standard).
fn insert_note(
    conn: &Connection,
    id: &str,
    vault: &str,
    section: &str,
    body: &str,
    title: &str,
    status: &str,
) {
    conn.execute(
        "INSERT INTO notes (id, vault_id, section, body_text, title, status) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![id, vault, section, body, title, status],
    )
    .expect("insert note test");
}

/// Insère une note avec `title = NULL` (aligné sur la prod post-migration 0005
/// pour les notes n'ayant pas de H1 extrait).
fn insert_note_null_title(
    conn: &Connection,
    id: &str,
    vault: &str,
    section: &str,
    body: &str,
    status: &str,
) {
    conn.execute(
        "INSERT INTO notes (id, vault_id, section, body_text, title, status) VALUES (?1,?2,?3,?4,NULL,?5)",
        rusqlite::params![id, vault, section, body, status],
    )
    .expect("insert note null title");
}

/// Corps d'une carte-feature avec les 5 wikilinks typés obligatoires.
fn feature_card_body(project: &str, feature_id: &str, release: &str, version: &str) -> String {
    format!(
        "[[project:{project}]] [[status:OPEN]] [[kind:FEATURE]] [[feature:{feature_id}]] [[release:{release}]] [[version:{project}/{version}]]\n\nCorps de test."
    )
}

/// Corps d'une carte changelog SANS `[[feature:]]` (ne doit PAS apparaître dans l'export).
fn changelog_card_body(project: &str, version: &str) -> String {
    format!(
        "[[project:{project}]] [[status:DONE]] [[kind:FIX]] [[version:{project}/{version}]]\n\nFix de test."
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Export miroir-site (défaut) — Règle A NOMENCLATURE §10e :
/// - F-37 released v0.5.2 → présent, version "v0.5.2"
/// - F-61 planned 0.6.4 → présent, version "v0.6.4"
/// - F-90 roadmap backlog → **présent** (Règle A), version "vX.Y.Z"
/// - F-10 dropped v0.4.0 → exclu (dropped seul exclu par défaut)
/// - carte changelog sans feature → exclue
///
/// Résultat trié par F-XX croissant.
#[test]
fn export_default_miroir_site() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    // F-37 : released, v0.5.2 (doit apparaître)
    insert_note(
        &conn,
        "n-f37",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "Feature audit F-37",
        "live",
    );
    // F-61 : planned, v0.6.4 (doit apparaître)
    insert_note(
        &conn,
        "n-f61",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-61", "planned", "0.6.4"),
        "Feature multi-lang F-61",
        "live",
    );
    // F-90 : roadmap, backlog — Règle A : inclus par défaut avec version "vX.Y.Z"
    insert_note(
        &conn,
        "n-f90",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-90", "roadmap", "backlog"),
        "Feature future F-90",
        "live",
    );
    // F-10 : dropped, v0.4.0 (exclu par défaut — seul dropped est exclu)
    insert_note(
        &conn,
        "n-f10",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-10", "dropped", "0.4.0"),
        "Feature abandonnée F-10",
        "live",
    );
    // Carte changelog sans [[feature:]] (exclue systématiquement)
    insert_note(
        &conn,
        "n-changelog",
        "main",
        "project-map",
        &changelog_card_body("gradatum", "0.5.2"),
        "Fix changelog",
        "live",
    );

    let opts = ExportOptions {
        include_dropped: false,
    };
    let features = export_features_from_conn(&conn, "main", opts).expect("export");

    // 3 features attendues : F-37, F-61, F-90 (backlog inclus — Règle A)
    assert_eq!(
        features.len(),
        3,
        "F-37, F-61 et F-90 attendus (backlog Règle A), got {features:?}"
    );

    // Tri par identifiant croissant : F-37 avant F-61 avant F-90
    assert_eq!(features[0].feature, "F-37");
    assert_eq!(features[0].release, "released");
    assert_eq!(features[0].version, Some("v0.5.2".to_string()));
    assert_eq!(features[0].title, "Feature audit F-37");

    assert_eq!(features[1].feature, "F-61");
    assert_eq!(features[1].release, "planned");
    assert_eq!(features[1].version, Some("v0.6.4".to_string()));
    assert_eq!(features[1].title, "Feature multi-lang F-61");

    // F-90 backlog → "vX.Y.Z" (Règle A)
    assert_eq!(features[2].feature, "F-90");
    assert_eq!(features[2].release, "roadmap");
    assert_eq!(
        features[2].version,
        Some("vX.Y.Z".to_string()),
        "backlog → vX.Y.Z (Règle A)"
    );
    assert_eq!(features[2].title, "Feature future F-90");
}

/// Sérialisation JSON : vérification du schéma exact
/// `{"feature":"F-37","release":"released","version":"v0.5.2","title":"…"}`.
#[test]
fn export_json_schema() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    insert_note(
        &conn,
        "n-f37",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "Feature audit F-37",
        "live",
    );

    let opts = ExportOptions {
        include_dropped: false,
    };
    let features = export_features_from_conn(&conn, "main", opts).expect("export");

    let json = serde_json::to_string_pretty(&features).expect("sérialisation");
    // Vérification schéma exact
    assert!(
        json.contains(r#""feature": "F-37""#),
        "champ feature:\n{json}"
    );
    assert!(
        json.contains(r#""release": "released""#),
        "champ release:\n{json}"
    );
    assert!(
        json.contains(r#""version": "v0.5.2""#),
        "champ version:\n{json}"
    );
    assert!(
        json.contains(r#""title": "Feature audit F-37""#),
        "champ title:\n{json}"
    );
}

/// Avec `include_dropped = true` : les cartes dropped sont incluses (backlog
/// était déjà inclus par défaut depuis Règle A — test vérifie la complétude).
#[test]
fn export_include_dropped_shows_all() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    // F-10 : dropped v0.4.0
    insert_note(
        &conn,
        "n-f10",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-10", "dropped", "0.4.0"),
        "Feature abandonnée F-10",
        "live",
    );
    // F-37 : released v0.5.2
    insert_note(
        &conn,
        "n-f37",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "Feature audit F-37",
        "live",
    );
    // F-90 : roadmap backlog
    insert_note(
        &conn,
        "n-f90",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-90", "roadmap", "backlog"),
        "Feature future F-90",
        "live",
    );

    let opts = ExportOptions {
        include_dropped: true,
    };
    let features = export_features_from_conn(&conn, "main", opts).expect("export");

    // 3 features attendues : F-10, F-37, F-90 (tri croissant)
    assert_eq!(
        features.len(),
        3,
        "F-10, F-37, F-90 attendus, got {features:?}"
    );

    assert_eq!(features[0].feature, "F-10");
    assert_eq!(features[0].release, "dropped");
    assert_eq!(features[0].version, Some("v0.4.0".to_string()));

    assert_eq!(features[1].feature, "F-37");

    assert_eq!(features[2].feature, "F-90");
    assert_eq!(features[2].release, "roadmap");
    // Règle A : backlog → "vX.Y.Z" (plus null)
    assert_eq!(
        features[2].version,
        Some("vX.Y.Z".to_string()),
        "backlog → vX.Y.Z (Règle A)"
    );
}

/// Les cartes `downgraded`/`garbage` sont exclues même avec `include_dropped`.
#[test]
fn export_excludes_downgraded_regardless() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    // Carte feature LIVE : doit apparaître
    insert_note(
        &conn,
        "n-f37",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "Feature audit F-37",
        "live",
    );
    // Carte feature downgraded : exclue (lifecycle index)
    insert_note(
        &conn,
        "n-f37-old",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "Feature audit F-37 (old)",
        "downgraded",
    );
    // Carte feature garbage : exclue
    insert_note(
        &conn,
        "n-f38",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-38", "planned", "0.6.0"),
        "Feature F-38 garbage",
        "garbage",
    );

    let opts = ExportOptions {
        include_dropped: true,
    };
    let features = export_features_from_conn(&conn, "main", opts).expect("export");

    assert_eq!(features.len(), 1, "seule la carte live doit apparaître");
    assert_eq!(features[0].feature, "F-37");
    assert_eq!(features[0].title, "Feature audit F-37");
}

/// R1+R2 : une carte-feature avec `title = NULL` (prod réelle post-migration 0005)
/// doit être exportée avec `title: ""` — jamais provoquer une erreur rusqlite
/// qui ferait échouer la commande entière.
///
/// Sans le fix R1, `row.get::<_, String>(2)?` sur une valeur NULL renvoie
/// `rusqlite::Error::InvalidColumnType` → `export_features_from_conn` propage
/// une `Err`, la commande CLI imprime une erreur et sort sans JSON.
#[test]
fn feature_with_null_title_returns_empty_string() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    // F-37 avec title = NULL (carte sans H1 extrait — cas réel prod)
    insert_note_null_title(
        &conn,
        "n-f37-notitle",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.5.2"),
        "live",
    );

    let opts = ExportOptions {
        include_dropped: false,
    };
    // Doit réussir (Ok), pas paniquer ni renvoyer Err.
    let features = export_features_from_conn(&conn, "main", opts)
        .expect("title NULL doit dégrader en chaîne vide, pas en erreur");

    assert_eq!(features.len(), 1, "la carte doit être exportée");
    assert_eq!(
        features[0].title, "",
        "title NULL → chaîne vide (dégradation gracieuse)"
    );
    assert_eq!(features[0].feature, "F-37");
}

/// S2 — Export miroir-site exclut les cartes-feature dont `kind != FEATURE`.
///
/// Les cartes avec `kind:FIX`/CHORE/SPIKE/TASK/ENHANCEMENT sont vault-only ;
/// seul `kind:FEATURE` alimente le site (export T2, Slice 1 S2).
///
/// - F-99 `kind:FIX` roadmap/backlog → exclu du miroir-site (include_dropped=false).
/// - F-37 `kind:FEATURE` released → inclus (inchangé).
/// - Mode audit `include_dropped=true` : lève le filtre kind (vault complet).
#[test]
fn export_excludes_non_feature_kind() {
    let conn = Connection::open_in_memory().expect("DB mémoire");
    create_schema(&conn);

    // F-99 : kind:FIX (mapping gov-todo debt→FIX) — vault-only, exclu du miroir-site.
    insert_note(
        &conn,
        "n-f99",
        "main",
        "project-map",
        "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] [[release:roadmap]] [[version:gradatum/backlog]]",
        "Dette technique F-99",
        "live",
    );
    // F-37 : kind:FEATURE released — doit être inclus dans le miroir-site.
    insert_note(
        &conn,
        "n-f37",
        "main",
        "project-map",
        &feature_card_body("gradatum", "F-37", "released", "0.6.3"),
        "Feature audit F-37",
        "live",
    );

    // Miroir-site (défaut include_dropped=false) : seul FEATURE.
    let features =
        export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");

    assert_eq!(
        features.len(),
        1,
        "seul F-37 kind:FEATURE attendu, F-99 kind:FIX exclu : {features:?}"
    );
    assert_eq!(features[0].feature, "F-37", "F-37 kind:FEATURE présent");

    // Mode audit (include_dropped=true) : filtre kind levé → toutes les cartes-feature.
    let features_audit = export_features_from_conn(
        &conn,
        "main",
        ExportOptions {
            include_dropped: true,
        },
    )
    .expect("export audit");

    assert_eq!(
        features_audit.len(),
        2,
        "mode audit doit inclure F-37 + F-99 : {features_audit:?}"
    );
    assert_eq!(features_audit[0].feature, "F-37");
    assert_eq!(features_audit[1].feature, "F-99");
}
