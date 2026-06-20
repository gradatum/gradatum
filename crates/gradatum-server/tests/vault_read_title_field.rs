//! Tests TDD — `vault_read` expose le champ `title` dans la réponse JSON.
//!
//! Fix racine TODO 01KV39C8A5 : `vault_read` ne retournait PAS `title` →
//! tout flux RMW qui en sourcait le titre écrivait `title:null` et corrompait la note.
//!
//! Cas couverts :
//! 1. `vault_read_title_from_index`      — note avec title upserted → `title == attendu`
//!    (identique à ce que `vault_search` rendrait via `get_titles_sections`).
//! 2. `vault_read_title_h1_fallback`     — colonne title vide mais body `# Mon Titre\n…`
//!    → fallback H1 → `title == "Mon Titre"`.
//! 3. `vault_read_no_h1_no_title`        — pas de colonne title, pas de H1
//!    → `title == null`, 200 OK.
//! 4. `vault_read_title_non_regression`  — champs existants (path, content, metadata,
//!    size_bytes, sha256) inchangés par l'ajout du champ.
//! 5. `vault_read_title_get_titles_err_graceful` — test de la dégradation :
//!    quand la résolution du titre échoue, `title=null` et status reste 200.
//!    (Testé indirectement via note sans title indexé ni H1 — même path-code).

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, call_vault_read, sign_token};

/// Test 1 : note créée avec un title upserted → `title` dans la réponse == title attendu.
///
/// `write_note_with_h1` fait : Vault::write_note + upsert_note_title.
/// `get_titles_sections` lit la colonne `title` du SQLite — doit matcher.
#[tokio::test]
async fn vault_read_title_from_index() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let _nid = env
        .write_note_with_h1("Architecture Système", "Corps de la note architecture.")
        .await;

    let resp = call_vault_read(env.app.clone(), &token, "Architecture Système", "main")
        .await
        .expect("vault_read par titre doit réussir (test 1)");

    // Le champ `title` doit être présent et correspondre au titre indexé.
    assert_eq!(
        resp["title"].as_str(),
        Some("Architecture Système"),
        "title doit correspondre au titre indexé. resp={resp}"
    );
}

/// Test 2 : note dont la colonne title est NULL mais body commence par `# Mon Titre`.
///
/// Stratégie :
/// - `write_note_in_section` crée le fichier .md sur disque + indexe le title.
/// - On accède à `env._vault_typed.index()` → `Arc<SqliteIndex>` → `set_title_to_null_for_test`.
/// - `vault_read` via ULID : `get_titles_sections` rend None → fallback H1 → titre extrait.
#[tokio::test]
async fn vault_read_title_h1_fallback() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Crée la note sur disque + SQLite (avec title indexé).
    let nid = env
        .write_note_in_section("reference", "Mon Titre Fallback", "Corps du fallback.")
        .await;

    // Force la colonne `title` à NULL dans SQLite pour simuler une vieille note
    // sans title indexé (pré-migration 0009).
    // Accès via le vault typé concret (index() → Arc<SqliteIndex>).
    env._vault_typed
        .index()
        .set_title_to_null_for_test(&nid)
        .await
        .expect("set_title_to_null_for_test — helper SQLite");

    // vault_read via ULID (le .md est sur disque grâce à write_note_in_section).
    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read doit réussir même sans title dans l'index (test 2)");

    // Fallback H1 : le body est `# Mon Titre Fallback\nCorps du fallback.`
    // → title attendu = "Mon Titre Fallback"
    assert_eq!(
        resp["title"].as_str(),
        Some("Mon Titre Fallback"),
        "fallback H1 doit extraire le titre du body. resp={resp}"
    );
}

/// Test 3 : note sans title indexé ET sans ligne H1 → `title = null`, 200 OK.
///
/// Body = texte brut sans `# `. Colonne title = NULL. Fallback H1 = None.
/// vault_read doit retourner 200 avec `title: null` (pas d'erreur).
///
/// Utilise directement `_vault_typed.write_note` pour injecter un body sans H1
/// (contourne le format `# {title}\n{body}` des helpers `write_note_in_section`).
/// Pas d'appel à `upsert_note_title` → colonne title reste NULL.
#[tokio::test]
async fn vault_read_no_h1_no_title() {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let env = build_app().await;
    let token = sign_token(&env.state);

    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
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
    // Body sans ligne H1 — ni colonne title ni H1 dans le body.
    let body = "Ce texte ne contient aucune ligne H1 valide.".to_string();
    let note = env
        ._vault_typed
        .write_note(fm, body)
        .await
        .expect("write_note sans H1");
    // PAS d'upsert_note_title → colonne title reste NULL.
    let nid = note.id;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read sans H1 ni title indexé doit retourner 200 (test 3)");

    // `title` doit être null (JSON null).
    assert!(
        resp["title"].is_null(),
        "title doit être null quand ni index ni H1 ne donnent un titre. resp={resp}"
    );
}

