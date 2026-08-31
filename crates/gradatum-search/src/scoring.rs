//! Auxiliary scoring functions for multi-factor ranking.
//!
//! ## Formulas
//!
//! ### Recency decay
//! `recency_factor(t_note, t_now) = exp(-λ × days_old)`
//! with λ = 0.01 (half-life ≈ 69 days).
//!
//! | Note age  | Factor |
//! |---|---|
//! | 0 days    | 1.000 |
//! | 7 days    | 0.932 |
//! | 30 days   | 0.741 |
//! | 90 days   | 0.407 |
//! | 1 year    | 0.026 |
//!
//! ### Normalised PageRank in-degree (clamped)
//! `pagerank_factor(in_degree) = (in_degree / (in_degree + NORM_CONST)).clamp(0.0, 1.0)`
//! NORM_CONST = 5: a note with 5 backlinks → factor = 0.5.
//! The clamp guarantees `factor ∈ [0.0, 1.0]` even for extreme values.
//!
//! ### Composite score
//! `score = rrf_score × (1 + α × recency) × (1 + β × pagerank) × (1 + γ × trust_decayed)`
//! α = 0.2 (recency), β = 0.1 (in-degree), γ = 0.15 (trust) — conservative coefficients.
//!
//! ### Trust decay
//! `trust_decayed = trust × 0.5^(age_days / half_life_days)` (per-`doc_kind` half-life).
//! `half_life_days = None` (e.g. `Static`, or the section-exempted palier A) → **no decay**
//! (`trust_decayed = trust`). The trust factor is applied **only** in the RRF layer
//! (never BM25), **after** the `forgotten`/`downgraded` SQL short-circuits. Modifier order:
//! `forgotten (short-circuit) > downgraded > [RRF × recency × pagerank × trust_decay]`.
//!
//! ### Controlled activation
//! The trust multiplier is **disableable** via a global flag (`trust_decay_enabled`).
//! Flag OFF ⇒ `composite_score_with_trust` returns **exactly** `composite_score`
//! (bit-identical to the pre-trust-factor scores) — non-regression guarantee.

/// Temporal recency decay factor.
///
/// Returns 1.0 for a note anchored right now; decays exponentially with age.
/// Future timestamps (clock skew) are clamped to 0 days → 1.0.
///
/// # Parameters
/// - `note_created_ms`: canonical anchor timestamp of the note in UTC epoch milliseconds.
///   Callers pass `hit.anchor_ms.unwrap_or(created_ms)` — the anchor is the
///   canonical event date (`occurred_at` / `event-date` / `valid_from` via
///   `temporal_index`), falling back to the ingestion timestamp (`notes.created`)
///   when absent. For static notes (`anchor_src = Created`), `anchor_ms == created_ms`
///   and the result is **bit-identical** to the legacy behaviour.
/// - `now_ms`: current timestamp in UTC epoch milliseconds.
///
/// # Returns
/// `f64` in `(0.0, 1.0]` — never 0.0 (`exp(x) > 0` for all real x).
#[must_use]
pub fn recency_factor(note_created_ms: i64, now_ms: i64) -> f64 {
    const LAMBDA: f64 = 0.01; // demi-vie ≈ 69 jours
    const MS_PER_DAY: f64 = 86_400_000.0;
    let delta_ms = (now_ms - note_created_ms).max(0);
    let days_old = (delta_ms as f64) / MS_PER_DAY;
    (-LAMBDA * days_old).exp()
}

/// Normalised PageRank in-degree factor, strictly clamped to `[0.0, 1.0]`.
///
/// `(in_degree / (in_degree + NORM_CONST)).clamp(0.0, 1.0)` → `[0.0, 1.0]`.
/// The clamp is defensive: the base formula is mathematically bounded by 1.0,
/// but the clamp documents the API invariant and guards against future formula changes.
///
/// Notes with no backlinks → `0.0`. Highly-linked notes (>>100) → close to `1.0`.
///
/// # Returns
/// `f64` in `[0.0, 1.0]`.
///
/// # Note
///
/// The `u64` signature makes a negative parameter a compile-time error —
/// the lower bound 0.0 is guaranteed by construction.
#[must_use]
pub fn pagerank_factor(in_degree: u64) -> f64 {
    const NORM_CONST: f64 = 5.0;
    let deg = in_degree as f64;
    let raw = deg / (deg + NORM_CONST);
    raw.clamp(0.0, 1.0)
}

/// Multi-factor composite score.
///
/// Formula: `rrf_score × (1 + α × recency) × (1 + β × pagerank)`
/// with α = 0.2 (recency boost max 20%) and β = 0.1 (pagerank boost max 10%).
///
/// When `recency = 0.0` and `pagerank = 0.0`, returns `rrf_score` unchanged.
/// Maximum boost: `rrf_score × 1.2 × 1.1 = rrf_score × 1.32`.
#[must_use]
pub fn composite_score(rrf_score: f64, recency: f64, pagerank: f64) -> f64 {
    const ALPHA: f64 = 0.2;
    const BETA: f64 = 0.1;
    rrf_score * (1.0 + ALPHA * recency) * (1.0 + BETA * pagerank)
}

/// Normalised BM25 score for the single-arm short-circuit (criterion 6).
///
/// `1.0 / (1.0 + |bm25|)` maps the negative BM25 score (SQLite FTS5: better
/// matches are closer to 0) to `(0.0, 1.0]` — a strong match (bm25 near 0)
/// approaches 1.0, a weak match (bm25 very negative) approaches 0.0. The
/// mapping is strictly monotone in `bm25`, so the single-arm ordering is
/// preserved.
///
/// This is the canonical BM25 normalisation for the repo (already documented
/// and tested in `handlers.rs::bm25_score_mapping_is_monotone_decreasing_in_zero_one_range`
/// — historically described but not applied to the exposed score).
#[must_use]
pub fn normalize_bm25(bm25: f64) -> f64 {
    1.0 / (1.0 + bm25.abs())
}

