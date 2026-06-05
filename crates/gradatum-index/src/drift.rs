//! Helper drift Phase A — pré-check 3 niveaux avant reconstruction.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.3.
//!
//! ## Algorithme Phase A
//!
//! Pour chaque entrée `file_checksums` :
//!
//! 1. **Niveau 1 — size strict** : si `on_disk_size == expected_size`
//!    ET `prefix-4KB hash == expected_hash_prefix_4kb` → fichier probablement inchangé.
//!    Court-circuit ~99% des fichiers stables (lectures séquentielles rapides).
//!
//! 2. **Niveau 3 — full sha256** : tout autre cas → hash complet.
//!    Détermine si le fichier est réellement modifié (`mismatch`) ou non (`match`).
//!
//! ## Responsabilités caller (T11)
//!
//! Ce helper retourne un `DriftScanResult` avec les compteurs et la liste des fichiers
//! manquants. La reconstruction (re-parse + re-index + re-embed) est à la charge du
//! caller `gradatum-vault::drift_orchestrator` (tâche T11).
//!
//! La détection des fichiers "untracked" (présents sur disque, absents de `file_checksums`)
//! est aussi la responsabilité de T11 — ce module ne fait que checker les entrées existantes.
//!
//! ## OpenDAL data path (convergence v81 §6)
//!
//! `scan_phase_a` accepte désormais un `&dyn gradatum_storage::Storage` à la place
//! d'un `&Path vault_root`. Les chemins `relative_path` des entrées `file_checksums`
//! sont déjà relatifs — compatibles directement avec le contrat Storage (chemins relatifs).
//! L'opération `stat` fournit la taille sur disque ; `read` fournit le contenu pour le hash.

use std::path::PathBuf;

use sha2::Digest as _;

use gradatum_core::error::GradatumError;
// list_file_checksums est une méthode inhérente pub(crate) sur SqliteIndex — pas besoin de trait.
use gradatum_storage::{Storage, StorageError};

use crate::SqliteIndex;

/// Résultat d'un scan Phase A.
///
/// Contient les compteurs de fichiers à chaque niveau de vérification
/// et la liste des fichiers manquants sur disque.
#[derive(Debug, Default, Clone)]
pub struct DriftScanResult {
    /// Fichiers dont size + prefix-4KB hash correspondent (probablement inchangés).
    /// Ne nécessitent pas de hash complet.
    pub level2_prefix_match: u64,

    /// Fichiers dont le hash complet correspond après divergence sur size ou prefix.
    /// Peut indiquer une modification cosmétique (mtime seul, padding, etc.).
    pub level3_full_hash_match: u64,

    /// Fichiers dont le hash complet diffère — drift confirmé, reconstruction requise.
    pub level3_full_hash_mismatch: u64,

    /// Chemins de fichiers absents sur disque (note référencée mais fichier supprimé).
    pub missing: Vec<PathBuf>,
}

/// Phase A 3 niveaux : size strict → prefix-4KB → full sha256.
///
/// Charge toutes les entrées `file_checksums` depuis `index`, puis vérifie
/// chaque fichier via `storage` (chemins relatifs — convergence v81 §6).
///
/// ## Migration OpenDAL
///
/// `storage` est enraciné sur `vault_root`. Les `relative_path` des entrées
/// `file_checksums` sont directement utilisables comme chemins relatifs Storage.
/// - `stat(relative_path)` → taille sur disque (équivalent `fs::metadata`)
/// - `read(relative_path)` → bytes complets (équivalent `fs::read`)
///
/// ## Erreurs
///
/// Retourne `GradatumError::Storage` si la lecture des checksums échoue.
/// Retourne `GradatumError::Storage` si la lecture d'un fichier existant échoue
/// (permissions, filesystem error). Les fichiers manquants sont collectés dans
/// `DriftScanResult::missing`, pas signalés en erreur.
#[must_use = "DriftScanResult contient les informations de drift — ne pas ignorer"]
pub async fn scan_phase_a(
    storage: &dyn Storage,
    index: &SqliteIndex,
) -> Result<DriftScanResult, GradatumError> {
    let entries = index.list_file_checksums().await?;
    let mut result = DriftScanResult::default();

    for entry in &entries {
        let rel = &entry.relative_path;

        // Niveau 0 — existence : stat NotFound → fichier manquant.
        let meta = match storage.stat(rel).await {
            Ok(m) => m,
            Err(StorageError::NotFound(_)) => {
                // Fichier référencé dans file_checksums mais absent sur disque.
                // On conserve un PathBuf pour compatibilité avec DriftScanResult::missing.
                result.missing.push(PathBuf::from(rel));
                continue;
            }
            Err(e) => {
                return Err(GradatumError::Storage(format!(
                    "stat drift entry '{}': {e}",
                    rel
                )));
            }
        };

        let on_disk_size = meta.size;

        if on_disk_size == entry.expected_size {
            // Niveau 2 : prefix-4KB hash — lire uniquement les 4096 premiers bytes.
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let prefix = compute_prefix_4kb_bytes(&bytes);
            if prefix == entry.expected_hash_prefix_4kb {
                // Fichier probablement inchangé — court-circuit
                result.level2_prefix_match += 1;
                continue;
            }
            // Prefix diffère malgré size identique → niveau 3 full hash (bytes déjà en mémoire)
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        } else {
            // Size diffère → niveau 3 full hash
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        }
    }

    Ok(result)
}

/// Hash SHA-256 des 4 premiers KB de `bytes`.
///
/// Opère en mémoire (bytes déjà chargés par `storage.read()`).
/// Prend au plus `min(bytes.len(), 4096)` bytes — cohérent avec l'ancien comportement.
fn compute_prefix_4kb_bytes(bytes: &[u8]) -> [u8; 32] {
    let prefix_len = bytes.len().min(4096);
    sha2::Sha256::digest(&bytes[..prefix_len]).into()
}

/// Hash SHA-256 complet de `bytes`.
///
/// Acceptable pour Phase 1 (notes Markdown typiquement < 1MB).
/// Phase 2+ : streaming hash si support de notes larges.
fn compute_full_sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_4kb_bytes_short_content() {
        // Vérifie que compute_prefix_4kb_bytes ne panique pas sur contenu < 4KB
        let hash = compute_prefix_4kb_bytes(b"hello");
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn full_sha256_bytes_empty() {
        let hash = compute_full_sha256_bytes(b"");
        // sha256("") = e3b0c44298fc1c149afb...
        assert_eq!(
            hash[0], 0xe3,
            "sha256 d'un contenu vide doit commencer par 0xe3"
        );
    }

    #[test]
    fn prefix_4kb_bytes_truncates_at_4096() {
        // Contenu de 5000 bytes → prefix = hash des 4096 premiers seulement
        let data = vec![0xABu8; 5000];
        let prefix = compute_prefix_4kb_bytes(&data);
        let full = compute_full_sha256_bytes(&data);
        // Les deux hashes doivent différer (contenus tronqués vs complets)
        assert_ne!(
            prefix, full,
            "prefix 4KB et full hash doivent différer pour >4KB"
        );
    }
}