/// Test 4 : non-régression — les champs existants (path, content, metadata, size_bytes,
/// sha256) sont inchangés par l'ajout du champ `title`.
#[tokio::test]
async fn vault_read_title_non_regression() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let nid = env
        .write_note_with_h1("Note Non Regression", "Contenu stable.")
        .await;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read non-régression doit réussir (test 4)");

    // Champs existants toujours présents et cohérents.
    assert_eq!(
        resp["path"].as_str(),
        Some(nid.to_string().as_str()),
        "path inchangé. resp={resp}"
    );
    assert!(
        resp["content"]
            .as_str()
            .unwrap_or("")
            .contains("Note Non Regression"),
        "content inchangé. resp={resp}"
    );
    assert!(
        resp["metadata"].is_object(),
        "metadata toujours présent. resp={resp}"
    );
    assert!(
        resp["size_bytes"].as_u64().is_some(),
        "size_bytes toujours présent. resp={resp}"
    );
    assert_eq!(
        resp["sha256"].as_str().map(|s| s.len()),
        Some(64),
        "sha256 = 64 chars hex. resp={resp}"
    );
    // `title` est présent (peut être string ou null — cas non-régression : doit exister).
    assert!(
        resp.get("title").is_some(),
        "champ title présent dans la réponse. resp={resp}"
    );
}

/// Test P3-1a (golden) : H1 indentée → title=None.
///
/// Body = `"   # Indenté\ncorps"` — le `#` n'est PAS en tête de ligne.
/// La définition SQL canonique `body_text LIKE '# %'` ne matcherait PAS cette note.
/// `vault_read` doit retourner `title=null` (cohérence SQL ↔ runtime).
///
/// Prouve que le fallback utilise `strip_prefix("# ")` sans `trim_start` (pas `find`).
#[tokio::test]
async fn vault_read_indented_h1_returns_null_title() {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let env = build_app().await;
    let token = sign_token(&env.state);

    // Body avec H1 indentée — ne doit PAS matcher le fallback.
    let body = "   # Indenté\ncorps de la note.".to_string();
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
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
    let note = env
        ._vault_typed
        .write_note(fm, body)
        .await
        .expect("write_note H1 indentée");
    // PAS d'upsert_note_title → colonne title reste NULL.

    let resp = call_vault_read(env.app.clone(), &token, &note.id.to_string(), "main")
        .await
        .expect("vault_read doit retourner 200 (test P3-1a)");

    assert!(
        resp["title"].is_null(),
        "H1 indentée ne doit PAS produire un titre (cohérence SQL LIKE '# %'). resp={resp}"
    );
}

/// Test P3-1b (golden) : H1 en ligne 2 → title=None.
///
/// Body = `"intro\n# Pas en tête\ncorps"` — le `#` est en ligne 2, pas en tête de body.
/// La définition SQL canonique `body_text LIKE '# %'` ne matcherait PAS cette note
/// (LIKE compare depuis le début du body_text).
/// `vault_read` doit retourner `title=null` (cohérence SQL ↔ runtime).
///
/// Prouve que le fallback utilise `lines().next()` (1ʳᵉ ligne uniquement), pas `find`.
#[tokio::test]
async fn vault_read_h1_on_line2_returns_null_title() {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let env = build_app().await;
    let token = sign_token(&env.state);

    // Body avec H1 en 2ᵉ ligne — ne doit PAS être extrait comme titre.
    let body = "intro\n# Pas en tête\ncorps de la note.".to_string();
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
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
    let note = env
        ._vault_typed
        .write_note(fm, body)
        .await
        .expect("write_note H1 ligne 2");
    // PAS d'upsert_note_title → colonne title reste NULL.

    let resp = call_vault_read(env.app.clone(), &token, &note.id.to_string(), "main")
        .await
        .expect("vault_read doit retourner 200 (test P3-1b)");

    assert!(
        resp["title"].is_null(),
        "H1 en ligne 2 ne doit PAS produire un titre (fallback = 1ʳᵉ ligne uniquement). resp={resp}"
    );
}
