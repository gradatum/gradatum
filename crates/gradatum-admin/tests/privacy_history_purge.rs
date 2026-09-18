//! Tests d'intégration — sub-commande `privacy history-purge` (F-160).
//!
//! Stratégie : monter une arborescence TempDir à la main, avec les notes et leurs
//! instantanés de version tels que le chemin d'écriture les dépose (`<vault>/.history/<id>/`),
//! puis appeler la commande et mesurer **sur les fichiers** ce qu'elle a détruit. Les
//! attendus sont comptés à partir de la fixture (nombre d'instantanés écrits par note,
//! octets lus sur le disque), jamais supposés depuis une constante du test.

use std::path::{Path, PathBuf};

use chrono::Utc;
use gradatum_admin::privacy_cmd::{self, PrivacyHistoryPurgeArgs};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use tempfile::TempDir;

/// Clé du bloc de confidentialité dans `frontmatter.extra`.
const BLOCK_KEY: &str = "privacy";

/// Texte porté par chaque instantané, pour que la survie comme la disparition se lisent
/// sur les octets et pas seulement sur un compteur.
const SNAPSHOT_BODY: &str = "texte en clair d'une version antérieure";

/// Un instantané écrit par la fixture : son chemin et sa taille sur le disque.
struct Snapshot {
    path: PathBuf,
    bytes: u64,
}

/// Un `Frontmatter` minimal valide — les champs optionnels au défaut.
fn minimal_frontmatter() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
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

/// Écrit une note `.md` dans `<root>/<relative_dir>/<id>.md` — portant ou non un bloc
/// `extra.privacy` de mode `mode` — et lui donne `history` instantanés de version.
///
/// Le `.md` passe par le writer réel : ce que la commande relit est ce que le produit
/// écrit, et non un YAML composé pour la circonstance.
fn write_fixture_note(
    root: &Path,
    relative_dir: &str,
    mode: Option<&str>,
    history: usize,
) -> (String, Vec<Snapshot>) {
    let id = NoteId::new();
    let id_str = id.to_string();

    let mut frontmatter = minimal_frontmatter();
    if let Some(mode) = mode {
        let mut block = toml::value::Table::new();
        block.insert("v".to_owned(), toml::Value::Integer(1));
        block.insert("mode".to_owned(), toml::Value::String(mode.to_owned()));
        block.insert("status".to_owned(), toml::Value::String("ok".to_owned()));
        frontmatter
            .extra
            .insert(BLOCK_KEY.to_owned(), toml::Value::Table(block));
    }

    let body = "corps de la note".to_owned();
    let note = Note {
        id,
        content_hash: ContentHash::compute(&frontmatter, &body),
        frontmatter,
        body: NoteBody { markdown: body },
        version: NoteVersion::initial(),
        integrity_signature: None,
    };
    let rendered = gradatum_markdown::write(&note).expect("rendu de la note");

    let dir = root.join(relative_dir);
    std::fs::create_dir_all(&dir).expect("arborescence de la note");
    std::fs::write(dir.join(format!("{id_str}.md")), rendered).expect("écriture de la note");

    // Les instantanés sont sous la racine du vault, jamais sous le locus : même règle que
    // le chemin d'écriture (`.history/` reste adressable si le locus change).
    let vault = relative_dir.split('/').next().unwrap_or("main");
    let history_dir = root.join(vault).join(".history").join(&id_str);
    let mut snapshots = Vec::new();
    if history > 0 {
        std::fs::create_dir_all(&history_dir).expect("répertoire d'historique");
        for i in 0..history {
            // Timestamps croissants et distincts : les noms de fichiers sont ce que le
            // compte rendu additionne, et deux instantanés homonymes seraient un seul.
            let path = history_dir.join(format!("{}.md", 1_700_000_000_000i64 + i as i64));
            let body = format!("---\n# {SNAPSHOT_BODY}\n---\n\n{SNAPSHOT_BODY} {i}");
            std::fs::write(&path, &body).expect("écriture de l'instantané");
            let bytes = std::fs::metadata(&path).expect("métadonnées").len();
            snapshots.push(Snapshot { path, bytes });
        }
    }
    (id_str, snapshots)
}

/// Tous les fichiers réguliers sous `dir`, récursivement.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Le nombre d'octets que les instantanés donnés occupent sur le disque.
fn total_bytes(snapshots: &[Snapshot]) -> u64 {
    snapshots.iter().map(|s| s.bytes).sum()
}

/// Exécute la commande sur `root`.
fn purge(root: &Path, dry_run: bool) -> privacy_cmd::PrivacyHistoryPurgeReport {
    privacy_cmd::run(&PrivacyHistoryPurgeArgs {
        root: root.to_path_buf(),
        dry_run,
    })
    .expect("history-purge")
}

