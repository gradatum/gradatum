//! Cluster synthesis producer.
//!
//! [`DistillSynthesizer`] is the seam that lets the deterministic template MVP
//! ([`TemplateSynthesizer`]) be swapped for a dedicated LLM gateway backend
//! without touching the distill handler that consumes it.

/// Synthesis output produced for a note cluster.
///
/// Produced by a [`DistillSynthesizer`] and written as a `PendingReview` note.
pub struct ClusterSynthesis {
    /// Title of the synthesis note.
    pub title: String,
    /// Markdown body of the synthesis note.
    pub body: String,
}

/// Synthesis error — propagated to mark the job `Failed` cleanly.
#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    /// The synthesis service (LLM gateway) is unavailable or failed.
    #[error("synthesis unavailable: {0}")]
    Unavailable(String),
}

/// Cluster synthesis producer.
///
/// Abstraction that allows substituting the deterministic implementation (MVP)
/// with a dedicated LLM gateway backend without touching the handler.
///
/// # Contract
///
/// - `synthesize` receives the notes of a cluster as `[(title, body)]` (≥ 1 note).
/// - Returns `Ok(ClusterSynthesis)`: title + body of the `PendingReview` note.
/// - Returns `Err(SynthesisError::Unavailable)`: the job MUST fail cleanly
///   (no partial note written — mitigation for gateway-down scenarios).
#[async_trait::async_trait]
pub trait DistillSynthesizer: Send + Sync {
    /// Synthesizes a note cluster into a synthesis note.
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError>;
}

/// Deterministic synthesizer — MVP (no LLM call).
///
/// Produces a structured synthesis note by concatenation: title derived from the
/// first cluster element, body listing source notes with an excerpt.
/// The note is written as `PendingReview` (requires human review) — editorial quality
/// is the reviewer's responsibility, not the automated step's.
///
/// ## Why deterministic at MVP
///
/// The worker injects no free-text generation client (the only wired LLM backend is
/// `gradatum_chat::LlmBackend`, specialised for curator classification — not free
/// completion). A dedicated `distill-semantic` gateway client is deferred:
/// the `PendingReview` output combined with the cron disabled by default keeps the step
/// safe, and the [`DistillSynthesizer`] abstraction allows plugging in an LLM without
/// refactoring the handler.
#[derive(Default)]
pub struct TemplateSynthesizer;

#[async_trait::async_trait]
impl DistillSynthesizer for TemplateSynthesizer {
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError> {
        if cluster.is_empty() {
            return Err(SynthesisError::Unavailable(
                "empty cluster — nothing to synthesize".to_string(),
            ));
        }
        // Title: derived from the first non-empty title in the cluster.
        let lead_title = cluster
            .iter()
            .map(|(t, _)| t.trim())
            .find(|t| !t.is_empty())
            .unwrap_or("related notes");
        let title = format!("Distilled synthesis — {lead_title}");

        // Body: header + list of source notes with bounded excerpt.
        let mut body = format!(
            "# {title}\n\n\
             > Distilled synthesis note (F-22) — **pending review**.\n\
             > Groups {} semantically close note(s).\n\n\
             ## Distilled sources\n\n",
            cluster.len()
        );
        for (i, (src_title, src_body)) in cluster.iter().enumerate() {
            let excerpt: String = src_body.trim().chars().take(280).collect();
            let display_title = if src_title.trim().is_empty() {
                "(untitled)"
            } else {
                src_title.trim()
            };
            body.push_str(&format!("### {}. {display_title}\n\n{excerpt}\n\n", i + 1));
        }
        Ok(ClusterSynthesis { title, body })
    }
}
