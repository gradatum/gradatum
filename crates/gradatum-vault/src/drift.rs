//! Orchestration drift Phase A — délègue à `gradatum-index::drift::scan_phase_a`.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.3.
//!
//! ## Responsabilités Phase 1
//!
//! - Appelle `scan_phase_a(vault_root, index)` qui vérifie les entrées `file_checksums`.
//! - Retourne un `DriftScanResult` avec compteurs et liste des fichiers manquants.
//! - La reconstruction (Phase B : re-parse + re-index + re-embed) est hors scope T11.
//!
//! ## Phase B / Phase C (T12+)
//!
//! - Phase B : walk filesystem pour détecter les `.md` non trackés dans `file_checksums`.
//! - Phase C : re-parse + re-index + emit `AuditEvent::DriftFixed` pour chaque fichier
//!   resynchonisé.

use gradatum_core::error::GradatumError;
use gradatum_index::drift::{scan_phase_a, DriftScanResult};

use crate::registry::Vault;

impl Vault {
    /// Déclenche un scan de drift Phase A sur l'ensemble du vault.
    ///
    /// ## Algorithme Phase A (3 niveaux)
    ///
    /// Pour chaque entrée dans la table `file_checksums` :
    /// 1. **Size + prefix-4KB** : si identiques → court-circuit (fichier probablement inchangé).
    /// 2. **Full SHA-256** : hash complet si size/prefix diffèrent.
    ///    - Match → `level3_full_hash_match` (modification cosmétique).
    ///    - Mismatch → `level3_full_hash_mismatch` (drift confirmé, reconstruction requise).
    ///
    /// Les fichiers absents sur disque sont collectés dans `DriftScanResult::missing`.
    ///
    /// ## OpenDAL data path (convergence v81 §6)
    ///
    /// `scan_phase_a` reçoit `&self.storage` (FileStorage OpenDAL) au lieu de `&self.root`.
    /// Les I/O drift passent désormais par l'abstraction Storage.
    ///
    /// ## Phase B / Phase C
    ///
    /// La détection des fichiers "untracked" (présents sur disque, absents de `file_checksums`)
    /// et la reconstruction (re-parse + re-index) sont reportées à T12+.
    ///
    /// ## Erreurs
    ///
    /// - `GradatumError::Storage` si la lecture des checksums depuis SQLite échoue.
    /// - `GradatumError::Storage` si la lecture d'un fichier existant échoue (permissions, etc.).
    pub async fn drift_check(&self) -> Result<DriftScanResult, GradatumError> {
        scan_phase_a(&self.storage, &self.index).await
    }
}
