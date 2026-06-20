//! Immutable identity types: `NoteId`, `ContentHash`, `NoteVersion`, `IntegritySignature`.
//!
//! ## Invariants
//!
//! - `NoteId`: ULID, unique, lexicographically sortable (timestamp prefix).
//!   Format: `01HQK0ABCDEF0123456789GHIJ` (26 Base32 chars).
//! - `ContentHash`: SHA-256 of `JCS(frontmatter) ++ "\n---\n" ++ body`.
//!   Cross-language deterministic via JCS RFC 8785. Enables drift detection.
//! - `NoteVersion`: monotonic counter incremented on every write.
//! - `IntegritySignature`: optional — HMAC-SHA256 or Ed25519 via `gradatum-acl-auth`.
//!
//! ## Why JCS?
//!
//! `serde_yml::to_string` is non-deterministic across library versions.
//! `serde_json::to_string` is non-canonical (key order not guaranteed).
//! JCS RFC 8785 is an IETF standard: sorted keys, canonical IEEE 754 floats,
//! normative string escaping. Produces bit-identical hashes in Rust, Python, Go, and JS.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::frontmatter::Frontmatter;

/// Primary relational key for every entity in Gradatum.
///
/// ULID (Universally Unique Lexicographically Sortable Identifier):
/// - 128 bits: 48-bit millisecond timestamp + 80-bit random component.
/// - Lexicographically sortable → newer notes sort after older ones.
/// - Monotone within the same millisecond (no collisions within a single process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub Ulid);

impl NoteId {
    /// Generates a new unique `NoteId`.
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// Returns the ULID timestamp in Unix milliseconds.
    ///
    /// Useful for sorting notes by insertion order without accessing the frontmatter.
    pub fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }

    /// Produces a **deterministic** `NoteId` from an arbitrary key.
    ///
    /// ## Construction
    ///
    /// ```text
    /// id = ULID::from_bytes(SHA-256(key)[..16])
    /// ```
    ///
    /// The leading 16 bytes of the SHA-256 hash are interpreted directly as the 128 bits
    /// of a ULID. The result is **not chronological** (the ULID timestamp bits come from
    /// the hash, not the clock) — this is intentional: ordinal position is not required
    /// for derived notes.
    ///
    /// ## Key format for code notes
    ///
    /// The caller constructs the key by concatenating components with `\x1f`
    /// (ASCII Unit Separator — never appears in a Rust identifier):
    /// ```text
    /// key = vault_id ‖ '\x1f' ‖ source_path ‖ '\x1f' ‖ kind ‖ '\x1f' ‖ qualified_name
    /// ```
    ///
    /// ## Guarantees
    ///
    /// - Determinism: same key → same `NoteId` (idempotent rebuild).
    /// - SHA-256 collision probability: negligible (2^128 for 16 bytes).
    /// - No ordinal: using an ordinal would break stability when a symbol is inserted
    ///   before another in the source file.
    ///
    /// ## Side effects
    ///
    /// None. Pure function.
    #[must_use]
    pub fn derived_from(key: &[u8]) -> NoteId {
        let hash = Sha256::digest(key);
        // Interpret the first 16 bytes of the hash as the 128 bits of a ULID.
        // The timestamp and random bits come from the hash — not chronological.
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash[..16]);
        NoteId(Ulid::from_bytes(bytes))
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

/// SHA-256 hash of the canonical content of a note.
///
/// **Canonicalisation = JCS RFC 8785** (`serde_jcs`):
/// ```text
/// input  = JCS(frontmatter_as_json) ++ b"\n---\n" ++ body_utf8
/// hash   = SHA-256(input)
/// ```
///
/// The hash is **independent of the on-disk format** (YAML vs TOML for the frontmatter):
/// the frontmatter is re-serialised as canonical JSON before hashing → reproducible
/// across languages (Python, Go, JavaScript, Rust) with the same JCS library.
///
/// ## Usage
///
/// - Drift detection: compare `ContentHash::compute(reparse(md))` against `notes.content_hash` in SQLite.
/// - Cache key in `gradatum-cache`: `(vault_id, content_hash)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// Computes the hash from a frontmatter and a body.
    ///
    /// # Panics
    ///
    /// Does not panic in practice. `serde_jcs::to_string` returns an error only when the
    /// serialised value contains a non-finite `f64` (`NaN` / `Infinity`).
    /// `Frontmatter` declares no `f32`/`f64` struct fields, so the only possible source of
    /// a non-finite float is a `toml::Value::Float` inside `extra`. In `serde_jcs`, a
    /// non-finite float is **coerced to `null`** rather than triggering an error — so the
    /// function will not panic even in that case; however, the hash will differ from what
    /// a strictly IEEE 754 serialiser would produce.
    ///
    /// **Caveat**: if a `toml::Value::Float(f64::NAN)` is inserted into `extra` programmatically
    /// (TOML does not parse `NaN` per RFC 3.3 §2.3, but it can be constructed in code),
    /// `serde_jcs` will serialise it as `null`, potentially producing a hash collision with
    /// a note that has a `null` for that field. Avoid non-finite floats in `extra`.
    pub fn compute(frontmatter: &Frontmatter, body: &str) -> Self {
        use sha2::{Digest, Sha256};

        // JCS RFC 8785: sorted keys, canonical IEEE 754 floats, normative string escaping.
        // Guarantees a bit-identical hash regardless of the producer or YAML library.
        let canonical = serde_jcs::to_string(frontmatter).expect(
            "Frontmatter est toujours sérialisable en JCS : \
             pas de f64::NAN/INFINITY possible (TOML RFC 3.3 garantit des floats finis)",
        );

        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hasher.update(b"\n---\n");
        hasher.update(body.as_bytes());

        ContentHash(hasher.finalize().into())
    }

    /// Returns the hexadecimal representation of the hash (64 lowercase chars).
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

