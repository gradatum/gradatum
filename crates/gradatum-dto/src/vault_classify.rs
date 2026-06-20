use serde::{Deserialize, Serialize};

use crate::default_main;

/// Request body for `vault_classify` — synchronous heuristic re-classification of an existing note.
///
/// Serialized via `bincode::serde::encode_to_vec` for the queue payload.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultClassifyRequest {
    /// ULID identifier of the note to re-classify.
    pub note_id: String,
    /// Target tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
}

/// Response body for `vault_classify` — result of the offline heuristic classification.
///
/// All classification is performed synchronously by the offline heuristic (zero LLM, zero
/// network calls). The `confidence` field reports the heuristic signal strength; `method`
/// is always `"heuristic"` in this endpoint.
///
/// # Confidence values
///
/// Two discrete values are emitted by the current heuristic mode:
///
/// - `0.9` — high-confidence match: the heuristic routed the note to a canonical section
///   with strong keyword evidence (`Admitted` outcome).
/// - `0.5` — ambiguous case: the heuristic produced a best-guess section but with low
///   confidence (`Pending` outcome). Human review may be needed.
///
/// The value `0.0` (`Rejected` outcome) is **reserved for a future LLM-review mode**
/// (not yet implemented). The current offline heuristic always emits `0.9` or `0.5`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultClassifyResponse {
    /// ULID identifier of the classified note.
    pub note_id: String,
    /// Current canonical section of the note (as stored in the vault index).
    pub current_section: String,
    /// Section suggested by the heuristic.
    ///
    /// Equals `current_section` when `confidence` is `0.0` (no reliable suggestion).
    pub suggested_section: String,
    /// Heuristic confidence signal — `0.9` (Admitted) or `0.5` (Pending) in the current
    /// heuristic mode. `0.0` is reserved for a future LLM-review mode (see type-level doc).
    pub confidence: f32,
    /// Classification method used — always `"heuristic"` for this endpoint.
    pub method: String,
}
