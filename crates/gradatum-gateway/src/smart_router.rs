//! SmartRouter — resolves `feature_id` to an `AliasTarget` with layered default parameters.
//!
//! Applies the following parameters to a completion request in decreasing priority order:
//!
//! 1. Explicit values from the client request (`temperature`, `max_tokens` when provided)
//! 2. AgentAware parameters for the `feature_id` (`[gateway."<feature_id>"]`)
//! 3. Alias defaults (`temperature_default`, `max_tokens_default`)
//!
//! Usage:
//! - The chat handler extracts the `X-Feature-Id` header and calls `apply()`.
//! - `apply()` mutates the request in place when overrides apply.

use crate::commons::chat::ChatCompletionRequest;
use crate::config::{AgentAwareParams, AliasTarget};

/// Applies SmartRouter parameters to a completion request.
///
/// Priority:
/// 1. Explicit values already in the request (never overwritten)
/// 2. `AgentAwareParams` (by `feature_id`, when provided)
/// 3. Alias defaults (`temperature_default`, `max_tokens_default`)
///
/// Returns the effective alias to use (may be overridden by `AgentAwareParams.alias_override`).
///
/// # Side effects
///
/// Mutates `request.temperature` and/or `request.max_tokens` when defaults exist
/// and the client did not supply those fields.
pub fn apply(
    request: &mut ChatCompletionRequest,
    alias: &AliasTarget,
    agent_params: Option<&AgentAwareParams>,
) -> AppliedRouting {
    // Resolve alias override from AgentAware when present.
    let alias_override = agent_params.and_then(|p| p.alias_override.as_deref());

    // Apply temperature in priority order.
    if request.temperature.is_none() {
        // Priority 2: AgentAware temperature.
        if let Some(t) = agent_params.and_then(|p| p.temperature) {
            request.temperature = Some(t);
        } else if let Some(t) = alias.temperature_default {
            // Priority 3: alias default.
            request.temperature = Some(t);
        }
    }

    // Apply max_tokens in priority order.
    if request.max_tokens.is_none() {
        // Priority 2: AgentAware max_tokens.
        if let Some(n) = agent_params.and_then(|p| p.max_tokens) {
            request.max_tokens = Some(n);
        } else if let Some(n) = alias.max_tokens_default {
            // Priority 3: alias default.
            request.max_tokens = Some(n);
        }
    }

    AppliedRouting {
        alias_override: alias_override.map(|s| s.to_owned()),
    }
}

/// Result of applying the SmartRouter.
#[derive(Debug, Clone)]
pub struct AppliedRouting {
    /// Alias overridden by `AgentAwareParams`, if applicable.
    ///
    /// `None` = no override; use the normally resolved alias.
    /// `Some(alias)` = use this alias instead.
    pub alias_override: Option<String>,
}

/// Sampling-preset selector, resolved from the two orthogonal axes by
/// [`resolve_mode`].
///
/// The mono-instance `agent-main` is driven by two orthogonal axes: reasoning
/// (think vs no-think) and modality (vision, triggered by an image). This enum
/// selects the **sampling preset** only, with priority vision > think > no-think
/// (see [`resolve_mode`]); the `enable_thinking` template flag follows the
/// reasoning axis independently (see [`apply_reasoning`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningMode {
    /// Fast path — thinking block suppressed (`enable_thinking = false`).
    NoThink,
    /// Deliberate path — thinking block open.
    Think,
    /// Multimodal path — an image is present in the request.
    Vision,
}

