//! Événements d'audit typés pour le trail Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.9.
//!
//! ## Design
//!
//! `AuditEvent` est le pivot de l'observabilité Gradatum :
//! - Double stockage : SQLite (requêtes) + JSONL (SIEM external SIEM).
//! - `AuditEventType` : enum typée avec variantes nommées — décision Q8.
//!   **Pas de `payload: serde_json::Value`** (perte de typage, diff difficile).
//! - `extra: ExtraFields` : champs hors-spec préservés verbatim (B8 forward-compat).
//!   Allocation lazy — `None` si aucun extra.
//! - `#[non_exhaustive]` sur `AuditEventType` : forward-compat SemVer-minor (§2.9 changelog).
//!
//! ## Corrélation
//!
//! `correlation_id: Option<Ulid>` — permet de regrouper plusieurs `AuditEvent` issus
//! d'une même opération atomique (ex. import batch, résolution d'overrides en cascade).
//! Omis en sérialisation si `None`.
//!
//! ## Décisions
//!
//! - Q8 : enum typée + ExtraFields (suppression payload Value redondant)
//! - §2.9 changelog : `#[non_exhaustive]` obligatoire pour ajout variante sans breaking change

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::author::AuthorRef;
use crate::frontmatter::ExtraFields;
use crate::identity::{ContentHash, NoteId, NoteVersion};
use crate::scope::OverrideScope;
use crate::status::NoteStatus;

/// Événement d'audit atomique.
///
/// Produit par chaque opération significative sur une note ou un override.
/// Persisté en SQLite (`audit_events` table) ET streamé en JSONL pour external SIEM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Note concernée par l'événement.
    pub note_id: NoteId,

    /// Type typé de l'événement — pas de `payload: Value` (décision Q8).
    pub event_type: AuditEventType,

    /// Acteur responsable de l'action.
    pub actor: AuthorRef,

    /// Timestamp UTC de l'événement.
    pub occurred_at: DateTime<Utc>,

    /// Champs hors-spec préservés verbatim — allocation lazy (B8 forward-compat).
    ///
    /// Omis en sérialisation si vide (économie taille JSONL external SIEM).
    #[serde(default, skip_serializing_if = "ExtraFields::is_empty")]
    pub extra: ExtraFields,

    /// ID de corrélation optionnel pour regrouper des événements d'une même opération.
    ///
    /// Omis en sérialisation si `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Ulid>,
}

/// Type d'événement d'audit.
///
/// Chaque variante porte exactement les données nécessaires — pas de champ `payload` générique.
/// Le tag JSON est en kebab-case (`#[serde(rename_all = "kebab-case")]`).
///
/// `#[non_exhaustive]` est **obligatoire** : de nouvelles variantes pourront être ajoutées
/// en SemVer-minor sans breaking change (les `match` dans les crates aval doivent avoir
/// un bras `_ => {}` ou `#[non_exhaustive]` handler).
///
/// Spec §2.9 changelog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AuditEventType {
    /// Note créée pour la première fois.
    Created,

    /// Frontmatter ou body de la note modifié.
    Updated {
        /// Liste des champs modifiés (ex. `["status", "tags"]`).
        fields_changed: Vec<String>,
    },

    /// Statut du cycle de vie changé (ex. `Draft → PendingReview`).
    StatusChanged {
        /// Statut précédent.
        from: NoteStatus,
        /// Nouveau statut.
        to: NoteStatus,
        /// Raison du changement (optionnelle — fournie par le curator ou l'humain).
        reason: Option<String>,
    },

    /// Embedding calculé ou re-calculé.
    Embedded {
        /// Identifiant de l'embedder utilisé (ex. `"bge-small-en-v1.5"`, `"bge-m3"`).
        embedder_id: String,
        /// Version du modèle (hash ou tag sémantique).
        model_version: String,
        /// Nombre de dimensions du vecteur produit.
        dim: u16,
    },

    /// Note indexée en FTS (recherche plein texte).
    Indexed {
        /// Nombre de tokens FTS générés.
        fts_tokens: u32,
    },

    /// Politique ACL modifiée sur la note.
    AclChanged {
        /// Diff textuel de la politique (format libre Phase 1).
        policy_diff: String,
    },

    /// Override appliqué sur la note.
    OverrideApplied {
        /// Scope de l'override.
        scope: OverrideScope,
        /// Discriminant du type d'override (ex. `"metadata"`, `"acl"`).
        override_type: String,
    },

    /// Override révoqué sur la note.
    OverrideRevoked {
        /// Scope de l'override révoqué.
        scope: OverrideScope,
        /// Discriminant du type d'override.
        override_type: String,
    },

    /// Score de la note re-calculé (decay + pagerank).
    ScoreRecomputed {
        /// Nouveau score de decay.
        new_decay: f32,
        /// Nouveau score pagerank.
        new_pagerank: f32,
    },

    /// Drift détecté entre le fichier Markdown sur disque et l'index SQLite.
    DriftDetected {
        /// Hash stocké dans l'index SQLite.
        stored_hash: ContentHash,
        /// Hash re-calculé depuis le fichier Markdown.
        computed_hash: ContentHash,
    },

    /// Note lue par un porteur (traçabilité accès).
    Read {
        /// Identifiant du porteur.
        bearer_id: String,
        /// Champs accédés (ex. `["body", "frontmatter.tags"]`).
        fields_accessed: Vec<String>,
    },

    /// Note supprimée (passage en Garbage ou suppression physique).
    Deleted {
        /// Raison optionnelle de la suppression.
        reason: Option<String>,
    },

    /// Note restaurée depuis une version antérieure.
    Restored {
        /// Version depuis laquelle la note a été restaurée.
        from_version: NoteVersion,
    },
}

