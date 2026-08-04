//! Optimistic locking for vault writes.
//!
//! `write_if_match` compares the expected `sha256` against the current note hash BEFORE
//! writing via `write_note_inner`. If the hash does not match → returns
//! `WriteResult::Conflict { current_sha256 }` without writing.
//!
//! # Wired into the live write path
//!
//! **`write_if_match` is the production compare-and-swap.** The live `vault_write` pipeline
//! reaches the vault through the internal persist API; its handler
//! (`handle_persist_curated`) routes an RMW request (`expected_sha256 = Some`) to
//! [`crate::Registry::write_if_match_internal`], which delegates here. A CREATE request
//! (`expected_sha256 = None`) keeps taking the unconditional `write_note_with_id_internal`
//! path. So a client supplying a `expected_sha256` on `POST /api/v1/vault_write` against a
//! live note now gets a genuine compare-and-swap: a stale hash yields a `Conflict` and the
//! note is left intact. The truth table below documents this function's contract.
//!
//! ## Contract
//!
//! "Note present/absent" here means **the `.md` file is readable on disk** (what
//! `read_note` checks) — NOT index presence. A *phantom* note (index entry present
//! but `.md` absent) is therefore seen as "absent" by this layer and resurrected.
//!
//! - `expected_sha256 = None` → unconditional write (backward-compatible behaviour).
//! - `expected_sha256 = Some(h)` AND `.md` absent (new OR phantom) → write
//!   (self-heal: the `.md` is (re)created). For a *phantom* the optimistic lock is
//!   structurally unverifiable (no current content to hash); the
//!   `vault_write { note_id = phantom, expected_sha256 = Some }` case is rejected
//!   **upstream** by the server overwrite guard (409) before any job is enqueued, so
//!   it never reaches this function. Any other enqueue path that bypasses that guard
//!   would write unconditionally here — known residual, see the server guard SSOT.
//! - `expected_sha256 = Some(h)` AND `.md` present AND `h == current` → write.
//! - `expected_sha256 = Some(h)` AND `.md` present AND `h != current` → Conflict.
//!
//! ## Async flow — end to end
//!
//! The client submits `expected_sha256` in the `POST /api/v1/vault_write` request.
//! The handler (`gradatum-server`) carries the value in `CurateSpec.expected_sha256`.
//! The worker (`handle_curate`) forwards it in the internal persist request.
//!
//! **The chain now completes.** On an RMW request the persist handler calls
//! `write_if_match` (via `Registry::write_if_match_internal`); on a hash mismatch it returns
//! HTTP 409 over the internal API, the worker maps it to `InternalClientError::Conflict` and
//! calls `queue.mark_conflict(...)`, moving the job to terminal `JobStatus::Conflict`
//! (readable via `GET /api/v1/jobs/{id}`). The write being asynchronous, there is no
//! synchronous 409 to the original `vault_write` caller — exactly the design intent.

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
    /// - `VaultError::Core(NoteNotFound)` is **handled internally**, never surfaced as
    ///   `Err`: when `read_note` reports it (new or phantom note), it is caught (see the
    ///   match arm below) and treated as "no current version" → the write proceeds.
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
            // TOCTOU assumé : la fenêtre read_note→write_note_inner n'est PAS protégée.
            // Ne pas se fier à un « invariant mono-worker » — il est faux : le worker
            // `curate` tourne à une concurrence par défaut de 2 (WorkerConfig::
            // default_concurrency), et les workers `forget` / `distill` / `embed` sont
            // enregistrés à côté de lui dans le même process. `forget` écrit sans
            // expected_sha256 et `distill` ignore le sien : ces chemins ne passent pas
            // par cette comparaison.
            // Portée réelle de la garde : sérialise deux read-modify-write `curate` sur
            // la même note, rien de plus.
            // Atomic upsert (SELECT+UPDATE sous le même rusqlite Mutex) deferred.
            //
            // Lire la version existante pour comparer les hashes.
            // NoteNotFound → `.md` absent (note neuve OU fantôme) → pas de contenu
            // courant à comparer → écriture directe (self-heal pour le fantôme).
            // Le cas fantôme + expected_sha256 = Some est filtré en amont (garde serveur
            // 409) — voir la truth-table du module.
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

    /// Note fantôme (index présent, `.md` absent) + expected_sha256 = None → self-heal :
    /// le `.md` est (re)créé et redevient relisible. Fige le cas 2 de la décision hybride.
    #[tokio::test]
    async fn write_if_match_resurrects_phantom_with_none() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::create(dir.path(), VaultId::new("main"))
            .await
            .unwrap();
        let id = NoteId::new();
        // Fabrique un fantôme : entrée d'index SANS fichier `.md` sur disque.
        vault
            .index()
            .seed_note_with_fts(&id.0.to_string(), "decisions", "# t\nfantome")
            .await
            .unwrap();
        // Précondition : la note est bien un fantôme (index présent, `.md` absent).
        assert!(
            matches!(
                vault.read_note(id).await,
                Err(VaultError::Core(
                    gradatum_core::error::GradatumError::NoteNotFound(_)
                ))
            ),
            "précondition : la note doit être un fantôme (NoteNotFound)"
        );
        // Self-heal : write_if_match(None) ressuscite le `.md`.
        let result = vault
            .write_if_match(minimal_fm(), "ressuscite".into(), id, None)
            .await
            .unwrap();
        assert!(
            matches!(result, WriteResult::Written { .. }),
            "fantôme + None doit ressusciter (Written)"
        );
        // Le `.md` est désormais relisible avec le nouveau contenu.
        let note = vault.read_note(id).await.unwrap();
        assert_eq!(
            note.body.markdown, "ressuscite",
            "self-heal : le `.md` recréé porte le nouveau contenu"
        );
    }
}
