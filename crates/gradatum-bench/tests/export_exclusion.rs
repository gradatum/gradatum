//! Fixture bench: an export that excludes the two reserved privacy tags,
//! proven by a full Parquet round-trip.
//!
//! This is the bench that 3.0.0 will consume once the real export predicate
//! lands. The export predicate itself lives elsewhere (see the reference
//! implementation `is_excluded` below, deliberately local to this test);
//! nothing here is product surface. The artefact is a temporary Parquet file
//! (removed on drop) written and read back by the same `parquet` crate — no
//! external tool, no network.
//!
//! Run / inspect locally:
//!   cargo test -p gradatum-bench --test export_exclusion
//!
// F-160 ruling L-G : le prédicat d'export appartient à F-146/F-290 (hors 2.4.0) ;
// ce banc livre le test que 3.0.0 consommera, avec une implémentation de référence
// minimale du prédicat locale au test. Comptage par jeton exact : `personal-classified-2026`
// n'est PAS exclue, et une note dont seul le CORPS mentionne le tag n'est PAS exclue.

use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use parquet::data_type::{ByteArray, ByteArrayType};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RowAccessor;
use parquet::schema::parser::parse_message_type;

/// The two reserved tags F-160 excludes from export.
///
/// Matching is whole-token: a tag *equal* to one of these excludes its note,
/// so `personal-classified-2026` (a different token) is never excluded.
const RESERVED_TAGS: [&str; 2] = ["personal-classified", "privacy-unscanned"];

/// Separator packing a note's tag set into a single Parquet UTF-8 column.
///
/// US (`0x1F`) cannot occur inside a tag, so splitting the column back into a
/// tag set is unambiguous and preserves whole-token semantics.
const TAG_SEP: char = '\u{1f}';

/// A note as carried by the fixture export.
struct Note {
    id: &'static str,
    tags: Vec<&'static str>,
    body: &'static str,
}

/// A row read back from the Parquet artefact: `(id, tags, body)`.
type ExportRow = (String, Vec<String>, String);

/// The reference export predicate — **local to this bench**.
///
/// The production predicate belongs to F-146/F-290 and is intentionally not
/// imported here. A note is excluded from the export when its tag set contains
/// any reserved tag, matched as a whole token.
fn is_excluded(tags: &[&str]) -> bool {
    tags.iter().any(|t| RESERVED_TAGS.contains(t))
}

/// The fixture corpus.
///
/// It deliberately contains at least one note per reserved tag (so the
/// anti-vacuity guard below is meaningful), plus the two "must be kept" edge
/// cases: a note tagged `personal-classified-2026`, and a note whose body — not
/// its tags — mentions the reserved token verbatim.
fn fixture_notes() -> Vec<Note> {
    vec![
        Note {
            id: "n-plain",
            tags: vec!["work", "idea"],
            body: "an ordinary note",
        },
        Note {
            id: "n-pc",
            tags: vec!["work", "personal-classified"],
            body: "a medical record",
        },
        Note {
            id: "n-pu",
            tags: vec!["privacy-unscanned"],
            body: "not scanned yet",
        },
        Note {
            id: "n-pc2026",
            tags: vec!["personal-classified-2026"],
            body: "a retrospective note",
        },
        Note {
            id: "n-bodymention",
            tags: vec!["work"],
            body: "this note discusses personal-classified data in prose",
        },
    ]
}

/// Write every note to a Parquet file at `path`, in PLAIN/UNCOMPRESSED encoding.
///
/// No predicate is applied here: the caller decides which notes to write. Three
/// required UTF-8 columns — `id`, `tags` (packed with [`TAG_SEP`]), `body`.
///
/// # Errors
/// Returns any Parquet schema, I/O, or column-write error.
fn write_notes(path: &Path, notes: &[Note]) -> Result<(), Box<dyn Error>> {
    let schema = Arc::new(parse_message_type(
        "message note {
            REQUIRED BYTE_ARRAY id (UTF8);
            REQUIRED BYTE_ARRAY tags (UTF8);
            REQUIRED BYTE_ARRAY body (UTF8);
        }",
    )?);
    let props = Arc::new(WriterProperties::builder().build());
    let file = File::create(path)?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;
    let mut row_group = writer.next_row_group()?;

    let sep = TAG_SEP.to_string();
    let ids: Vec<ByteArray> = notes
        .iter()
        .map(|n| ByteArray::from(n.id.as_bytes()))
        .collect();
    let tags: Vec<ByteArray> = notes
        .iter()
        .map(|n| ByteArray::from(n.tags.join(&sep).as_bytes()))
        .collect();
    let bodies: Vec<ByteArray> = notes
        .iter()
        .map(|n| ByteArray::from(n.body.as_bytes()))
        .collect();

    for column in [&ids, &tags, &bodies] {
        let mut col_writer = row_group.next_column()?.ok_or("expected a column writer")?;
        col_writer
            .typed::<ByteArrayType>()
            .write_batch(column, None, None)?;
        col_writer.close()?;
    }
    row_group.close()?;
    writer.close()?;
    Ok(())
}

