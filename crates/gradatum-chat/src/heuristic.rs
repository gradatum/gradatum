//! Offline heuristic classifier — regex/keyword.
//!
//! Operates without any network or LLM dependency.
//!
//! ## Confidence levels
//!
//! | Confidence | Interpretation                                                      |
//! |------------|---------------------------------------------------------------------|
//! | 0.95       | Clear admission — strong keywords (decision/architecture)           |
//! | 0.80       | Likely admission — engagement signals (wikilink)                    |
//! | 0.65       | Ambiguous → caller may escalate to LLM (recommended threshold: 0.7)|
//! | 0.50       | Likely rejection (text too short or non-informative)                |
//!

use async_trait::async_trait;
use regex::Regex;
use std::sync::OnceLock;

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

// --- Patterns compilés une seule fois ---

/// Obsidian wikilink pattern `[[anything]]`.
fn re_wikilink() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[\[[^\]]+\]\]").expect("re_wikilink est un pattern littéral valide")
    })
}

/// Keywords signalling a decision or architecture note.
fn re_decisions() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(decision|architecture|lesson|lessons.learned)\b")
            .expect("re_decisions est un pattern littéral valide")
    })
}

/// Keywords signalling a bug or issue.
fn re_bugs() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(bug|issue|error|erreur|crash|traceback|panic)\b")
            .expect("re_bugs est un pattern littéral valide")
    })
}

/// Heuristic classifier — works fully offline.
///
/// Constructed with `Heuristic::new()`. All fields are configurable
/// for tests.
pub struct Heuristic {
    /// Minimum body length to consider it "substantial" (confidence boost +0.10).
    pub min_body_len_for_boost: usize,
    /// Maximum body length for the "body too short" flag (confidence 0.50).
    pub short_body_threshold: usize,
}

impl Default for Heuristic {
    fn default() -> Self {
        Self::new()
    }
}

impl Heuristic {
    /// Creates a classifier with default thresholds.
    pub fn new() -> Self {
        Self {
            min_body_len_for_boost: 200,
            short_body_threshold: 50,
        }
    }

    /// Synchronous classification logic — exposed for unit tests.
    ///
    /// Returns `(confidence, proposed_status, reason)`.
    pub fn classify_sync(&self, note: &Note) -> (f32, NoteStatus, String) {
        let body = &note.body.markdown;
        let has_tags = !note.frontmatter.tags.is_empty();

        // --- Score de base ---
        let mut confidence: f32 = 0.50;
        let mut reason_parts: Vec<&str> = Vec::new();

        // Corpus trop court → rejet probable, on sort tôt
        if body.chars().count() < self.short_body_threshold {
            return (
                0.50,
                NoteStatus::PendingReview,
                "corps trop court — revue humaine conseillée".into(),
            );
        }

        // --- Signaux positifs ---

        // Wikilinks → signal d'engagement
        if re_wikilink().is_match(body) {
            confidence += 0.15;
            reason_parts.push("wikilink");
        }

        // Corps substantiel
        if body.chars().count() > self.min_body_len_for_boost {
            confidence += 0.10;
            reason_parts.push("corps long");
        }

        // Frontmatter avec tags
        if has_tags {
            confidence += 0.10;
            reason_parts.push("tags présents");
        }

        // --- Patterns sémantiques ---

        if re_decisions().is_match(body) {
            // Décision / architecture / lesson → admission claire
            confidence = confidence.max(0.80);
            let reason = if reason_parts.is_empty() {
                "keyword decision/architecture/lesson — admission recommandée".into()
            } else {
                format!(
                    "keyword decision/architecture/lesson + {} — admission recommandée",
                    reason_parts.join(", ")
                )
            };
            // Seuil 0.95 si multiples signaux cumulés
            if confidence >= 0.90 {
                confidence = 0.95;
            }
            return (confidence, NoteStatus::Live, reason);
        }

        if re_bugs().is_match(body) {
            // Bug / issue → debug section, revue recommandée
            confidence = confidence.max(0.65);
            return (
                confidence,
                NoteStatus::PendingReview,
                format!(
                    "keyword bug/issue/error{}",
                    if reason_parts.is_empty() {
                        String::new()
                    } else {
                        format!(" + {}", reason_parts.join(", "))
                    }
                ),
            );
        }

        // Pas de pattern sémantique fort — statut ambigu
        let status = if confidence >= 0.70 {
            NoteStatus::Live
        } else {
            NoteStatus::PendingReview
        };

        let reason = if reason_parts.is_empty() {
            "aucun signal sémantique fort".into()
        } else {
            format!("signaux: {}", reason_parts.join(", "))
        };

        (confidence.min(1.0), status, reason)
    }
}

#[async_trait]
impl Chat for Heuristic {
    async fn classify_curator(
        &self,
        note: &Note,
        _context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        let (confidence, proposed_status, reason) = self.classify_sync(note);
        Ok(CuratorVerdict {
            proposed_status,
            confidence,
            reason,
            backend: ChatBackend::Heuristic,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Heuristic
    }
}
