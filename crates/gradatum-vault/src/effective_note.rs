//! `EffectiveNote` cache with checksum validation on hit.
//!
//! ## Design
//!
//! - `get_effective_note(id, scope)`: composite key `(VaultId, NoteId, scope_hash_u64)` —
//!   `vault_id` partitions the key, which is fail-safe across vaults.
//! - Cache hit: validator calls `Index::get_content_hash(id)` to verify freshness.
//!   Match → return cached value. Mismatch → invalidate + cache miss path.
//! - Cache miss: reads from SQLite index + disk, inserts into cache. **No override is
//!   applied** — see the note on `scope` below.
//!
//! ## `scope` is a cache-key discriminant only (no override resolution)
//!
//! Despite the type name, this module resolves **no** override. On a cache miss the
//! frontmatter returned is the one parsed from the `.md` file, verbatim. `scope` only
//! partitions the cache key, so two distinct scopes yield **identical content** for the
//! same note — they merely occupy separate cache slots.
//!
//! The machinery exists but is not wired: `NoteMetadataOverride::resolve`
//! (`gradatum-vault::overrides`) has no production caller (its only callers are that
//! module's own unit tests), and no read path queries the `note_overrides` table (its
//! only `SELECT` lives in `SqliteIndex::count_child_rows_for_test`).
//! `lifecycle::effective_note_to_note` states the same thing.
//!
//! ## Validator adapter
//!
//! `EffectiveNoteCache::get` expects `validator: FnOnce(NoteId) -> Fut`
//! where `Fut::Output = Result<ContentHash, E>`.
//!
//! `Index::get_content_hash` returns `Result<Option<ContentHash>, GradatumError>`.
//! `None` is mapped to `GradatumError::NoteNotFound(id)` to match the contract.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::{NoteId, NoteVersion};
// Note Étape 0.1 : get_content_hash et get_note sont des méthodes publiques sur SqliteIndex.
// DocumentStore as _ serait nécessaire si on passait par Arc<dyn DocumentStore>.
use gradatum_core::note::EffectiveNote;
use gradatum_core::scope::OverrideScope;

use crate::registry::Vault;

impl Vault {
    /// Returns the `EffectiveNote` for a `(NoteId, OverrideScope)` pair.
    ///
    /// ## Behaviour
    ///
    /// - **Valid cache hit**: validator confirms the SQLite `ContentHash` matches.
    ///   Returns `Arc<EffectiveNote>` from cache without disk access.
    /// - **Stale cache hit**: SQLite hash differs → invalidate + cache miss path.
    /// - **Cache miss**: reads from SQLite index + disk, inserts into cache. The
    ///   frontmatter returned is the one parsed from the `.md` file, verbatim.
    ///
    /// ## `scope` does not select an override
    ///
    /// **No override is applied on any path.** `scope` only partitions the cache key:
    /// calling this method with two different scopes returns the **same content**, from
    /// two distinct cache slots. Do not rely on `scope` to obtain a scope-specific view
    /// of a note — that resolution is not wired (see the module documentation).
    ///
    /// ## Cache key
    ///
    /// `(VaultId, NoteId, scope_hash_u64)` — `vault_id` partitions the key, a `NoteId`
    /// being unique only within a vault. The scope hash is computed with `DefaultHasher`
    /// over a stable string representation such as `"vault:main"`.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent from the index.
    /// - `GradatumError::Storage` if the SQLite hash validation or disk read fails.
    pub async fn get_effective_note(
        &self,
        id: NoteId,
        scope: &OverrideScope,
    ) -> Result<Arc<EffectiveNote>, GradatumError> {
        // Calcul du scope hash pour la clé composite (VaultId, NoteId, u64).
        // Le vault_id partitionne la clé : un NoteId n'est unique qu'au sein d'un
        // vault, la clé DOIT le porter sinon collision cross-vault (C4 — fail-safe).
        let scope_key = scope_to_hash(scope);
        let key = (self.vault_id.clone(), id, scope_key);

        // Clone Arc<SqliteIndex> pour le validator closure ('static bound de moka)
        let index = Arc::clone(&self.index);
        // Copie owned du vault_id : la closure moka est 'static, elle ne peut pas
        // emprunter `self`. Le validator DOIT être scopé au vault de l'instance (C4-1e,
        // C2) sinon une note homonyme d'un autre vault fausse la fraîcheur du cache.
        let vault_id = self.vault_id.as_str().to_owned();

        // Cache hit path : validator vérifie la fraîcheur du ContentHash depuis SQLite
        let cached = self
            .cache
            .get(key.clone(), move |note_id| async move {
                // Adapter : Option<ContentHash> → Result<ContentHash, GradatumError>
                // None = note absente de l'index → NoteNotFound (stale cache entry)
                index
                    .get_content_hash(&vault_id, note_id)
                    .await?
                    .ok_or(GradatumError::NoteNotFound(note_id))
            })
            .await?;

        if let Some(arc) = cached {
            return Ok(arc);
        }

        // Cache miss — T4 P2.0c : fetch depuis index + storage + insert cache.
        let vault_id = self.vault_id.as_str();
        let id_str = id.to_string();

        // Récupérer le record depuis l'index SQLite.
        let record = self
            .index
            .get_note(vault_id, &id_str)
            .await?
            .ok_or(GradatumError::NoteNotFound(id))?;

        // Construire le chemin disque : essayer sans locus puis avec section.
        let path_no_locus = format!("{}/{}.md", vault_id, id_str);
        let path_with_section = format!("{}/{}/{}.md", vault_id, record.section, id_str);

        let md_bytes = if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
            self.storage
                .read(&path_no_locus)
                .await
                .map_err(|e| GradatumError::Storage(format!("read .md {path_no_locus}: {e}")))?
        } else {
            self.storage
                .read(&path_with_section)
                .await
                .map_err(|e| GradatumError::Storage(format!("read .md {path_with_section}: {e}")))?
        };

