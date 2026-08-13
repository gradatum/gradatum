//! Tests d'intégration de `gradatum-admin backfill-authors`.
//!
//! Contrat vérifié (exigences opérateur v2.0.0 — ré-attribution des 585 notes sans auteur) :
//!   1. une note sans auteur est attribuée (frontmatter `.md` ET colonne d'index) ;
//!   2. une note qui porte déjà un auteur n'est jamais touchée ;
//!   3. une identité inconnue du preset ACL est refusée (pas écrite) ;
//!   4. relancer ne change rien (idempotence — la sélection est son propre point de reprise) ;
//!   5. après passage, le frontmatter et l'index disent la même chose (auteur + content_hash).
//!
//! Bonus robustesse : dry-run sans effet · note à auteur différent non clobberée ·
//! note au layout legacy `<vault>/<section>/<id>.md` réécrite au MÊME chemin · reprise
//! après écriture `.md` partielle (index encore vide → note ré-attrapée).

use std::path::Path;

use gradatum_admin::backfill_authors::{BackfillAuthorsArgs, run};

// ── Fabrique de mini-vault jetable ──────────────────────────────────────────

/// Schéma minimal de la table `notes` — sous-ensemble strict de la migration 0001
/// couvrant exactement les colonnes lues/écrites par `backfill-authors`.
fn create_schema(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE notes (
            id                  TEXT    NOT NULL,
            vault_id            TEXT    NOT NULL,
            locus               TEXT,
            section             TEXT    NOT NULL,
            status              TEXT    NOT NULL,
            author_kind         TEXT,
            author_id           TEXT,
            author_display_name TEXT,
            created             INTEGER NOT NULL,
            updated             INTEGER,
            content_hash        BLOB    NOT NULL,
            body_text           TEXT    NOT NULL,
            PRIMARY KEY (vault_id, id)
        );",
    )
    .expect("créer schéma notes");
}

/// Layout canonique : `<root>/vault/.gradatum/index.db` + `<root>/config/bearer.toml`.
fn setup(root: &Path) -> rusqlite::Connection {
    let gradatum = root.join("vault").join(".gradatum");
    std::fs::create_dir_all(&gradatum).expect("mkdir vault/.gradatum");
    std::fs::create_dir_all(root.join("config")).expect("mkdir config");

    // Preset ACL minimal : seule l'identité `main-agent` est déclarée.
    std::fs::write(
        root.join("config").join("bearer.toml"),
        "[[consumer]]\n\
         identity = \"main-agent\"\n\
         description = \"Orchestrator\"\n\
         read_patterns = [\"**\"]\n\
         write_patterns = [\"**\"]\n\
         sees_personal_classified = false\n",
    )
    .expect("écrire preset");

    let conn = rusqlite::Connection::open(gradatum.join("index.db")).expect("open index.db");
    create_schema(&conn);
    conn
}

/// Réouvre l'index.db du layout pour vérification.
fn open_index(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join("vault").join(".gradatum").join("index.db"))
        .expect("réouvrir index.db")
}

/// Corps `.md` valide SANS auteur (chemin nominal des 585 notes).
fn md_without_author(vault: &str, section: &str) -> String {
    format!(
        "---\n\
         schema_version: 1\n\
         vault_id: {vault}\n\
         section: {section}\n\
         status: live\n\
         created: \"2026-05-04T11:00:00Z\"\n\
         ---\n\n\
         # Titre\n\n\
         Corps de la note.\n"
    )
}

/// Corps `.md` valide AVEC un auteur humain déjà présent.
fn md_with_author(vault: &str, section: &str, kind: &str, id: &str) -> String {
    format!(
        "---\n\
         schema_version: 1\n\
         vault_id: {vault}\n\
         section: {section}\n\
         status: live\n\
         author:\n  \
         kind: {kind}\n  \
         id: {id}\n\
         created: \"2026-05-04T11:00:00Z\"\n\
         ---\n\n\
         # Titre\n\n\
         Corps de la note.\n"
    )
}

