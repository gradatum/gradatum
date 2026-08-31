//! Note-write convergence guard.
//!
//! ## The property this module imposes
//!
//! Everything the product writes converges through a single funnel,
//! [`crate::Vault::write_note_inner`]: it persists the `.md`, writes the index row, and
//! records the drift footprint in `file_checksums` — in one geste. That convergence is what
//! keeps a note's representations in agreement.
//!
//! Historically, convergence was **caller discipline**: any code holding the vault storage
//! handle could write a note `.md` straight to disk (`storage().write(path, bytes)`), skipping
//! the index and the drift footprint. The 2026-05 bulk import did exactly that from a shell
//! script and produced, 99 days later, 37 orphan files, 11 vectorless notes, and — because the
//! drift table never learned of them — nothing able to signal the gap. Three separate recovery
//! efforts — reindexing orphaned files, backfilling vectorless notes, and drift detection — for
//! one non-converging entry point.
//!
//! This guard turns that convergence into an **imposed** property. [`NoteWriteGuard`] decorates the
//! vault's [`Storage`] handle: a note-file write arriving through the ordinary [`Storage::write`]
//! surface is **refused outright** ([`StorageError::WriteRejected`]) — it never succeeds
//! silently. The funnel writes note files through the privileged
//! [`NoteWriteGuard::write_note_file`] channel, which is `pub(crate)` and therefore reachable
//! only from inside this crate.
//!
//! ## Exactly what this guard covers — and what it does NOT
//!
//! The guard closes the non-converging path for **every in-process holder of the storage
//! handle**: any code in this process — admin tooling, migrations, restores, a future
//! importer — that reaches storage through [`crate::Vault::storage`] can no longer write a note
//! `.md` without the funnel. For that population the orphan is impossible *by construction*.
//!
//! It does **not** — and cannot — cover an **out-of-process** writer. The 2026-05 incident was
//! a shell script writing `.md` files straight to the filesystem: the OS lets any process do
//! that, and no Rust type interposes on it. That vector is closed by **policy**, not by this
//! guard — mass ingestion now goes through the HTTP API (→ the funnel), so no direct-disk
//! import path remains. Do not read this guard as "the vault directory on disk is protected":
//! it protects the in-process write surface, and a direct write to the files behind its back
//! is still physically possible (and, being invisible to the drift table until scanned, is
//! exactly the class the drift scan detects).
//!
//! ## Direction and asymmetry (a deliberate split, not an omission)
//!
//! This is **prevention, not detection**, and it is deliberately **one-directional**. It
//! prevents the *orphan* direction — a note file written outside the funnel (index/drift
//! footprint missing). It does **not** prevent the inverse *phantom* direction — a raw
//! deletion of a `.md` that leaves a dangling index row: that write goes through `delete`,
//! which the guard passes through untouched. The phantom direction is covered by **detection**
//! (the drift scan covers both directions), consistent with the split: prevention for the
//! orphan here, detection for the phantom there.
//!
//! This does not remove the bulk-ingestion use case — the sanctioned path (HTTP API → funnel,
//! `scripts/import-bulk-legacy-vault.sh`) is untouched. Only the *non-converging* in-process
//! write path is closed.

use async_trait::async_trait;
use gradatum_storage::{Storage, StorageEntry, StorageError};

/// Decorator over the vault [`Storage`] backend that imposes note-write convergence.
///
/// Delegates every operation to the inner backend, with a single exception: [`Storage::write`]
/// refuses a **note path** (see [`is_note_path`]). The write funnel bypasses that refusal via
/// the privileged [`Self::write_note_file`].
pub(crate) struct NoteWriteGuard {
    inner: Box<dyn Storage>,
}

impl NoteWriteGuard {
    /// Wraps a storage backend behind the note-write guard.
    pub(crate) fn new(inner: Box<dyn Storage>) -> Self {
        Self { inner }
    }

    /// Privileged note-file write — the **only** sanctioned channel to persist a note `.md`.
    ///
    /// Bypasses the note-path guard and writes straight to the inner backend. It is
    /// `pub(crate)` on purpose: reachable only from within `gradatum-vault`, and called
    /// solely by [`crate::Vault::write_note_inner`], so a note file is only ever produced in
    /// the same geste that writes the index row and the drift footprint. External crates
    /// never see this method; they reach storage through the guarded [`Storage::write`] and
    /// therefore cannot write a note file that skips the funnel.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`] from the inner backend (I/O, backend failure).
    pub(crate) async fn write_note_file(
        &self,
        path: &str,
        content: &[u8],
    ) -> Result<(), StorageError> {
        self.inner.write(path, content).await
    }
}

