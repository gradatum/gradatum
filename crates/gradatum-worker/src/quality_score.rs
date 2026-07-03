//! Deterministic quality scorer for distilled notes (pure module, zero I/O).
//! grounding (embedding-cosine) x f17 x f47 x num/entity penalties -> quality_score.

use crate::distill_cluster::cosine_similarity;

/// Component-wise centroid of a set of same-dimension embeddings.
/// Returns an empty vector if `embeddings` is empty (or dimensions are inconsistent).
#[must_use]
pub fn centroid(embeddings: &[Vec<f32>]) -> Vec<f32> {
    let Some(first) = embeddings.first() else {
        return Vec::new();
    };
    let dim = first.len();
    if dim == 0 || embeddings.iter().any(|e| e.len() != dim) {
        return Vec::new();
    }
    let mut acc = vec![0.0f32; dim];
    for e in embeddings {
        for (a, v) in acc.iter_mut().zip(e.iter()) {
            *a += *v;
        }
    }
    let n = embeddings.len() as f32;
    for a in &mut acc {
        *a /= n;
    }
    acc
}

/// Extract numeric tokens (`\d+([.,]\d+)?`) from a text, normalized (separator `.`).
fn numbers_of(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ((ch == '.' || ch == ',') && !cur.is_empty()) {
            cur.push(if ch == ',' { '.' } else { ch });
        } else if !cur.is_empty() {
            out.push(cur.trim_end_matches('.').to_string());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.trim_end_matches('.').to_string());
    }
    out
}

/// Numeric-coherence penalty in [0.5, 1.0]. 1.0 = every number in the synthesis appears in
/// at least one source. Each orphan number subtracts 0.15 (floor 0.5).
///
/// # Errors
///
/// This function is infallible — always returns a value in `[0.5, 1.0]`.
#[must_use]
pub fn num_coherence_penalty(synth: &str, sources: &[String]) -> f32 {
    let synth_nums = numbers_of(synth);
    if synth_nums.is_empty() {
        return 1.0;
    }
    let src_nums: std::collections::HashSet<String> =
        sources.iter().flat_map(|s| numbers_of(s)).collect();
    let orphans = synth_nums.iter().filter(|n| !src_nums.contains(*n)).count();
    (1.0 - 0.15 * orphans as f32).max(0.5)
}

/// Naive "entity" tokens: words starting with an uppercase letter, length >= 3.
fn entities_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| {
            let mut cs = w.chars();
            matches!(cs.next(), Some(f) if f.is_uppercase()) && w.chars().count() >= 3
        })
        .map(|w| w.to_lowercase())
        .collect()
}

/// Orphan-entity penalty in [0.5, 1.0]. An entity in the synthesis absent (case-insensitive)
/// from every source → -0.10 per orphan (floor 0.5).
///
/// # Errors
///
/// This function is infallible — always returns a value in `[0.5, 1.0]`.
#[must_use]
pub fn entity_orphan_penalty(synth: &str, sources: &[String]) -> f32 {
    let synth_ents = entities_of(synth);
    if synth_ents.is_empty() {
        return 1.0;
    }
    let src_text = sources.join(" ").to_lowercase();
    let orphans = synth_ents.iter().filter(|e| !src_text.contains(*e)).count();
    (1.0 - 0.10 * orphans as f32).max(0.5)
}

/// Inputs to the quality score (all pre-computed — pure module).
pub struct QualityInputs<'a> {
    pub synth_embedding: &'a [f32],
    pub source_centroid: &'a [f32],
    pub synth_body: &'a str,
    pub source_texts: &'a [String],
    pub f17_sources: f32,
    pub f47_sources: f32,
}

/// Decomposed quality-score result (for logging + disposition decision).
#[derive(Debug, Clone, Copy)]
pub struct QualityScore {
    pub score: f32,
    pub grounding: f32,
    pub num_penalty: f32,
    pub entity_penalty: f32,
}

/// Compose the score: grounding x f17 x f47 x num_penalty x entity_penalty, clamped to \[0,1\].
#[must_use]
pub fn score_quality(inp: &QualityInputs<'_>) -> QualityScore {
    let grounding = cosine_similarity(inp.synth_embedding, inp.source_centroid).clamp(0.0, 1.0);
    let num_penalty = num_coherence_penalty(inp.synth_body, inp.source_texts);
    let entity_penalty = entity_orphan_penalty(inp.synth_body, inp.source_texts);
    let score = (grounding
        * inp.f17_sources.clamp(0.0, 1.0)
        * inp.f47_sources.clamp(0.0, 1.0)
        * num_penalty
        * entity_penalty)
        .clamp(0.0, 1.0);
    QualityScore {
        score,
        grounding,
        num_penalty,
        entity_penalty,
    }
}
