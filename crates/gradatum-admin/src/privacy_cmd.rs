//! Sub-command `privacy history-purge` — destroys the version history a masking
//! deployment may have left behind.
//!
//! ## Why a command exists at all
//!
//! A masking write destroys the snapshots that were already under `.history/<id>/`
//! (see the vault's write path): the mode is irreversible, and a note whose history still
//! describes what was removed is not masked. What that leaves uncovered is everything
//! written **before** the deployment entered `mask` — those snapshots hold the clear text
//! of notes the operator has since decided to rewrite, and no later write reaches them,
//! because a snapshot is only ever destroyed by a write that masks something.
//!
//! The command is therefore the operator's one-shot cleanup, run once after the switch.
//!
//! ## Which notes it selects
//!
//! The block a pass writes into `extra.privacy` records the mode that pass **ran under**.
//! A note whose block says `mode = "mask"` was judged by a masking deployment — that is
//! the durable evidence, and it is readable on disk without opening the index or the
//! database. The command makes no other inference: it never guesses a mode from a body,
//! from timestamps, or from the deployment's current configuration.
//!
//! ## What it does not do
//!
//! - It never touches a note the block does not identify as masked. A note still carrying
//!   `report_only`, or no block at all, keeps its history: its snapshots are plaintext
//!   copies of a body no pass has destroyed, and removing them is a decision this command
//!   has no evidence to make.
//! - It is not an erasure. It removes files under `.history/<id>/` — the markdown copies
//!   the write path keeps. Note bodies also live in `index.db`, in `queue.sqlite`, in the
//!   HTTP audit journal and in the embedding index, and none of those are reached here.
//! - It does not republish the note or touch its current version: only the history
//!   directories are inspected.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use walkdir::WalkDir;

/// Directory name the write path uses for a note's version history.
const HISTORY_DIR_NAME: &str = ".history";

/// Key of the privacy block in `frontmatter.extra`.
const BLOCK_KEY: &str = "privacy";

/// Mode a block records when the pass ran under a policy that rewrites bodies.
const MODE_MASK: &str = "mask";

/// `privacy` sub-commands.
#[derive(Debug, Subcommand)]
pub enum PrivacyCmd {
    /// Destroys the version history of the notes a masking deployment rewrote.
    #[command(name = "history-purge")]
    HistoryPurge(PrivacyHistoryPurgeArgs),
}

/// Arguments for `privacy history-purge`.
#[derive(Debug, Clone, Args)]
pub struct PrivacyHistoryPurgeArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,
    /// Reports what would be destroyed without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// Dispatches a `privacy` sub-command and prints its count.
///
/// # Errors
///
/// Propagates the error of the selected sub-command (see [`run`]).
pub fn dispatch(cmd: PrivacyCmd) -> Result<()> {
    match cmd {
        PrivacyCmd::HistoryPurge(args) => {
            let report = run(&args)?;
            println!(
                "privacy history-purge: notes={} snapshots={} bytes={}{}",
                report.notes_purged,
                report.snapshots_destroyed,
                report.bytes_destroyed,
                if args.dry_run { " (dry-run)" } else { "" },
            );
            if report.notes_skipped > 0 {
                // Second line, never folded into the count above: an operator reading
                // `notes=1` must not have to wonder whether a note was passed over.
                println!(
                    "privacy history-purge: notes_unreadable={} (their history was left in place)",
                    report.notes_skipped
                );
            }
            Ok(())
        }
    }
}

/// What one run found and destroyed.
///
/// `notes`, `snapshots` and `bytes` are the operator's count. `notes_skipped` is the
/// residue that count would otherwise hide: a note whose `.md` could not be parsed has an
/// unknown mode, and reporting it as nothing-to-do would make an unreadable file look
/// like a clean one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PrivacyHistoryPurgeReport {
    /// Notes whose history directory was (or would be) emptied — counted per **note
    /// identifier**, not per directory: an archived note carries its history in a second
    /// place, and that is one note, not two.
    pub notes_purged: usize,
    /// Snapshot files destroyed (or that would be destroyed).
    pub snapshots_destroyed: usize,
    /// Bytes those snapshot files occupied (or would free).
    pub bytes_destroyed: u64,
    /// Notes left alone because no `.md` of theirs could be read.
    pub notes_skipped: usize,
}

