//! Integration tests — F-176: note-write convergence is an **imposed** property.
//!
//! The 2026-05 bulk import wrote note `.md` files straight to disk, outside the write funnel,
//! and produced orphans invisible to the drift table for 99 days. F-176 closes that path: a
//! note-file write that does not converge with the funnel must **fail**, never succeed
//! silently. These tests attempt the contournement and verify it does not pass — an invariant
//! no test has tried to violate is an intention, not an invariant.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::status::NoteStatus;
use gradatum_storage::StorageError;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// GC due far in the future so the archived note is never garbage-collected mid-test.
const FAR_FUTURE_GC_DUE: i64 = 9_999_999_999_999;

/// THE contournement. A note-path write arriving through the ordinary storage handle — exactly
/// what the May import did — is refused outright, and leaves nothing behind: no orphan file on
/// disk, no index row. This is the case the whole card exists to close.
#[tokio::test]
async fn raw_note_write_is_refused_and_leaves_no_orphan() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    let note_path = format!("main/{id}.md");
    let body = b"---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: live\n\
                 created: \"2026-05-08T10:00:00Z\"\n---\n\n# Smuggled\n\nBypass attempt.\n";

    // The write funnel is bypassed — this is the raw storage surface.
    let err = vault
        .storage()
        .write(&note_path, body)
        .await
        .expect_err("a raw note-path write MUST be refused (F-176)");
    assert!(
        matches!(err, StorageError::WriteRejected(_)),
        "the refusal must be the typed WriteRejected, got: {err:?}"
    );

    // Fail-closed: nothing was written. No orphan `.md`, no index row.
    assert!(
        !vault.storage().exists(&note_path).await.unwrap(),
        "the refused write must not have created an orphan .md on disk"
    );
    assert!(
        vault.read_note(id).await.is_err(),
        "the refused write must not have produced an indexed note"
    );
}

/// The guard is a *note-file* gate, not a blanket write-ban. A non-note write through the same
/// raw handle still succeeds — otherwise the funnel's own bookkeeping (`.history/` snapshots,
/// archive tombstones) would break. This isolates the clause that keeps the internal subtrees
/// writable: only the note-path shape is refused.
#[tokio::test]
async fn non_note_write_through_raw_handle_still_succeeds() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Hidden subtree with a ULID stem: the funnel writes `.history/` snapshots exactly like
    // this. Must remain allowed (only the leading-dot segment separates it from a note path).
    let id = NoteId::new();
    let history_path = format!("main/.history/{id}/1712345678.md");
    vault
        .storage()
        .write(&history_path, b"snapshot bytes")
        .await
        .expect("a hidden-subtree write must remain allowed");
    assert!(vault.storage().exists(&history_path).await.unwrap());

    // Non-ULID stem: not a note either.
    vault
        .storage()
        .write("main/README.md", b"# readme\n")
        .await
        .expect("a non-note .md write must remain allowed");
    assert!(vault.storage().exists("main/README.md").await.unwrap());
}

/// The sanctioned path still works and still converges. The funnel writes the note through the
/// privileged channel — the `.md` lands on disk AND the index resolves it. This proves (a) the
/// use case is not removed (criterion 3) and (b) the privileged channel is load-bearing: route
/// the funnel back through the guarded `write` and this note would be refused.
#[tokio::test]
async fn funnel_write_still_converges() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let note = vault
        .write_note(build_minimal_frontmatter(), "Legit body".into())
        .await
        .expect("the funnel is the sanctioned write path and must succeed");

    // Artefact on disk.
    assert!(
        vault
            .storage()
            .exists(&format!("main/{}.md", note.id))
            .await
            .unwrap(),
        "the funnel must have persisted the .md"
    );
    // Artefact in the index — read-back resolves the note.
    let read = vault
        .read_note(note.id)
        .await
        .expect("note must be indexed");
    assert_eq!(read.id, note.id);
}

/// Archive → restore round-trip. `restore_archive` rewrites the note `.md` through the funnel
/// (`write_note_with_id` → the privileged channel), so restoration must **traverse the guard**,
/// not be refused, and the restored note must be fully reconstituted: back on disk AND
/// resolvable from the index (a derived representation `read_note` only returns if the index
/// row and the on-disk `.md` are both present and consistent).
///
/// Mordant: re-route restoration onto the guarded surface (e.g. write the `.md` back via
/// `storage().write` instead of the funnel) and this test reddens — `restore_archive` returns
/// `WriteRejected` instead of `Ok`. "Safe by construction but locked by no test" is exactly the
/// formula this lot exists to close: the round-trip is now verified, not merely reasoned.
#[tokio::test]
async fn archive_then_restore_traverses_the_guard_and_reconstitutes() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Written through the funnel, then archived: its `.md` leaves the note path for `.archive/`.
    let note = vault
        .write_note(build_minimal_frontmatter(), "Corps à restaurer".into())
        .await
        .unwrap();
    let id = note.id;
    let note_path = format!("main/{id}.md");
    vault
        .archive_note(id, Some("test-admin".into()), FAR_FUTURE_GC_DUE)
        .await
        .expect("archive_note");
    assert!(
        !vault.storage().exists(&note_path).await.unwrap(),
        "after archiving, the .md must have left the note path"
    );

    // Replicate the server's index cascade: `archive_note` deliberately does NOT de-index (the
    // server choke point runs `delete_note_from_index` as a separate step). Without it, restore
    // refuses on an ULID collision. This mirrors the real archive flow — it is not a shortcut.
    vault
        .index()
        .delete_note_from_index("main", &id.to_string())
        .await
        .expect("de-index (server cascade)");

    // Restoration rewrites the `.md` via the funnel — it MUST traverse the guard (no WriteRejected).
    let outcome = vault
        .restore_archive(id)
        .await
        .expect("restore must traverse the guard, not be refused");
    assert_eq!(
        outcome.status,
        NoteStatus::PendingReview,
        "restore returns the note to quarantine"
    );

    // Derived representations reconstituted: `.md` back on disk AND resolvable from the index.
    assert!(
        vault.storage().exists(&note_path).await.unwrap(),
        "restoration must have rewritten the .md at the note path"
    );
    let read = vault
        .read_note(id)
        .await
        .expect("the restored note must resolve from the index (derived representation)");
    assert_eq!(read.id, id);
}