/// Normalised semantic (cosine) score for the single-arm short-circuit.
///
/// The cosine is already normalised to `[-1, 1]`; clamping to `[0, 1]` removes
/// the anti-correlation tail — a note anti-correlated with the query cannot be
/// "more relevant than a non-match". Monotone in `cosine`, so the single-arm
/// ordering is preserved.
#[must_use]
pub fn normalize_semantic(cosine: f32) -> f64 {
    (cosine as f64).clamp(0.0, 1.0)
}

/// Weighted fusion score on normalised magnitudes (criterion 10).
///
/// Replaces the rank-only fusion (RRF) in the nominal two-arm case: instead of
/// summing `1/(k + rank)`, the magnitude of each arm is kept and blended as
/// `w_bm25 × normalize_bm25(bm25) + w_sem × normalize_semantic(cosine)`, with
/// `w_bm25 = w_sem = 0.5` (both arms equal when both respond).
///
/// # Absence semantics
///
/// `None` for an arm means the note is **absent** from that arm → contribution
/// `0.0`. This is distinct from `Some(0.0)` (a present-but-neutral score): for
/// BM25, `normalize_bm25(0.0) = 1.0` (perfect match), so using `0.0` as a
/// sentinel for "absent" would silently turn a missing note into the best
/// lexical match. A note present in only one arm therefore lands at
/// `0.5 × its-arm-normalised-score` — penalised relative to notes matching both
/// arms, which is exactly the discriminator the rank fusion was throwing away.
///
/// # Guarantees
///
/// - `weighted_fusion_score(Some(0.0), Some(1.0)) = 1.0` (both arms maximal).
/// - Result ∈ `[0.0, 1.0]` (both normalisers are bounded).
/// - Strictly monotone in each present arm's score.
#[must_use]
pub fn weighted_fusion_score(bm25: Option<f64>, semantic: Option<f32>) -> f64 {
    const W_BM25: f64 = 0.5;
    const W_SEM: f64 = 0.5;
    let n_bm25 = bm25.map_or(0.0, normalize_bm25);
    let n_sem = semantic.map_or(0.0, normalize_semantic);
    W_BM25 * n_bm25 + W_SEM * n_sem
}

/// Trust boost coefficient in the composite score (conservative).
///
/// Maximum trust boost (`trust_decayed = 1.0`): `× 1.15` (15 %).
pub const GAMMA_TRUST: f64 = 0.15;

/// Runtime trust-decay configuration resolved for scoring.
///
/// Built from server config; consumed by search handlers.
/// `enabled = false` ⇒ scoring applies no trust factor (pre-trust-factor scores).
///
/// Half-lives are indexed by **`doc_kind`** (`Event` decays, `Static` does not), not by
/// provenance. The base trust value is derived from the section via
/// `gradatum_core::section::trust_for_section_str` (table `SECTION_TRUST_SCORES`).
#[derive(Debug, Clone)]
pub struct TrustDecayConfig {
    /// Enables the trust multiplier. `false` = pre-trust-factor non-regression mode.
    pub enabled: bool,
    /// Half-lives (days) per `doc_kind`. Absent `doc_kind` = no decay (`None`).
    pub half_life_days: std::collections::HashMap<String, f64>,
}

/// Default trust-decay half-lives, in days.
///
/// **Single source of truth**: this table is the sole definition of the decay
/// half-lives. `gradatum-server::config::ScoringConfig` consumes it via
/// [`default_half_lives`] rather than redefining the literals — values live in
/// exactly one place.
///
/// The key is the **`doc_kind`** (CoALA temporal axis), not the provenance:
/// `Event` (what happened — `debug`, `agent-issues`) decays; `Static` does not.
/// Exception: **tier A** (`council`, `decisions`, `project-map`, `identity`) is exempt
/// from decay at the **section** level (`TrustDecayConfig::resolve_for_section`), even
/// when its `doc_kind` is `Event` — the `council` case: an act of governance does not
/// lose its authority as it ages. `doc_kind` is a deterministic function of `section` on
/// the measured corpus: it is not a second independent lever.
///
/// The `Event = 90.0` days value reuses the calibration already documented in the repo
/// for decay-trust: the former `distilled = 90d` default (the table's only entry), and
/// `session_trace.retention_days` is also 90 days (CHANGELOG v0.4.2). 90 days ≥ the
/// documented recency half-life (`λ = 0.01` → ≈ 69 days, [`recency_factor`]): decay-trust
/// stays a gentler side effect than recency, consistent with the composite formula.
///
/// A `doc_kind` **absent** from this table → **no decay** (`half_life = None`,
/// non-perishable trust) — this is the `Static` case.
pub const DEFAULT_TRUST_HALF_LIVES: &[(&str, f64)] = &[("Event", 90.0)];

/// Builds the default half-life map from [`DEFAULT_TRUST_HALF_LIVES`].
///
/// Shared single source between `TrustDecayConfig::default` (scoring) and
/// `ScoringConfig::default_half_lives` (server config).
#[must_use]
pub fn default_half_lives() -> std::collections::HashMap<String, f64> {
    DEFAULT_TRUST_HALF_LIVES
        .iter()
        .map(|(k, v)| ((*k).to_string(), *v))
        .collect()
}

impl Default for TrustDecayConfig {
    /// Default: decay enabled, half-lives from [`DEFAULT_TRUST_HALF_LIVES`]
    /// (`Event = 90 days`), `Static` without decay.
    fn default() -> Self {
        Self {
            enabled: true,
            half_life_days: default_half_lives(),
        }
    }
}

impl TrustDecayConfig {
    /// Resolves trust parameters for a given hit.
    ///
    /// Returns `Some((trust, age_days, half_life))` when decay is enabled and
    /// trust is present; otherwise `None` (causing `composite_score_with_trust`
    /// to neutralise the trust factor → pre-trust-factor score).
    ///
    /// - `decay_key` is the `doc_kind` (`"Event"` / `"Static"`). `half_life` =
    ///   `Some(h)` if `decay_key` is in the map; `None` otherwise (no decay —
    ///   non-perishable, e.g. `Static`).
    #[must_use]
    pub fn resolve(
        &self,
        trust: Option<f32>,
        decay_key: Option<&str>,
        age_days: f64,
    ) -> Option<(f64, f64, Option<f64>)> {
        if !self.enabled {
            return None;
        }
        let trust = trust? as f64;
        let half_life = decay_key.and_then(|p| self.half_life_days.get(p).copied());
        Some((trust, age_days, half_life))
    }

