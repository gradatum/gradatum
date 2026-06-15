//! Optimistic locking for vault writes.
//!
//! `write_if_match` compares the expected `sha256` against the current note hash BEFORE
//! writing via `write_note_inner`. If the hash does not match → returns
//! `WriteResult::Conflict { current_sha256 }` without writing.
//!
//! ## Contract
//!
//! - `expected_sha256 = None` → unconditional write (backward-compatible behaviour).
//! - `expected_sha256 = Some(h)` AND note absent → write (note is new).
//! - `expected_sha256 = Some(h)` AND note present AND `h == current` → write.
//! - `expected_sha256 = Some(h)` AND note present AND `h != current` → Conflict.
//!
//! ## Async flow
//!
//! The client submits `expected_sha256` in the `POST /api/v1/vault_write` request.
//! The handler (`gradatum-server`) carries the value in `CurateSpec.expected_sha256`.
//! The worker (`handle_curate`) calls `write_if_match` with the expected hash.
//! On `Conflict` → the job is marked terminal with `JobStatus::Conflict` (readable
//! via `GET /api/v1/jobs/{id}`). No synchronous HTTP 409 (write is asynchronous).

use crate::{error::VaultError, registry::Vault};
use gradatum_core::{frontmatter::Frontmatter, identity::NoteId};

/// Result of a `write_if_match` call.
///
/// - `Written`: the note was written (hash after write).
/// - `Conflict`: the note was NOT written — the current hash is provided so
///   the client can resolve the conflict (3-way merge or abandon).
#[derive(Debug, PartialEq)]
pub enum WriteResult {
    /// Write succeeded.
    Written {
        /// SHA-256 hash of the note after writing.
        new_sha256: [u8; 32],
    },
    /// Optimistic-lock conflict — the note was NOT written.
    Conflict {
        /// Current SHA-256 hash (the version held by the concurrent winner).
        current_sha256: [u8; 32],
    },
}

