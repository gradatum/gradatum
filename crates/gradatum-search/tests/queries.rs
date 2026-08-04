//! Tests d'intégration des 7 méthodes query de `SqliteIndex` — T3 P2.0c.
//!
//! Chaque test correspond à une méthode implémentée :
//! `distinct_authors`, `distinct_tags`, `backlinks`, `neighbors`,
//! `trace_lineage`, `title_lookup`, `get_note`.
//!
//! Fixtures : 3 notes dans vault "main" avec auteurs, tags et liens wikilink.

use chrono::Utc;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_index::SqliteIndex;
use tempfile::TempDir;

// ── Helpers de fixtures ───────────────────────────────────────────────────────

/// Construit une note avec auteur et tags pour les tests.
fn make_note_with_author(
    vault_id: &str,
    section: Section,
    body: &str,
    author_id: &str,
    author_display_name: Option<&str>,
    tags: &[&str],
) -> Note {
    let author = AuthorRef {
        kind: AuthorKind::Human,
        id: author_id.to_string(),
        display_name: author_display_name.map(|s| s.to_string()),
    };

    let valid_tags: smallvec::SmallVec<[Tag; 4]> =
        tags.iter().filter_map(|t| Tag::new(*t).ok()).collect();

    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: valid_tags,
        author: Some(author),
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let note_body = NoteBody {
        markdown: body.to_string(),
    };
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id: NoteId::new(),
        frontmatter,
        body: note_body,
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}

/// Construit une note sans auteur (pour tester l'exclusion dans distinct_authors).
fn make_note_bare(vault_id: &str, section: Section, body: &str) -> Note {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section,
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
    let note_body = NoteBody {
        markdown: body.to_string(),
    };
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id: NoteId::new(),
        frontmatter,
        body: note_body,
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}

/// Initialise un index en mémoire et insère 3 notes de fixtures + 2 liens.
///
/// - Note A : auteur "alice" / display "Alice Dupont" / tags "rust architecture"
/// - Note B : auteur "bob" / display "Bob Martin" / tags "rust debug"
/// - Note C : auteur "alice" (même auteur) / tags "architecture"
/// - Lien A → B (wikilink)
/// - Lien A → C (wikilink)
///
/// Retourne `(TempDir, index, note_a_id, note_b_id, note_c_id)`.
async fn index_with_fixtures() -> (TempDir, SqliteIndex, String, String, String) {
    let dir = TempDir::new().expect("tempdir");
    let index = SqliteIndex::open(&dir.path().join("index.db"))
        .await
        .expect("open SqliteIndex");

    let note_a = make_note_with_author(
        "main",
        Section::Architecture,
        "# Test Note Title\nCorps de la note A.",
        "alice",
        Some("Alice Dupont"),
        &["rust", "architecture"],
    );
    let note_b = make_note_with_author(
        "main",
        Section::Debug,
        "# Debug Note\nCorps de la note B.",
        "bob",
        Some("Bob Martin"),
        &["rust", "debug"],
    );
    let note_c = make_note_with_author(
        "main",
        Section::Architecture,
        "# Architecture Note\nCorps de la note C.",
        "alice",
        Some("Alice Dupont"),
        &["architecture"],
    );

    let id_a = note_a.id.to_string();
    let id_b = note_b.id.to_string();
    let id_c = note_c.id.to_string();

    index.upsert_note(&note_a).await.expect("upsert note_a");
    index.upsert_note(&note_b).await.expect("upsert note_b");
    index.upsert_note(&note_c).await.expect("upsert note_c");

    // Liens : A → B et A → C (wikilinks)
    index
        .upsert_link("main", &id_a, &id_b)
        .await
        .expect("link A→B");
    index
        .upsert_link("main", &id_a, &id_c)
        .await
        .expect("link A→C");

    (dir, index, id_a, id_b, id_c)
}

// ── Tests — 7 méthodes ────────────────────────────────────────────────────────

#[tokio::test]
async fn distinct_authors_returns_unique_authors() {
    let (_dir, index, _a, _b, _c) = index_with_fixtures().await;

    let authors = index
        .distinct_authors("main")
        .await
        .expect("distinct_authors");

    // 2 auteurs distincts : "Alice Dupont" (2 notes) + "Bob Martin" (1 note).
    assert_eq!(authors.len(), 2, "devrait retourner 2 auteurs distincts");

    // Alice en premier (plus de notes).
    assert_eq!(authors[0].name, "Alice Dupont");
    assert_eq!(authors[0].note_count, 2);
    assert_eq!(authors[1].name, "Bob Martin");
    assert_eq!(authors[1].note_count, 1);
}