/// Applies a reasoning mode's sampling preset to a request as **defaults**.
///
/// Layering is identical to [`apply`]: a field already set by the client is
/// never overwritten — the preset only fills `None` fields.
///
/// # Side effects
///
/// Mutates `temperature`, `top_p`, `top_k`, `min_p` and `presence_penalty` when
/// they are `None`.
pub(crate) fn apply_mode_sampling(request: &mut ChatCompletionRequest, mode: ReasoningMode) {
    // Presets sampling per-mode (reco Bob 2026-07-10, tunable). Appliqués en défauts.
    // ECON: constantes nommées en code, pas de config TOML tant qu'aucun opérateur ne
    // les tune (review-cut-dead-config). Upgrade -> table `[gateway.mode.<mode>]` si un
    // tuning à chaud devient nécessaire. Le mono-instance sert tous les modes depuis un
    // seul launch : les défauts per-mode DOIVENT venir du per-requête, pas des flags.
    let (temperature, top_p, top_k, min_p, presence_penalty) = match mode {
        // no-think : temp 0.2–0.4 (0.4 = base Bob), top_p 0.9, top_k 20, min_p 0, presence 1.0
        ReasoningMode::NoThink => (0.4_f32, 0.9_f32, 20_u32, 0.0_f32, 1.0_f32),
        // think : temp 0.6, top_p 0.95, top_k 20, min_p 0, presence 1.0–1.5 (1.2)
        ReasoningMode::Think => (0.6, 0.95, 20, 0.0, 1.2),
        // vision : temp 0.1–0.3 (0.2), top_p 0.9, top_k 20, min_p 0, presence 1.0
        ReasoningMode::Vision => (0.2, 0.9, 20, 0.0, 1.0),
    };
    if request.temperature.is_none() {
        request.temperature = Some(temperature);
    }
    if request.top_p.is_none() {
        request.top_p = Some(top_p);
    }
    if request.top_k.is_none() {
        request.top_k = Some(top_k);
    }
    if request.min_p.is_none() {
        request.min_p = Some(min_p);
    }
    if request.presence_penalty.is_none() {
        request.presence_penalty = Some(presence_penalty);
    }
}

/// Resolves the **sampling-preset** mode from the two orthogonal axes.
///
/// Priority for the sampling preset: vision > think > no-think. This is distinct
/// from the `enable_thinking` flag, which follows `reasoning` independently — a
/// `vision=true, reasoning=true` request uses the *vision* sampling preset AND
/// opens the think block (see [`apply_reasoning`]).
fn resolve_mode(reasoning: bool, vision: bool) -> ReasoningMode {
    if vision {
        ReasoningMode::Vision
    } else if reasoning {
        ReasoningMode::Think
    } else {
        ReasoningMode::NoThink
    }
}

/// Injects the `enable_thinking` boolean into `chat_template_kwargs`.
///
/// Qwen3.5's chat template toggles reasoning via the boolean template flag
/// `enable_thinking` (proven empirically in A0) — NOT the inline `/think` /
/// `/no_think` tokens. The flag is set explicitly (`true` or `false`) rather than
/// relying on the template default, so the intent survives a template change.
///
/// Layering: a client-provided `enable_thinking` is never overwritten; other
/// existing kwargs are preserved. A non-object `chat_template_kwargs` (malformed
/// for llama.cpp) is replaced by a fresh object carrying the flag.
fn set_enable_thinking(request: &mut ChatCompletionRequest, reasoning: bool) {
    use serde_json::{Map, Value};
    match request.chat_template_kwargs {
        Some(Value::Object(ref mut map)) => {
            map.entry("enable_thinking")
                .or_insert_with(|| Value::Bool(reasoning));
        }
        _ => {
            let mut map = Map::new();
            map.insert("enable_thinking".to_owned(), Value::Bool(reasoning));
            request.chat_template_kwargs = Some(Value::Object(map));
        }
    }
}

/// Applies the resolved reasoning/modality to a request (two orthogonal axes).
///
/// - **Sampling preset** — from [`resolve_mode`] (priority vision > think >
///   no-think), applied as defaults via [`apply_mode_sampling`].
/// - **`enable_thinking`** — follows the `reasoning` axis, injected into
///   `chat_template_kwargs` via [`set_enable_thinking`].
///
/// Consequence (documented coherence choice): a `vision=true, reasoning=true`
/// request uses the vision sampling preset while still opening the think block —
/// the modality axis drives sampling, the reasoning axis drives thinking.
///
/// Client-explicit sampling fields and a client-provided `enable_thinking` are
/// never overwritten. The `reasoning` axis is supplied by the router (B2) / an
/// override (B3) / a default; `vision` is the presence of an image in the request.
pub(crate) fn apply_reasoning(request: &mut ChatCompletionRequest, reasoning: bool, vision: bool) {
    apply_mode_sampling(request, resolve_mode(reasoning, vision));
    set_enable_thinking(request, reasoning);
}

