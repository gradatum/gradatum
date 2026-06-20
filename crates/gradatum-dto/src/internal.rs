//! DTOs pour l'API interne server-to-worker (Wave 2, v0.5.3).
//!
//! Ces types ne sont JAMAIS exposés dans le MCP stub ni dans l'OpenAPI public.
//! Ils sont uniquement consommés par le listener interne sur `127.0.0.1:19092`.
//!
//! ## Isolation
//!
//! Les routes `/internal/v1/*` sont montées sur un binding séparé (loopback uniquement)
//! et ne sont JAMAIS fusionnées avec le router public (`/api/v1/*`).

use serde::{Deserialize, Serialize};

/// Requête `POST /internal/v1/persist/curated` — pipeline 5 writes séquentiels.
///
/// ## Limite transactionnelle
///
/// Les writes sont séquentiels, non atomiques (`Arc<dyn Index>` utilise rusqlite
/// via Mutex, sans pool sqlx). Un échec intermédiaire laisse l'état partiellement
/// écrit — chaque write non-bloquant est loggué WARN mais ne bloque pas la réponse.
/// Le vault (write_note_with_id) est toujours le premier write — si il échoue,
/// la requête retourne 409 (Conflict) ou 500 (Storage), sans tenter les writes suivants.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistCuratedRequest {
    /// ULID de la note (string, 26 chars uppercase).
    pub note_id: String,
    /// Identifiant du tenant (ex: `"main"`).
    pub tenant_id: String,
    /// Titre de la note (utilisé pour `upsert_note_title`).
    pub title: String,
    /// Corps Markdown complet.
    pub body: String,
    /// Section canonique (ex: `"decisions"`, `"lessons-learned"`).
    pub section: String,
    /// Tags de la note.
    pub tags: Vec<String>,
    /// Auteur de la note (ex: `"main-agent"`).
    pub author: Option<String>,
    /// Statut de la note (ex: `"live"`, `"draft"`).
    pub status: String,
    /// Score de confiance [0.0, 1.0] (optionnel — omis si non défini).
    pub trust: Option<f32>,
    /// SHA-256 hex (64 chars) pour la garde optimistic-lock (Fix-B).
    ///
    /// Si présent : `write_note_with_id` applique la garde CAS.
    /// Si absent : création pure (pas de vérification hash).
    pub expected_sha256: Option<String>,
    /// Entrée temporelle inline (optionnelle).
    pub temporal: Option<TemporalEntryDto>,
    /// Liens à upsert (src → dst dans le même vault).
    pub links: Vec<LinkDto>,
    /// Provenance de la note (ex: `"distilled"`, `"human-decision"`).
    pub provenance: Option<String>,
}

/// Requête `POST /internal/v1/persist/embedding` — stockage d'un vecteur.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistEmbeddingRequest {
    /// ULID de la note cible.
    pub note_id: String,
    /// Identifiant du modèle embedder (ex: `"bge-m3"`).
    pub embedder_id: String,
    /// Dimension du vecteur.
    pub dim: u16,
    /// Vecteur d'embedding.
    pub vector: Vec<f32>,
}

/// Requête `POST /internal/v1/persist/forget` — marquage oubli sémantique.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistForgetRequest {
    /// ULID de la note à oublier.
    pub note_id: String,
    /// Identifiant du tenant.
    pub tenant_id: String,
    /// Corps Markdown avec frontmatter `forget=true`.
    pub body: String,
    /// Section de la note.
    pub section: String,
    /// Agent ayant déclenché l'oubli (loggé dans le frontmatter `forgotten_by`).
    pub forgotten_by: Option<String>,
}

/// Requête `POST /internal/v1/persist/distill` — mise à jour d'une note distillée.
///
/// Utilisé par le pipeline de distillation pour mettre à jour le contenu
/// d'une note existante avec un trust réévalué.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistDistillRequest {
    /// ULID de la note à mettre à jour.
    pub note_id: String,
    /// Identifiant du tenant.
    pub tenant_id: String,
    /// Nouveau titre.
    pub title: String,
    /// Nouveau corps Markdown.
    pub body: String,
    /// Section (conservée ou mise à jour).
    pub section: String,
    /// Nouveau score de confiance.
    pub trust: Option<f32>,
    /// SHA-256 hex (64 chars) pour la garde optimistic-lock.
    pub expected_sha256: Option<String>,
    /// Si `true`, ajoute `processed = true` dans les ExtraFields (marquage source distillée).
    ///
    /// Utilisé par `mark_source_processed` pour marquer une source après distillation.
    #[serde(default)]
    pub mark_processed: bool,
    /// Si présent, insère `derived-into = <ulid>` dans les ExtraFields.
    ///
    /// Référence vers la note de synthèse produite depuis cette source.
    pub derived_into: Option<String>,
    /// ULIDs sources ayant produit cette note de synthèse (utilisé pour `derived-from`).
    ///
    /// Présent uniquement lors de la création de la note de synthèse (premier appel,
    /// `mark_processed = false`). Inséré dans les ExtraFields de la note.
    #[serde(default)]
    pub derived_from: Vec<String>,
}

/// Entrée temporelle inline — évite l'import de `TemporalEntry` core dans le DTO.
///
/// Sérialisée en snake_case pour cohérence avec les autres DTOs publics.
#[derive(Debug, Serialize, Deserialize)]
pub struct TemporalEntryDto {
    /// Timestamp de l'ancre en millisecondes Unix.
    pub anchor_ms: i64,
    /// Source de l'ancre : `"occurred_at"` | `"event-date"` | `"valid_from"` | `"created"`.
    pub anchor_src: String,
    /// Type de document CoALA (ex: `"Event"`, `"Static"`).
    pub doc_kind: String,
    /// Timestamp de fin de validité (optionnel, `None` = valide indéfiniment).
    pub valid_until_ms: Option<i64>,
}

/// Lien à upsert (wikilink src → dst dans le même vault).
#[derive(Debug, Serialize, Deserialize)]
pub struct LinkDto {
    /// ULID source du lien.
    pub src: String,
    /// ULID destination du lien.
    pub dst: String,
}

/// Réponse succès pour les handlers `persist/*`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistOkResponse {
    /// ULID de la note créée ou mise à jour.
    pub note_id: String,
    /// Toujours `"ok"` en cas de succès.
    pub status: String,
}

/// Réponse succès pour `persist/embedding`.
#[derive(Debug, Serialize, Deserialize)]
pub struct EmbeddingOkResponse {
    /// ULID de la note cible.
    pub note_id: String,
    /// Identifiant du modèle embedder.
    pub embedder_id: String,
    /// Dimension du vecteur stocké.
    pub dim: usize,
}