    /// Resolves the trust parameters of a hit **from the section**.
    ///
    /// The base trust is derived from the section (`gradatum_core::section::trust_for_section_str`,
    /// table `SECTION_TRUST_SCORES`, justified in `section.rs`); the decay axis is the
    /// `doc_kind` derived from the section (`section_str_to_doc_kind`). An unknown section falls
    /// back to the neutral `0.5` (the corpus's current behavior) and `Static` (non-perishable).
    ///
    /// **Tier A exempt**: `council`, `decisions`, `project-map`, `identity` **never** decay
    /// (the "an act is not re-judged" doctrine, `section::is_trust_non_decaying`) — even when
    /// the `doc_kind` is `Event` (the `council` case). The exemption applies to the SECTION,
    /// not to `doc_kind`: on the measured corpus, `doc_kind` is redundant with `section`
    /// (`section::TRUST_NON_DECAYING_SECTIONS`). `debug` / `agent-issues` always decay with the
    /// `Event` half-life (90 d).
    ///
    /// Returns `None` when `enabled = false` (neutralises the factor → pre-trust score,
    /// bit-for-bit identical to `composite_score`).
    #[must_use]
    pub fn resolve_for_section(
        &self,
        section: &str,
        age_days: f64,
    ) -> Option<(f64, f64, Option<f64>)> {
        if !self.enabled {
            return None;
        }
        let trust = gradatum_core::section::trust_for_section_str(section);
        // Palier A (council/decisions/project-map/identity) : exemption de décroissance —
        // un acte ne perd pas son autorité en vieillissant. Portée par la SECTION
        // (`doc_kind` est redondant avec `section` sur le corpus mesuré, cf. section.rs).
        let half_life = if gradatum_core::section::is_trust_non_decaying(section) {
            None
        } else {
            let doc_kind = gradatum_core::section::section_str_to_doc_kind(section);
            self.half_life_days.get(doc_kind).copied()
        };
        Some((trust, age_days, half_life))
    }
}

/// Trust temporal decay factor.
///
/// `trust_decayed = trust × 0.5^(age_days / half_life_days)`.
///
/// # Parameters
/// - `trust`: trust score of the note (`[0.0, 1.0]`, column `notes.trust`).
/// - `age_days`: age of the note in days (≥ 0; negative values clamped to 0).
/// - `half_life_days`: half-life of the provenance. `None` ⇒ **no decay**
///   (non-perishable provenance, e.g. `human-decision`) → returns `trust` unchanged.
///
/// # Returns
/// `f64` in `[0.0, 1.0]` (clamped). `0.5^x` is bounded by 1.0 for `x ≥ 0`.
///
/// # Side effects
/// None. Pure function.
#[must_use]
pub fn trust_decay_factor(trust: f64, age_days: f64, half_life_days: Option<f64>) -> f64 {
    let trust_clamped = trust.clamp(0.0, 1.0);
    match half_life_days {
        // Pas de demi-vie (provenance non périssable) ou demi-vie non-positive (garde) :
        // le trust ne décroît pas.
        None => trust_clamped,
        Some(hl) if hl <= 0.0 => trust_clamped,
        Some(hl) => {
            let age = age_days.max(0.0);
            let decay = 0.5_f64.powf(age / hl);
            (trust_clamped * decay).clamp(0.0, 1.0)
        }
    }
}

/// Wire scoring weights (local mirror of `gradatum-dto::ScoringWeights`).
///
/// Wire/lib decoupling: `gradatum-search` does not depend on `gradatum-dto`.
/// The server converts `ScoringWeights` → `ScoringWeightsWire` before calling
/// [`resolve_weights`].
///
/// All fields are optional — `None` lets the pipeline use the default value.
#[derive(Debug, Clone, Default)]
pub struct ScoringWeightsWire {
    /// Recency weight (default: 0.2).
    pub recency: Option<f64>,
    /// PageRank weight (default: 0.1).
    pub pagerank: Option<f64>,
    /// Trust weight (default: [`GAMMA_TRUST`]).
    pub trust: Option<f64>,
}

/// Resolved scoring weights (defaults applied).
///
/// Produced by [`resolve_weights`] from an optional [`ScoringWeightsWire`].
/// Consumed by [`composite_score_weighted`].
///
/// # Defaults
///
/// `{ recency: 0.2, pagerank: 0.1, trust: GAMMA_TRUST }` — identical to the α/β/γ
/// coefficients of [`composite_score`] and [`composite_score_with_trust`].
#[derive(Debug, Clone, Copy)]
pub struct ResolvedWeights {
    /// Recency weight (α, default 0.2).
    pub recency: f64,
    /// PageRank weight (β, default 0.1).
    pub pagerank: f64,
    /// Trust weight (γ, default [`GAMMA_TRUST`]).
    pub trust: f64,
}

impl Default for ResolvedWeights {
    /// Defaults: α=0.2, β=0.1, γ=[`GAMMA_TRUST`] (0.15).
    ///
    /// With these defaults, [`composite_score_weighted`] reproduces [`composite_score_with_trust`]
    /// **bit-for-bit**.
    fn default() -> Self {
        Self {
            recency: 0.2,
            pagerank: 0.1,
            trust: GAMMA_TRUST,
        }
    }
}