/// Destroys the version history of every note a masking deployment rewrote.
///
/// # Errors
///
/// Returns an error if `root` is not an existing directory — an absent root is a wrong
/// path, and a report of zeros would read as a corpus with nothing to purge.
pub fn run(args: &PrivacyHistoryPurgeArgs) -> Result<PrivacyHistoryPurgeReport> {
    let root = args.root.as_path();
    if !root.is_dir() {
        bail!(
            "root {} is not a directory — refusing to report an empty purge for a path that holds nothing",
            root.display()
        );
    }

    // First pass: every `<id>/` directory under a `.history/` directory, wherever the tree
    // keeps it (a vault root, or the mirrored copy of an archived note).
    let histories = collect_history_dirs(root);

    let mut report = PrivacyHistoryPurgeReport::default();
    if histories.is_empty() {
        return Ok(report);
    }

    // Second pass: the mode each note identifier carries, read from the block a pass
    // wrote. A note that appears more than once (live and archived) is masked if any of
    // its copies says so: the criterion is the note, and the two copies share an identity.
    let (masked, unreadable) = read_modes(root, histories.keys())?;

    for (id, dirs) in &histories {
        // The snapshots themselves decide whether a note has anything at stake. A note
        // whose `.history/<id>/` exists but is empty is not a note this run purges — the
        // directory a purge leaves behind is what a note with no history looks like, and
        // counting it would make a second run report the same work as the first.
        let snapshots: Vec<PathBuf> = dirs.iter().flat_map(|dir| regular_files(dir)).collect();
        if snapshots.is_empty() {
            continue;
        }
        if !masked.contains(id) {
            if unreadable.contains(id) {
                report.notes_skipped += 1;
            }
            continue;
        }
        report.notes_purged += 1;
        for file in snapshots {
            let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            if args.dry_run {
                report.snapshots_destroyed += 1;
                report.bytes_destroyed += bytes;
                continue;
            }
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    report.snapshots_destroyed += 1;
                    report.bytes_destroyed += bytes;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %file.display(),
                        err = %e,
                        "privacy history-purge: snapshot removal failed — left in place"
                    );
                }
            }
        }
        // The directory itself is left to the filesystem: an empty `.history/<id>/` is
        // what a note with no history looks like, and removing directories is not what
        // the count above promises.
    }

    tracing::debug!(
        notes = report.notes_purged,
        snapshots = report.snapshots_destroyed,
        bytes = report.bytes_destroyed,
        skipped = report.notes_skipped,
        "privacy history-purge: run complete"
    );
    Ok(report)
}

/// Maps every note identifier found under a `.history/` directory to the directories that
/// hold its snapshots.
///
/// One identifier may appear several times: the live vault and the archived copy each keep
/// their own `.history/<id>/`.
fn collect_history_dirs(root: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut histories: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() || entry.file_name() != HISTORY_DIR_NAME {
            continue;
        }
        let Ok(children) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.filter_map(Result::ok) {
            let is_dir = child.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            histories
                .entry(child.file_name().to_string_lossy().into_owned())
                .or_default()
                .push(child.path());
        }
    }
    histories
}

/// Reads the privacy block of every `.md` whose note identifier is under a history
/// directory, and returns the identifiers split into masked and unreadable.
///
/// Only the notes that have a history are parsed: a note with nothing to purge has no
/// bearing on the report, and reading it could only cost time.
///
/// # Errors
///
/// Returns an error if the tree cannot be walked at all — a scan that stopped early would
/// report a partial count as a complete one.
fn read_modes<'a>(
    root: &Path,
    ids: impl Iterator<Item = &'a String>,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let wanted: BTreeSet<&str> = ids.map(String::as_str).collect();
    let mut masked = BTreeSet::new();
    // A note is only skipped once nothing of it could be read: an identifier with an
    // archived copy that parses is not unreadable, and a passing copy that says `mask`
    // still selects it.
    let mut readable = BTreeSet::new();

    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        if path.components().any(|c| c.as_os_str() == HISTORY_DIR_NAME) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !wanted.contains(stem) {
            continue;
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err = %e,
                    "privacy history-purge: .md unreadable — its note is skipped"
                );
                continue;
            }
        };
        let parsed = match gradatum_markdown::parse(&raw) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err = %e,
                    "privacy history-purge: .md does not parse — its note is skipped"
                );
                continue;
            }
        };
        readable.insert(stem.to_owned());
        if block_mode(&parsed.frontmatter) == Some(MODE_MASK) {
            masked.insert(stem.to_owned());
        }
    }

    let unreadable = wanted
        .iter()
        .filter(|id| !readable.contains(**id))
        .map(|id| (*id).to_owned())
        .collect();
    Ok((masked, unreadable))
}

/// The mode recorded in a note's privacy block, or `None` when the note carries none.
///
/// A block that is not a table, or whose `mode` is not a string, is read as no mode: the
/// command selects on what the block states, and a malformed block states nothing.
fn block_mode(frontmatter: &gradatum_core::frontmatter::Frontmatter) -> Option<&str> {
    frontmatter.extra.get(BLOCK_KEY)?.get("mode")?.as_str()
}

/// Every regular file under `dir`, recursively.
fn regular_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => out.extend(regular_files(&path)),
            Ok(kind) if kind.is_file() => out.push(path),
            _ => {}
        }
    }
    out
}
