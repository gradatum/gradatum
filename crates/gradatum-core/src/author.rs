//! Référence auteur typée multi-agent.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.5.
//!
//! Gradatum distingue 4 kinds d'auteurs pour permettre un audit trail précis
//! des modifications (B12 : multi-agent author pattern).

use serde::{Deserialize, Serialize};

/// Catégorie d'auteur.
///
/// Décision Q4 brainstorming the maintainer 2026-05-03 — 4 variantes couvrent tous les
/// producteurs de notes dans un environnement multi-agent :
/// humain, orchestrateur principal, sous-agent délégué, processus système.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthorKind {
    /// Humain. `id` = email | username | bearer subject.
    Human,
    /// Agent orchestrateur principal.
    ///
    /// `id` = `"claude-code-server"` | `"claude-code-local"` | etc.
    MainAgent,
    /// Sous-agent délégué par le main agent.
    ///
    /// `id` = `"backend"` | `"tester"` | `"reviewer"` | `"editor"` | `"planner"` | ...
    SubAgent,
    /// Processus système automatisé.
    ///
    /// `id` = `"legacy-vault"` | `"cron-decay"` | `"post-push-hook"` | `"migrate-tool"` | ...
    System,
}

/// Référence à l'auteur d'une note.
///
/// Structuré pour permettre un audit trail précis et cross-agent.
/// Le champ `display_name` est optionnel — omis en sérialisation si absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuthorRef {
    /// Catégorie de l'auteur.
    pub kind: AuthorKind,

    /// Identifiant opaque, interprété selon `kind`.
    pub id: String,

    /// Nom lisible pour affichage. Omis si `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl AuthorRef {
    /// Construit un auteur humain sans display name.
    pub fn human(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::Human,
            id: id.into(),
            display_name: None,
        }
    }

    /// Construit un agent orchestrateur principal.
    pub fn main_agent(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::MainAgent,
            id: id.into(),
            display_name: None,
        }
    }

    /// Construit un sous-agent.
    pub fn sub_agent(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::SubAgent,
            id: id.into(),
            display_name: None,
        }
    }

    /// Construit un auteur système (processus automatisé).
    pub fn system(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::System,
            id: id.into(),
            display_name: None,
        }
    }
}
