//! F-160 — the `extra.privacy` block a detection pass records on the note it scanned.
//!
//! ## What these tests measure
//!
//! The block is the only durable evidence that a pass ran, and the only input a later
//! review or re-scan reads back. Four properties are load-bearing:
//!
//! - **Round trip.** The block survives YAML serialisation key by key. This is proved
//!   before anything else: `extra` is a `BTreeMap<String, toml::Value>` crossing a YAML
//!   writer and parser, and no existing test covered that shape (`gradatum-core`'s
//!   round-trip test feeds JSON, the v1-parity one is `#[ignore]`).
//! - **Presence of `counts`.** A block always carries `counts`, empty or not: a pass that
//!   found nothing and a pass that never ran are different states, and a missing key
//!   would collapse them.
//! - **Integer thousandths.** `score_m` is a `u16` in thousandths. `extra` carries no
//!   float, and the content hash has zero tolerance for a re-serialiser that would render
//!   `0.94` as `0.9399999976158142`.
//! - **Transit.** The pass merges its block into whatever `extra` the write carried: a
//!   rewrite through the compare-and-swap does not drop the note's other extra keys.
//!
//! The ceiling case is measured too: above `MAX_SPANS` the block is marked truncated and
//! the note is marked unscanned, because the span list is no longer the whole picture.

mod common;
use common::build_minimal_frontmatter;

use std::path::Path;
use std::sync::Arc;

use gradatum_core::frontmatter::ExtraFields;
use gradatum_core::identity::NoteId;
use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_vault::{Vault, WriteResult};
use tempfile::TempDir;

/// The product ceiling on the spans a block carries.
const MAX_SPANS: usize = 256;

// ── Detectors ────────────────────────────────────────────────────────────────

/// Reports a fixed span list, in the coordinates of the window it is handed.
///
/// The window is the whole body in these tests, so the coordinates are body
/// coordinates; every span must still fall inside the body, since validation rejects
/// what it cannot apply.
struct FixedSpans(Vec<PiiSpan>);

impl PiiDetector for FixedSpans {
    fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(self.0.clone(), 0))
    }
}

/// Reports one span covering the whole body.
struct AnySpan(PiiCategory, f32);

impl PiiDetector for AnySpan {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(Detection::new(
            vec![PiiSpan::new(0, input.text.len(), self.0, self.1)],
            0,
        ))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fm_for(vault: &str) -> gradatum_core::frontmatter::Frontmatter {
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = VaultId::new(vault);
    fm.section = Section::Decisions;
    fm.status = NoteStatus::Live;
    fm
}

/// A vault whose detector is `detector`, under the delivered budgets.
async fn vault_with(root: &Path, detector: Arc<dyn PiiDetector>) -> Vault {
    std::fs::create_dir_all(root).expect("vault root");
    Vault::create(root, VaultId::new("main"))
        .await
        .expect("vault creation")
        .with_detector(detector, PrivacyBudgets::default())
}

/// The `privacy` block read back from a note, as a TOML table.
fn block_of(note: &gradatum_core::note::Note) -> toml::value::Table {
    note.frontmatter
        .extra
        .get("privacy")
        .unwrap_or_else(|| panic!("the note must carry an `extra.privacy` block"))
        .as_table()
        .expect("the block is a table")
        .clone()
}

/// Reads one string field of a block.
fn string_field(block: &toml::value::Table, key: &str) -> String {
    block
        .get(key)
        .unwrap_or_else(|| panic!("key `{key}` must be present"))
        .as_str()
        .unwrap_or_else(|| panic!("key `{key}` must be a string"))
        .to_owned()
}

// ── (a) round trip ───────────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_block_read_back_is_identical_to_block_written() {
    let dir = TempDir::new().unwrap();
    let body = "contact alice@example.org maintenant".to_string();
    // The e-mail literal sits at 8..25 of the body.
    let spans = vec![PiiSpan::new(8, 17, PiiCategory::Email, 0.94)];
    let vault = vault_with(dir.path(), Arc::new(FixedSpans(spans))).await;