/// Resolves scoring weights by substituting defaults for any `None` fields.
///
/// # Parameters
///
/// - `w`: optional wire weights. `None` → all defaults.
///
/// # Returns
///
/// [`ResolvedWeights`] with each field set to either the wire value or the default.
///
/// # Examples
///
/// ```
/// # use gradatum_search::scoring::{ScoringWeightsWire, resolve_weights};
/// let w = resolve_weights(None); // all defaults
/// assert!((w.recency - 0.2).abs() < 1e-9);
///
/// let wire = ScoringWeightsWire { recency: Some(0.5), ..Default::default() };
/// let w2 = resolve_weights(Some(&wire));
/// assert!((w2.recency - 0.5).abs() < 1e-9);
/// assert!((w2.pagerank - 0.1).abs() < 1e-9); // default preserved
/// ```
#[must_use]
pub fn resolve_weights(w: Option<&ScoringWeightsWire>) -> ResolvedWeights {
    let d = ResolvedWeights::default();
    match w {
        None => d,
        Some(x) => ResolvedWeights {
            recency: x.recency.unwrap_or(d.recency),
            pagerank: x.pagerank.unwrap_or(d.pagerank),
            trust: x.trust.unwrap_or(d.trust),
        },
    }
}

/// Generalizes the existing multiplicative form with configurable weights.
///
/// Formula: `rrf × (1 + w.recency × recency) × (1 + w.pagerank × pagerank)
///           [× (1 + w.trust × trust_decayed)]` — the trust term is applied **only**
/// when `trust_params.is_some()` (same condition as [`composite_score_with_trust`]).
///
/// # Parity
///
/// With [`ResolvedWeights::default()`] (w.recency=0.2, w.pagerank=0.1, w.trust=GAMMA_TRUST),
/// reproduces [`composite_score_with_trust`] **bit-for-bit** in both branches
/// (`None` and `Some`).
///
/// # Parameters
///
/// - `rrf`: raw RRF score (`Candidate.rrf_score`).
/// - `recency`: recency factor (`recency_factor(created_ms, now_ms)`).
/// - `pagerank`: PageRank factor (`pagerank_factor(in_degree)`).
/// - `trust_params`: `Some((trust, age_days, half_life))` or `None` (trust disabled).
/// - `w`: resolved weights (produced by [`resolve_weights`]).
///
/// # Errors
///
/// Pure function — infaillible.
#[must_use]
pub fn composite_score_weighted(
    rrf: f64,
    recency: f64,
    pagerank: f64,
    trust_params: Option<(f64, f64, Option<f64>)>,
    w: &ResolvedWeights,
) -> f64 {
    let mut s = rrf * (1.0 + w.recency * recency) * (1.0 + w.pagerank * pagerank);
    if let Some((trust, age_days, hl)) = trust_params {
        s *= 1.0 + w.trust * trust_decay_factor(trust, age_days, hl);
    }
    s
}

/// Multi-factor composite score **with** trust decay.
///
/// Extends [`composite_score`] by adding the factor `(1 + γ × trust_decayed)`.
///
/// Formula: `rrf × (1 + α·recency) × (1 + β·pagerank) × (1 + γ·trust_decayed)`.
///
/// # Non-regression guarantee (flag OFF)
///
/// When `trust_params = None`, returns **exactly** the same value as [`composite_score`]
/// (trust factor = `1.0`, no additional floating-point operation) — bit-identical
/// to the pre-trust-factor scores. This is the disable lever (`trust_decay_enabled = false`).
///
/// # Parameters
/// - `rrf_score`, `recency`, `pagerank`: identical to [`composite_score`].
/// - `trust_params`: `Some((trust, age_days, half_life_days))` to apply decay,
///   `None` to neutralise (pre-trust-factor behaviour).
#[must_use]
pub fn composite_score_with_trust(
    rrf_score: f64,
    recency: f64,
    pagerank: f64,
    trust_params: Option<(f64, f64, Option<f64>)>,
) -> f64 {
    let base = composite_score(rrf_score, recency, pagerank);
    match trust_params {
        // Flag OFF : aucun facteur trust → identique bit-à-bit à composite_score.
        None => base,
        Some((trust, age_days, half_life_days)) => {
            let trust_decayed = trust_decay_factor(trust, age_days, half_life_days);
            base * (1.0 + GAMMA_TRUST * trust_decayed)
        }
    }
}

/// Usage-salience scoring parameters, resolved from server config.
///
/// Same role as [`TrustDecayConfig`] for the trust factor: the *absence* of these
/// params at the call site (config `enabled = false`) is the disable lever —
/// scores stay bit-identical to the salience-free baseline.
#[derive(Debug, Clone)]
pub struct SalienceParams {
    /// Boost coefficient — max boost `× (1 + gamma)`. Spec default: `0.10`.
    pub gamma: f64,
    /// Soft-saturation constant (> 0, validated at config load). Spec default: `10.0`.
    pub k_norm: f64,
    /// Per-kind weights; a kind absent from the map weighs `0.0` (ignored).
    pub kind_weights: std::collections::HashMap<String, f64>,
}

/// Normalised salience `s / (s + k_norm)` ∈ `[0, 1)`.
///
/// `weighted_sum = 0` ⇒ exactly `0.0` (neutral downstream factor).
///
/// # Errors
///
/// Pure function — infaillible.
#[must_use]
pub fn salience_factor(weighted_sum: f64, k_norm: f64) -> f64 {
    if weighted_sum <= 0.0 {
        return 0.0;
    }
    weighted_sum / (weighted_sum + k_norm)
}

/// Weighted sum `Σ (w_kind × count)` over the per-kind usage counts of one note.
///
/// Unknown kinds (absent from `kind_weights`) contribute `0.0`.
///
/// # Errors
///
/// Pure function — infaillible.
#[must_use]
pub fn salience_weighted_sum(
    counts: &[(String, u64)],
    kind_weights: &std::collections::HashMap<String, f64>,
) -> f64 {
    counts
        .iter()
        .map(|(kind, count)| kind_weights.get(kind).copied().unwrap_or(0.0) * (*count as f64))
        .sum()
}

