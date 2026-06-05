//! Lifecycle CRUD des notes — création, persistance, mise à jour de statut.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.3 + §2.6.
//!
//! ## Phase 1 scope
//!
//! - `write_note` : calcule le `ContentHash`, persiste le `.md` sur disque, upsert l'index SQLite.
//! - `read_note` : stub Phase 1 — retourne `GradatumError::NoteNotFound`. Implémenté T12+.
//! - `update_status` : stub Phase 1 — retourne `Ok(())`. Implémenté T12+ avec state machine.
//!
//! ## Invariants
//!
//! - `vault_id` dans le frontmatter est toujours égal à `self.tenant_id` (forcé si absent).
//! - `updated` est mis à jour à `Utc::now()` à chaque écriture.
//! - Chemin on-disk : `<root>/<tenant>/<locus>/<id>.md` ou `<root>/<tenant>/<id>.md`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::Utc;
use gradatum_cache::CacheKey;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
// DocumentStore : write_note, get_content_hash, get_note, list_by_status (Étape 0.1).
use gradatum_core::note::{EffectiveNote, Note, NoteBody};
use gradatum_core::status::NoteStatus;
use gradatum_core::DocumentStore as _;
use gradatum_storage::Storage as _;

use crate::error::VaultError;
use crate::registry::Vault;

impl Vault {
    /// Écrit une note dans le vault.
    ///
    /// ## Opérations
    ///
    /// 1. Force `vault_id = self.tenant_id` si absent du frontmatter.
    /// 2. Met à jour `frontmatter.updated` à `Utc::now()`.
    /// 3. Calcule `ContentHash::compute(&frontmatter, &body)`.
    /// 4. Génère un nouveau `NoteId` (ULID).
    /// 5. Sérialise la note en Markdown via `gradatum-markdown::write`.
    /// 6. Persiste le `.md` à `<root>/<tenant>/<locus?>/<id>.md` via `FileStorage`.
    /// 7. Upsert l'index SQLite via `Index::upsert_note`.
    ///
    /// ## Erreurs
    ///
    /// - `VaultError::Core(GradatumError::Markdown(...))` si la sérialisation échoue.
    /// - `VaultError::Storage(...)` si l'écriture disque échoue.
    /// - `VaultError::Core(GradatumError::Storage(...))` si l'upsert SQLite échoue.
    pub async fn write_note(
        &self,
        mut frontmatter: Frontmatter,
        body: String,
    ) -> Result<Note, VaultError> {
        // Invariant : vault_id doit correspondre au tenant courant
        if frontmatter.vault_id.0.is_empty() {
            frontmatter.vault_id = self.tenant_id.clone();
        }

        // Mise à jour du timestamp de modification
        frontmatter.updated = Some(Utc::now());

        let body_obj = NoteBody { markdown: body };

        // Calcul du ContentHash JCS (§2.2) — déterministe cross-langage
        let content_hash = ContentHash::compute(&frontmatter, &body_obj.markdown);

        let id = NoteId::new();
        let note = Note {
            id,
            frontmatter,
            body: body_obj,
            version: NoteVersion::initial(),
            content_hash,
            integrity_signature: None,
        };

        // Chemin relatif on-disk : <tenant>/<locus?>/<id>.md
        let md_path = note_md_relative_path(&note);

        // Sérialisation Markdown (§5.1)
        let md_content = gradatum_markdown::write(&note)
            .map_err(|e| GradatumError::Markdown(format!("sérialisation md: {e}")))?;

        // Persistance sur disque via OpenDAL FileStorage
        self.storage
            .write(&md_path, md_content.as_bytes())
            .await
            .map_err(|e| VaultError::Storage(format!("write md {md_path}: {e}")))?;

        // Upsert dans l'index SQLite (FTS5 + note_overrides + file_checksums)
        // Étape 0.1 : upsert_note est devenu write_note via DocumentStore trait.
        self.index.write_note(&note).await?;

        Ok(note)
    }