    let note = vault.write_note(fm_for("main"), body).await.expect("write");
    let read_back = vault.read_note(note.id).await.expect("read back");

    let block = block_of(&read_back);

    // The key set is exactly §4.1's: nothing missing, nothing invented.
    let mut keys: Vec<&str> = block.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "counts",
            "detector",
            "mapping_v",
            "mode",
            "model_digest",
            "model_source",
            "policy",
            "review",
            "scanned_at",
            "score_mode",
            "sidecar_v",
            "spans",
            "spans_digest",
            "status",
            "v",
        ],
        "the block carries exactly the modelled keys"
    );

    assert_eq!(block["v"].as_integer(), Some(1), "v: 1");
    assert_eq!(
        string_field(&block, "mode"),
        "report_only",
        "the delivered mode is report_only"
    );
    assert_eq!(string_field(&block, "status"), "ok", "the pass completed");
    assert_eq!(
        string_field(&block, "review"),
        "pending",
        "a fresh block awaits review"
    );
    assert_eq!(
        block["mapping_v"].as_integer(),
        Some(1),
        "mapping_v is an integer, like score_m"
    );

    // The only value the test cannot predict is the pass instant; it is a string,
    // never a TOML datetime, and it is RFC 3339.
    let scanned_at = string_field(&block, "scanned_at");
    let parsed = chrono::DateTime::parse_from_rfc3339(&scanned_at).unwrap_or_else(|e| {
        panic!("scanned_at must be an RFC 3339 string, got {scanned_at:?}: {e}")
    });
    assert_eq!(parsed.offset().local_minus_utc(), 0, "recorded in UTC");

    // `policy` and `spans_digest` are `sha256:` digests, never raw material.
    for key in ["policy", "spans_digest"] {
        let value = string_field(&block, key);
        assert!(
            value.starts_with("sha256:") && value.len() == "sha256:".len() + 64,
            "{key} is a sha256 digest, got {value:?}"
        );
    }

    // The spans, key by key.
    let spans_read = block["spans"].as_array().expect("spans is an array");
    assert_eq!(spans_read.len(), 1, "one span was reported");
    let span = spans_read[0].as_table().expect("a span is a table");
    assert_eq!(
        span["start"].as_integer(),
        Some(8),
        "byte offset of the span"
    );
    assert_eq!(span["len"].as_integer(), Some(17), "span length");
    assert_eq!(
        span["category"].as_str(),
        Some("email"),
        "the category is written by its canonical label"
    );
    assert_eq!(
        span["score_m"].as_integer(),
        Some(940),
        "0.94 in thousandths"
    );

    // The counts agree with the spans.
    let counts = block["counts"].as_table().expect("counts is a table");
    assert_eq!(counts["email"].as_integer(), Some(1));
}

// ── (b) counts is always present ─────────────────────────────────────────────

#[tokio::test]
async fn counts_is_present_even_when_empty() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(FixedSpans(Vec::new()))).await;

    let note = vault
        .write_note(fm_for("main"), "no personal data here".into())
        .await
        .expect("write");
    let read_back = vault.read_note(note.id).await.expect("read back");
    let block = block_of(&read_back);

    let counts = block
        .get("counts")
        .expect("a scan that found nothing still records an empty `counts`")
        .as_table()
        .expect("counts is a table");
    assert!(
        counts.is_empty(),
        "nothing was found, so nothing is counted"
    );
    assert_eq!(
        string_field(&block, "status"),
        "ok",
        "an empty result is a completed pass, not an incomplete one"
    );
    assert_eq!(
        block["spans"].as_array().map(Vec::len),
        Some(0),
        "the span list is empty, and present"
    );
}

// ── (c) the digest covers the triplets, never the scores ─────────────────────

