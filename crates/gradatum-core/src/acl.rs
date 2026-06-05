//! Traits ACL (Access Control List) pour Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.11.
//!
//! ## Design
//!
//! Les traits ACL vivent dans `gradatum-core` pour permettre aux crates aval
//! (`gradatum-vault`, `gradatum-curator`) de dépendre des interfaces sans importer
//! `gradatum-acl-policy` (anti-cycle).
//!
//! Phase 1 : seul `allow_read` est matériellement utilisé par `gradatum-vault`.
//! Les autres méthodes sont implémentées en Phase 2+ dans `gradatum-acl-policy`.
//!
//! ## ACLFilter
//!
//! `ACLFilter` généralise le visibility marker `personal-classified` du prédécesseur
//! (legacy vault v1.6.2, R13) — au lieu d'un marqueur hardcodé, le filtre est pluggable
//! via ce trait.

use crate::note::Note;
use crate::scope::BearerId;

/// Politique de contrôle d'accès à une note.
///
/// Implémentée par `gradatum-acl-policy` en Phase 2+.
///
/// Phase 1 : `gradatum-vault` utilise une implémentation par défaut `PermissivePolicy`
/// permissive single-tenant default — override with a stricter AclPolicy for multi-tenant deployments.
pub trait AclPolicy: Send + Sync {
    /// Autorise la lecture de la note par ce porteur ?
    ///
    /// Phase 1 : toujours `true` (monoutilisateur).
    /// Phase 2+ : vérifie les règles ACL dans le `Frontmatter` de la note.
    fn allow_read(&self, note: &Note, bearer: &BearerId) -> bool;

    /// Autorise l'écriture/modification de la note par ce porteur ?
    fn allow_write(&self, note: &Note, bearer: &BearerId) -> bool;

    /// Autorise la suppression de la note par ce porteur ?
    fn allow_delete(&self, note: &Note, bearer: &BearerId) -> bool;
}

/// Filtre de visibilité sur une liste de notes.
///
/// Généralise le visibility marker `personal-classified` du legacy vault v1.6.2 (R13).
/// Au lieu d'un marqueur hardcodé, la logique de filtrage est injectée via ce trait.
///
/// Utilisé par `gradatum-vault` dans les endpoints de listing pour masquer les notes
/// non-accessibles au bearer courant.
pub trait ACLFilter: Send + Sync {
    /// Filtre `notes` et retourne uniquement les notes accessibles au `bearer`.
    ///
    /// Retourne des références vers les notes du slice d'entrée — zéro copie.
    fn filter<'a>(&self, notes: &'a [Note], bearer: &BearerId) -> Vec<&'a Note>;
}