/// Insère une ligne d'index cohérente avec le `.md` écrit sur disque, et pose le `.md`
/// au chemin `rel` (relatif au répertoire `vault/`). `content_hash` est recalculé depuis
/// le `.md` (parse), pour un état de départ index==disque non ambigu.
#[expect(
    clippy::too_many_arguments,
    reason = "fabrique de test lisible — regrouper en struct n'apporte rien ici"
)]
fn seed_note(
    conn: &rusqlite::Connection,
    root: &Path,
    id: &str,
    vault: &str,
    section: &str,
    locus: Option<&str>,
    author_id: Option<&str>,
    rel: &str,
    md: &str,
) {
    let parsed = gradatum_markdown::parse(md).expect("parse md seed");
    let hash = parsed.content_hash.0.to_vec();

    let author_kind = author_id.map(|_| "human");
    conn.execute(
        "INSERT INTO notes (id, vault_id, locus, section, status, author_kind, author_id,
                            author_display_name, created, updated, content_hash, body_text)
         VALUES (?1, ?2, ?3, ?4, 'live', ?5, ?6, NULL, 0, NULL, ?7, ?8)",
        rusqlite::params![
            id,
            vault,
            locus,
            section,
            author_kind,
            author_id,
            hash,
            parsed.body.markdown,
        ],
    )
    .expect("insert note");

    let path = root.join("vault").join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir note dir");
    std::fs::write(&path, md).expect("write md");
}

fn args(root: &Path, author: &str, dry_run: bool) -> BackfillAuthorsArgs {
    BackfillAuthorsArgs {
        root: root.to_path_buf(),
        tenant: "main".to_string(),
        author: author.to_string(),
        dry_run,
        limit: None,
    }
}

// ── Lectures de vérification ────────────────────────────────────────────────

fn index_author(conn: &rusqlite::Connection, id: &str) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT author_kind, author_id FROM notes WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .expect("lire author index")
}

fn index_hash(conn: &rusqlite::Connection, id: &str) -> Vec<u8> {
    conn.query_row(
        "SELECT content_hash FROM notes WHERE id = ?1",
        rusqlite::params![id],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .expect("lire content_hash index")
}

fn read_md(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join("vault").join(rel)).expect("relire md")
}

// ── 1. Attribution nominale ─────────────────────────────────────────────────

#[tokio::test]
async fn assigns_author_to_note_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    seed_note(
        &conn,
        dir.path(),
        "01AAAA000000000000000000A1",
        "main",
        "decisions",
        None,
        None,
        "main/01AAAA000000000000000000A1.md",
        &md_without_author("main", "decisions"),
    );
    drop(conn);

    let report = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(report.notes_scanned, 1);
    assert_eq!(report.authors_assigned, 1);
    assert_eq!(report.skipped_drift, 0);

    let conn = open_index(dir.path());
    let (kind, id) = index_author(&conn, "01AAAA000000000000000000A1");
    assert_eq!(kind.as_deref(), Some("main-agent"), "kind index");
    assert_eq!(id.as_deref(), Some("main-agent"), "id index");

    let md = read_md(dir.path(), "main/01AAAA000000000000000000A1.md");
    assert!(md.contains("author:"), "frontmatter porte un auteur: {md}");
    assert!(
        md.contains("main-agent"),
        "frontmatter porte main-agent: {md}"
    );
}

// ── 2. Note déjà attribuée : intacte ────────────────────────────────────────

#[tokio::test]
async fn leaves_note_with_existing_author_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    seed_note(
        &conn,
        dir.path(),
        "01BBBB000000000000000000B1",
        "main",
        "decisions",
        None,
        Some("alice"),
        "main/01BBBB000000000000000000B1.md",
        &md_with_author("main", "decisions", "human", "alice"),
    );
    let hash_before = index_hash(&conn, "01BBBB000000000000000000B1");
    let md_before = read_md(dir.path(), "main/01BBBB000000000000000000B1.md");
    drop(conn);

    let report = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(report.notes_scanned, 0, "aucune candidate");
    assert_eq!(report.authors_assigned, 0);

    let conn = open_index(dir.path());
    let (_, id) = index_author(&conn, "01BBBB000000000000000000B1");
    assert_eq!(id.as_deref(), Some("alice"), "auteur existant préservé");
    assert_eq!(index_hash(&conn, "01BBBB000000000000000000B1"), hash_before);
    assert_eq!(
        read_md(dir.path(), "main/01BBBB000000000000000000B1.md"),
        md_before,
        ".md non réécrit"
    );
}