/// Origin of the resolved `reasoning` decision, in precedence order.
///
/// Precedence: an explicit caller override wins over the router's decision, which
/// wins over the server default (no-think). Logged for observability — the routing
/// (think → no-think) must never be a silent downgrade (council 01KWVXAWB3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningSource {
    /// Explicit caller override (e.g. the `X-Reasoning-Mode` header).
    Override,
    /// Router decision (curator B2 or its cheap deterministic pre-classifier).
    Router,
    /// Router unavailable (busy / timeout / down) → no-think fallback. Never silent
    /// (logged + counted) — preserves the ctx-gating observability invariant.
    Fallback,
    /// Server default (no-think) — router disabled / not configured.
    Default,
}

impl ReasoningSource {
    /// Stable lowercase label for structured logs.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Router => "router",
            Self::Fallback => "fallback",
            Self::Default => "default",
        }
    }
}

/// Resolves the `reasoning` axis by precedence: **override > router outcome > default(no-think)**.
///
/// `router_outcome` is the router subsystem's result — `(decision, source)` where the
/// source is [`ReasoningSource::Router`] (pre-classifier or curator) or
/// [`ReasoningSource::Fallback`] (curator unavailable). `None` when the router is disabled,
/// in which case the no-think default applies. The caller override always wins.
pub(crate) fn resolve_reasoning(
    override_decision: Option<bool>,
    router_outcome: Option<(bool, ReasoningSource)>,
) -> (bool, ReasoningSource) {
    if let Some(r) = override_decision {
        (r, ReasoningSource::Override)
    } else if let Some((r, source)) = router_outcome {
        (r, source)
    } else {
        (false, ReasoningSource::Default)
    }
}

