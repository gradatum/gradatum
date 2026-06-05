//! Trait d'abstraction de l'index Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.11.
//!
//! ## Design (v0.3.0 — Étape 0.1)
//!
//! `Index` est devenu une **façade supertrait** qui compose les 3 traits granulaires :
//! - [`DocumentStore`](crate::DocumentStore) — CRUD notes
//! - [`IndexStore`](crate::IndexStore) — FTS, overrides, checksums, scoring composite
//! - [`VectorStore`](crate::VectorStore) — embeddings + recherche sémantique cosine
//!
//! Le trait `Index` historique vit dans `gradatum-core` (pas dans `gradatum-index`) — décision Q5DAG :
//! les crates consommateurs (`gradatum-vault`, `gradatum-curator`) importent ce trait abstrait,
//! pas l'implémentation concrète `SqliteIndex` (Phase 1, livrée dans `gradatum-index`).
//!
//! Avantage : `gradatum-vault` ne dépend pas de `gradatum-index` → pas de cycle.
//!
//! ## Compat backward
//!
//! `Index as _` reste fonctionnel pour les consommateurs existants. Pour accéder aux
//! méthodes granulaires (ex. `list_file_checksums`), importer également le sous-trait
//! correspondant (`IndexStore as _`, `DocumentStore as _`, `VectorStore as _`).
//!
//! ## FileChecksumEntry
//!
//! Entrée de la table `file_checksums` — drift detection fichier par fichier.
//! Permet de détecter un fichier modifié hors de Gradatum sans re-hasher tout le vault.
//! Stratégie : (1) vérification rapide mtime + size, (2) hash partiel 4KB, (3) hash complet.
//!
//! ## Overrides génériques
//!
//! `upsert_override_raw` / `get_override_raw` stockent tout payload override en TOML
//! dans la table générique `note_overrides` (décision Q7/B20).
//! Le schéma est identifié par (`override_type`, `schema_version`) → validation via
//! `gradatum-core::schema_registry`.
//!
//! ## Méthodes restées concrètes hors-trait (SqliteIndex uniquement)
//!
//! Les méthodes suivantes de `SqliteIndex` ne sont PAS promues en trait à v0.3.0 :
//! - `search_fts_with_snippet` : retourne `SearchHitRaw` (type dans `gradatum-index`) — promotion
//!   différée v0.4.0 (blast radius type public).
//! - Méthodes admin/lifecycle (`downgrade_note`, `patch_note_status`, `list_notes`, etc.) :
//!   sémantique opérationnelle, pas de consommateur trait-based prévu à v0.3.0.
//! - Méthodes seed/bench : utilitaires test/perf, hors contrat public.

use serde::{Deserialize, Serialize};

use crate::document_store::DocumentStore;
use crate::index_store::IndexStore;
use crate::vector_store::VectorStore;

/// Façade historique — combinaison des 3 traits de storage.
///
/// Conservée pour la compat des call sites existants en v0.3.0.
/// Les nouveaux consommateurs DOIVENT dépendre des traits granulaires
/// (`DocumentStore` / `IndexStore` / `VectorStore`) — plus précis et évolutifs.
///
/// ## Blanket impl
///
/// Tout type qui implémente les 3 sous-traits est automatiquement un `Index`.
/// Aucun `impl Index for T` manuel n'est nécessaire.
pub trait Index: DocumentStore + IndexStore + VectorStore {}

/// Blanket impl : tout type implémentant les 3 sous-traits devient un `Index`.
impl<T: DocumentStore + IndexStore + VectorStore + ?Sized> Index for T {}

/// Entrée de drift detection par fichier.
///
/// Stockée dans la table `file_checksums`. Permet de détecter les modifications
/// hors-Gradatum en vérifiant (mtime + size) avant de re-hasher le fichier entier.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChecksumEntry {
    /// Chemin relatif depuis la racine du vault (ex. `"decisions/2026-05-04-my-note.md"`).
    pub relative_path: String,

    /// Type de fichier.
    pub file_kind: FileKind,

    /// Taille attendue en bytes.
    pub expected_size: u64,

    /// Hash SHA-256 des 4 premiers KB (vérification rapide avant hash complet).
    ///
    /// Évite de lire tout un gros fichier pour détecter qu'il n'a pas changé.
    pub expected_hash_prefix_4kb: [u8; 32],

    /// Hash SHA-256 complet du fichier.
    pub expected_hash: [u8; 32],

    /// mtime Unix epoch (secondes) attendu.
    pub expected_mtime: i64,

    /// Timestamp Unix epoch de la dernière vérification réussie.
    pub last_verified: i64,
}

/// Catégorie de fichier trackée dans `file_checksums`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileKind {
    /// Fichier Markdown d'une note.
    Note,
    /// Fichier TOML d'un override.
    Override,
    /// Fichier de configuration du vault.
    Config,
}

/// Enregistrement complet d'une note retourné par [`DocumentStore::get_note`].
///
/// Struct portable défini dans `gradatum-core` pour permettre son usage
/// via le trait `DocumentStore` sans dépendance vers `gradatum-index` (décision Q5DAG).
#[derive(Debug, Clone)]
pub struct NoteRecord {
    /// Identifiant ULID de la note.
    pub id: String,
    /// Identifiant du vault.
    pub vault_id: String,
    /// Section thématique (ex. `"decisions"`, `"architecture"`).
    pub section: String,
    /// Statut de la note (ex. `"live"`, `"pending-review"`).
    pub status: String,
    /// Corps Markdown complet.
    pub body_text: String,
    /// Auteur de la note (display name ou id, peut être absent).
    pub author: Option<String>,
    /// Tags espace-séparés (depuis `notes.tags`, migration 0003).
    pub tags_raw: Option<String>,
    /// Hash de contenu SHA-256 (32 bytes) — pour calcul hex dans les handlers.
    pub content_hash: Vec<u8>,
    /// Timestamp de création (epoch ms).
    pub created: i64,
    /// Timestamp de dernière mise à jour (epoch ms, peut être absent).
    pub updated: Option<i64>,
    /// Titre H1 Markdown de la note (extrait au curate, peut être absent).
    ///
    /// Peuplé par la migration 0005 (backfill) et mis à jour à chaque curate.
    /// `None` si la note n'a pas de ligne `# ...` en première position.
    pub title: Option<String>,
}
