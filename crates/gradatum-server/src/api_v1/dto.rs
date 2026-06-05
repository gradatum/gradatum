//! DTOs de l'API v1 — parité stricte legacy vault v1.6.2.
//!
//! Les `Vault*Request` structs vivent dans `gradatum-dto` (single source of truth
//! pour les contrats wire HTTP partagés avec `gradatum-mcp-stub` et `gradatum-sdk-rs`).
//! Les `Vault*Response` structs restent locales — strictement server-internal.
//!
//! Méthodes couvertes :
//! - `POST /api/v1/vault_search`  → [`VaultSearchRequest`] / [`VaultSearchResponse`]
//! - `POST /api/v1/vault_read`    → [`VaultReadRequest`] / [`VaultReadResponse`]
//! - `POST /api/v1/vault_list`    → [`VaultListRequest`] / [`VaultListResponse`]
//! - `GET  /api/v1/vault_status`  → [`VaultStatusResponse`]
//! - `POST /api/v1/vault_graph`   → [`VaultGraphRequest`] / [`VaultGraphResponse`]
//! - `GET  /api/v1/vault_links`   → alias thin vault_graph depth=1
//! - `POST /api/v1/vault_trace`   → [`VaultTraceRequest`] / [`VaultTraceResponse`]
//! - `POST /api/v1/vault_context` → [`VaultContextRequest`] / [`VaultContextResponse`]
//! - `GET  /api/v1/vault_authors` → [`VaultAuthorsResponse`]
//! - `GET  /api/v1/vault_tags`    → [`VaultTagsResponse`]

use serde::Serialize;

// ── Re-exports depuis gradatum-dto (single source of truth) ───────────────────
pub use gradatum_dto::{
    VaultClassifyRequest, VaultContextRequest, VaultDowngradeRequest, VaultGraphRequest,
    VaultLinksRequest, VaultListRequest, VaultReadRequest, VaultSearchRequest, VaultTraceRequest,
    VaultWriteRequest,
};

// ── vault_search ─────────────────────────────────────────────────────────────

/// Résultat individuel d'une recherche.
///
/// Phase 2.x.2 alpha.11 patch.1 — F2 : ajout du champ `title` (H1 extrait
/// post-curate, peut être absent). Permet aux clients de connaître le titre
/// humain sans devoir lire la note complète.
#[derive(Debug, Serialize)]
pub struct SearchHit {
    /// Chemin de la note (ex. `"decisions/my-note"`).
    pub path: String,
    /// Score de pertinence (0.0–1.0).
    pub score: f32,
    /// Titre H1 de la note (extrait post-curate, peut être absent — sérialisé `null`).
    pub title: Option<String>,
    /// Extrait de la note (snippet FTS5 natif).
    pub snippet: Option<String>,
}

/// Réponse `vault_search`.
#[derive(Debug, Serialize)]
pub struct VaultSearchResponse {
    /// Liste des résultats de recherche (peut être vide).
    pub items: Vec<SearchHit>,
}

// ── vault_read ────────────────────────────────────────────────────────────────

/// Réponse `vault_read`.
#[derive(Debug, Serialize)]
pub struct VaultReadResponse {
    /// Chemin de la note lue.
    pub path: String,
    /// Contenu Markdown de la note.
    pub content: String,
    /// Métadonnées YAML frontmatter de la note (format JSON pour transport).
    pub metadata: Option<serde_json::Value>,
    /// Taille en octets du contenu.
    pub size_bytes: u64,
    /// SHA-256 du contenu (hex, 64 chars).
    pub sha256: String,
}

// ── vault_list ────────────────────────────────────────────────────────────────

/// Entrée individuelle dans la liste d'un vault.
#[derive(Debug, Serialize)]
pub struct VaultEntry {
    /// Chemin de la note.
    pub path: String,
    /// Taille en octets.
    pub size_bytes: u64,
    /// Date de dernière modification (ISO 8601 UTC).
    pub modified_at: String,
}

/// Réponse `vault_list`.
#[derive(Debug, Serialize)]
pub struct VaultListResponse {
    /// Entrées listées.
    pub entries: Vec<VaultEntry>,
    /// Curseur pour la page suivante (absent si fin de liste).
    pub next_cursor: Option<String>,
    /// Nombre total de notes (sans pagination).
    pub total: u64,
}

// ── vault_status ──────────────────────────────────────────────────────────────

/// Réponse `vault_status` (GET sans body).
#[derive(Debug, Serialize)]
pub struct VaultStatusResponse {
    /// Tenant ID concerné.
    pub tenant_id: String,
    /// Nombre de notes indexées.
    pub note_count: u64,
    /// Taille totale des notes en octets.
    pub total_size_bytes: u64,
    /// Version du schéma d'index (ex. `"v1"`).
    pub index_version: String,
    /// Timestamp de dernière re-indexation (ISO 8601 UTC).
    pub last_indexed_at: Option<String>,
    /// Santé du vault (`"healthy"` / `"degraded"` / `"offline"`).
    pub health: String,
}

