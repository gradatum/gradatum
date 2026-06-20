//! Typed multi-agent author reference.
//!
//! Gradatum distinguishes four author kinds to enable a precise audit trail
//! of note modifications via the multi-agent author pattern.

use serde::{Deserialize, Serialize};

/// Author category.
///
/// Four variants cover all note producers in a multi-agent environment:
/// human, main orchestrating agent, delegated sub-agent, automated system process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthorKind {
    /// Human operator. `id` = email | username | bearer subject.
    Human,
    /// Main orchestrating agent.
    ///
    /// `id` = `"mcp-client"` | `"local-agent"` | etc.
    MainAgent,
    /// Sub-agent delegated by the main agent.
    ///
    /// `id` = `"backend"` | `"tester"` | `"reviewer"` | `"editor"` | `"planner"` | …
    SubAgent,
    /// Automated system process.
    ///
    /// `id` = `"legacy-vault"` | `"cron-decay"` | `"post-push-hook"` | `"migrate-tool"` | …
    System,
}

/// Reference to a note's author.
///
/// Structured to enable a precise, cross-agent audit trail.
/// `display_name` is optional — omitted in serialisation when absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuthorRef {
    /// Author category.
    pub kind: AuthorKind,

    /// Opaque identifier, interpreted according to `kind`.
    pub id: String,

    /// Human-readable display name. Omitted when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl AuthorRef {
    /// Constructs a human author with no display name.
    pub fn human(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::Human,
            id: id.into(),
            display_name: None,
        }
    }

    /// Constructs a main orchestrating agent author.
    pub fn main_agent(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::MainAgent,
            id: id.into(),
            display_name: None,
        }
    }

    /// Constructs a sub-agent author.
    pub fn sub_agent(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::SubAgent,
            id: id.into(),
            display_name: None,
        }
    }

    /// Constructs a system (automated process) author.
    pub fn system(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::System,
            id: id.into(),
            display_name: None,
        }
    }
}