#[tokio::test]
async fn distinct_authors_excludes_notes_without_author() {
    let dir = TempDir::new().expect("tempdir");
    let index = SqliteIndex::open(&dir.path().join("idx.db"))
        .await
        .expect("open");

    // Note sans auteur.
    let bare = make_note_bare("main", Section::Reference, "note sans auteur");
    index.upsert_note(&bare).await.expect("upsert bare");

    let authors = index.distinct_authors("main").await.expect("authors");
    assert_eq!(
        authors.len(),
        0,
        "les notes sans auteur ne doivent pas apparaître"
    );
}

#[tokio::test]
async fn distinct_tags_returns_unique_tags_with_counts() {
    let (_dir, index, _a, _b, _c) = index_with_fixtures().await;

    let tags = index.distinct_tags("main").await.expect("distinct_tags");

    // Tags attendus : rust(2), architecture(2), debug(1)
    assert!(!tags.is_empty(), "devrait retourner des tags");

    let rust_entry = tags.iter().find(|(t, _)| t == "rust");
    assert!(rust_entry.is_some(), "tag 'rust' absent");
    assert_eq!(
        rust_entry.unwrap().1,
        2,
        "tag 'rust' doit apparaître dans 2 notes"
    );

    let arch_entry = tags.iter().find(|(t, _)| t == "architecture");
    assert!(arch_entry.is_some(), "tag 'architecture' absent");
    assert_eq!(
        arch_entry.unwrap().1,
        2,
        "'architecture' doit avoir count=2"
    );

    let debug_entry = tags.iter().find(|(t, _)| t == "debug");
    assert!(debug_entry.is_some(), "tag 'debug' absent");
    assert_eq!(debug_entry.unwrap().1, 1, "'debug' doit avoir count=1");
}

#[tokio::test]
async fn backlinks_returns_inbound_wikilinks() {
    let (_dir, index, id_a, id_b, _c) = index_with_fixtures().await;

    // Note B est pointée par A → backlink de B = [A].
    let backlinks = index
        .backlinks("main", &id_b)
        .await
        .expect("backlinks note B");

    assert_eq!(backlinks.len(), 1, "note B a 1 backlink (depuis A)");
    assert_eq!(backlinks[0], id_a, "le backlink de B doit être A");
}

#[tokio::test]
async fn backlinks_returns_empty_for_note_with_no_inbound_links() {
    let (_dir, index, id_a, _b, _c) = index_with_fixtures().await;

    // Note A n'est pointée par personne.
    let backlinks = index
        .backlinks("main", &id_a)
        .await
        .expect("backlinks note A");

    assert_eq!(backlinks.len(), 0, "note A n'a pas de backlinks");
}

#[tokio::test]
async fn neighbors_returns_graph_depth_1() {
    let (_dir, index, id_a, id_b, id_c) = index_with_fixtures().await;

    // À depth=1 depuis A : voisins = B + C.
    let neighbors = index
        .neighbors("main", &id_a, 1)
        .await
        .expect("neighbors depth=1");

    assert_eq!(neighbors.len(), 2, "A a 2 voisins directs (B + C)");
    assert!(
        neighbors.contains(&id_b),
        "B doit être dans les voisins de A"
    );
    assert!(
        neighbors.contains(&id_c),
        "C doit être dans les voisins de A"
    );
}

#[tokio::test]
async fn trace_lineage_returns_parents_and_children() {
    let (_dir, index, id_a, id_b, id_c) = index_with_fixtures().await;

    // Lignée de A : parents=[] (personne ne pointe vers A), enfants=[B, C].
    let lineage_a = index.trace_lineage("main", &id_a).await.expect("lineage A");

    assert_eq!(lineage_a.parents.len(), 0, "A n'a pas de parents");
    assert_eq!(lineage_a.children.len(), 2, "A a 2 enfants (B et C)");
    assert!(lineage_a.children.contains(&id_b));
    assert!(lineage_a.children.contains(&id_c));

    // Lignée de B : parents=[A], enfants=[].
    let lineage_b = index.trace_lineage("main", &id_b).await.expect("lineage B");

    assert_eq!(lineage_b.parents.len(), 1, "B a 1 parent (A)");
    assert_eq!(lineage_b.parents[0], id_a);
    assert_eq!(lineage_b.children.len(), 0, "B n'a pas d'enfants");
}

#[tokio::test]
async fn title_lookup_returns_id_for_exact_h1_title() {
    let (_dir, index, id_a, _b, _c) = index_with_fixtures().await;

    // Note A a body_text commençant par "# Test Note Title\n..."
    let found = index
        .title_lookup("main", "Test Note Title")
        .await
        .expect("title_lookup");

    assert!(
        found.is_some(),
        "note avec titre 'Test Note Title' doit être trouvée"
    );
    assert_eq!(found.unwrap(), id_a, "doit retourner l'id de la note A");
}