// ── 3. Identité inconnue : refus ────────────────────────────────────────────

#[tokio::test]
async fn refuses_unknown_identity() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    seed_note(
        &conn,
        dir.path(),
        "01CCCC000000000000000000C1",
        "main",
        "decisions",
        None,
        None,
        "main/01CCCC000000000000000000C1.md",
        &md_without_author("main", "decisions"),
    );
    drop(conn);

    let err = run(args(dir.path(), "ghost", false)).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ghost") && msg.to_lowercase().contains("identit"),
        "message doit nommer l'identité refusée : {msg}"
    );

    // Rien n'a été écrit : la note reste sans auteur.
    let conn = open_index(dir.path());
    let (_, id) = index_author(&conn, "01CCCC000000000000000000C1");
    assert_eq!(id, None, "identité refusée ⇒ aucune écriture");
}

// ── 4. Idempotence : relancer ne change rien ────────────────────────────────

#[tokio::test]
async fn rerun_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    seed_note(
        &conn,
        dir.path(),
        "01DDDD000000000000000000D1",
        "main",
        "decisions",
        None,
        None,
        "main/01DDDD000000000000000000D1.md",
        &md_without_author("main", "decisions"),
    );
    drop(conn);

    let r1 = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(r1.authors_assigned, 1);

    let hash_after_first = index_hash(&open_index(dir.path()), "01DDDD000000000000000000D1");

    let r2 = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(r2.notes_scanned, 0, "2e passage : plus aucune candidate");
    assert_eq!(r2.authors_assigned, 0);

    assert_eq!(
        index_hash(&open_index(dir.path()), "01DDDD000000000000000000D1"),
        hash_after_first,
        "hash inchangé au 2e passage"
    );
}

// ── 5. Cohérence frontmatter ⇔ index après passage ──────────────────────────

#[tokio::test]
async fn frontmatter_and_index_agree_after_pass() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    let rel = "main/01EEEE000000000000000000E1.md";
    seed_note(
        &conn,
        dir.path(),
        "01EEEE000000000000000000E1",
        "main",
        "decisions",
        None,
        None,
        rel,
        &md_without_author("main", "decisions"),
    );
    drop(conn);

    let _report = run(args(dir.path(), "main-agent", false)).await.unwrap();

    // Re-parse du `.md` réécrit.
    let md = read_md(dir.path(), rel);
    let parsed = gradatum_markdown::parse(&md).expect("parse md final");
    let fm_author = parsed.frontmatter.author.expect("frontmatter a un auteur");
    assert_eq!(fm_author.id, "main-agent");

    let conn = open_index(dir.path());
    let (kind, id) = index_author(&conn, "01EEEE000000000000000000E1");
    // Le frontmatter et l'index nomment le même auteur.
    assert_eq!(id.as_deref(), Some(fm_author.id.as_str()));
    assert_eq!(kind.as_deref(), Some("main-agent"));

    // content_hash index == hash recalculé depuis le `.md` (drift-check cohérent).
    assert_eq!(
        index_hash(&conn, "01EEEE000000000000000000E1"),
        parsed.content_hash.0.to_vec(),
        "content_hash index doit matcher le .md réécrit"
    );
}

// ── 6. Dry-run : aucun effet ────────────────────────────────────────────────