/// Read the Parquet file at `path` back into `(id, tags, body)` rows.
///
/// The `tags` column is split on [`TAG_SEP`] to recover the whole-token tag set.
///
/// # Errors
/// Returns any Parquet open, schema, or row-decode error.
fn read_back(path: &Path) -> Result<Vec<ExportRow>, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = SerializedFileReader::new(file)?;
    let mut rows = Vec::new();
    for row in reader.get_row_iter(None)? {
        let row = row?;
        let id = row.get_string(0)?.clone();
        let packed = row.get_string(1)?;
        let body = row.get_string(2)?.clone();
        let tags: Vec<String> = if packed.is_empty() {
            Vec::new()
        } else {
            packed.split(TAG_SEP).map(str::to_string).collect()
        };
        rows.push((id, tags, body));
    }
    Ok(rows)
}

/// True if any row carries a reserved tag, matched whole-token.
fn count_reserved(rows: &[ExportRow]) -> usize {
    rows.iter()
        .filter(|(_, tags, _)| tags.iter().any(|t| RESERVED_TAGS.contains(&t.as_str())))
        .count()
}

// Anti-vacuity guard (this IS the "red first" step, kept as a permanent
// sentinel). Without the predicate, the artefact MUST leak >= 1 row per reserved
// tag; otherwise the two green tests below would be green because the reader is
// broken, not because the predicate works.
#[test]
fn writing_without_the_predicate_leaks_both_reserved_tags() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_notes(tmp.path(), &fixture_notes()).unwrap();
    let rows = read_back(tmp.path()).unwrap();

    let pc = rows
        .iter()
        .filter(|(_, tags, _)| tags.iter().any(|t| t == "personal-classified"))
        .count();
    let pu = rows
        .iter()
        .filter(|(_, tags, _)| tags.iter().any(|t| t == "privacy-unscanned"))
        .count();

    assert!(
        pc >= 1,
        "baseline must leak >= 1 `personal-classified` row (else the round-trip is vacuous)"
    );
    assert!(
        pu >= 1,
        "baseline must leak >= 1 `privacy-unscanned` row (else the round-trip is vacuous)"
    );
}

#[test]
fn export_excludes_both_reserved_tags_read_back_from_parquet() {
    let kept: Vec<Note> = fixture_notes()
        .into_iter()
        .filter(|n| !is_excluded(&n.tags))
        .collect();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_notes(tmp.path(), &kept).unwrap();
    let rows = read_back(tmp.path()).unwrap();

    assert_eq!(
        count_reserved(&rows),
        0,
        "no exported row may carry a reserved tag"
    );
}

#[test]
fn export_keeps_personal_classified_2026_and_a_body_mentioning_the_tag() {
    let kept: Vec<Note> = fixture_notes()
        .into_iter()
        .filter(|n| !is_excluded(&n.tags))
        .collect();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_notes(tmp.path(), &kept).unwrap();
    let rows = read_back(tmp.path()).unwrap();

    let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
    assert!(
        ids.contains(&"n-pc2026"),
        "`personal-classified-2026` is a distinct token, not reserved: kept"
    );
    assert!(
        ids.contains(&"n-bodymention"),
        "a body mention is not a tag: kept"
    );

    let mention = rows
        .iter()
        .find(|(id, _, _)| id == "n-bodymention")
        .unwrap();
    assert!(
        mention.2.contains("personal-classified"),
        "the body mention survives export intact"
    );
}
