//! Factory backend-agnostique + helpers partagés pour la suite de parité `Index`.
//!
//! ## Factory
//!
//! [`make_index`] retourne `Arc<dyn Index>` — le backend concret est sélectionné par
//! la variable d'environnement `GRADATUM_INDEX_BACKEND` :
//!
//! | Valeur | Backend | Statut |
//! |---|---|---|
//! | absente / `"sqlite"` | `SqliteIndex::open_in_memory` | défaut, LIVE |
//!
//! **Ajouter un backend** = 1 bras `match` ci-dessous + 1 entrée dans la matrix CI
//! `index-backends`. Aucun test à dupliquer : tous les invariants sont écrits contre
//! le type effacé `Arc<dyn Index>`.
//!
//! ## Pourquoi backend-agnostique
//!
//! W1 a effacé le type de l'index dans le worker (`Arc<dyn Index>`). Sans suite de
//! parité, aucune garantie d'équivalence ne pourrait être donnée pour un backend
//! alternatif (F-25 Gold). Cette suite verrouille le contrat observable du trait.
//!
//! ## Périmètre — invariants HORS trait `Index` (écart plan assumé)
//!
//! Deux invariants listés dans le plan v0.4.5 W2 ne s'expriment PAS sur la surface
//! du trait `Index` et ne sont donc PAS portés ici — ils sont couverts ailleurs :
//!
//! | Invariant | Couche réelle | Couverture |
//! |---|---|---|
//! | **history CoW** (versioning `.history` copy-on-write) | vault | suites `gradatum-vault` |
//! | **ordre RRF** (Reciprocal Rank Fusion FTS ⊕ sémantique) | server/search | `gradatum-server/tests/vault_search_rrf_path.rs` |
//!
//! La parité d'index garantit les *entrées* que RRF consomme (ordres FTS et
//! sémantique, cf. `fts_semantic_search.rs`) ; la fusion elle-même reste testée à
//! la couche serveur. Le CoW est une responsabilité d'écriture vault, sans méthode
//! correspondante sur `Index`.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::index::Index;
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Nom de la variable d'environnement sélectionnant le backend testé.
pub const BACKEND_ENV: &str = "GRADATUM_INDEX_BACKEND";

/// Construit un index neuf (schéma migré, vide) pour le backend courant.
///
/// Le backend est lu depuis `GRADATUM_INDEX_BACKEND` (défaut : `"sqlite"`).
/// Un backend inconnu provoque un `panic!` explicite (échec de configuration de
/// test, pas un faux négatif).
///
/// # Panics
///
/// Si l'ouverture de l'index échoue, ou si la valeur de `GRADATUM_INDEX_BACKEND`
/// ne correspond à aucun backend connu.
pub async fn make_index() -> Arc<dyn Index> {
    let backend = std::env::var(BACKEND_ENV).unwrap_or_else(|_| "sqlite".to_string());
    match backend.as_str() {
        "sqlite" => Arc::new(
            SqliteIndex::open_in_memory()
                .await
                .expect("SqliteIndex::open_in_memory — backend sqlite"),
        ),
        other => panic!(
            "{BACKEND_ENV}={other:?} : backend inconnu. Backends supportés : \"sqlite\". \
             Ajouter un bras dans tests/common/mod.rs::make_index + 1 entrée matrix CI."
        ),
    }
}

/// Étiquette lisible du backend courant (pour messages d'assertion).
pub fn backend_label() -> String {
    std::env::var(BACKEND_ENV).unwrap_or_else(|_| "sqlite".to_string())
}

// ── Constructeurs de notes ────────────────────────────────────────────────────

/// `Frontmatter` minimal : section `Decisions`, statut `Live`, sans locus ni tags.
pub fn minimal_frontmatter(vault_id: &str) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// `Frontmatter` avec section + statut choisis (le reste = [`minimal_frontmatter`]).
pub fn frontmatter_with(vault_id: &str, section: Section, status: NoteStatus) -> Frontmatter {
    Frontmatter {
        section,
        status,
        ..minimal_frontmatter(vault_id)
    }
}

/// Construit une `Note` complète (id généré) avec frontmatter + corps donnés.
pub fn make_note(fm: Frontmatter, body: &str) -> Note {
    let hash = ContentHash::compute(&fm, body);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

/// Construit une `Note` avec un `NoteId` imposé (round-trip déterministe).
pub fn make_note_with_id(id: NoteId, fm: Frontmatter, body: &str) -> Note {
    let hash = ContentHash::compute(&fm, body);
    Note {
        id,
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

/// Timestamp UTC déterministe (epoch ms) pour les tests temporels reproductibles.
pub fn fixed_ms(epoch_ms: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(epoch_ms)
        .single()
        .expect("epoch_ms valide")
}