/// Fixed per-axis output-token reserve for the ctx-gating headroom (named caps, no ratio).
///
/// The `<think>` block is output that callers rarely size via `max_tokens`; without a
/// floor, the cap-check undercounts it (0 when `max_tokens` is unset) and an oversized
/// reasoning request could overflow the slot ctx mid-generation (freeze). The reserve
/// tracks the **reasoning** axis primarily (the think block is the overflow risk), with a
/// vision bump for no-think multimodal answers. Used as a floor
/// (`max(max_tokens, reserve)`) before the **fail-loud** cap-check — never a silent
/// downgrade.
pub(crate) fn reasoning_output_reserve(reasoning: bool, vision: bool) -> u64 {
    // Safety caps (protection overflow ctx / DoS) — `const` autorisées par ADN 4.
    const THINK_RESERVE: u64 = 4096;
    const VISION_RESERVE: u64 = 1024;
    const NOTHINK_RESERVE: u64 = 512;
    match (reasoning, vision) {
        (true, _) => THINK_RESERVE,
        (false, true) => VISION_RESERVE,
        (false, false) => NOTHINK_RESERVE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::{ContentPart, ImageUrlDetail, Message, MessageContent, Role};
    use crate::config::AgentAwareParams;

    /// Lit le flag `enable_thinking` dans `chat_template_kwargs` (helper de test).
    fn enable_thinking_of(req: &ChatCompletionRequest) -> Option<bool> {
        req.chat_template_kwargs
            .as_ref()
            .and_then(|v| v.get("enable_thinking"))
            .and_then(serde_json::Value::as_bool)
    }

    fn make_request(temperature: Option<f32>, max_tokens: Option<u32>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user("bonjour")],
            max_tokens,
            stream: None,
            temperature,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            stop: None,
            tools: None,
            tool_choice: None,
            chat_template_kwargs: None,
        }
    }

    fn make_alias(temp: Option<f32>, max_tokens: Option<u32>) -> AliasTarget {
        AliasTarget {
            provider: "p".to_string(),
            model: "m".to_string(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: temp,
            max_tokens_default: max_tokens,
            vision_capable: false,
        }
    }

    #[test]
    fn test_no_override_when_request_has_values() {
        let mut req = make_request(Some(0.5), Some(100));
        let alias = make_alias(Some(0.9), Some(200));
        let result = apply(&mut req, &alias, None);
        // Les valeurs explicites ne doivent pas être écrasées.
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.max_tokens, Some(100));
        assert!(result.alias_override.is_none());
    }

    #[test]
    fn test_alias_defaults_applied_when_request_empty() {
        let mut req = make_request(None, None);
        let alias = make_alias(Some(0.7), Some(512));
        apply(&mut req, &alias, None);
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(512));
    }

    #[test]
    fn test_agent_params_override_alias_defaults() {
        let mut req = make_request(None, None);
        let alias = make_alias(Some(0.7), Some(512));
        let agent = AgentAwareParams {
            temperature: Some(0.1),
            max_tokens: Some(1024),
            alias_override: None,
        };
        apply(&mut req, &alias, Some(&agent));
        // AgentAware prime sur les défauts alias.
        assert_eq!(req.temperature, Some(0.1));
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn test_request_values_prime_over_agent_params() {
        let mut req = make_request(Some(0.3), Some(50));
        let alias = make_alias(Some(0.7), Some(512));
        let agent = AgentAwareParams {
            temperature: Some(0.1),
            max_tokens: Some(1024),
            alias_override: None,
        };
        apply(&mut req, &alias, Some(&agent));
        // Les valeurs explicites de la requête ne doivent pas être modifiées.
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.max_tokens, Some(50));
    }

    #[test]
    fn test_alias_override_returned() {
        let mut req = make_request(None, None);
        let alias = make_alias(None, None);
        let agent = AgentAwareParams {
            temperature: None,
            max_tokens: None,
            alias_override: Some("other-alias".to_string()),
        };
        let result = apply(&mut req, &alias, Some(&agent));
        assert_eq!(result.alias_override.as_deref(), Some("other-alias"));
    }

    #[test]
    fn test_no_alias_defaults_no_change() {
        let mut req = make_request(None, None);
        let alias = make_alias(None, None);
        apply(&mut req, &alias, None);
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
    }

    // --- Per-mode sampling (A2) ---

    #[test]
    fn mode_sampling_applique_le_preset_de_chaque_mode() {
        // (mode, temp, top_p, top_k, min_p, presence) — reco Bob 2026-07-10.
        let cas = [
            (
                ReasoningMode::NoThink,
                0.4_f32,
                0.9_f32,
                20_u32,
                0.0_f32,
                1.0_f32,
            ),
            (ReasoningMode::Think, 0.6, 0.95, 20, 0.0, 1.2),
            (ReasoningMode::Vision, 0.2, 0.9, 20, 0.0, 1.0),
        ];
        for (mode, temp, top_p, top_k, min_p, presence) in cas {
            let mut req = make_request(None, None);
            apply_mode_sampling(&mut req, mode);
            assert_eq!(
                (
                    req.temperature,
                    req.top_p,
                    req.top_k,
                    req.min_p,
                    req.presence_penalty
                ),
                (
                    Some(temp),
                    Some(top_p),
                    Some(top_k),
                    Some(min_p),
                    Some(presence)
                ),
                "preset incorrect pour {mode:?}"
            );
        }
    }

    #[test]
    fn mode_sampling_nepas_ecraser_les_valeurs_client() {
        let mut req = make_request(Some(0.15), None);
        req.top_k = Some(5);
        apply_mode_sampling(&mut req, ReasoningMode::Think);
        // Valeurs client explicites conservées.
        assert_eq!(req.temperature, Some(0.15));
        assert_eq!(req.top_k, Some(5));
        // Champs absents remplis depuis le preset Think.
        assert_eq!(req.top_p, Some(0.95));
        assert_eq!(req.min_p, Some(0.0));
        assert_eq!(req.presence_penalty, Some(1.2));
    }

    #[test]
    fn sans_application_de_mode_aucun_champ_sampling_force() {
        // Mode non résolu : apply_mode_sampling n'est pas appelé → les champs restent
        // None → le moteur applique ses défauts de lancement (pas de forçage gateway).
        let req = make_request(None, None);
        assert_eq!(
            (req.top_k, req.min_p, req.presence_penalty, req.top_p),
            (None, None, None, None)
        );
    }

    #[test]
    fn serialisation_omet_les_champs_sampling_none() {
        // skip_serializing_if : champs None absents du body forwardé (non-breaking).
        let req = make_request(None, None);
        let v = serde_json::to_value(&req).expect("sérialisation ChatCompletionRequest");
        assert!(v.get("top_k").is_none());
        assert!(v.get("min_p").is_none());
        assert!(v.get("presence_penalty").is_none());
    }

    #[test]
    fn serialisation_emet_les_champs_sampling_some() {
        // Prouve que le forward (.json(&req)) propage les champs settés : la même
        // struct est re-sérialisée vers l'engine.
        let mut req = make_request(None, None);
        apply_mode_sampling(&mut req, ReasoningMode::Think);
        let v = serde_json::to_value(&req).expect("sérialisation ChatCompletionRequest");
        assert_eq!(v.get("top_k").and_then(serde_json::Value::as_u64), Some(20));
        assert!(v.get("min_p").is_some());
        assert!(v.get("presence_penalty").is_some());
        assert!(v.get("temperature").is_some());
    }

    // --- Axes {vision}×{reasoning} + enable_thinking (B1) ---

    #[test]
    fn resolve_mode_priorise_vision_puis_think() {
        assert_eq!(resolve_mode(false, false), ReasoningMode::NoThink);
        assert_eq!(resolve_mode(true, false), ReasoningMode::Think);
        assert_eq!(resolve_mode(false, true), ReasoningMode::Vision);
        // vision prime sur reasoning POUR LE PRESET sampling (l'axe think reste actif ailleurs).
        assert_eq!(resolve_mode(true, true), ReasoningMode::Vision);
    }

    #[test]
    fn apply_reasoning_les_4_combos() {
        // (reasoning, vision, enable_thinking attendu, preset (temp, top_p, presence)).
        let cas = [
            (false, false, false, 0.4_f32, 0.9_f32, 1.0_f32), // texte no-think
            (true, false, true, 0.6, 0.95, 1.2),              // texte think
            (false, true, false, 0.2, 0.9, 1.0),              // vision no-think
            (true, true, true, 0.2, 0.9, 1.0), // vision think : preset VISION + think ON
        ];
        for (reasoning, vision, et, temp, top_p, presence) in cas {
            let mut req = make_request(None, None);
            apply_reasoning(&mut req, reasoning, vision);
            assert_eq!(
                enable_thinking_of(&req),
                Some(et),
                "enable_thinking pour r={reasoning} v={vision}"
            );
            assert_eq!(
                (req.temperature, req.top_p, req.presence_penalty),
                (Some(temp), Some(top_p), Some(presence)),
                "preset sampling pour r={reasoning} v={vision}"
            );
        }
    }

    #[test]
    fn apply_reasoning_nepas_ecraser_enable_thinking_client() {
        let mut req = make_request(None, None);
        req.chat_template_kwargs = Some(serde_json::json!({"enable_thinking": true, "foo": 1}));
        // reasoning=false, mais le client a explicitement mis enable_thinking=true.
        apply_reasoning(&mut req, false, false);
        assert_eq!(
            enable_thinking_of(&req),
            Some(true),
            "enable_thinking client jamais écrasé"
        );
        // Les autres kwargs client sont préservés.
        assert_eq!(
            req.chat_template_kwargs
                .as_ref()
                .and_then(|v| v.get("foo"))
                .and_then(serde_json::Value::as_i64),
            Some(1)
        );
    }

    #[test]
    fn apply_reasoning_nepas_ecraser_temperature_client() {
        let mut req = make_request(Some(0.15), None);
        apply_reasoning(&mut req, true, false); // Think
        assert_eq!(req.temperature, Some(0.15), "temp client conservée");
        assert_eq!(req.top_p, Some(0.95), "top_p rempli depuis le preset Think");
        assert_eq!(enable_thinking_of(&req), Some(true));
    }

    #[test]
    fn apply_reasoning_ne_mute_pas_les_messages() {
        // Axe vision : l'image doit rester intacte (apply_reasoning ne touche pas messages).
        let img = Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrlDetail {
                    url: "data:image/png;base64,abc".to_owned(),
                },
            }]),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let mut req = make_request(None, None);
        req.messages = vec![img];
        apply_reasoning(&mut req, true, true); // vision + think
        assert_eq!(req.messages.len(), 1, "aucun message ajouté/supprimé");
        assert!(
            req.messages.iter().any(|m| m.has_image()),
            "l'image doit rester présente"
        );
    }

    // --- Précédence + headroom ctx-gating (B3) ---

    #[test]
    fn resolve_reasoning_override_gagne_sur_routeur() {
        // (a) Override /think gagne sur un routeur no-think.
        let (r, src) = resolve_reasoning(Some(true), Some((false, ReasoningSource::Router)));
        assert!(r, "override think doit gagner");
        assert_eq!(src, ReasoningSource::Override);
    }

    #[test]
    fn resolve_reasoning_propage_le_resultat_routeur() {
        let (r, src) = resolve_reasoning(None, Some((true, ReasoningSource::Router)));
        assert!(r);
        assert_eq!(src, ReasoningSource::Router);
        // Le fallback est propagé tel quel (source distincte, jamais silencieux).
        let (r2, src2) = resolve_reasoning(None, Some((false, ReasoningSource::Fallback)));
        assert!(!r2);
        assert_eq!(src2, ReasoningSource::Fallback);
    }

    #[test]
    fn resolve_reasoning_defaut_no_think_quand_rien() {
        // (b) Routeur désactivé + pas d'override → défaut no-think.
        let (r, src) = resolve_reasoning(None, None);
        assert!(!r, "défaut = no-think");
        assert_eq!(src, ReasoningSource::Default);
    }

    #[test]
    fn reasoning_source_labels_stables() {
        // (c) La source loggée reflète la précédence.
        assert_eq!(ReasoningSource::Override.as_str(), "override");
        assert_eq!(ReasoningSource::Router.as_str(), "router");
        assert_eq!(ReasoningSource::Fallback.as_str(), "fallback");
        assert_eq!(ReasoningSource::Default.as_str(), "default");
    }

    #[test]
    fn headroom_reserve_par_axe() {
        assert_eq!(reasoning_output_reserve(true, false), 4096); // think
        assert_eq!(reasoning_output_reserve(true, true), 4096); // vision+think → think domine
        assert_eq!(reasoning_output_reserve(false, true), 1024); // vision no-think
        assert_eq!(reasoning_output_reserve(false, false), 512); // texte no-think
    }

    #[test]
    fn headroom_corrige_le_sous_comptage_a_max_tokens_none() {
        // (d)+(e) reasoning=on SANS max_tokens : l'ancien calcul comptait 0 en sortie
        // → une requête near-limit passait puis débordait mi-génération. Le plancher
        // `think_reserve` est désormais compté avant le cap-check (fail-loud).
        use crate::token_counter::estimate_input_tokens;
        let mut req = make_request(None, None); // max_tokens = None
        req.messages = vec![Message::user("a".repeat(4000))];
        let input = estimate_input_tokens(&req);

        // Budget corrigé = max(max_tokens=0, reserve think=4096) = 4096.
        let output_budget =
            u64::from(req.max_tokens.unwrap_or(0)).max(reasoning_output_reserve(true, false));
        assert_eq!(
            output_budget, 4096,
            "reserve think comptée même sans max_tokens"
        );

        // Cap juste au-dessus de l'input mais sous input+reserve.
        let cap = input + 1000; // < input + 4096
        assert!(input <= cap, "ancien total (input + 0) aurait passé le cap");
        assert!(
            input.saturating_add(output_budget) > cap,
            "total corrigé dépasse le cap → 413 fail-loud (pas de débordement mi-génération)"
        );
    }
}
