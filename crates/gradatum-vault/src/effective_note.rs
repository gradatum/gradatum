//! Cache `EffectiveNote` avec validation de checksum sur hit.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md`
//! §6.1 risque #5 (D-perf-2 / B22).
//!
//! ## Design Phase 1
//!
//! - `get_effective_note(id, scope)` : clé composite `(NoteId, scope_hash_u64)`.
//! - Cache hit : validator appelle `Index::get_content_hash(id)` pour vérifier la fraîcheur.
//!   Match → retour cache. Mismatch → invalidation + `None` (cache miss path).
//! - Cache miss : Phase 1 stub retourne `GradatumError::NoteNotFound`.
//!   T12+ enrichit avec fetch `Note` depuis disque + apply overrides + insert cache.
//!
//! ## Validator adapter
//!
//! `EffectiveNoteCache::get` attend `validator: FnOnce(NoteId) -> Fut`
//! où `Fut::Output = Result<ContentHash, E>`.
//!
//! `Index::get_content_hash` retourne `Result<Option<ContentHash>, GradatumError>`.
//! Le `None` est mappé en `GradatumError::NoteNotFound(id)` pour correspondre au contrat.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::{NoteId, NoteVersion};
// Note Étape 0.1 : get_content_hash et get_note sont des méthodes publiques sur SqliteIndex.
// DocumentStore as _ serait nécessaire si on passait par Arc<dyn DocumentStore>.
use gradatum_core::note::EffectiveNote;
use gradatum_core::scope::OverrideScope;
use gradatum_storage::Storage as _;

use crate::registry::Vault;

impl Vault {
    /// Retourne la `EffectiveNote` pour un `(NoteId, OverrideScope)`.
    ///
    /// ## Comportement Phase 1
    ///
    /// - **Cache hit valide** : le validator confirme que le `ContentHash` SQLite correspond.
    ///   Retourne `Arc<EffectiveNote>` depuis le cache sans accès disque.
    /// - **Cache hit stale** : hash SQLite différent → invalidation + cache miss path.
    /// - **Cache miss** : Phase 1 stub retourne `GradatumError::NoteNotFound`.
    ///   T12+ : lit la note depuis disque, applique les overrides actifs, insère dans le cache.
    ///
    /// ## Clé de cache
    ///
    /// `(NoteId, scope_hash_u64)` — le hash du scope est calculé via `DefaultHasher`
    /// à partir d'une représentation string stable (ex. `"vault:main"`).
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` si la note n'est pas dans le cache (Phase 1 stub).
    /// - `GradatumError::Storage` si la validation du hash SQLite échoue.
    pub async fn get_effective_note(
        &self,
        id: NoteId,
        scope: &OverrideScope,
    ) -> Result<Arc<EffectiveNote>, GradatumError> {
        // Calcul du scope hash pour la clé composite (NoteId, u64)
        let scope_key = scope_to_hash(scope);
        let key = (id, scope_key);

        // Clone Arc<SqliteIndex> pour le validator closure ('static bound de moka)
        let index = Arc::clone(&self.index);

        // Cache hit path : validator vérifie la fraîcheur du ContentHash depuis SQLite
        let cached = self
            .cache
            .get(key, move |note_id| async move {
                // Adapter : Option<ContentHash> → Result<ContentHash, GradatumError>
                // None = note absente de l'index → NoteNotFound (stale cache entry)
                index
                    .get_content_hash(note_id)
                    .await?
                    .ok_or(GradatumError::NoteNotFound(note_id))
            })
            .await?;

        if let Some(arc) = cached {
            return Ok(arc);
        }

        // Cache miss — T4 P2.0c : fetch depuis index + storage + insert cache.
        let vault_id = self.tenant_id.as_str();
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

/// Calcule un hash `u64` stable depuis un `OverrideScope`.
///
/// Produit une représentation string déterministe pour chaque variant,
/// puis hashe via `DefaultHasher`. Le hash est cohérent dans un même process
/// (le seed de `DefaultHasher` est aléatoire entre processus en Rust stable,
/// mais pour une clé de cache in-process c'est suffisant).
fn scope_to_hash(scope: &OverrideScope) -> u64 {
    let repr = match scope {
        OverrideScope::Vault(v) => format!("vault:{}", v.as_str()),
        OverrideScope::Locus(l) => format!("locus:{}", l.as_str()),
        OverrideScope::Bearer(b) => format!("bearer:{}", b.as_str()),
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
        let locus = scope_to_hash(&OverrideScope::Locus(LocusId::new("main")));
        assert_ne!(
            vault, locus,
            "vault et locus avec même id doivent avoir des hashes distincts"
        );
    }
}
