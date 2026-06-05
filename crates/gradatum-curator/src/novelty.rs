//! Filtre de nouveauté — SHA-256 + MinHash 128 permutations Jaccard 0.92.
//!
//! Utilisé en premier dans la cascade curator pour détecter les doublons exacts
//! (par hash de contenu) et les quasi-doublons (via estimation Jaccard MinHash).

use sha2::{Digest, Sha256};

/// Seuil de similarité au-dessus duquel une note est considérée dupliquée.
pub const NOVELTY_THRESHOLD: f32 = 0.92;

/// Seuil de similarité au-dessus duquel une note est considérée révision d'une existante.
pub const REVISION_THRESHOLD: f32 = 0.70;

/// Calcule un hash SHA-256 du corps normalisé (trim + lowercase).
///
/// Utilisé pour la détection de doublons exacts avant MinHash.
pub fn content_hash(body: &str) -> String {
    let normalized = body.trim().to_lowercase();
    let mut h = Sha256::new();
    h.update(normalized.as_bytes());
    // sha2 ≥0.11 : Output<Sha256> est un Array<u8,32> — plus de LowerHex natif.
    let digest: [u8; 32] = h.finalize().into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Décompose un texte en k-shingles (k-grams de mots) encodés en u64 via SHA-256.
///
/// Retourne un vecteur vide si le texte contient moins de `k` mots.
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
            .expect("SHA-256 digest contient toujours ≥ 8 octets");
        out.push(u64::from_le_bytes(bytes));
    }
    out
}

/// Calcule la signature MinHash pour `num_perms` permutations.
///
/// Chaque composante de la signature est le minimum des valeurs de shingle
/// après une permutation pseudo-aléatoire basée sur des constantes de Fibonacci.
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

/// Estimation de similarité Jaccard via deux signatures MinHash.
///
/// Retourne 0.0 si les signatures ont des longueurs différentes ou sont vides.
pub fn jaccard_estimate(a: &[u64], b: &[u64]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let matches = a.iter().zip(b).filter(|(x, y)| x == y).count();
    matches as f32 / a.len() as f32
}

/// Verdict de nouveauté produit par [`assess_novelty`].
#[derive(Debug, Clone)]
pub enum NoveltyVerdict {
    /// Note admise — contenu suffisamment différent de l'existant.
    Admitted,
    /// Note identifiée comme révision d'une note existante.
    RevisionOf {
        /// Identifiant ULID de la note existante similaire.
        existing_id: String,
        /// Score de similarité Jaccard estimé (MinHash 128 perms).
        similarity: f32,
    },
    /// Note identifiée comme doublon d'une note existante (similarité ≥ 0.92).
    Duplicate {
        /// Identifiant ULID de la note dupliquée.
        existing_id: String,
        /// Score de similarité Jaccard estimé (MinHash 128 perms).
        similarity: f32,
    },
}

/// Évalue la nouveauté d'une note par rapport à un ensemble de notes existantes.
///
/// `new_shingles` : k-shingles de la nouvelle note (calculés par [`shingles`]).
/// `existing` : liste de paires `(note_id, shingles)` pour les notes existantes.
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
