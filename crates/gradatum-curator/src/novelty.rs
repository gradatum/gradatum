//! Novelty filter — SHA-256 + MinHash 128 permutations Jaccard 0.92.
//!
//! Detects exact duplicates (by content hash) and near-duplicates (via MinHash
//! Jaccard estimation). This module is **provided but not wired into
//! [`CuratorPipeline`]'s `process` in 1.0.0** (planned post-1.0); the pipeline
//! currently emits the default `novelty = Admitted` verdict for every note.
//!
//! [`CuratorPipeline`]: crate::CuratorPipeline

use sha2::{Digest, Sha256};

/// Similarity threshold above which a note is considered a duplicate.
pub const NOVELTY_THRESHOLD: f32 = 0.92;

/// Similarity threshold above which a note is considered a revision of an existing note.
pub const REVISION_THRESHOLD: f32 = 0.70;

/// Computes a SHA-256 hash of the normalised body (trimmed + lowercased).
///
/// Used for exact duplicate detection before MinHash.
pub fn content_hash(body: &str) -> String {
    let normalized = body.trim().to_lowercase();
    let mut h = Sha256::new();
    h.update(normalized.as_bytes());
    // sha2 ≥0.11 : Output<Sha256> est un Array<u8,32> — plus de LowerHex natif.
    let digest: [u8; 32] = h.finalize().into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Splits text into k-shingles (word k-grams) encoded as `u64` via SHA-256.
///
/// Returns an empty vector when the text contains fewer than `k` words.
pub fn shingles(text: &str, k: usize) -> Vec<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < k {
        return vec![];
    }
    let mut out = Vec::with_capacity(words.len().saturating_sub(k) + 1);
    for w in words.windows(k) {
        let mut h = Sha256::new();
        for word in w {
            h.update(word.as_bytes());
            h.update(b" ");
        }
        let digest = h.finalize();
        // Sécurité : on prend les 8 premiers octets du digest SHA-256 (256 bits).
        // La tranche [0..8] est garantie par la taille fixe du digest SHA-256.
        let bytes: [u8; 8] = digest[0..8]
            .try_into()
            .expect("SHA-256 digest always contains ≥ 8 bytes");
        out.push(u64::from_le_bytes(bytes));
    }
    out
}

/// Computes the MinHash signature for `num_perms` permutations.
///
/// Each component of the signature is the minimum shingle value after a
/// pseudo-random permutation based on Fibonacci constants.
pub fn minhash_signature(shingles: &[u64], num_perms: usize) -> Vec<u64> {
    let mut sig = vec![u64::MAX; num_perms];
    for &sh in shingles {
        for (i, s) in sig.iter_mut().enumerate() {
            // Permutation via multiplication par une constante de Fibonacci
            let permuted = sh
                .wrapping_mul(0x9E3779B97F4A7C15_u64.wrapping_add(i as u64))
                .wrapping_add(0xBF58476D1CE4E5B9_u64);
            if permuted < *s {
                *s = permuted;
            }
        }
    }
    sig
}

/// Estimates Jaccard similarity from two MinHash signatures.
///
/// Returns 0.0 when the signatures have different lengths or are empty.
pub fn jaccard_estimate(a: &[u64], b: &[u64]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let matches = a.iter().zip(b).filter(|(x, y)| x == y).count();
    matches as f32 / a.len() as f32
}

/// Novelty verdict produced by [`assess_novelty`].
#[derive(Debug, Clone)]
pub enum NoveltyVerdict {
    /// Note admitted — content sufficiently different from existing notes.
    Admitted,
    /// Note identified as a revision of an existing note.
    RevisionOf {
        /// ULID identifier of the similar existing note.
        existing_id: String,
        /// Estimated Jaccard similarity score (MinHash 128 perms).
        similarity: f32,
    },
    /// Note identified as a duplicate of an existing note (similarity ≥ 0.92).
    Duplicate {
        /// ULID identifier of the duplicated note.
        existing_id: String,
        /// Estimated Jaccard similarity score (MinHash 128 perms).
        similarity: f32,
    },
}

/// Evaluates the novelty of a note relative to a set of existing notes.
///
/// `new_shingles`: k-shingles of the new note (computed by [`shingles`]).
/// `existing`: list of `(note_id, shingles)` pairs for existing notes.
pub fn assess_novelty(new_shingles: &[u64], existing: &[(String, Vec<u64>)]) -> NoveltyVerdict {
    let new_sig = minhash_signature(new_shingles, 128);
    let mut best: Option<(String, f32)> = None;
    for (id, sh) in existing {
        let sim = jaccard_estimate(&new_sig, &minhash_signature(sh, 128));
        if best.as_ref().is_none_or(|(_, b)| sim > *b) {
            best = Some((id.clone(), sim));
        }
    }
    match best {
        Some((id, sim)) if sim >= NOVELTY_THRESHOLD => NoveltyVerdict::Duplicate {
            existing_id: id,
            similarity: sim,
        },
        Some((id, sim)) if sim >= REVISION_THRESHOLD => NoveltyVerdict::RevisionOf {
            existing_id: id,
            similarity: sim,
        },
        _ => NoveltyVerdict::Admitted,
    }
}