impl Vault {
    /// Writes a note with optional current-hash verification (optimistic locking).
    ///
    /// ## Parameters
    ///
    /// - `frontmatter`: frontmatter of the new version.
    /// - `body`: Markdown body of the new version.
    /// - `id`: pre-allocated ULID (honoured via `write_note_inner`).
    /// - `expected_sha256`: expected hash (`None` = unconditional).
    ///
    /// ## Behaviour
    ///
    /// See the module doc for the complete truth table.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(NoteNotFound)` cannot occur here (handled by read-before-write).
    /// - `VaultError::Storage` / `VaultError::Markdown`: I/O errors.
    /// - `VaultError::Conflict` is never returned as `Err` — it is a `WriteResult` variant.
    pub async fn write_if_match(
        &self,
        frontmatter: Frontmatter,
        body: String,
        id: NoteId,
        expected_sha256: Option<[u8; 32]>,
    ) -> Result<WriteResult, VaultError> {
        if let Some(expected) = expected_sha256 {
            // Single-worker invariant: TOCTOU non-issue as gradatum-worker runs
            // single-instance (un seul goroutine `handle_curate` actif à la fois
            // par déploiement). La fenêtre read_note→write_note_inner ne peut être
            // interrompue par un autre write_if_match concurrent.
            // Atomic upsert (SELECT+UPDATE sous le même rusqlite Mutex) deferred —
            // requis uniquement si multi-worker devient nécessaire (v0.5+, F-41 phase 2).
            //
            // Lire la version existante pour comparer les hashes.
            // NoteNotFound → note nouvelle → pas de conflit possible → écriture directe.
            match self.read_note(id).await {
                Ok(existing) => {
                    let current = existing.content_hash.0;
                    if current != expected {
                        // Conflit : le hash attendu est périmé.
                        return Ok(WriteResult::Conflict {
                            current_sha256: current,
                        });
                    }
                    // Hash courant == attendu → on peut écrire en toute sécurité.
                }
                Err(VaultError::Core(gradatum_core::error::GradatumError::NoteNotFound(_))) => {
                    // Note nouvelle — pas de conflit possible.
                }
                Err(other) => return Err(other),
            }
        }
        // Écriture effective — délègue au chemin commun (read-before-write inclus).
        let written = self.write_note_inner(frontmatter, body, id).await?;
        Ok(WriteResult::Written {
            new_sha256: written.content_hash.0,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::frontmatter::Frontmatter;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;
    use tempfile::TempDir;

    /// Construit un Frontmatter minimal valide pour les tests.
    fn minimal_fm() -> Frontmatter {
        Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Decisions,
            status: NoteStatus::Draft,
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

    /// expected_sha256 = None → écriture inconditionnelle.
    #[tokio::test]
    async fn write_if_match_none_is_unconditional() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::create(dir.path(), VaultId::new("main"))
            .await
            .unwrap();
        let id = NoteId::new();
        let result = vault
            .write_if_match(minimal_fm(), "corps".into(), id, None)
            .await
            .unwrap();
        assert!(
            matches!(result, WriteResult::Written { .. }),
            "None doit retourner Written"
        );
    }

    /// Hash courant correct → écriture réussie.
    #[tokio::test]
    async fn write_if_match_correct_hash_writes() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::create(dir.path(), VaultId::new("main"))
            .await
            .unwrap();
        let id = NoteId::new();
        // Premier write pour obtenir le hash courant.
        let r1 = vault
            .write_if_match(minimal_fm(), "v1".into(), id, None)
            .await
            .unwrap();
        let WriteResult::Written {
            new_sha256: hash_v1,
        } = r1
        else {
            panic!("premier write doit retourner Written");
        };
        // Deuxième write avec le bon hash → doit réussir.
        let r2 = vault
            .write_if_match(minimal_fm(), "v2".into(), id, Some(hash_v1))
            .await
            .unwrap();
        assert!(
            matches!(r2, WriteResult::Written { .. }),
            "hash courant correct doit retourner Written"
        );
    }

    /// Hash périmé → Conflict, note non écrasée.
    #[tokio::test]
    async fn write_if_match_conflict_on_stale_hash() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::create(dir.path(), VaultId::new("main"))
            .await
            .unwrap();
        let id = NoteId::new();
        // Premier write → hash v1.
        let r1 = vault
            .write_if_match(minimal_fm(), "v1".into(), id, None)
            .await
            .unwrap();
        let WriteResult::Written {
            new_sha256: hash_v1,
        } = r1
        else {
            panic!("premier write doit retourner Written");
        };
        // Écriture concurrente change le hash courant.
        vault
            .write_if_match(minimal_fm(), "v2".into(), id, None)
            .await
            .unwrap();
        // Tentative avec l'ancien hash → Conflict.
        let r3 = vault
            .write_if_match(minimal_fm(), "v3".into(), id, Some(hash_v1))
            .await
            .unwrap();
        match r3 {
            WriteResult::Conflict { current_sha256 } => {
                // current_sha256 doit être le hash de v2, pas de v1.
                assert_ne!(
                    current_sha256, hash_v1,
                    "current_sha256 sur Conflict doit être celui de v2"
                );
            }
            WriteResult::Written { .. } => panic!("hash périmé doit retourner Conflict"),
        }
        // La note ne doit PAS avoir été écrasée par v3.
        let note = vault.read_note(id).await.unwrap();
        assert_eq!(
            note.body.markdown, "v2",
            "note ne doit pas être écrasée par v3 sur Conflict"
        );
    }

    /// Note nouvelle + expected_sha256 Some → écriture (pas de conflit possible).
    #[tokio::test]
    async fn write_if_match_new_note_with_expected_writes() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::create(dir.path(), VaultId::new("main"))
            .await
            .unwrap();
        let id = NoteId::new();
        // Hash arbitraire — la note n'existe pas encore.
        let arbitrary_hash = [0u8; 32];
        let result = vault
            .write_if_match(minimal_fm(), "nouveau".into(), id, Some(arbitrary_hash))
            .await
            .unwrap();
        assert!(
            matches!(result, WriteResult::Written { .. }),
            "note nouvelle avec expected doit retourner Written"
        );
    }
}