    /// Lit une note par identifiant ULID.
    ///
    /// ## Algorithme (T4 P2.0c)
    ///
    /// 1. **Cache hit** : vérifie la présence dans `EffectiveNoteCache` + valide le checksum
    ///    via `index.get_content_hash` (protection B22 contre stale cache concurrent).
    ///    Si valide → retourne la note directement, incrémente `cache_hits`.
    ///    Si stale → invalide l'entrée, passe au cache miss.
    /// 2. **Cache miss** : `index.get_note(vault_id, id)` → `NoteRecord`.
    ///    Lit le `.md` sur disque via `storage.read(path)` pour obtenir le Markdown complet.
    ///    Parse via `gradatum_markdown::parse` → `ParsedNote` → `Note` complète.
    ///    Insère dans le cache pour les appels suivants.
    ///
    /// ## Chemin disque
    ///
    /// Tente d'abord `<vault_id>/<id>.md` (sans locus).
    /// Si absent, tente `<vault_id>/<section>/<id>.md` (locus = section).
    ///
    /// ## Erreurs
    ///
    /// - `VaultError::Core(GradatumError::NoteNotFound)` si absent de l'index.
    /// - `VaultError::Storage(...)` si le fichier .md est introuvable sur disque.
    /// - `VaultError::Markdown(...)` si le parse échoue.
    pub async fn read_note(&self, id: NoteId) -> Result<Note, VaultError> {
        let vault_id = self.tenant_id.as_str();
        let id_str = id.to_string();

        // ── 1. Cache hit path ─────────────────────────────────────────────────
        // Clé composite : (NoteId, scope_hash=0 pour read_note sans scope override).
        let cache_key: CacheKey = (id, 0u64);
        let index_for_validator = Arc::clone(&self.index);
        let id_for_validator = id;

        let cached = self
            .cache
            .get(cache_key, move |note_id| async move {
                // Validator : lit le hash courant depuis SQLite.
                // None = note absente de l'index → stale entry.
                index_for_validator
                    .get_content_hash(note_id)
                    .await?
                    .ok_or(GradatumError::NoteNotFound(id_for_validator))
            })
            .await
            .map_err(VaultError::Core)?;

        if let Some(effective) = cached {
            // Cache hit valide — reconstruire Note depuis EffectiveNote.
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(effective_note_to_note(&effective, id));
        }

        // ── 2. Cache miss path ────────────────────────────────────────────────
        // Vérifier que la note existe dans l'index.
        let record = self
            .index
            .get_note(vault_id, &id_str)
            .await
            .map_err(VaultError::Core)?
            .ok_or(VaultError::Core(GradatumError::NoteNotFound(id)))?;

        // Construire le chemin disque : essayer sans locus puis avec section comme locus.
        let path_no_locus = format!("{}/{}.md", vault_id, id_str);
        let path_with_section = format!("{}/{}/{}.md", vault_id, record.section, id_str);

        let md_bytes = if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
            self.storage
                .read(&path_no_locus)
                .await
                .map_err(|e| VaultError::Storage(format!("read .md {path_no_locus}: {e}")))?
        } else {
            self.storage
                .read(&path_with_section)
                .await
                .map_err(|e| VaultError::Storage(format!("read .md {path_with_section}: {e}")))?
        };

        let md_str = String::from_utf8(md_bytes)
            .map_err(|e| VaultError::Storage(format!("UTF-8 decode .md {id_str}: {e}")))?;

        // Parse le Markdown complet pour reconstruire la Note.
        let parsed =
            gradatum_markdown::parse(&md_str).map_err(|e| VaultError::Markdown(e.to_string()))?;

        // Reconstruire la version depuis `record.version` si disponible (Phase 1 : 1 par défaut).
        let note = Note {
            id,
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            version: NoteVersion::initial(),
            content_hash: parsed.content_hash,
            integrity_signature: None,
        };