/// Monotonic version counter per note.
///
/// Incremented on every write by `gradatum-worker`. The pair `(NoteId, NoteVersion)`
/// is unique in the store (uniqueness invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteVersion(pub u32);

impl NoteVersion {
    /// Initial version at note creation.
    pub fn initial() -> Self {
        Self(1)
    }

    /// Returns the next version (current + 1).
    ///
    /// Uses [`u32::saturating_add`] to avoid overflow at the theoretical maximum.
    /// In practice, a version counter reaching `u32::MAX` indicates a data anomaly
    /// (4 billion writes on a single note), but the saturating behaviour preserves
    /// safety over a panic or silent wrapping.
    pub fn next(&self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Optional cryptographic signature.
///
/// Absent by default: drift detection via `ContentHash` is sufficient for most use cases.
/// Vault-scoped HMAC-SHA256 or Ed25519 is available via `gradatum-acl-auth` with bearer auth.
///
/// Separates accidental drift (`ContentHash`) from malicious tampering (`IntegritySignature`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntegritySignature(pub Vec<u8>);

#[cfg(test)]
mod tests_note_version {
    use super::*;

    /// `next()` incrémente normalement pour une valeur courante.
    #[test]
    fn next_increments_by_one() {
        let v = NoteVersion(5);
        assert_eq!(v.next().0, 6);
    }

    /// `next()` au bord haut sature à `u32::MAX` (pas de panique, pas de wrapping).
    #[test]
    fn next_saturates_at_max() {
        let v = NoteVersion(u32::MAX);
        assert_eq!(
            v.next().0,
            u32::MAX,
            "next() doit saturer à u32::MAX et non paniquer ou wrapper"
        );
    }

    /// `next()` sur `u32::MAX - 1` donne `u32::MAX` (borne -1 → max, pas saturation).
    #[test]
    fn next_at_max_minus_one_reaches_max() {
        let v = NoteVersion(u32::MAX - 1);
        assert_eq!(v.next().0, u32::MAX);
    }
}

#[cfg(test)]
mod tests_derived_from {
    use super::*;

    /// Séparateur utilisé pour construire la clé dérivée (ASCII Unit Separator).
    const SEP: u8 = 0x1f;

    fn build_key(parts: &[&str]) -> Vec<u8> {
        let mut key = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                key.push(SEP);
            }
            key.extend_from_slice(part.as_bytes());
        }
        key
    }

    /// Déterminisme : même clé → même NoteId.
    #[test]
    fn derived_from_deterministic() {
        let key = build_key(&["code-gradatum", "src/lib.rs", "fn", "my_function"]);
        let id1 = NoteId::derived_from(&key);
        let id2 = NoteId::derived_from(&key);
        assert_eq!(id1, id2, "même clé doit produire le même NoteId");
    }

    /// Distinction : clés différentes → NoteIds différents.
    #[test]
    fn derived_from_distinct_keys_produce_distinct_ids() {
        let key_a = build_key(&["code-gradatum", "src/lib.rs", "fn", "func_a"]);
        let key_b = build_key(&["code-gradatum", "src/lib.rs", "fn", "func_b"]);
        let id_a = NoteId::derived_from(&key_a);
        let id_b = NoteId::derived_from(&key_b);
        assert_ne!(
            id_a, id_b,
            "clés différentes doivent produire des NoteIds différents"
        );
    }

    /// Distinction path : même nom mais source_path différent → NoteIds différents.
    #[test]
    fn derived_from_different_paths_distinct() {
        let key_a = build_key(&["code-gradatum", "src/a.rs", "fn", "helper"]);
        let key_b = build_key(&["code-gradatum", "src/b.rs", "fn", "helper"]);
        assert_ne!(
            NoteId::derived_from(&key_a),
            NoteId::derived_from(&key_b),
            "même nom dans des fichiers différents = NoteIds distincts"
        );
    }

    /// Format ULID : 26 caractères Base32 valides.
    #[test]
    fn derived_from_produces_valid_ulid_string() {
        let key = build_key(&["code-test", "src/main.rs", "struct", "MyStruct"]);
        let id = NoteId::derived_from(&key);
        let s = id.to_string();
        assert_eq!(s.len(), 26, "ULID doit faire 26 caractères, got: '{s}'");
        // Caractères Base32 Crockford valides : 0-9, A-Z sauf I, L, O, U.
        for ch in s.chars() {
            assert!(
                matches!(ch, '0'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='T' | 'V'..='Z'),
                "caractère ULID invalide: '{ch}' dans '{s}'"
            );
        }
    }

    /// Stabilité : la valeur exacte du hash ne doit pas changer sans intention.
    /// Ce test documente la valeur attendue pour détecter toute régression.
    #[test]
    fn derived_from_stable_value() {
        let key = build_key(&[
            "code-gradatum",
            "crates/gradatum-core/src/lib.rs",
            "fn",
            "stable_fn",
        ]);
        let id = NoteId::derived_from(&key);
        let s = id.to_string();
        // Valeur FIGÉE le 2026-06-13 — calculée une fois via SHA-256("code-gradatum\x1f...").
        // Si ce test échoue, l'algorithme de dérivation a changé. Toute modification intentionnelle
        // exige de recalculer cette valeur et de mettre à jour le commentaire + date.
        assert_eq!(
            s, "0EQB76CD42PNQN1YPGZK4QNNWW",
            "NoteId::derived_from doit être déterministe cross-version (non-régression)"
        );
    }
}