// ── Ce que la commande détruit, et ce qu'elle laisse ─────────────────────────

/// Trois notes du même vault, trois situations : une note que le mode a réécrite (bloc
/// `mode = "mask"`), une note observée (`report_only`), une note jamais passée. La
/// commande ne doit toucher que la première — et le contrôle porte sur les fichiers, pas
/// sur le compte rendu seul.
#[test]
fn privacy_purge_destroys_only_the_history_of_the_notes_the_masking_mode_rewrote() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();

    let (masked_id, masked_snapshots) = write_fixture_note(root, "main/decisions", Some("mask"), 3);
    let (reported_id, reported_snapshots) =
        write_fixture_note(root, "main/decisions", Some("report_only"), 2);
    let (never_id, never_snapshots) = write_fixture_note(root, "main/decisions", None, 1);

    assert_eq!(
        masked_snapshots.len(),
        3,
        "la fixture doit remettre trois instantanés à la commande"
    );

    let report = purge(root, false);

    assert_eq!(report.notes_purged, 1, "une note réécrite, une seule");
    assert_eq!(
        report.snapshots_destroyed, 3,
        "les trois instantanés de la note masquée"
    );
    assert_eq!(
        report.bytes_destroyed,
        total_bytes(&masked_snapshots),
        "les octets comptés sont ceux des fichiers réellement supprimés"
    );
    assert_eq!(
        report.notes_skipped, 0,
        "aucune note illisible dans la fixture"
    );

    // Mesure sur le disque : la note réécrite n'a plus d'instantané, les deux autres si.
    assert!(
        files_under(&root.join("main/.history").join(&masked_id)).is_empty(),
        "l'historique de la note réécrite doit être vide"
    );
    for snapshot in reported_snapshots.iter().chain(never_snapshots.iter()) {
        assert!(
            snapshot.path.exists(),
            "{} ne devait pas être touché",
            snapshot.path.display()
        );
    }
    assert!(
        !files_under(&root.join("main/.history").join(&reported_id)).is_empty(),
        "une note observée garde son historique"
    );
    assert!(
        !files_under(&root.join("main/.history").join(&never_id)).is_empty(),
        "une note jamais passée garde son historique"
    );
}

/// `--dry-run` rend le même compte et ne supprime rien : le compte est celui de ce qui
/// serait détruit, et chaque fichier est retrouvé intact.
#[test]
fn privacy_dry_run_reports_what_it_would_destroy_and_destroys_nothing() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();

    let (id, snapshots) = write_fixture_note(root, "main/decisions", Some("mask"), 2);
    let history_dir = root.join("main/.history").join(&id);

    let report = purge(root, true);

    assert_eq!(report.notes_purged, 1);
    assert_eq!(report.snapshots_destroyed, 2);
    assert_eq!(report.bytes_destroyed, total_bytes(&snapshots));
    assert_eq!(
        files_under(&history_dir).len(),
        2,
        "aucun instantané n'est supprimé en dry-run"
    );

    // Contrôle de non-vacuité : la même racine, sans dry-run, détruit bien ces fichiers.
    let real = purge(root, false);
    assert_eq!(real.notes_purged, 1);
    assert_eq!(real.snapshots_destroyed, 2);
    assert!(
        files_under(&history_dir).is_empty(),
        "l'exécution réelle détruit ce que le dry-run annonçait"
    );
}

/// Rejouer la commande ne détruit rien de plus et ne compte rien de plus : la seconde
/// exécution trouve un historique vide, pas un historique à refaire.
#[test]
fn privacy_second_run_finds_nothing_left_to_purge() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();
    write_fixture_note(root, "main/decisions", Some("mask"), 2);

    let first = purge(root, false);
    assert_eq!(first.snapshots_destroyed, 2);

    let second = purge(root, false);
    assert_eq!(second.notes_purged, 0);
    assert_eq!(second.snapshots_destroyed, 0);
    assert_eq!(second.bytes_destroyed, 0);
}

/// Une note masquée dont le `.md` est illisible n'est pas silencieusement conforme : elle
/// est comptée à part, et rien n'est détruit pour elle — sauter une note est un écart que
/// l'opérateur doit voir, pas un zéro qui ressemble à un corpus propre.
#[test]
fn privacy_unreadable_note_is_reported_and_left_alone() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();

    let (id, snapshots) = write_fixture_note(root, "main/decisions", Some("mask"), 1);
    std::fs::write(
        root.join("main/decisions").join(format!("{id}.md")),
        "pas de frontmatter du tout",
    )
    .expect("note illisible");

    let report = purge(root, false);

    assert_eq!(report.notes_purged, 0);
    assert_eq!(report.snapshots_destroyed, 0);
    assert_eq!(report.notes_skipped, 1, "l'écart est compté, pas tu");
    assert!(
        snapshots[0].path.exists(),
        "une note qu'on ne sait pas lire n'autorise aucune destruction"
    );
}