// ── vault_graph ───────────────────────────────────────────────────────────────

/// Arc du graphe (lien entre deux notes).
#[derive(Debug, Serialize)]
pub struct GraphEdge {
    /// Note source.
    pub from: String,
    /// Note cible.
    pub to: String,
    /// Type de lien (ex. `"wikilink"`, `"embed"`).
    pub kind: String,
}

/// Réponse `vault_graph`.
#[derive(Debug, Serialize)]
pub struct VaultGraphResponse {
    /// Noeuds du graphe (chemins de notes).
    pub nodes: Vec<String>,
    /// Arcs du graphe.
    pub edges: Vec<GraphEdge>,
}

// ── vault_links (alias thin vault_graph depth=1) ──────────────────────────────

/// Réponse `vault_links` — structure identique à `VaultGraphResponse`.
pub type VaultLinksResponse = VaultGraphResponse;

// ── vault_trace ───────────────────────────────────────────────────────────────

/// Entrée de résultat de traçage.
#[derive(Debug, Serialize)]
pub struct TraceEntry {
    /// Chemin de la note.
    pub path: String,
    /// Score de pertinence (0.0–1.0).
    pub score: f32,
    /// Extrait contextuel de la note.
    pub snippet: Option<String>,
    /// Tags de la note.
    pub tags: Vec<String>,
}

/// Réponse `vault_trace`.
#[derive(Debug, Serialize)]
pub struct VaultTraceResponse {
    /// Résultats de traçage.
    pub entries: Vec<TraceEntry>,
}

// ── vault_context ─────────────────────────────────────────────────────────────

/// Réponse `vault_context`.
#[derive(Debug, Serialize)]
pub struct VaultContextResponse {
    /// Contexte formaté pour injection dans un prompt LLM.
    pub context: String,
    /// Nombre de tokens estimés dans le contexte.
    pub estimated_tokens: u32,
    /// Sources utilisées pour construire le contexte (chemins de notes).
    pub sources: Vec<String>,
}

// ── vault_authors ─────────────────────────────────────────────────────────────

/// Réponse `vault_authors` (GET sans body).
#[derive(Debug, Serialize)]
pub struct VaultAuthorsResponse {
    /// Liste des auteurs distincts identifiés dans le vault.
    pub authors: Vec<AuthorEntry>,
}

/// Entrée auteur.
#[derive(Debug, Serialize)]
pub struct AuthorEntry {
    /// Nom ou identifiant de l'auteur.
    pub name: String,
    /// Nombre de notes attribuées à cet auteur.
    pub note_count: u64,
}

// ── vault_tags ────────────────────────────────────────────────────────────────

/// Réponse `vault_tags` (GET sans body).
#[derive(Debug, Serialize)]
pub struct VaultTagsResponse {
    /// Liste des tags distincts avec leur fréquence.
    pub tags: Vec<TagEntry>,
}

/// Entrée tag.
#[derive(Debug, Serialize)]
pub struct TagEntry {
    /// Valeur du tag (ex. `"architecture"`, `"urgent"`).
    pub tag: String,
    /// Nombre de notes portant ce tag.
    pub note_count: u64,
}

// ── DTOs write (P2.0b — async 202 enqueue pattern) ────────────────────────────

/// Réponse 202 Accepted — job enqueued (legacy `jobs_v2` — job_id entier SQLite).
#[derive(Debug, Serialize)]
pub struct EnqueuedResponse {
    /// Identifiant du job dans la queue legacy (entier SQLite auto-incrémenté).
    pub job_id: i64,
    /// Statut immédiat (`"queued"`).
    pub status: &'static str,
    /// URL de poll pour suivre le statut (`/api/v1/jobs/<id>`).
    pub poll_url: String,
}

/// Réponse 202 Accepted — job enqueued (Phase 1.2 `gradatum_jobs` — job_id ULID string).
///
/// Utilisé par les handlers bridgés vers `state.job_store` (Apalis `gradatum_jobs`).
/// Contrairement à [`EnqueuedResponse`], le `job_id` est un ULID alphanumérique 26 chars.
#[derive(Debug, Serialize)]
pub struct EnqueuedResponseUlid {
    /// Identifiant du job dans `gradatum_jobs` (ULID 26 chars).
    pub job_id: String,
    /// Statut immédiat (`"queued"`).
    pub status: &'static str,
    /// URL de poll pour suivre le statut (`/api/v1/jobs/<id>/v2`).
    pub poll_url: String,
}

/// Réponse `GET /api/v1/jobs/<id>` — statut d'un job.
#[derive(Debug, Serialize)]
pub struct JobStatusResponse {
    /// Identifiant du job.
    pub job_id: i64,
    /// Statut courant (`"pending"` | `"leased"` | `"done"` | `"dead"`).
    pub status: String,
    /// Nombre de tentatives effectuées.
    pub attempts: i32,
    /// Dernière erreur (absent si aucune).
    pub last_error: Option<String>,
}