// ─── P2.0b — HTTP service audit types (caveat C4) ──────────────────────────

/// Sous-module dédié aux types d'audit du service HTTP gradatum-server (P2.0b).
///
/// Distinct des types Phase 1 (`AuditEvent`, `AuditEventType`) qui restent inchangés.
/// Les types ci-dessous sont flat (sans enum typée) pour correspondre au contrat
/// JSONL défini en design spec P2.0.
pub mod http {
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};

    /// Acteur ayant déclenché l'opération HTTP auditée.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HttpAuditActor {
        /// Key ID du token JWT.
        pub kid: String,
        /// Subject du token JWT.
        pub sub: String,
        /// Audience du token JWT.
        pub aud: String,
    }

    /// Événement d'audit flat pour le service HTTP gradatum-server.
    ///
    /// Persisté en JSONL avec rotation daily (mode 0640). Distinct de
    /// `AuditEvent` Phase 1 (enum typée pour SQLite+SIEM interne).
    ///
    /// ## Champs
    ///
    /// - `ts` : timestamp UTC de l'opération.
    /// - `event` : nom libre de l'opération (ex. `"vault_write"`, `"vault_read"`).
    /// - `actor` : porteur JWT extrait par le middleware.
    /// - `tenant_id` : identifiant tenant concerné.
    /// - `locus` : section/note path (ex. `"decisions/ma-note"`).
    /// - `note_id` : ULID de la note si applicable.
    /// - `content_hash` : `sha256:<hex>` JCS RFC 8785 du contenu canonique.
    /// - `outcome` : résultat de l'opération (ex. `"admitted"`, `"rejected"`, `"error"`).
    /// - `curator` : métadonnées curator optionnelles (score, labels, etc.).
    /// - `request_id` : identifiant de corrélation de la requête HTTP.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HttpAuditEvent {
        /// Timestamp UTC de l'opération.
        pub ts: DateTime<Utc>,
        /// Nom de l'opération auditée.
        pub event: String,
        /// Acteur ayant déclenché l'opération.
        pub actor: HttpAuditActor,
        /// Identifiant du tenant concerné.
        pub tenant_id: String,
        /// Chemin section/note (ex. `"decisions/ma-note"`).
        pub locus: String,
        /// ULID de la note si applicable — omis si absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub note_id: Option<String>,
        /// Hash JCS RFC 8785 + SHA-256 du contenu canonique — omis si absent.
        ///
        /// Format : `"sha256:<hex64>"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub content_hash: Option<String>,
        /// Résultat de l'opération.
        pub outcome: String,
        /// Métadonnées curator optionnelles — omis si absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub curator: Option<serde_json::Value>,
        /// Identifiant de corrélation de la requête HTTP.
        pub request_id: String,
    }

    /// Trait sink pour l'enregistrement des événements d'audit HTTP.
    ///
    /// La production impl est `JsonlFileSink` (dans `gradatum-server`).
    /// Une impl `NoopAuditSink` peut être utilisée dans les tests.
    #[async_trait]
    pub trait AuditSink: Send + Sync + 'static {
        /// Enregistre un événement d'audit de façon durable.
        ///
        /// ## Effets de bord
        ///
        /// Implémentations production : écriture disque + flush.
        /// Erreur I/O retournée sans panic.
        async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error>;
    }

    /// Calcule le hash JCS RFC 8785 + SHA-256 d'une valeur JSON.
    ///
    /// Retourne `"sha256:<hex>"`. Deux valeurs JSON équivalentes (champs
    /// dans un ordre différent) produisent le même hash grâce à la
    /// canonicalisation JCS.
    ///
    /// ## Erreurs
    ///
    /// Retourne `serde_json::Error` si la valeur ne peut pas être canonicalisée
    /// (ex. nombre NaN, clé flottante de map — `serde_jcs::to_string` délègue
    /// à l'infrastructure serde_json pour les erreurs).
    pub fn content_hash_jcs(value: &serde_json::Value) -> Result<String, serde_json::Error> {
        use sha2::{Digest, Sha256};
        let canonical = serde_jcs::to_string(value)?;
        let mut h = Sha256::new();
        h.update(canonical.as_bytes());
        // sha2 ≥0.11 : Output<Sha256> est un Array<u8,32> — plus de LowerHex natif.
        let digest: [u8; 32] = h.finalize().into();
        Ok(format!(
            "sha256:{}",
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
}
