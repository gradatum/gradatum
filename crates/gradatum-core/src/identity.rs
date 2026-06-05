//! Couche 1 — Identity immutable : NoteId, ContentHash, NoteVersion, IntegritySignature.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.2.
//!
//! ## Invariants
//!
//! - `NoteId` : ULID, unique, sortable lexicographiquement (timestamp prefix).
//!   Format : `01HQK0ABCDEF0123456789GHIJ` (26 chars Base32).
//! - `ContentHash` : SHA-256 de `JCS(frontmatter) ++ "\n---\n" ++ body`.
//!   Déterministe cross-langage via JCS RFC 8785. Permet drift detection.
//! - `NoteVersion` : compteur monotone incrémenté à chaque écriture.
//! - `IntegritySignature` : Phase 1 = toujours `None`. Phase 2+ HMAC/Ed25519.
//!
//! ## Pourquoi JCS ?
//!
//! `serde_yml::to_string` non-déterministe entre versions de lib.
//! `serde_json::to_string` non-canonique (ordre clés non garanti).
//! JCS RFC 8785 = standard IETF : clés ordonnées, floats IEEE 754 canonical,
//! strings escapées de façon normative. Hash bit-identique Rust/Python/Go/JS.
//! Décision Q4 brainstorming the maintainer 2026-05-03.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::frontmatter::Frontmatter;

/// Clé primaire relationnelle de tout dans Gradatum.
///
/// ULID (Universally Unique Lexicographically Sortable Identifier) :
/// - 128 bits : 48 bits timestamp ms + 80 bits random.
/// - Sortable lexicographiquement → les notes plus récentes sont "après" les anciennes.
/// - Monotone dans le même milliseconde (pas de collision dans un process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub Ulid);

impl NoteId {
    /// Génère un nouveau NoteId unique.
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// Retourne le timestamp ULID en millisecondes Unix.
    ///
    /// Utile pour trier les notes par ordre d'insertion sans accéder au frontmatter.
    pub fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }
}

impl Default for NoteId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for NoteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hash SHA-256 du contenu canonique d'une note.
///
/// **Canonicalisation = JCS RFC 8785** (`serde_jcs`) :
/// ```text
/// input  = JCS(frontmatter_as_json) ++ b"\n---\n" ++ body_utf8
/// hash   = SHA-256(input)
/// ```
///
/// Le hash est **indépendant du format on-disk** (YAML vs TOML pour le frontmatter) :
/// le frontmatter est re-sérialisé en JSON canonique avant le hash → reproductible
/// cross-langage (Python, Go, JavaScript, Rust) avec la même lib JCS.
///
/// ## Utilisation
///
/// - Drift detection : compare `ContentHash::compute(reparse(md))` vs `notes.content_hash` SQLite.
/// - Cache key dans `gradatum-cache` : `(vault_id, content_hash)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// Calcule le hash depuis un frontmatter et un body.
    ///
    /// # Panics
    ///
    /// Ne panique jamais en production. `serde_jcs::to_string` retourne une erreur
    /// uniquement si `Frontmatter` contient des types non-sérialisables en JSON (ex. `f32::NAN`).
    /// `Frontmatter` ne contient aucun float → `expect` est justifié ici.
    pub fn compute(frontmatter: &Frontmatter, body: &str) -> Self {
        use sha2::{Digest, Sha256};

        // JCS RFC 8785 : clés ordonnées, floats IEEE 754, strings normatives.
        // Garantit un hash bit-identique quel que soit le producer ou la lib YAML.
        let canonical = serde_jcs::to_string(frontmatter).expect(
            "Frontmatter est toujours sérialisable en JCS (pas de f32::NAN ni de types non-JSON)",
        );

        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hasher.update(b"\n---\n");
        hasher.update(body.as_bytes());

        ContentHash(hasher.finalize().into())
    }

    /// Retourne la représentation hexadécimale du hash (64 chars lowercase).
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

/// Compteur de version monotone par note.
///
/// Incrémenté à chaque écriture par `gradatum-worker`. Le couple `(NoteId, NoteVersion)`
/// est unique dans le store (invariant spec §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteVersion(pub u32);

impl NoteVersion {
    /// Version initiale à la création d'une note.
    pub fn initial() -> Self {
        Self(1)
    }

    /// Retourne la version suivante (version courante + 1).
    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

/// Signature cryptographique optionnelle (Phase 2+).
///
/// **Phase 1** : toujours `None` dans `Note`. La détection de drift via `ContentHash` suffit.
/// **Phase 2+** : HMAC-SHA256 ou Ed25519 signature vault-scoped, implémentée dans
/// `gradatum-acl-auth` avec la couche bearer auth.
///
/// Sépare drift accidentel (`ContentHash` Phase 1) de tamper malveillant
/// (`IntegritySignature` Phase 2+) — décision Q5/B13 brainstorming the maintainer 2026-05-03.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntegritySignature(pub Vec<u8>);