#[tokio::test]
async fn dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    let rel = "main/01FFFF000000000000000000F1.md";
    seed_note(
        &conn,
        dir.path(),
        "01FFFF000000000000000000F1",
        "main",
        "decisions",
        None,
        None,
        rel,
        &md_without_author("main", "decisions"),
    );
    let hash_before = index_hash(&conn, "01FFFF000000000000000000F1");
    let md_before = read_md(dir.path(), rel);
    drop(conn);

    let report = run(args(dir.path(), "main-agent", true)).await.unwrap();
    assert!(report.dry_run);
    assert_eq!(report.notes_scanned, 1, "candidate comptée");
    // Preview : la note SERAIT attribuée, mais rien n'est écrit (vérifié plus bas).
    assert_eq!(report.authors_assigned, 1, "dry-run compte le would-assign");

    let conn = open_index(dir.path());
    assert_eq!(index_author(&conn, "01FFFF000000000000000000F1").1, None);
    assert_eq!(index_hash(&conn, "01FFFF000000000000000000F1"), hash_before);
    assert_eq!(read_md(dir.path(), rel), md_before, ".md intact en dry-run");
}

// ── 7. Auteur différent dans le `.md` : ne pas clobberer ────────────────────

#[tokio::test]
async fn skips_note_whose_md_already_carries_a_different_author() {
    // État de dérive : index author_id NULL (⇒ candidate) mais le `.md` porte déjà
    // un auteur DIFFÉRENT de la cible. Ne jamais écraser un auteur existant.
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    let rel = "main/01GGGG000000000000000000G1.md";
    seed_note(
        &conn,
        dir.path(),
        "01GGGG000000000000000000G1",
        "main",
        "decisions",
        None,
        None, // index vide
        rel,
        &md_with_author("main", "decisions", "human", "alice"), // .md rempli
    );
    let md_before = read_md(dir.path(), rel);
    drop(conn);

    let report = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(report.notes_scanned, 1);
    assert_eq!(report.authors_assigned, 0, "auteur différent ⇒ non touché");
    assert_eq!(report.skipped_drift, 1);

    assert_eq!(read_md(dir.path(), rel), md_before, ".md non clobberé");
    let conn = open_index(dir.path());
    assert_eq!(index_author(&conn, "01GGGG000000000000000000G1").1, None);
}

// ── 8. Layout legacy `<vault>/<section>/<id>.md` : réécrit au MÊME chemin ────

#[tokio::test]
async fn rewrites_legacy_section_path_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    // Note physiquement sous `<vault>/<section>/<id>.md`, sans locus en index.
    let rel = "main/decisions/01HHHH000000000000000000H1.md";
    seed_note(
        &conn,
        dir.path(),
        "01HHHH000000000000000000H1",
        "main",
        "decisions",
        None,
        None,
        rel,
        &md_without_author("main", "decisions"),
    );
    drop(conn);

    let report = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(report.authors_assigned, 1);

    // Réécrit au MÊME chemin (pas d'orphelin à la racine tenant).
    let md = read_md(dir.path(), rel);
    assert!(md.contains("main-agent"), "auteur au chemin legacy");
    assert!(
        !dir.path()
            .join("vault/main/01HHHH000000000000000000H1.md")
            .exists(),
        "aucun .md orphelin créé à la racine tenant"
    );
}

// ── 9. Reprise après écriture `.md` partielle ───────────────────────────────

#[tokio::test]
async fn resumes_when_md_written_but_index_not() {
    // Simule une interruption : le `.md` porte déjà la cible mais l'index est resté
    // NULL. La sélection (index author_id NULL) doit ré-attraper la note et solder
    // l'index — le `.md` étant réécrit à l'identique (idempotent).
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path());
    let rel = "main/01IIII000000000000000000I1.md";
    seed_note(
        &conn,
        dir.path(),
        "01IIII000000000000000000I1",
        "main",
        "decisions",
        None,
        None, // index NULL (pas encore soldé)
        rel,
        &md_with_author("main", "decisions", "main-agent", "main-agent"), // .md déjà écrit
    );
    drop(conn);

    let report = run(args(dir.path(), "main-agent", false)).await.unwrap();
    assert_eq!(report.notes_scanned, 1);
    assert_eq!(
        report.authors_assigned, 1,
        "note ré-attrapée et index soldé"
    );

    let conn = open_index(dir.path());
    assert_eq!(
        index_author(&conn, "01IIII000000000000000000I1")
            .1
            .as_deref(),
        Some("main-agent")
    );
}