/// L'identifiant d'une note peut apparaître dans deux historiques (la note vivante et sa
/// copie archivée) : le mode lu sur l'une vaut pour l'identifiant, et les deux
/// répertoires sont détruits.
#[test]
fn privacy_every_history_directory_of_a_masked_note_is_purged() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();

    let (id, live) = write_fixture_note(root, "main/decisions", Some("mask"), 1);
    let archived_dir = root.join("main/.archive/main/.history").join(&id);
    std::fs::create_dir_all(&archived_dir).expect("historique archivé");
    let archived = archived_dir.join("1700000009999.md");
    std::fs::write(&archived, SNAPSHOT_BODY).expect("écriture");
    std::fs::create_dir_all(root.join("main/.archive/main/decisions")).expect("arborescence");
    std::fs::copy(
        root.join("main/decisions").join(format!("{id}.md")),
        root.join("main/.archive/main/decisions")
            .join(format!("{id}.md")),
    )
    .expect("copie de la note");

    let report = purge(root, false);

    assert_eq!(report.notes_purged, 1, "un identifiant, deux répertoires");
    assert_eq!(report.snapshots_destroyed, 2);
    assert!(!live[0].path.exists());
    assert!(!archived.exists());
}

/// Une racine sans `.history/` n'est pas une erreur : le compte est vide, et c'est la
/// réponse juste pour un corpus qui n'a jamais versionné de note.
#[test]
fn privacy_root_without_history_reports_zero() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();
    write_fixture_note(root, "main/decisions", Some("mask"), 0);

    let report = purge(root, false);
    assert_eq!(report.notes_purged, 0);
    assert_eq!(report.snapshots_destroyed, 0);
    assert_eq!(report.bytes_destroyed, 0);
    assert_eq!(report.notes_skipped, 0);
}

/// Une racine qui n'existe pas est refusée, pas rendue à zéro : un chemin fautif n'est
/// pas un corpus propre, et un compte vide se lirait comme tel.
#[test]
fn privacy_missing_root_is_an_error() {
    let tmp = TempDir::new().expect("TempDir");
    let missing = tmp.path().join("absent");
    let outcome = privacy_cmd::run(&PrivacyHistoryPurgeArgs {
        root: missing,
        dry_run: false,
    });
    assert!(outcome.is_err(), "une racine absente doit être une erreur");
}

/// A symlink planted in a history directory is neither followed nor removed.
///
/// `.history/<id>/` is a directory an operator can write to, and a purge that walked it
/// naively would follow a link out of the tree and destroy a file the command was never
/// pointed at. The fixture plants a link to a file outside the root; the run must destroy
/// the regular snapshot it finds, leave the link in place, and leave the target with its
/// bytes unchanged.
///
/// The counts are what make the "not followed" half measurable: a link counted as a
/// snapshot would report a destruction that did not happen, and a link deleted would
/// report one the operator never asked for.
#[test]
fn privacy_symlink_in_history_is_neither_followed_nor_removed() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path();

    // The target lives in its own directory, outside the scanned root: a file under
    // `root` could be reached by the walk without any link being followed, and the test
    // would then prove nothing about links.
    let outside = TempDir::new().expect("TempDir");
    let target = outside.path().join("cible-hors-arbre.md");
    const TARGET_BODY: &str = "contenu hors arbre, jamais designe par la commande";
    std::fs::write(&target, TARGET_BODY).expect("cible");

    let (id, snapshots) = write_fixture_note(root, "main/decisions", Some("mask"), 1);
    let history_dir = root.join("main").join(".history").join(&id);
    let link = history_dir.join("lien.md");
    std::os::unix::fs::symlink(&target, &link).expect("lien symbolique");

    let report = purge(root, false);

    assert_eq!(report.notes_purged, 1);
    assert_eq!(
        report.snapshots_destroyed, 1,
        "the regular snapshot is the only thing destroyed"
    );
    assert_eq!(report.bytes_destroyed, total_bytes(&snapshots));
    assert!(!snapshots[0].path.exists(), "l'instantané est détruit");
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "le lien est laissé en place"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("cible lisible"),
        TARGET_BODY,
        "la cible garde ses octets"
    );
}