/// Applies the salience factor: `composite × (1 + gamma × salience)`.
///
/// `weighted_sum = 0` ⇒ returns `composite` unchanged (bit-identical, no extra
/// floating-point op) — same non-regression contract as `composite_score_with_trust(None)`.
///
/// # Errors
///
/// Pure function — infaillible.
#[must_use]
pub fn apply_salience(composite: f64, weighted_sum: f64, params: &SalienceParams) -> f64 {
    let s = salience_factor(weighted_sum, params.k_norm);
    if s == 0.0 {
        return composite;
    }
    composite * (1.0 + params.gamma * s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // F-110 Phase 2 : salience_factor — zéro usage ⇒ 0.0 exact (facteur neutre en aval)
    #[test]
    fn salience_factor_zero_usage_is_zero() {
        assert_eq!(salience_factor(0.0, 10.0), 0.0);
    }

    // Saturation douce : s == k_norm ⇒ 0.5 exact
    #[test]
    fn salience_factor_saturates_at_half_when_sum_equals_k() {
        assert_eq!(salience_factor(10.0, 10.0), 0.5);
    }

    // Monotone croissante, bornée < 1
    #[test]
    fn salience_factor_is_monotonic_and_bounded() {
        let a = salience_factor(1.0, 10.0);
        let b = salience_factor(100.0, 10.0);
        assert!(a < b && b < 1.0);
    }

    // Somme pondérée : kinds connus pondérés, kind inconnu ignoré (poids 0)
    #[test]
    fn salience_weighted_sum_weights_kinds_and_ignores_unknown() {
        let mut w = std::collections::HashMap::new();
        w.insert("read".to_string(), 3.0);
        w.insert("search-hit".to_string(), 0.5);
        let counts = vec![
            ("read".to_string(), 2u64),          // 2×3.0 = 6.0
            ("search-hit".to_string(), 4u64),    // 4×0.5 = 2.0
            ("kind-inconnu".to_string(), 99u64), // ignoré
        ];
        assert_eq!(salience_weighted_sum(&counts, &w), 8.0);
    }

    // apply_salience : weighted_sum = 0 ⇒ composite inchangé bit-à-bit
    #[test]
    fn apply_salience_zero_sum_is_identity() {
        let p = SalienceParams {
            gamma: 0.10,
            k_norm: 10.0,
            kind_weights: Default::default(),
        };
        let c = 0.032_774_5_f64;
        assert_eq!(apply_salience(c, 0.0, &p), c);
    }

    // apply_salience : formule complète c × (1 + γ·s/(s+K))
    #[test]
    fn apply_salience_full_formula() {
        let p = SalienceParams {
            gamma: 0.10,
            k_norm: 10.0,
            kind_weights: Default::default(),
        };
        let got = apply_salience(1.0, 10.0, &p); // salience = 0.5 → ×1.05
        assert!((got - 1.05).abs() < 1e-12);
    }

    /// `TrustDecayConfig::default` derives its half-lives from the shared const.
    /// F-261 : non-regression — `Event = 90d`, `Static` absent (no decay), no other source.
    #[test]
    fn default_half_lives_match_const_source() {
        let map = default_half_lives();
        assert_eq!(map.len(), DEFAULT_TRUST_HALF_LIVES.len());
        for (k, v) in DEFAULT_TRUST_HALF_LIVES {
            assert_eq!(map.get(*k).copied(), Some(*v));
        }
        let cfg = TrustDecayConfig::default();
        assert_eq!(cfg.half_life_days, map);
        assert_eq!(cfg.half_life_days.get("Event").copied(), Some(90.0));
        assert!(!cfg.half_life_days.contains_key("Static"));
        assert!(cfg.enabled);
    }

    // T11-1 : Note fraîche → recency_factor = 1.0
    #[test]
    fn recency_factor_fresh_note_returns_one() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let f = recency_factor(now_ms, now_ms);
        assert!(
            (f - 1.0).abs() < 0.001,
            "note créée maintenant → factor ≈ 1.0, got {f}"
        );
    }

    // T11-2 : Note vieille de 30 jours → decay visible
    #[test]
    fn recency_factor_decays_over_30_days() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let thirty_days_ago = now_ms - 30 * 24 * 3_600_000i64;
        let f = recency_factor(thirty_days_ago, now_ms);
        // exp(-0.01 * 30) ≈ 0.7408
        assert!(f > 0.70 && f < 0.76, "30j → factor ≈ 0.74, got {f}");
    }

    // T11-3 : Note vieille d'un an → decay fort mais positif
    #[test]
    fn recency_factor_one_year_is_positive_and_small() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let one_year_ago = now_ms - 365 * 24 * 3_600_000i64;
        let f = recency_factor(one_year_ago, now_ms);
        // exp(-0.01 * 365) ≈ 0.0257
        assert!(f > 0.01 && f < 0.05, "1an → factor ≈ 0.026, got {f}");
    }

    // T11-4 : Timestamp futur (horloge dérivée) → clampé à 1.0
    #[test]
    fn recency_factor_future_timestamp_clamped_to_one() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let future = now_ms + 86_400_000i64; // +1 jour
        let f = recency_factor(future, now_ms);
        assert!(
            (f - 1.0).abs() < 0.001,
            "timestamp futur → factor = 1.0 (clampé), got {f}"
        );
    }

    // T11-5 : Monotonie — note plus ancienne = factor plus petit
    #[test]
    fn recency_factor_is_monotonically_decreasing() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let f_7d = recency_factor(now_ms - 7 * 24 * 3_600_000i64, now_ms);
        let f_30d = recency_factor(now_ms - 30 * 24 * 3_600_000i64, now_ms);
        let f_90d = recency_factor(now_ms - 90 * 24 * 3_600_000i64, now_ms);
        assert!(
            f_7d > f_30d && f_30d > f_90d,
            "monotonie décroissante requise"
        );
    }

    // T11-6 : pagerank_factor — note sans backlinks → 0.0
    #[test]
    fn pagerank_factor_zero_backlinks_returns_zero() {
        let f = pagerank_factor(0);
        assert!(
            (f - 0.0).abs() < f64::EPSILON,
            "0 backlinks → pagerank = 0.0"
        );
    }

    // T11-7 : pagerank_factor — 5 backlinks → 0.5 (normalization_constant = 5)
    #[test]
    fn pagerank_factor_five_backlinks_returns_half() {
        let f = pagerank_factor(5);
        assert!(
            (f - 0.5).abs() < 0.001,
            "5 backlinks avec norm=5 → factor = 0.5, got {f}"
        );
    }

    // T11-8 : pagerank_factor — borne supérieure < 1.0 même avec beaucoup de backlinks
    #[test]
    fn pagerank_factor_bounded_below_one() {
        let f = pagerank_factor(10_000);
        assert!(
            f < 1.0 && f > 0.99,
            "10k backlinks → factor ≈ 0.9999.., got {f}"
        );
    }

    // T11-8b : pagerank_factor clampé strictement dans [0.0, 1.0] — caveat B-P0-1
    #[test]
    fn pagerank_factor_is_strictly_in_zero_one_range() {
        // u64::MAX ne peut pas faire dépasser 1.0 grâce au clamp explicite.
        let f = pagerank_factor(u64::MAX);
        assert!(
            (0.0..=1.0).contains(&f),
            "pagerank_factor doit être ∈ [0.0, 1.0], got {f}"
        );
    }

    // T11-9 : composite_score — sans bonus → rrf_score inchangé
    #[test]
    fn composite_score_no_bonus_equals_rrf() {
        // recency=0.0, pagerank=0.0 → (1+0.2×0)×(1+0.1×0) = 1.0 → score = rrf
        let rrf = 0.0322;
        let cs = composite_score(rrf, 0.0, 0.0);
        assert!(
            (cs - rrf).abs() < 1e-9,
            "pas de bonus → composite = rrf, got {cs}"
        );
    }

    // T11-10 : composite_score — bonus maximum reasonable
    #[test]
    fn composite_score_with_full_bonus_is_bounded() {
        // recency=1.0 (note d'aujourd'hui), pagerank=1.0 (clampé)
        // composite = rrf × 1.2 × 1.1 = rrf × 1.32
        let rrf = 0.0322;
        let cs = composite_score(rrf, 1.0, 1.0);
        let expected = rrf * 1.2 * 1.1;
        assert!(
            (cs - expected).abs() < 1e-9,
            "bonus max: rrf×1.32, got {cs} vs {expected}"
        );
    }

    // ── F-17 trust decay ─────────────────────────────────────────────────────

    // T17-1 : pas de demi-vie → trust inchangé (provenance non périssable).
    #[test]
    fn trust_decay_no_half_life_returns_trust() {
        assert!((trust_decay_factor(0.95, 1000.0, None) - 0.95).abs() < 1e-9);
        // Même très âgée, human-decision (None) ne décroît pas.
        assert!((trust_decay_factor(0.95, 100_000.0, None) - 0.95).abs() < 1e-9);
    }

    // T17-2 : à l'âge = demi-vie → trust × 0.5.
    #[test]
    fn trust_decay_at_one_half_life_halves() {
        let f = trust_decay_factor(0.60, 90.0, Some(90.0));
        assert!((f - 0.30).abs() < 1e-6, "0.60 × 0.5^1 = 0.30, got {f}");
    }

    // T17-3 : à l'âge = 2× demi-vie → trust × 0.25.
    #[test]
    fn trust_decay_at_two_half_lives_quarters() {
        let f = trust_decay_factor(0.60, 180.0, Some(90.0));
        assert!((f - 0.15).abs() < 1e-6, "0.60 × 0.5^2 = 0.15, got {f}");
    }

    // T17-4 : âge 0 → trust intégral.
    #[test]
    fn trust_decay_fresh_note_full_trust() {
        let f = trust_decay_factor(0.60, 0.0, Some(90.0));
        assert!((f - 0.60).abs() < 1e-9, "âge 0 → 0.5^0 = 1, got {f}");
    }

    // T17-5 : âge négatif (horloge dérivée) clampé à 0 → trust intégral.
    #[test]
    fn trust_decay_negative_age_clamped() {
        let f = trust_decay_factor(0.60, -10.0, Some(90.0));
        assert!((f - 0.60).abs() < 1e-9, "âge négatif clampé, got {f}");
    }

    // T17-6 : demi-vie non-positive (garde) → pas de decay.
    #[test]
    fn trust_decay_nonpositive_half_life_no_decay() {
        assert!((trust_decay_factor(0.60, 100.0, Some(0.0)) - 0.60).abs() < 1e-9);
        assert!((trust_decay_factor(0.60, 100.0, Some(-5.0)) - 0.60).abs() < 1e-9);
    }

    // T17-7 : trust hors borne clampé dans [0,1].
    #[test]
    fn trust_decay_clamps_trust_input() {
        assert!((trust_decay_factor(1.5, 0.0, None) - 1.0).abs() < 1e-9);
        assert!((trust_decay_factor(-0.5, 0.0, None) - 0.0).abs() < 1e-9);
    }

    // T17-8 : FLAG OFF (trust_params=None) → STRICTEMENT identique à composite_score.
    #[test]
    fn composite_with_trust_flag_off_is_identical_to_v043() {
        let rrf = 0.0322;
        for &(rec, pr) in &[(0.0, 0.0), (1.0, 1.0), (0.5, 0.3), (0.74, 0.5)] {
            let base = composite_score(rrf, rec, pr);
            let off = composite_score_with_trust(rrf, rec, pr, None);
            assert_eq!(
                base.to_bits(),
                off.to_bits(),
                "flag OFF doit être bit-identique : base={base}, off={off} (rec={rec}, pr={pr})"
            );
        }
    }

    // T17-9 : FLAG ON applique le facteur (1 + γ × trust_decayed).
    #[test]
    fn composite_with_trust_flag_on_applies_factor() {
        let rrf = 0.0322;
        let base = composite_score(rrf, 0.0, 0.0);
        // trust=0.60, âge=0, demi-vie 90 → trust_decayed=0.60 → ×(1 + 0.15×0.60)=×1.09.
        let with = composite_score_with_trust(rrf, 0.0, 0.0, Some((0.60, 0.0, Some(90.0))));
        let expected = base * (1.0 + GAMMA_TRUST * 0.60);
        assert!(
            (with - expected).abs() < 1e-12,
            "facteur trust attendu : {expected}, got {with}"
        );
        assert!(with > base, "trust positif → score boosté");
    }

    // T17-10 : note ancienne distillée (decay fort) → boost trust quasi-nul.
    #[test]
    fn composite_with_trust_aged_distilled_minimal_boost() {
        let rrf = 0.0322;
        let base = composite_score(rrf, 0.0, 0.0);
        // trust=0.60, âge=900j, demi-vie 90 → 0.5^10 ≈ 0.00098 → trust_decayed ≈ 0.00059.
        let with = composite_score_with_trust(rrf, 0.0, 0.0, Some((0.60, 900.0, Some(90.0))));
        assert!(
            (with - base).abs() < base * 0.001,
            "note très âgée → boost trust négligeable : base={base}, with={with}"
        );
    }

    // ── TrustDecayConfig::resolve / resolve_for_section ─────────────────────

    // Défaut : decay activé, Event=90j présent, Static absent (no decay).
    #[test]
    fn trust_decay_config_default() {
        let cfg = TrustDecayConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.half_life_days.get("Event").copied(), Some(90.0));
        assert!(!cfg.half_life_days.contains_key("Static"));
    }

    // enabled=false → resolve retourne None (neutralise le facteur trust).
    #[test]
    fn resolve_disabled_returns_none() {
        let cfg = TrustDecayConfig {
            enabled: false,
            half_life_days: std::collections::HashMap::new(),
        };
        assert!(cfg.resolve(Some(0.6), Some("Event"), 10.0).is_none());
        assert!(cfg.resolve_for_section("decisions", 10.0).is_none());
    }

    // trust absent → None (pas de facteur).
    #[test]
    fn resolve_no_trust_returns_none() {
        let cfg = TrustDecayConfig::default();
        assert!(cfg.resolve(None, Some("Event"), 10.0).is_none());
    }

    // doc_kind Event (avec demi-vie) → Some avec half_life.
    #[test]
    fn resolve_event_with_half_life() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve(Some(0.6), Some("Event"), 45.0);
        assert_eq!(r, Some((0.6_f32 as f64, 45.0, Some(90.0))));
    }

    // doc_kind Static (sans demi-vie) → Some avec half_life=None (pas de decay).
    #[test]
    fn resolve_static_no_decay() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve(Some(0.95), Some("Static"), 1000.0);
        assert_eq!(r, Some((0.95_f32 as f64, 1000.0, None)));
    }

    // doc_kind None → half_life None (pas de decay).
    #[test]
    fn resolve_doc_kind_none_no_decay() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve(Some(0.5), None, 50.0);
        assert_eq!(r, Some((0.5_f32 as f64, 50.0, None)));
    }

    // ── F-261 : resolve_for_section — trust dérivé de la section, decay par doc_kind ──

    // Section Event (debug) → trust dérivé 0.40 + half_life 90j.
    #[test]
    fn resolve_for_section_event_decays() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve_for_section("debug", 45.0);
        assert_eq!(r, Some((0.40_f64, 45.0, Some(90.0))));
    }

    // Section Static (decisions) → trust dérivé 0.95 + pas de decay.
    #[test]
    fn resolve_for_section_static_no_decay() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve_for_section("decisions", 1000.0);
        assert_eq!(r, Some((0.95_f64, 1000.0, None)));
    }

    // Section inconnue (hors canon) → neutre 0.5 + doc_kind Static (pas de decay).
    #[test]
    fn resolve_for_section_unknown_neutral() {
        let cfg = TrustDecayConfig::default();
        let r = cfg.resolve_for_section("notes", 50.0);
        assert_eq!(r, Some((0.50_f64, 50.0, None)));
    }

    // resolve_for_section ne lit JAMAIS la colonne : le trust est purement dérivé.
    #[test]
    fn resolve_for_section_is_pure_function_of_section() {
        let cfg = TrustDecayConfig::default();
        for section in [
            "debug",
            "council",
            "agent-issues",
            "decisions",
            "notes",
            "reference",
        ] {
            let a = cfg.resolve_for_section(section, 7.0);
            let b = cfg.resolve_for_section(section, 7.0);
            assert_eq!(a, b, "déterministe pour {section}");
        }
    }

    // F-261 (2026-08-25) : palier A exempté — un verdict council CONSERVE 0.95 à 31/60/180 j.
    // Avant : 31j→0.75 (sous lessons-learned/architecture), 60j→0.60 (sous reference),
    // 180j→0.237. La doctrine « un acte ne se rejuge pas » rend ce décroissance absurde.
    #[test]
    fn resolve_for_section_council_never_decays() {
        let cfg = TrustDecayConfig::default();
        for age in [31.0, 60.0, 180.0] {
            let r = cfg.resolve_for_section("council", age);
            assert_eq!(
                r,
                Some((0.95_f64, age, None)),
                "council à {age}j doit ne pas décroître"
            );
            let f = trust_decay_factor(0.95, age, None);
            assert!(
                (f - 0.95).abs() < 1e-12,
                "trust_decay_factor(0.95, {age}j, None) doit rester 0.95, got {f}"
            );
        }
    }

    // F-261 (2026-08-25) : le palier A ENTIER (council/decisions/project-map/identity) est
    // non-décroissant ; debug/agent-issues périment toujours avec la demi-vie Event=90j.
    #[test]
    fn resolve_for_section_tier_a_all_non_decaying() {
        let cfg = TrustDecayConfig::default();
        for section in ["council", "decisions", "project-map", "identity"] {
            assert!(
                gradatum_core::section::is_trust_non_decaying(section),
                "{section} doit être dans TRUST_NON_DECAYING_SECTIONS"
            );
            let r = cfg.resolve_for_section(section, 500.0);
            assert_eq!(
                r,
                Some((0.95_f64, 500.0, None)),
                "{section} ne doit pas décroître"
            );
        }
        // Contrôle : Event hors palier A périme toujours (demi-vie 90j conservée).
        // debug = 0.40 (palier D), agent-issues = 0.60 (palier C) — les deux périment.
        let expected_trust = [("debug", 0.40_f64), ("agent-issues", 0.60_f64)];
        for (section, trust) in expected_trust {
            assert!(!gradatum_core::section::is_trust_non_decaying(section));
            let r = cfg.resolve_for_section(section, 45.0);
            assert_eq!(r, Some((trust, 45.0, Some(90.0))), "{section} doit périr");
        }
    }

    // ── T6 — ScoringWeights + composite_score_weighted ───────────────────────

    // P0-2 : poids par défaut ⇒ reproduit composite_score_with_trust bit-pour-bit.
    #[test]
    fn weighted_defaults_match_existing_multiplicative_bit_for_bit() {
        let (rrf, rec, pr) = (0.5_f64, 0.9_f64, 0.3_f64);
        let trust = Some((0.8_f64, 10.0_f64, Some(90.0)));
        let expected = composite_score_with_trust(rrf, rec, pr, trust);
        let got = composite_score_weighted(rrf, rec, pr, trust, &ResolvedWeights::default());
        assert!(
            (got - expected).abs() < 1e-12,
            "défauts doivent reproduire l'existant : got={got} expected={expected}"
        );
    }

    // Poids nuls ⇒ tous les facteurs (1+0·x)=1 ⇒ score == rrf pur (forme multiplicative).
    #[test]
    fn weighted_zero_weights_collapse_to_rrf() {
        let got = composite_score_weighted(
            0.5,
            0.9,
            0.3,
            None,
            &ResolvedWeights {
                recency: 0.0,
                pagerank: 0.0,
                trust: 0.0,
            },
        );
        assert!(
            (got - 0.5).abs() < 1e-9,
            "poids nuls → score = rrf = 0.5, got {got}"
        );
    }

    // resolve_weights(None) retourne les défauts (0.2 / 0.1 / GAMMA_TRUST).
    #[test]
    fn resolve_weights_falls_back_to_defaults() {
        let w = resolve_weights(None);
        assert!((w.recency - 0.2).abs() < 1e-9, "recency défaut 0.2");
        assert!((w.pagerank - 0.1).abs() < 1e-9, "pagerank défaut 0.1");
        assert!(
            (w.trust - GAMMA_TRUST).abs() < 1e-9,
            "trust défaut GAMMA_TRUST={GAMMA_TRUST}"
        );
    }

    // ── F-162 critère 10 — weighted_fusion_score ─────────────────────────────

    // Deux bras au maximum : 0.5×n_bm25 + 0.5×n_sem = 0.5×1.0 + 0.5×1.0 = 1.0.
    #[test]
    fn weighted_fusion_both_arms_maximal_is_one() {
        let s = weighted_fusion_score(Some(0.0), Some(1.0));
        assert!((s - 1.0).abs() < 1e-12, "deux bras max → 1.0, got {s}");
    }

    // Absence d'un bras → contribution 0.0 (pas confondu avec un score neutre).
    #[test]
    fn weighted_fusion_absent_arm_contributes_zero() {
        // BM25 présent, sémantique absente → 0.5 × normalize_bm25(-0.5) = 0.5 × 0.6667.
        let s = weighted_fusion_score(Some(-0.5), None);
        let expect = 0.5 * normalize_bm25(-0.5);
        assert!(
            (s - expect).abs() < 1e-12,
            "bras absent → 0.5×n_bm25 seul, got {s} vs {expect}"
        );
        // Sémantique présente, BM25 absent → 0.5 × normalize_semantic(0.9).
        let s2 = weighted_fusion_score(None, Some(0.9));
        let expect2 = 0.5 * normalize_semantic(0.9);
        assert!(
            (s2 - expect2).abs() < 1e-12,
            "bras absent → 0.5×n_sem seul, got {s2} vs {expect2}"
        );
    }

    // Le sentinelle Some(0.0) pour BM25 N'EST PAS l'absence : normalize_bm25(0.0)=1.0.
    // Absent (None) ≠ présent-neutre (Some(0.0)) — le piège documenté.
    #[test]
    fn weighted_fusion_bm25_zero_is_not_absence() {
        let absent = weighted_fusion_score(None, Some(0.0));
        let present_zero = weighted_fusion_score(Some(0.0), Some(0.0));
        assert!(
            present_zero > absent,
            "Some(0.0) BM25 = match parfait (1.0) ≠ absent (0.0) : present={present_zero} absent={absent}"
        );
    }

    // Une note dans les DEUX bras bat une note dans un seul bras à magnitude égale.
    #[test]
    fn weighted_fusion_two_arms_beats_one_arm() {
        let both = weighted_fusion_score(Some(-0.5), Some(0.9));
        let only_bm25 = weighted_fusion_score(Some(-0.5), None);
        assert!(
            both > only_bm25,
            "les deux bras additionnent : both={both} only_bm25={only_bm25}"
        );
    }

    // Monotonie stricte : un meilleur score d'un bras → fusion strictement supérieure.
    #[test]
    fn weighted_fusion_is_strictly_monotone() {
        let weak = weighted_fusion_score(Some(-10.0), Some(0.3));
        let strong = weighted_fusion_score(Some(-0.5), Some(0.9));
        assert!(strong > weak, "monotone : strong={strong} > weak={weak}");
    }

    // Borne : résultat toujours dans [0, 1].
    #[test]
    fn weighted_fusion_bounded_zero_one() {
        let min = weighted_fusion_score(None, Some(0.0));
        let max = weighted_fusion_score(Some(0.0), Some(1.0));
        assert!((0.0..=1.0).contains(&min), "min ∈ [0,1], got {min}");
        assert!((0.0..=1.0).contains(&max), "max ∈ [0,1], got {max}");
    }
}