#[tokio::test]
async fn title_lookup_returns_none_for_unknown_title() {
    let (_dir, index, _a, _b, _c) = index_with_fixtures().await;

    let found = index
        .title_lookup("main", "Titre Inexistant")
        .await
        .expect("title_lookup missing");

    assert!(found.is_none(), "titre inexistant doit retourner None");
}

/// P2 — Collision colonne `title` vs H1 : la colonne gagne (passe 1 prioritaire).
///
/// `title_lookup` opère en deux passes :
///   - **Passe 1** : exact-match colonne `notes.title` (priorité absolue, index SQL).
///   - **Passe 2** : fallback LIKE sur `body_text LIKE '# {title}\n%'` (H1 Markdown).
///
/// Ce test documente le comportement de passe 1 prioritaire lorsque :
///   - Note A a `title='dup'` en colonne (mais PAS `# dup` dans body_text).
///   - Note B a `# dup` comme H1 dans body_text (mais colonne title = NULL).
///
/// → `title_lookup("main", "dup")` doit retourner l'id de la note A (colonne).
///
/// ## Pourquoi ce comportement est correct
///
/// `upsert_note_title` peuple la colonne après curation : c'est la valeur
/// canonique extraite par le worker. Elle prime sur le H1 raw dans body_text
/// qui peut diverger (casse, ponctuation) avant la normalisation curator.
///
/// ## Conséquence pour les âmes (F-34 v0.7.3)
///
/// `soul_instructions` (mcp.rs) résout `identity/<agent>` via `title_lookup`.
/// Si deux notes ont le même titre (ex: `identity/main`) — colonne vs H1 —
/// la note avec la colonne peuplée gagne. Les seeds de production DOIVENT
/// appeler `upsert_note_title` après écriture pour garantir la résolvabilité.
#[tokio::test]
async fn title_lookup_collision_column_wins_over_h1() {
    let dir = TempDir::new().expect("TempDir collision");
    let index = SqliteIndex::open(&dir.path().join("idx.db"))
        .await
        .expect("SqliteIndex::open collision");

    // Note A : body_text sans H1 "dup", mais colonne title = "dup" (via upsert_note_title).
    let note_a = make_note_bare("main", Section::Architecture, "Corps A sans H1 dup.");
    let id_a = note_a.id.to_string();
    index
        .upsert_note(&note_a)
        .await
        .expect("upsert note_a collision");
    index
        .upsert_note_title("main", &note_a.id, "dup")
        .await
        .expect("upsert_note_title note_a collision");

    // Note B : body_text commence par `# dup\n` (H1), mais colonne title = NULL.
    let note_b = make_note_bare("main", Section::Debug, "# dup\nCorps B avec H1 dup.");
    let id_b = note_b.id.to_string();
    index
        .upsert_note(&note_b)
        .await
        .expect("upsert note_b collision");
    // Note B : PAS d'appel upsert_note_title → colonne title reste NULL.

    // Pré-condition : note B est résolvable via H1 seul (passe 2 fonctionnelle).
    // Vérification indépendante : si on cherche un titre absent en colonne, la passe
    // H1 doit fonctionner. On n'appelle pas title_lookup("main", "dup") ici pour ne
    // pas polluer le test principal ; on vérifie juste que la fixture est cohérente.

    // Résultat attendu : la note A (colonne) gagne sur la note B (H1 fallback).
    let found = index
        .title_lookup("main", "dup")
        .await
        .expect("title_lookup collision");

    assert!(
        found.is_some(),
        "title_lookup doit trouver au moins une note pour 'dup': id_a={id_a} id_b={id_b}"
    );
    let found_id = found.unwrap();
    assert_eq!(
        found_id, id_a,
        "P2 : colonne title doit avoir priorité sur H1 body_text — \
         attendu id_a={id_a}, obtenu {found_id} (id_b={id_b})"
    );
}

#[tokio::test]
async fn get_note_returns_full_record() {
    let (_dir, index, id_a, _b, _c) = index_with_fixtures().await;

    let record = index.get_note("main", &id_a).await.expect("get_note A");

    assert!(record.is_some(), "note A doit être trouvée");
    let n = record.unwrap();
    assert_eq!(n.id, id_a);
    assert_eq!(n.vault_id, "main");
    assert_eq!(n.section, "architecture");
    // Author : display_name "Alice Dupont" (COALESCE)
    assert_eq!(n.author.as_deref(), Some("Alice Dupont"));
    // body_text contient le titre H1
    assert!(
        n.body_text.starts_with("# Test Note Title"),
        "body_text doit commencer par le titre H1"
    );
}

#[tokio::test]
async fn get_note_returns_none_for_missing_id() {
    let (_dir, index, _a, _b, _c) = index_with_fixtures().await;

    let result = index
        .get_note("main", "01HMXJ2K3TQRZ7VBCDEFGH1234")
        .await
        .expect("get_note missing");

    assert!(result.is_none(), "id inexistant doit retourner None");
}