/// Returns `true` when `path` is the on-disk path of a **note** file.
///
/// A note path is `<segment>/…/<ULID>.md` where:
/// - the final component ends in `.md`;
/// - the stem (final component without `.md`) is a syntactically valid ULID;
/// - **no** path segment starts with `.` — this excludes the vault's internal subtrees
///   (`.history/` copy-on-write snapshots, `.archive/` tombstones, `.gradatum/` metadata),
///   which are legitimately written by the funnel's own bookkeeping and must stay writable
///   through the ordinary surface.
///
/// The shape mirrors the funnel's own path builder (`note_md_relative_path`) and the
/// orphan scanner's classification (`reindex-orphans`): a ULID-stemmed `.md` under a
/// non-hidden subtree is a note, anything else is not.
///
/// The separator is `/` (the [`Storage`] path contract is Unix-relative).
fn is_note_path(path: &str) -> bool {
    // A hidden segment anywhere → internal subtree, never a note file.
    // (An empty segment from a leading/trailing/double slash is not hidden.)
    if path.split('/').any(|seg| seg.starts_with('.')) {
        return false;
    }
    let Some(file) = path.rsplit('/').next() else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".md") else {
        return false;
    };
    ulid::Ulid::from_string(stem).is_ok()
}

#[async_trait]
impl Storage for NoteWriteGuard {
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read(path).await
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError> {
        if is_note_path(path) {
            return Err(StorageError::WriteRejected(format!(
                "note file '{path}' cannot be written through the raw storage handle: it would \
                 bypass the convergence funnel (index row + drift footprint) and create an \
                 orphan. Use Vault::write_note / write_note_with_id instead (F-176)."
            )));
        }
        self.inner.write(path, content).await
    }

    async fn delete(&self, path: &str) -> Result<(), StorageError> {
        self.inner.delete(path).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError> {
        self.inner.list(prefix).await
    }

    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError> {
        self.inner.stat(path).await
    }

    async fn exists(&self, path: &str) -> Result<bool, StorageError> {
        self.inner.exists(path).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), StorageError> {
        self.inner.create_dir(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::is_note_path;
    use gradatum_core::identity::NoteId;

    /// A canonical note path — the shape the funnel writes. This is the case the guard must
    /// catch; every other test isolates one clause that makes a look-alike *not* a note.
    #[test]
    fn ulid_md_at_tenant_root_is_a_note() {
        let id = NoteId::new();
        assert!(is_note_path(&format!("main/{id}.md")));
    }

    #[test]
    fn ulid_md_under_a_locus_is_a_note() {
        let id = NoteId::new();
        assert!(is_note_path(&format!("main/knowledge/rust/{id}.md")));
    }

    // ── Clause: hidden segment. SOLE discriminant here — the stem IS a valid ULID and the
    // extension IS `.md`, so neither the ULID clause nor the `.md` clause can reject it; only
    // the leading-dot `.archive/` subtree makes it a non-note. Removing the hidden clause flips
    // this to `true`. This is the real path `move_file` writes when archiving a note, so the
    // clause is load-bearing: without it, the guard would reject archiving. (A `.history/`
    // snapshot path — `.../<timestamp>.md` — is a *non-isolating* case: its stem is not a ULID,
    // so it is already excluded by the ULID clause; the funnel's real `.history/` write staying
    // allowed is covered end-to-end by `non_note_write_through_raw_handle_still_succeeds`.) ───
    #[test]
    fn ulid_md_under_archive_is_not_a_note() {
        let id = NoteId::new();
        assert!(!is_note_path(&format!("main/.archive/main/{id}.md")));
    }

    // ── Clause: ULID stem. SOLE discriminant here — non-hidden and `.md`, only the stem
    // (not a ULID) makes it a non-note. ─────────────────────────────────────────────────────
    #[test]
    fn non_ulid_md_is_not_a_note() {
        assert!(!is_note_path("main/README.md"));
        // A 26-char-but-invalid-alphabet stem is still rejected (Crockford base32).
        assert!(!is_note_path("main/IIIIIIIIIIIIIIIIIIIIIIIIII.md"));
    }

    // ── Clause: `.md` extension. SOLE discriminant here — non-hidden and ULID stem, only the
    // extension makes it a non-note. ────────────────────────────────────────────────────────
    #[test]
    fn ulid_without_md_extension_is_not_a_note() {
        let id = NoteId::new();
        assert!(!is_note_path(&format!("main/{id}.txt")));
        assert!(!is_note_path(&format!("main/{id}")));
    }

    #[test]
    fn gradatum_metadata_is_not_a_note() {
        assert!(!is_note_path(".gradatum/index.db"));
    }
}