#[tokio::test]
async fn spans_digest_excludes_scores() {
    let dir = TempDir::new().unwrap();
    let body = "un corps de longueur suffisante".to_string();

    // Same offsets and category, two different scores.
    let low = vault_with(
        &dir.path().join("low"),
        Arc::new(FixedSpans(vec![PiiSpan::new(
            3,
            5,
            PiiCategory::Email,
            0.10,
        )])),
    )
    .await;
    let high = vault_with(
        &dir.path().join("high"),
        Arc::new(FixedSpans(vec![PiiSpan::new(
            3,
            5,
            PiiCategory::Email,
            0.99,
        )])),
    )
    .await;

    let a = low
        .write_note(fm_for("main"), body.clone())
        .await
        .expect("write low");
    let b = high
        .write_note(fm_for("main"), body.clone())
        .await
        .expect("write high");

    let digests = [
        string_field(
            &block_of(&low.read_note(a.id).await.unwrap()),
            "spans_digest",
        ),
        string_field(
            &block_of(&high.read_note(b.id).await.unwrap()),
            "spans_digest",
        ),
    ];
    assert_eq!(
        digests[0], digests[1],
        "the score is excluded from the digest: a re-pass of the same model on the same \
         body must not reset the review"
    );

    // A different category over the same bytes is a different triplet.
    let other = vault_with(
        &dir.path().join("other"),
        Arc::new(FixedSpans(vec![PiiSpan::new(
            3,
            5,
            PiiCategory::Phone,
            0.10,
        )])),
    )
    .await;
    let c = other
        .write_note(fm_for("main"), body)
        .await
        .expect("write other");
    let other_digest = string_field(
        &block_of(&other.read_note(c.id).await.unwrap()),
        "spans_digest",
    );
    assert_ne!(
        digests[0], other_digest,
        "the category is part of the triplet"
    );
}

// ── (d) no float ever reaches `extra` ────────────────────────────────────────

#[tokio::test]
async fn score_m_is_an_integer_never_a_float() {
    let dir = TempDir::new().unwrap();
    // 0.9399 is the kind of value a widening to f64 would render as
    // 0.93990000000000000213... — the digest tolerance is zero.
    let vault = vault_with(dir.path(), Arc::new(AnySpan(PiiCategory::Secret, 0.9399))).await;

    let note = vault
        .write_note(fm_for("main"), "un secret quelconque".into())
        .await
        .expect("write");
    let read_back = vault.read_note(note.id).await.expect("read back");
    let block = block_of(&read_back);

    let span = block["spans"]
        .as_array()
        .expect("spans is an array")
        .first()
        .expect("one span")
        .as_table()
        .expect("a span is a table");
    let score_m = span
        .get("score_m")
        .expect("score_m must be present")
        .as_integer()
        .expect("score_m is an integer, never a float");
    assert_eq!(score_m, 940, "0.9399 rounds to 940 thousandths");

    // Stronger: no float survives anywhere in the block.
    fn find_float(value: &toml::Value) -> Option<String> {
        match value {
            toml::Value::Float(f) => Some(f.to_string()),
            toml::Value::Array(items) => items.iter().find_map(find_float),
            toml::Value::Table(map) => map.values().find_map(find_float),
            _ => None,
        }
    }
    assert_eq!(
        find_float(&toml::Value::Table(block)),
        None,
        "`extra` carries no float: a float would break the content hash on the first \
         re-serialisation that renders it differently"
    );
}

// ── (e) the ceiling marks the block and the note ─────────────────────────────

