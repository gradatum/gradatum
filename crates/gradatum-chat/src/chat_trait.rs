//! Trait `Chat` + types de contrat partagés entre toutes les implémentations.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md`
//! §2.13 (`CuratorConfig.llm_review_*`) + §0.4 A11 (curator LLM gating).
//!
//! ## Invariant d'isolation
//!
//! `gradatum-chat` ne communique pas directement avec le disque, SQLite ou le
//! scheduler. Son seul effet de bord est un appel réseau optionnel (HttpChat).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::error::ChatError;

/// Classificateur de notes pour le curator.
///
/// Implémenté par :
/// - [`crate::heuristic::Heuristic`] — offline, regex/keyword (invariant #3 / R1)
/// - [`crate::http::HttpChat`] — reqwest OpenAI-compat (D-perf-3)
/// - [`crate::noop::Noop`] — zéro logique, utile en tests
/// - [`crate::circuit_breaker::CircuitBreakerChat`] — decorator pattern
#[async_trait]
pub trait Chat: Send + Sync {
    /// Classifie une note pour décider de son admission dans le vault.
    ///
    /// Retourne un `CuratorVerdict` avec le statut proposé, un score de confiance
    /// 0.0-1.0 et une raison textuelle.
    ///
    /// # Effets de bord
    ///
    /// - `Heuristic` : aucun.
    /// - `HttpChat` : appel réseau vers un endpoint OpenAI-compatible.
    /// - `CircuitBreakerChat` : mise à jour atomique des compteurs de failures.
    async fn classify_curator(
        &self,
        note: &Note,
        context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError>;

    /// Identifie le type de backend sans downcast.
    fn backend_kind(&self) -> ChatBackend;
}

/// Contexte optionnel fourni par l'appelant pour améliorer la classification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorContext {
    /// IDs de notes similaires déjà présentes dans le vault (hint de dédup).
    pub similar_note_ids: Vec<String>,
    /// Tags du vault courant (contexte thématique).
    pub vault_tags: Vec<String>,
}

/// Verdict retourné par un classifieur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorVerdict {
    /// Statut proposé pour la note après classification.
    pub proposed_status: NoteStatus,
    /// Confiance dans la décision — intervalle `0.0..=1.0`.
    pub confidence: f32,
    /// Explication textuelle de la décision (loggée, jamais exposée en API publique).
    pub reason: String,
    /// Backend qui a produit ce verdict.
    pub backend: ChatBackend,
}

/// Discriminant de backend — évite le downcast dans les logs/métriques.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatBackend {
    /// Classificateur heuristique offline (regex/keywords).
    Heuristic,
    /// Backend HTTP OpenAI-compatible.
    Http,
    /// Backend noop — toujours `PendingReview`, confiance zéro.
    Noop,
}