        let md_str = String::from_utf8(md_bytes)
            .map_err(|e| GradatumError::Storage(format!("UTF-8 decode {id_str}: {e}")))?;

        let parsed = gradatum_markdown::parse(&md_str)
            .map_err(|e| GradatumError::Storage(format!("parse .md {id_str}: {e}")))?;

        let effective = Arc::new(EffectiveNote {
            id,
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            version: NoteVersion::initial(),
            content_hash: parsed.content_hash,
        });

        // Insérer dans le cache pour les appels suivants.
        let content_hash = effective.content_hash;
        self.cache
            .insert(key, Arc::clone(&effective), content_hash)
            .await;

        Ok(effective)
    }
}

/// Computes a stable `u64` hash from an `OverrideScope`.
///
/// Produces a deterministic string representation for each variant,
/// then hashes it via `DefaultHasher`. The hash is consistent within a single process
/// (`DefaultHasher`'s seed is randomised across processes in stable Rust,
/// but this is sufficient for an in-process cache key).
fn scope_to_hash(scope: &OverrideScope) -> u64 {
    let repr = match scope {
        OverrideScope::Vault(v) => format!("vault:{}", v.as_str()),
        // Le `vault` entre dans la clé de hash : deux vaults distincts au même locus/bearer
        // doivent produire des hashes distincts (isolation de cache, miroir de la PK composite).
        OverrideScope::Locus { vault, locus } => {
            format!("locus:{}:{}", vault.as_str(), locus.as_str())
        }
        OverrideScope::Bearer { vault, bearer } => {
            format!("bearer:{}:{}", vault.as_str(), bearer.as_str())
        }
    };
    let mut hasher = DefaultHasher::new();
    repr.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::{LocusId, VaultId};

    #[test]
    fn scope_hash_vault_is_stable_in_process() {
        let s1 = scope_to_hash(&OverrideScope::Vault(VaultId::new("main")));
        let s2 = scope_to_hash(&OverrideScope::Vault(VaultId::new("main")));
        assert_eq!(
            s1, s2,
            "hash doit être stable pour la même valeur dans le même process"
        );
    }

    #[test]
    fn scope_hash_vault_vs_locus_differ() {
        let vault = scope_to_hash(&OverrideScope::Vault(VaultId::new("main")));
        let locus = scope_to_hash(&OverrideScope::Locus {
            vault: VaultId::new("main"),
            locus: LocusId::new("main"),
        });
        assert_ne!(
            vault, locus,
            "vault et locus avec même id doivent avoir des hashes distincts"
        );
    }
}