        // Insérer dans le cache pour les appels suivants.
        let effective = Arc::new(note_to_effective_note(&note));
        self.cache
            .insert(cache_key, effective, note.content_hash)
            .await;

        Ok(note)
    }

    /// Met à jour le statut d'une note avec validation de la state machine.
    ///
    /// **Phase 1 stub** — retourne `Ok(())`.
    ///
    /// L'implémentation complète vérifie `NoteStatus::can_transition_to(target)`
    /// et propage `GradatumError::InvalidStatusTransition` si invalide.
    /// Reporté à T12+ avec la couche lifecycle complète.
    pub async fn update_status(
        &self,
        _id: NoteId,
        _target: NoteStatus,
        _reason: Option<String>,
    ) -> Result<(), VaultError> {
        // Phase 1 stub — state machine enforcement en T12+.
        Ok(())
    }
}

// ── Helpers de conversion cache ───────────────────────────────────────────────

/// Convertit une `EffectiveNote` (depuis le cache) en `Note` complète.
///
/// En Phase 1, `EffectiveNote` est structurellement identique à `Note`
/// (pas d'overrides appliqués). Reconstitue la `Note` depuis ses champs.
fn effective_note_to_note(effective: &EffectiveNote, id: NoteId) -> Note {
    Note {
        id,
        frontmatter: effective.frontmatter.clone(),
        body: effective.body.clone(),
        version: effective.version,
        content_hash: effective.content_hash,
        integrity_signature: None,
    }
}

/// Convertit une `Note` en `EffectiveNote` pour insertion dans le cache.
///
/// En Phase 1, pas d'overrides — la projection est directe.
fn note_to_effective_note(note: &Note) -> EffectiveNote {
    EffectiveNote {
        id: note.id,
        frontmatter: note.frontmatter.clone(),
        body: note.body.clone(),
        version: note.version,
        content_hash: note.content_hash,
    }
}

/// Construit le chemin relatif on-disk d'une note.
///
/// Format : `<tenant>/<locus>/<id>.md` ou `<tenant>/<id>.md` si pas de locus.
/// Le chemin est toujours relatif à la racine du vault (passé tel quel à `Storage::write`).
fn note_md_relative_path(note: &Note) -> String {
    let tenant = note.frontmatter.vault_id.as_str();
    let id_str = note.id.to_string();
    match note.frontmatter.locus.as_ref() {
        Some(locus) => format!("{}/{}/{}.md", tenant, locus.as_str(), id_str),
        None => format!("{}/{}.md", tenant, id_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;

    fn build_minimal_frontmatter() -> Frontmatter {
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
        }
    }

    #[test]
    fn note_md_relative_path_no_locus() {
        let fm = build_minimal_frontmatter();
        let body = NoteBody {
            markdown: "test".into(),
        };
        let hash = ContentHash::compute(&fm, "test");
        let id = NoteId::new();
        let note = Note {
            id,
            frontmatter: fm,
            body,
            version: NoteVersion::initial(),
            content_hash: hash,
            integrity_signature: None,
        };
        let path = note_md_relative_path(&note);
        assert!(path.starts_with("main/"));
        assert!(path.ends_with(".md"));
    }

    #[test]
    fn note_md_relative_path_with_locus() {
        use gradatum_core::scope::LocusId;
        let mut fm = build_minimal_frontmatter();
        fm.locus = Some(LocusId::new("my-locus"));
        let body = NoteBody {
            markdown: "test".into(),
        };
        let hash = ContentHash::compute(&fm, "test");
        let id = NoteId::new();
        let note = Note {
            id,
            frontmatter: fm,
            body,
            version: NoteVersion::initial(),
            content_hash: hash,
            integrity_signature: None,
        };
        let path = note_md_relative_path(&note);
        assert!(path.starts_with("main/my-locus/"));
        assert!(path.ends_with(".md"));
    }
}