#[tokio::test]
async fn spans_above_max_spans_yield_spans_truncated_and_unscanned() {
    let dir = TempDir::new().unwrap();
    // 300 one-byte spans, none overlapping (fusion unions overlaps, not neighbours).
    let body = "a".repeat(400);
    let spans: Vec<PiiSpan> = (0..300)
        .map(|i| PiiSpan::new(i, 1, PiiCategory::Email, 0.5))
        .collect();
    let vault = vault_with(dir.path(), Arc::new(FixedSpans(spans))).await;

    let note = vault.write_note(fm_for("main"), body).await.expect("write");
    let read_back = vault.read_note(note.id).await.expect("read back");
    let block = block_of(&read_back);

    assert_eq!(
        string_field(&block, "status"),
        "spans_truncated",
        "past the ceiling the span list is no longer the whole picture"
    );
    assert_eq!(
        block["spans"].as_array().map(Vec::len),
        Some(MAX_SPANS),
        "the span list is cut at the product ceiling"
    );
    assert_eq!(
        block["counts"]["email"].as_integer(),
        Some(300),
        "the counts are the authority above the ceiling: they cover the whole pass"
    );

    assert!(
        read_back
            .frontmatter
            .tags
            .iter()
            .any(|tag| tag.as_str() == "privacy-unscanned"),
        "a truncated block marks the note unscanned, so a rescan can find it"
    );
}

// ── (f) transit through the read-modify-write path ───────────────────────────

#[tokio::test]
async fn extra_transits_through_the_rmw_path() {
    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(AnySpan(PiiCategory::Email, 1.0))).await;

    // The note is written once with an extra key that is none of the pass's business.
    let mut carrier = toml::value::Table::new();
    carrier.insert("kind".into(), toml::Value::String("code-symbol".into()));
    carrier.insert("depth".into(), toml::Value::Integer(3));
    let mut first = fm_for("main");
    let mut extra = ExtraFields::empty();
    extra.insert("cs".into(), toml::Value::Table(carrier.clone()));
    first.extra = extra;

    let id = NoteId::new();
    let v1 = vault
        .write_note_with_id(first, "corps v1".into(), id)
        .await
        .expect("create");

    // The curate rewrite builds its frontmatter from scratch — the shape the curated
    // persist path posts — and therefore carries no `extra` of its own. The transit
    // source is the note on disk, never an echo of what the caller re-posted.
    let rewritten = vault
        .write_if_match(
            fm_for("main"),
            "corps v2".into(),
            id,
            Some(v1.content_hash.0),
        )
        .await
        .expect("the current hash is accepted");
    assert!(
        matches!(rewritten, WriteResult::Written { .. }),
        "the rewrite must take effect"
    );

    let read_back = vault.read_note(id).await.expect("read back");
    assert_eq!(
        read_back.frontmatter.extra.get("cs"),
        Some(&toml::Value::Table(carrier)),
        "a rewrite does not drop the extra keys the pass does not recompute"
    );
    let block = block_of(&read_back);
    assert_eq!(
        string_field(&block, "status"),
        "ok",
        "the rewritten note carries the block of the second pass"
    );
}

// ── (g) the block is excluded from the history hash ──────────────────────────

#[tokio::test]
async fn privacy_is_excluded_from_history_hashing() {
    assert!(
        gradatum_core::history::HISTORY_EXCLUDED_FIELDS.contains(&"privacy"),
        "`scanned_at` changes on every pass: without this exclusion a re-scan would \
         snapshot a copy of the body for an unchanged note"
    );

    let dir = TempDir::new().unwrap();
    let vault = vault_with(dir.path(), Arc::new(FixedSpans(Vec::new()))).await;

    let note = vault
        .write_note(fm_for("main"), "corps identique".into())
        .await
        .expect("write");
    let written = vault.read_note(note.id).await.expect("read back");
    assert!(
        written.frontmatter.extra.get("privacy").is_some(),
        "the note under test does carry the block"
    );

    // The same note, with the block taken out of its frontmatter and nothing else
    // touched: `created`, `updated` and the body are the identical values of one
    // single write, so the block is the only difference the hash can see.
    let without_block = {
        let mut stripped = written.clone();
        stripped.frontmatter.extra = ExtraFields::empty();
        stripped
    };

    assert_eq!(
        gradatum_core::history::sha256_for_history(&written),
        gradatum_core::history::sha256_for_history(&without_block),
        "the block is invisible to the history hash: a re-scan of unchanged content \
         must not snapshot the body"
    );
}
