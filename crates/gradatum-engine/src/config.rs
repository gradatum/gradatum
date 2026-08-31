//! Configuration for `gradatum-engine` — parses `conf.d/70-engine.toml`.
//!
//! ## Configuration sources
//!
//! The local TOML file handed to [`EngineConfig::load_local`] is the **only** source.
//! Neither the process environment nor any remote endpoint contributes a value.
//!
//! The central source `/api/v1/config/:binary` remains a deferred figment provider,
//! deliberately not implemented.
//!
//! ### Why the environment is not a source
//!
//! This module used to advertise a second source: variables prefixed with
//! `GRADATUM_ENGINE_`. None of them ever reached a field. The deserialization target is
//! a private wrapper carrying the `[engine]` table, and the provider had no key
//! splitting configured — so no flat environment key could ever address
//! `engine.<field>`. The promise failed *silently*: an operator overriding a port or a
//! model path started on a configuration that was not the one they asked for, without
//! a single warning.
//!
//! The promise was removed rather than wired, for two reasons.
//!
//! 1. **The prefix is already taken, by a secret.** Every deployed unit exports
//!    `GRADATUM_ENGINE_API_KEY` through its `EnvironmentFile=` — surveyed 2026-08-18,
//!    5 engine units out of 5, and it is the only variable of that prefix any of them
//!    carries. Making the prefix address configuration fields would turn one name into
//!    two meanings.
//! 2. **It would route a secret through figment.** `figment::Error` renders the
//!    offending value in its `Display` — the leak `gradatum_core::config::redact_figment_error`
//!    exists to contain. The api-key keeps its own, narrower door: a single
//!    `std::env::var` read in the binary, straight into a `Zeroizing<String>`.
//!
//! ## Security
//!
//! Validation of `model_path` (canonicalization + prefix check against
//! `/opt/gradatum/models/`) and of `bind_addr` (rejection of wildcards `0.0.0.0`/`::`)
//! is performed by [`EngineConfig::validate()`].
//! `load_local()` only parses and deserializes — call `validate()` afterwards
//! (the binary always does this after `load_local()`).
//! `load_local()` alone does NOT validate the model path or the bind address.
use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// Loaded model type — determines the inference context.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// Text-generation model (chat/completions).
    Chat,
    /// Embedding model (embeddings).
    Embed,
}

/// Inference runtime configured under `[engine].runtime`.
///
/// - `LlamaServer`: supervisor for a native `llama-server` subprocess.
///   Replaces the previous FFI backend — **BREAKING CONFIG CHANGE**: rename
///   `llamacpp` → `llamaserver` in deployed TOML files (or omit `runtime` to use
///   the default).
/// - `Onnx`: deferred — the value is recognised and parsed, but the binary returns
///   an explicit error (branch exists, not implemented).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// Native `llama-server` supervisor (replaces FFI llama-cpp-2).
    #[default]
    LlamaServer,
    /// ONNX Runtime backend — deferred (design-only).
    Onnx,
}

/// Speculative-decoding strategy passed to `llama-server` via `--spec-type`.
///
/// Closed set mirroring the `llama-server` `--spec-type` accepted values. Deserialization
/// rejects any value outside this set (serde "unknown variant" error listing the valid set),
/// so no separate string validation is needed — this is the same closed-set-as-enum pattern
/// as [`ModelKind`] and [`RuntimeKind`].
///
/// - `Draft*` variants drive draft-model speculative decoding and **require** a companion
///   [`EngineConfig::draft_model_path`] (checked by [`EngineConfig::validate`]).
/// - `Ngram*` variants are self-contained (n-gram based) and must **not** carry a draft
///   model — supplying one is rejected as dead configuration.
///
/// `--spec-type` is intentionally excluded from `ALLOWED_EXTRA_FLAGS`: the strategy must be
/// configured through this dedicated field, never via `extra_args`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub enum SpecType {
    /// Draft-model speculative decoding, simple strategy.
    #[serde(rename = "draft-simple")]
    DraftSimple,
    /// Draft-model speculative decoding, EAGLE-3 strategy.
    #[serde(rename = "draft-eagle3")]
    DraftEagle3,
    /// Draft-model speculative decoding, Multi-Token-Prediction (MTP) strategy.
    #[serde(rename = "draft-mtp")]
    DraftMtp,
    /// N-gram lookup, simple strategy (no draft model).
    #[serde(rename = "ngram-simple")]
    NgramSimple,
    /// N-gram map, key strategy (no draft model).
    #[serde(rename = "ngram-map-k")]
    NgramMapK,
    /// N-gram map, key-4-value strategy (no draft model).
    #[serde(rename = "ngram-map-k4v")]
    NgramMapK4v,
    /// N-gram modulo strategy (no draft model).
    #[serde(rename = "ngram-mod")]
    NgramMod,
    /// N-gram cache strategy (no draft model).
    #[serde(rename = "ngram-cache")]
    NgramCache,
}

impl SpecType {
    /// Canonical `--spec-type` argument value (inverse of the serde rename).
    pub(crate) fn as_arg(&self) -> &'static str {
        match self {
            Self::DraftSimple => "draft-simple",
            Self::DraftEagle3 => "draft-eagle3",
            Self::DraftMtp => "draft-mtp",
            Self::NgramSimple => "ngram-simple",
            Self::NgramMapK => "ngram-map-k",
            Self::NgramMapK4v => "ngram-map-k4v",
            Self::NgramMod => "ngram-mod",
            Self::NgramCache => "ngram-cache",
        }
    }

    /// `true` for the `draft-*` strategies, which require a separate draft model.
    fn requires_draft_model(&self) -> bool {
        matches!(self, Self::DraftSimple | Self::DraftEagle3 | Self::DraftMtp)
    }
}

/// Configuration for a `gradatum-engine` instance.
///
/// Each instance corresponds to one loaded model (curator chat or embed).
/// Fields are nested under `[engine]` in the TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    /// Path to the GGUF file. Canonicalized and prefix-validated at runtime.
    pub model_path: String,
    /// Model type: `chat` or `embed`.
    pub model_kind: ModelKind,
    /// Inference runtime (default: `llamaserver`).
    #[serde(default)]
    pub runtime: RuntimeKind,
    /// Warm-up strategy. **Accepted and validated, but not yet acted upon**: the engine
    /// always warms up eagerly today, whatever this is set to. Setting `"lazy"` changes
    /// nothing. Kept so existing configuration files keep loading, and so the strategy
    /// can be honoured without a breaking change once implemented.
    // not yet wired (v2.1) — no branch reads this field; `health.rs::warm_up_state` is a
    // runtime state (loading/ready), unrelated to this setting.
    #[serde(default = "default_warmup")]
    pub warm_up: String,
    /// Number of layers to offload to the GPU (0 = CPU only).
    #[serde(default)]
    pub gpu_layers: u32,
    /// Number of CPU threads for inference.
    #[serde(default = "default_threads")]
    pub n_threads: u32,
    /// KV-context size in tokens.
    #[serde(default = "default_ctx")]
    pub context_len: u32,
    /// TCP port the engine server listens on (interface determined by `bind_addr`).
    pub port: u16,

    /// Bind address for the engine server.
    ///
    /// Default `None` = `127.0.0.1` (loopback-only).
    ///
    /// To expose the engine on the LAN, set a specific routable unicast IP for that
    /// interface — never `0.0.0.0` or `::` (wildcards are rejected by `validate()`
    /// with fail-closed semantics).
    ///
    /// Example TOML: `bind_addr = "203.0.113.5"` (use the real LAN IP).
    #[serde(default)]
    pub bind_addr: Option<IpAddr>,

    /// TCP port for the `/metrics` listener (loopback-only, distinct from `port`).
    ///
    /// `/metrics` exposes internal Prometheus metrics and must never be reachable
    /// on the LAN. This port is always bound to `127.0.0.1` regardless of `bind_addr`.
    /// Default: `port + 1`.
    #[serde(default)]
    pub metrics_port: Option<u16>,
    /// Inference timeout in seconds.
    /// On expiry → `EngineError::Timeout` (HTTP 504) → the gateway triggers its fallback.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Intended cap on tokens generated per chat request (default 512).
    ///
    /// **Not enforced.** This value is parsed and then ignored: nothing caps generation
    /// length today. Do not rely on it as a safety limit — to bound generation, pass
    /// `--n-predict` explicitly through `extra_args`, which the child process does honour.
    // not yet wired (v2.1) — never reaches `build_child_args`; wiring it to `--n-predict`
    // would suddenly cap every running engine at the 512 default, so it is a deliberate
    // behavioural change, not a mechanical fix.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Base URL of the gradatum server (for the event-log and JWT exchange).
    ///
    /// - `Some(url)`: the engine builds an `HttpEventSink` and posts events to
    ///   `{url}/api/v1/event-log`. The URL is validated by `validate_loopback_url`
    ///   (anti-SSRF: **loopback only** — a literal IP must satisfy `is_loopback()`, and a
    ///   hostname must resolve to loopback addresses exclusively. A routable unicast
    ///   address is rejected) — this validation is performed at
    ///   **binary startup** (`bin/gradatum-engine.rs`), not by [`EngineConfig::validate`]
    ///   (which validates `model_path` and `bind_addr` only).
    /// - `None`: the engine uses an `InMemorySink` (test/dev sink, no POST).
    ///   Automatically falls back to `NoopEventSink` if the JWT exchange fails
    ///   (best-effort).
    ///
    /// Default: `None` (no event-log). Set `gradatum_url = "http://127.0.0.1:19090"`
    /// explicitly to enable the event-log in production.
    #[serde(default)]
    pub gradatum_url: Option<String>,

    /// Semantic identifier of the emitting agent — propagated in `QaEventDto.agent_id`.
    ///
    /// Each engine declares its role so that cross-role vs. role-specific learning
    /// can be distinguished in the event-log. Conventional values:
    /// `engine-curator`, `engine-embed`, `engine-vision`, `engine-deep`.
    ///
    /// - `Some(id)`: all `RequestServed` events carry this `agent_id`.
    /// - `None` (default): `agent_id` remains `None` in the event (backward-compatible —
    ///   existing configs without this field are unaffected).
    ///
    /// Example TOML: `agent_id = "engine-curator"`.
    #[serde(default)]
    pub agent_id: Option<String>,

    // -------------------------------------------------------------------------
    // llama-server supervisor fields
    // -------------------------------------------------------------------------
    /// Path to the `llama-server` binary.
    ///
    /// Canonicalized and validated against the allowed prefixes
    /// (`/usr/local/bin/`, `/opt/gradatum/bin/`, or `/opt/llama-`) at supervisor
    /// construction (canonicalization prevents TOCTOU).
    #[serde(default = "default_llama_server_bin")]
    pub llama_server_bin: PathBuf,

    /// TCP port the `llama-server` subprocess listens on (loopback, distinct from `port`).
    ///
    /// Must be > 1024. The supervisor listens on `port`; the child listens on `child_port`.
    /// The reqwest proxy routes to `127.0.0.1:child_port`.
    #[serde(default = "default_child_port")]
    pub child_port: u16,

    /// Number of parallel slots (`--parallel`) passed to `llama-server`.
    ///
    /// `llama-server` manages its own concurrency.
    #[serde(default = "default_parallel")]
    pub parallel: u32,

    /// Additional pass-through arguments for `llama-server`.
    ///
    /// Appended as-is after the arguments derived from the config.
    /// Example: `["--flash-attn"]`.
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Path to the multimodal projector (mmproj GGUF) for vision models.
    ///
    /// `None` = no vision. When `Some`, the path is canonicalized and validated under
    /// `/opt/gradatum/models/` by `validate()` (same constraint as `model_path`),
    /// then injected as `--mmproj <path>` in the `llama-server` command.
    ///
    /// **Never via `extra_args`**: `--mmproj` is intentionally excluded from
    /// `ALLOWED_EXTRA_FLAGS` — vision must be configured through this dedicated field.
    #[serde(default)]
    pub mmproj_path: Option<PathBuf>,

    /// Speculative-decoding draft model (GGUF), injected as `--spec-draft-model <path>`.
    ///
    /// `None` = no draft-model speculative decoding. When `Some`, the path is canonicalized
    /// and validated under `/opt/gradatum/models/` by [`EngineConfig::validate`] (same
    /// constraint as `model_path`/`mmproj_path`).
    ///
    /// Coherence with [`Self::spec_type`] is enforced by `validate()`: it must be present
    /// **iff** `spec_type` is a `draft-*` variant.
    ///
    /// **Runtime requirement**: `--spec-draft-model` (and the `draft-*` strategies) require
    /// `llama-server` ≥ b9780. Older builds do **not** support draft-model speculative
    /// decoding — a `draft-*` config against such a build makes the child crash-loop within
    /// the bounded restart budget with no cause visible on the engine side.
    ///
    /// **Never via `extra_args`**: `--spec-draft-model` is intentionally excluded from
    /// `ALLOWED_EXTRA_FLAGS`.
    #[serde(default)]
    pub draft_model_path: Option<String>,

    /// Speculative-decoding strategy, injected as `--spec-type <value>`.
    ///
    /// `None` = no speculative decoding. See [`SpecType`] for the closed set of accepted
    /// values (deserialization rejects anything else). Coherence with
    /// [`Self::draft_model_path`] is enforced by [`EngineConfig::validate`].
    ///
    /// **Runtime requirement**: the `draft-*` variants require `llama-server` ≥ b9780. Older
    /// builds only support the `ngram-*` variants — a `draft-*` config against such a build
    /// makes the child crash-loop within the bounded restart budget with no cause visible on
    /// the engine side.
    ///
    /// **Never via `extra_args`**: `--spec-type` is intentionally excluded from
    /// `ALLOWED_EXTRA_FLAGS`.
    #[serde(default)]
    pub spec_type: Option<SpecType>,

    /// Maximum number of speculative draft tokens, injected as `--spec-draft-n-max <n>`.
    ///
    /// `None` = the `llama-server` default: behaviour is unchanged, bit-for-bit identical
    /// to omitting the flag. When `Some(n)`, tunes how many tokens the speculation
    /// proposes per step.
    ///
    /// **Model-agnostic**: this is a generic speculation knob — no per-model logic. The optimal
    /// value depends on the draft-model acceptance profile and is chosen by the operator via
    /// config, never hard-coded here.
    ///
    /// Coherence with [`Self::spec_type`] is enforced by [`EngineConfig::validate`]: `Some(n)`
    /// requires `spec_type` to be set (a draft-token budget with no speculation strategy would be
    /// dead config), and `n` must be within `1..=16` (generic safety bounds, not a model value).
    ///
    /// **Never via `extra_args`**: `--spec-draft-n-max` is intentionally excluded from
    /// `ALLOWED_EXTRA_FLAGS` — it must go through this dedicated field.
    #[serde(default)]
    pub spec_draft_n_max: Option<u32>,

    /// Minimum draft-token probability for speculative decoding, injected as
    /// `--spec-draft-p-min <p>`.
    ///
    /// `None` = the `llama-server` default: behaviour is unchanged, bit-for-bit identical
    /// to omitting the flag. When `Some(p)`, draft tokens whose greedy probability falls
    /// below `p` are pruned before verification (confidence-based pruning).
    ///
    /// **Model-agnostic**: this is a generic speculation knob — no per-model logic. The optimal
    /// value depends on the draft-model confidence profile and is chosen by the operator via
    /// config, never hard-coded here.
    ///
    /// Coherence with [`Self::spec_type`] is enforced by [`EngineConfig::validate`]: `Some(p)`
    /// requires `spec_type` to be set (a pruning threshold with no speculation strategy would be
    /// dead config), and `p` must be finite and within `0.0..=1.0` (a probability; `NaN` and
    /// out-of-range values are rejected).
    ///
    /// **Never via `extra_args`**: `--spec-draft-p-min` is intentionally excluded from
    /// `ALLOWED_EXTRA_FLAGS` — it must go through this dedicated field.
    #[serde(default)]
    pub spec_draft_p_min: Option<f32>,

    /// `llama-server` startup timeout in seconds.
    ///
    /// The supervisor polls `/health` until this timeout. On expiry → unhealthy.
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,

    /// Maximum **total** restart budget (global budget, not a per-window rate limit).
    ///
    /// Decremented on each crash. Reset to the maximum only if the child was stable for
    /// at least `min_stable_uptime_secs` before crashing (prevents flapping). `0` = no
    /// restart allowed → unhealthy on the first crash.
    /// On exhaustion → `HealthState::Unhealthy` → gateway fallback.
    #[serde(default = "default_restart_max")]
    pub child_restart_max: u32,

    /// Minimum time the child must remain in the `ready` state before a crash is
    /// considered "stable", triggering a reset of the restart budget and backoff.
    ///
    /// If the child crashes sooner after reaching `ready`, the crash is counted as
    /// flapping: the budget is consumed without a reset and the backoff continues to
    /// escalate. Default: 30 s.
    #[serde(default = "default_min_stable_uptime")]
    pub min_stable_uptime_secs: u64,

    /// Maximum request body size on the main port, in bytes.
    ///
    /// Vision images encoded in base64 exceed 1 MiB, so the default is raised to
    /// 32 MiB. Requests exceeding this limit receive `413 Payload Too Large`.
    #[serde(default = "default_body_limit")]
    pub body_limit_bytes: usize,
}

fn default_warmup() -> String {
    "eager".into()
}
fn default_threads() -> u32 {
    8
}
fn default_ctx() -> u32 {
    32_768
}
fn default_timeout() -> u64 {
    120
}
fn default_max_tokens() -> u32 {
    512
}
fn default_llama_server_bin() -> PathBuf {
    PathBuf::from("/usr/local/bin/llama-server")
}
fn default_child_port() -> u16 {
    11436
}
fn default_parallel() -> u32 {
    4
}
fn default_startup_timeout() -> u64 {
    60
}
fn default_restart_max() -> u32 {
    3
}
fn default_min_stable_uptime() -> u64 {
    30
}
fn default_body_limit() -> usize {
    32 * 1024 * 1024 // 32 MiB
}

/// Wrapper for figment/toml deserialization (`[engine]` section).
#[derive(Deserialize)]
struct Wrapper {
    engine: EngineConfig,
}

/// Validates that an IP address is not a wildcard, broadcast, or multicast address
/// (fail-closed).
///
/// ## Policy
///
/// - REJECTED: `0.0.0.0` (IPv4 unspecified), `::` (IPv6 unspecified),
///   `::ffff:0.0.0.0` (IPv4-mapped unspecified — binds all IPv4 interfaces on
///   Linux when `net.ipv6.bindv6only=0`), `255.255.255.255` (IPv4 broadcast),
///   `::ffff:255.255.255.255` (mapped broadcast), any multicast address.
///   Principle: only a specific unicast IP chosen explicitly by the operator.
/// - ALLOWED: loopback (127.x.x.x, `::1`, `::ffff:127.0.0.1`), specific routable
///   unicast (fixed LAN IP configured by the operator), IPv4-mapped unicast
///   (e.g. `::ffff:203.0.113.5`).
///
/// ## IPv4-mapped addresses
///
/// For any V6 address, `to_ipv4_mapped()` is called first. If a V4 is extracted,
/// V4 rules apply (unspecified, broadcast, multicast). This covers
/// `::ffff:0.0.0.0` (mapped unspecified) and `::ffff:255.255.255.255` (mapped broadcast)
/// without an exhaustive deny-list.
///
/// # Errors
/// Returns `anyhow::Error` if the address is forbidden, with an explanatory message.
fn validate_bind_addr(addr: IpAddr) -> Result<(), anyhow::Error> {
    match addr {
        IpAddr::V4(v4) => check_v4_addr(v4, &addr.to_string())?,
        IpAddr::V6(v6) => {
            // Decode the V4-mapped address before any other check (stable since Rust 1.63).
            // ::ffff:X.X.X.X → apply the same rules as IPv4 X.X.X.X.
            if let Some(v4_mapped) = v6.to_ipv4_mapped() {
                check_v4_addr(v4_mapped, &addr.to_string())?;
            } else {
                // Pure V6 (non-mapped)
                if v6.is_unspecified() {
                    anyhow::bail!(
                        "bind_addr '::' is not allowed (C1 fail-closed) — \
                         wildcard IPv6 bind exposes the engine on all interfaces. \
                         Use ::1 (loopback) or a specific LAN unicast IP."
                    );
                }
                if v6.is_multicast() {
                    anyhow::bail!("bind_addr '{addr}' is not allowed — IPv6 multicast address.");
                }
            }
        }
    }
    Ok(())
}

/// Canonicalizes a path and verifies that it is under `/opt/gradatum/models/`.
///
/// Used for `model_path` and `mmproj_path`. The file must exist
/// (canonicalize fails otherwise).
///
/// # Errors
/// Returns `anyhow::Error` if the path is inaccessible or outside the allowed prefix.
fn validate_model_prefix(path: &std::path::Path) -> Result<(), anyhow::Error> {
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize failed for {} : {e}", path.display()))?;
    if !canonical.starts_with("/opt/gradatum/models/") {
        anyhow::bail!(
            "path must be under /opt/gradatum/models/ (P1-6): {}",
            canonical.display()
        );
    }
    Ok(())
}

/// Validates the coherence between `spec_type` and `draft_model_path` (pure, no filesystem).
///
/// Enforces the biconditional "a draft model is present **iff** the strategy is `draft-*`",
/// which rules out silently-ignored (dead) configuration:
///
/// - `spec_type` is `draft-*` but no `draft_model_path` → error (draft strategy needs a model).
/// - `draft_model_path` is set but `spec_type` is `None` → error (strictest choice — no
///   implicit `draft-simple` default).
/// - `draft_model_path` is set but `spec_type` is an `ngram-*` variant → error (the draft
///   model would never be used).
///
/// `ngram-*` without a draft model, and both `None`, are valid.
///
/// # Errors
/// Returns `anyhow::Error` with an explanatory message on any incoherent combination.
fn validate_spec_decoding(
    spec_type: Option<&SpecType>,
    draft_model_path: Option<&str>,
) -> Result<(), anyhow::Error> {
    match (spec_type, draft_model_path.is_some()) {
        (Some(st), false) if st.requires_draft_model() => anyhow::bail!(
            "spec_type '{}' requires draft_model_path to be set (draft-* strategies need a draft model)",
            st.as_arg()
        ),
        (None, true) => anyhow::bail!(
            "draft_model_path is set but spec_type is missing — \
             set spec_type to a draft-* value or remove draft_model_path"
        ),
        (Some(st), true) if !st.requires_draft_model() => anyhow::bail!(
            "draft_model_path is set but spec_type '{}' does not use a draft model — \
             remove draft_model_path or select a draft-* spec_type",
            st.as_arg()
        ),
        _ => Ok(()),
    }
}

/// Validates the speculative draft-token budget `spec_draft_n_max` (pure, no filesystem).
///
/// Generic, model-agnostic bounds — no per-model value is baked in:
///
/// - `Some(n)` while `spec_type` is `None` → error (a draft-token budget with no speculation
///   strategy is silently-ignored dead config).
/// - `n` outside `1..=16` → error (`0` disables speculation implicitly, which must instead be
///   expressed by leaving `spec_type` unset; the upper cap is a generic anti-footgun bound).
///
/// `None` (any `spec_type`) is valid → the `llama-server` default applies unchanged.
///
/// # Errors
/// Returns `anyhow::Error` with an explanatory message on any incoherent combination.
fn validate_spec_draft_n_max(
    spec_type: Option<&SpecType>,
    n_max: Option<u32>,
) -> Result<(), anyhow::Error> {
    let Some(n) = n_max else {
        return Ok(());
    };
    if spec_type.is_none() {
        anyhow::bail!(
            "spec_draft_n_max is set but spec_type is missing — \
             set spec_type to enable speculation or remove spec_draft_n_max"
        );
    }
    if !(1..=16).contains(&n) {
        anyhow::bail!(
            "spec_draft_n_max {n} is out of range (allowed: 1..=16); \
             leave spec_type unset to disable speculation instead of using 0"
        );
    }
    Ok(())
}

/// Validates the speculative draft-token pruning threshold `spec_draft_p_min` (pure, no fs).
///
/// Generic, model-agnostic bounds — no per-model value is baked in:
///
/// - `Some(p)` while `spec_type` is `None` → error (a pruning threshold with no speculation
///   strategy is silently-ignored dead config).
/// - `p` non-finite (`NaN`, `±∞`) or outside `0.0..=1.0` → error (`p` is a probability).
///
/// `None` (any `spec_type`) is valid → the `llama-server` default applies unchanged.
///
/// # Errors
/// Returns `anyhow::Error` with an explanatory message on any incoherent combination.
fn validate_spec_draft_p_min(
    spec_type: Option<&SpecType>,
    p_min: Option<f32>,
) -> Result<(), anyhow::Error> {
    let Some(p) = p_min else {
        return Ok(());
    };
    if spec_type.is_none() {
        anyhow::bail!(
            "spec_draft_p_min is set but spec_type is missing — \
             set spec_type to enable speculation or remove spec_draft_p_min"
        );
    }
    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
        anyhow::bail!(
            "spec_draft_p_min {p} is invalid (must be a finite probability in 0.0..=1.0)"
        );
    }
    Ok(())
}

/// Checks constraints on an IPv4 address (internal helper).
///
/// `display` is the textual representation of the original address (may be an
/// IPv4-mapped notation such as `::ffff:0.0.0.0`) used in the error message.
fn check_v4_addr(v4: std::net::Ipv4Addr, display: &str) -> Result<(), anyhow::Error> {
    if v4.is_unspecified() {
        anyhow::bail!(
            "bind_addr '{display}' is not allowed (C1 fail-closed) — \
             wildcard bind exposes the engine on all interfaces. \
             Use 127.0.0.1 (loopback) or a specific LAN unicast IP."
        );
    }
    if v4.is_broadcast() {
        anyhow::bail!("bind_addr '{display}' is not allowed — broadcast address.");
    }
    if v4.is_multicast() {
        anyhow::bail!("bind_addr '{display}' is not allowed — multicast address.");
    }
    Ok(())
}

impl EngineConfig {
    /// Parses from a raw TOML string — intended for unit tests only.
    ///
    /// # Errors
    /// Returns an error if the TOML is invalid or a required field is missing.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        Ok(toml::from_str::<Wrapper>(s)?.engine)
    }

    /// Validates the config after parsing.
    ///
    /// Must be called after `load_local()` or `from_toml()`. The binary always does this.
    /// Direct consumers of `load_local()` must also call it.
    ///
    /// ## Validations performed
    ///
    /// - `model_path`: canonicalizable (file accessible) and under `/opt/gradatum/models/`.
    /// - `bind_addr`: REJECTED if `0.0.0.0` (IPv4 unspecified), `::` (IPv6 unspecified),
    ///   or any multicast address (fail-closed — wildcard binds are never allowed).
    ///   ALLOWED: loopback (127.x.x.x, `::1`) or a specific routable unicast address.
    ///
    /// # Errors
    /// Returns `anyhow::Error` if the path is invalid, outside the prefix, or if
    /// `bind_addr` is a wildcard.
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        // --- body_limit_bytes: upper anti-DoS cap (256 MiB) ---
        const MAX_BODY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
        if self.body_limit_bytes > MAX_BODY_LIMIT_BYTES {
            anyhow::bail!(
                "body_limit_bytes {} exceeds the cap {} (256 MiB)",
                self.body_limit_bytes,
                MAX_BODY_LIMIT_BYTES
            );
        }

        // --- model_path ---
        validate_model_prefix(std::path::Path::new(&self.model_path))
            .map_err(|e| anyhow::anyhow!("model_path : {e}"))?;

        // --- mmproj_path (same constraint as model_path) ---
        if let Some(mmproj) = &self.mmproj_path {
            validate_model_prefix(mmproj).map_err(|e| anyhow::anyhow!("mmproj_path : {e}"))?;
        }

        // --- speculative decoding: coherence first (cheap, no fs), then path prefix ---
        validate_spec_decoding(self.spec_type.as_ref(), self.draft_model_path.as_deref())?;
        validate_spec_draft_n_max(self.spec_type.as_ref(), self.spec_draft_n_max)?;
        validate_spec_draft_p_min(self.spec_type.as_ref(), self.spec_draft_p_min)?;
        if let Some(draft) = &self.draft_model_path {
            validate_model_prefix(std::path::Path::new(draft))
                .map_err(|e| anyhow::anyhow!("draft_model_path : {e}"))?;
        }

        // --- bind_addr: fail-closed ---
        if let Some(addr) = self.bind_addr {
            validate_bind_addr(addr)?;
        }

        Ok(())
    }

    /// Returns the resolved bind address: `bind_addr` if set, otherwise `127.0.0.1`.
    ///
    /// Guarantees a loopback fallback when `bind_addr` is absent.
    pub fn resolved_bind_addr(&self) -> IpAddr {
        self.bind_addr
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    }

    /// Returns the resolved metrics port: `metrics_port` if set, otherwise `port + 1`.
    ///
    /// The metrics listener is always bound to `127.0.0.1` (loopback-only).
    pub fn resolved_metrics_port(&self) -> u16 {
        self.metrics_port
            .unwrap_or_else(|| self.port.saturating_add(1))
    }

    /// Loads the config from a local TOML file — the single configuration source.
    ///
    /// The process environment contributes nothing: no `GRADATUM_ENGINE_*` variable
    /// overrides any field, by design (see the module documentation). The central
    /// source (`/api/v1/config/:binary`) is a deferred figment provider and is
    /// deliberately not implemented.
    ///
    /// **Security note**: this method only parses and deserializes. It does NOT validate
    /// `model_path`. Call [`EngineConfig::validate()`] afterwards for path and bind-address
    /// guarantees.
    ///
    /// # Errors
    /// Returns `figment::Error` if the file is absent, malformed, or a required field is missing.
    pub fn load_local(path: &std::path::Path) -> Result<Self, Box<figment::Error>> {
        use figment::{
            Figment,
            providers::{Format, Toml},
        };
        let w: Wrapper = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .map_err(Box::new)?;
        Ok(w.engine)
    }

    /// Returns the model alias — derived from the GGUF filename for the event-log.
    ///
    /// Example: `/opt/gradatum/models/qwen3-4b.gguf` → `"qwen3-4b"`.
    pub fn model_alias(&self) -> String {
        std::path::Path::new(&self.model_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("engine")
            .to_string()
    }

    /// Returns the gateway provider alias, derived from `model_kind`.
    ///
    /// Example: `ModelKind::Chat` → `"engine-curator"`, `ModelKind::Embed` → `"engine-embed"`.
    pub fn provider_alias(&self) -> String {
        match self.model_kind {
            ModelKind::Chat => "engine-curator".into(),
            ModelKind::Embed => "engine-embed".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_instance() {
        let toml = r#"
[engine]
model_path  = "/opt/models/qwen3-4b.gguf"
model_kind  = "chat"
warm_up     = "eager"
gpu_layers  = 0
n_threads   = 8
context_len = 32768
port        = 11435
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.model_kind, ModelKind::Chat);
        assert_eq!(c.gpu_layers, 0);
        assert_eq!(c.port, 11435);
        assert_eq!(
            c.runtime,
            RuntimeKind::LlamaServer,
            "défaut runtime = llamaserver (PIVOT v2)"
        );
        assert_eq!(c.timeout_secs, 120, "défaut timeout = 120s");
        assert_eq!(
            c.gradatum_url, None,
            "défaut gradatum_url = None (InMemorySink)"
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"vision\"\nport=1\n";
        assert!(EngineConfig::from_toml(toml).is_err());
    }

    #[test]
    fn parses_onnx_runtime_seam() {
        // Design-only : la config ONNX est parsée (la branche existe), mais le wiring
        // binaire la refusera explicitement (non implémenté).
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"embed\"\nruntime=\"onnx\"\nport=11436\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.runtime, RuntimeKind::Onnx);
    }

    #[test]
    fn parses_llamaserver_runtime() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nruntime=\"llamaserver\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.runtime,
            RuntimeKind::LlamaServer,
            "runtime=llamaserver parsé"
        );
    }

    #[test]
    fn parses_pivot_v2_fields() {
        let toml = r#"
[engine]
model_path       = "/opt/gradatum/models/qwen3-4b.gguf"
model_kind       = "chat"
port             = 11435
child_port       = 11436
parallel         = 4
startup_timeout_secs = 90
child_restart_max    = 5
extra_args       = ["--flash-attn"]
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.child_port, 11436, "child_port parsé");
        assert_eq!(c.parallel, 4, "parallel parsé");
        assert_eq!(c.startup_timeout_secs, 90, "startup_timeout_secs parsé");
        assert_eq!(c.child_restart_max, 5, "child_restart_max parsé");
        assert_eq!(c.extra_args, vec!["--flash-attn"], "extra_args parsé");
    }

    #[test]
    fn defaults_pivot_v2_fields() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.llama_server_bin,
            std::path::PathBuf::from("/usr/local/bin/llama-server"),
            "défaut llama_server_bin"
        );
        assert_eq!(c.child_port, 11436, "défaut child_port");
        assert_eq!(c.parallel, 4, "défaut parallel");
        assert_eq!(c.startup_timeout_secs, 60, "défaut startup_timeout_secs");
        assert_eq!(c.child_restart_max, 3, "défaut child_restart_max");
        assert!(c.extra_args.is_empty(), "défaut extra_args = vide");
        assert_eq!(
            c.min_stable_uptime_secs, 30,
            "défaut min_stable_uptime_secs = 30s"
        );
    }

    #[test]
    fn parses_min_stable_uptime_secs() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nmin_stable_uptime_secs=60\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.min_stable_uptime_secs, 60,
            "min_stable_uptime_secs parsé depuis TOML"
        );
    }

    /// `validate()` rejects a `model_path` outside `/opt/gradatum/models/`.
    ///
    /// `/tmp` is always present on Linux (`canonicalize` succeeds) but falls outside the prefix.
    #[test]
    fn validate_rejects_model_path_outside_prefix() {
        // Écrire un fichier réel dans /tmp pour que canonicalize() réussisse
        let tmp_path = "/tmp/gradatum-engine-test-model.gguf";
        let _ = std::fs::write(tmp_path, b"fake-gguf");

        let toml =
            format!("[engine]\nmodel_path=\"{tmp_path}\"\nmodel_kind=\"chat\"\nport=11435\n");
        let c = EngineConfig::from_toml(&toml).unwrap();
        let result = c.validate();
        // Nettoyage best-effort
        let _ = std::fs::remove_file(tmp_path);
        assert!(
            result.is_err(),
            "validate() doit rejeter un model_path hors /opt/gradatum/models/"
        );
    }

    #[test]
    fn validate_rejects_nonexistent_model_path() {
        let toml = "[engine]\nmodel_path=\"/opt/gradatum/models/does-not-exist.gguf\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = c.validate();
        assert!(
            result.is_err(),
            "validate() doit rejeter un model_path non-existant (canonicalize échoue)"
        );
    }

    // --- bind_addr C1 ---

    #[test]
    fn bind_addr_default_is_none_resolves_to_loopback() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(c.bind_addr.is_none(), "défaut bind_addr = None");
        let resolved = c.resolved_bind_addr();
        assert_eq!(
            resolved,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "bind_addr None → résolu 127.0.0.1"
        );
    }

    #[test]
    fn bind_addr_loopback_explicit_ok() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"127.0.0.1\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.bind_addr, Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
        assert!(
            validate_bind_addr(c.bind_addr.unwrap()).is_ok(),
            "127.0.0.1 doit être accepté"
        );
    }

    #[test]
    fn bind_addr_routable_unicast_accepted() {
        // 203.0.113.5 = TEST-NET-3 (RFC 5737) — IP unicast non-loopback de test
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"203.0.113.5\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(
            validate_bind_addr(c.bind_addr.unwrap()).is_ok(),
            "203.0.113.5 (unicast routable) doit être accepté"
        );
    }

    #[test]
    fn bind_addr_0_0_0_0_rejected() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"0.0.0.0\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), "0.0.0.0 doit être rejeté (C1 fail-closed)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("0.0.0.0") && msg.contains("not allowed"),
            "error message must cite 0.0.0.0 and 'not allowed': {msg}"
        );
    }

    #[test]
    fn bind_addr_ipv6_unspecified_rejected() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"::\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), ":: doit être rejeté (C1 fail-closed)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("::") && msg.contains("not allowed"),
            "error message must cite :: and 'not allowed': {msg}"
        );
    }

    #[test]
    fn bind_addr_multicast_rejected() {
        // 224.0.0.1 = adresse multicast IPv4
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"224.0.0.1\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), "adresse multicast doit être rejetée");
    }

    /// IPv4-mapped unspecified `::ffff:0.0.0.0` must be REJECTED.
    ///
    /// On Linux with `net.ipv6.bindv6only=0` (default), `bind(::ffff:0.0.0.0)` is
    /// equivalent to `bind(0.0.0.0)` — listening on all IPv4 interfaces.
    #[test]
    fn bind_addr_ipv4_mapped_unspecified_rejected() {
        let addr: IpAddr = "::ffff:0.0.0.0".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "::ffff:0.0.0.0 (IPv4-mapped unspecified) doit être rejeté (P0 fail-closed)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not allowed"),
            "error message must contain 'not allowed': {msg}"
        );
    }

    /// IPv4 broadcast `255.255.255.255` must be REJECTED.
    #[test]
    fn bind_addr_broadcast_rejected() {
        let addr: IpAddr = "255.255.255.255".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "255.255.255.255 (broadcast) doit être rejeté"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not allowed"),
            "error message must contain 'not allowed': {msg}"
        );
    }

    /// IPv4-mapped broadcast `::ffff:255.255.255.255` must be REJECTED.
    #[test]
    fn bind_addr_ipv4_mapped_broadcast_rejected() {
        let addr: IpAddr = "::ffff:255.255.255.255".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "::ffff:255.255.255.255 (broadcast mappé) doit être rejeté"
        );
    }

    /// `::ffff:203.0.113.5` (IPv4-mapped unicast, RFC 5737 TEST-NET-3) must be ACCEPTED.
    ///
    /// An IPv4-mapped unicast address is a valid routable IP — an operator may legitimately
    /// configure a bind using mapped notation. The unspecified/broadcast/multicast rules
    /// do not apply to `203.0.113.5`.
    #[test]
    fn bind_addr_ipv4_mapped_unicast_accepted() {
        // ::ffff:203.0.113.5 = TEST-NET-3 (RFC 5737) en notation IPv4-mapped
        let addr: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_ok(),
            "::ffff:203.0.113.5 (unicast mappé) doit être accepté : {result:?}"
        );
    }

    #[test]
    fn metrics_port_default_is_port_plus_one() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.resolved_metrics_port(),
            11436,
            "metrics_port défaut = port + 1"
        );
    }

    #[test]
    fn metrics_port_explicit_config() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nmetrics_port=19091\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.resolved_metrics_port(),
            19091,
            "metrics_port configuré parsé"
        );
    }

    #[test]
    fn parses_timeout_secs() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\ntimeout_secs=60\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.timeout_secs, 60);
    }

    /// `max_tokens` is parsed from TOML with a default of 512.
    #[test]
    fn parses_max_tokens_with_default() {
        let toml_default = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml_default).unwrap();
        assert_eq!(c.max_tokens, 512, "défaut max_tokens = 512");

        let toml_custom =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nmax_tokens=256\n";
        let c2 = EngineConfig::from_toml(toml_custom).unwrap();
        assert_eq!(c2.max_tokens, 256, "max_tokens custom parsé");
    }

    #[test]
    fn parses_gradatum_url() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\ngradatum_url=\"http://127.0.0.1:19090\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.gradatum_url,
            Some("http://127.0.0.1:19090".to_string()),
            "gradatum_url parsé depuis TOML"
        );
    }

    #[test]
    fn gradatum_url_default_is_none() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.gradatum_url, None,
            "défaut gradatum_url = None (InMemorySink — pas d'event-log sans config explicite)"
        );
    }

    #[test]
    fn parses_agent_id() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nagent_id=\"engine-curator\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.agent_id,
            Some("engine-curator".to_string()),
            "agent_id parsé depuis TOML (F-19 M1)"
        );
    }

    #[test]
    fn agent_id_default_is_none() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.agent_id, None,
            "défaut agent_id = None (rétrocompat — configs sans le champ inchangées)"
        );
    }

    #[test]
    fn model_alias_from_path() {
        let toml = "[engine]\nmodel_path=\"/opt/gradatum/models/qwen3-4b.gguf\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.model_alias(), "qwen3-4b");
    }

    #[test]
    fn provider_alias_chat_embed() {
        let toml_chat = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let toml_embed = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"embed\"\nport=1\n";
        assert_eq!(
            EngineConfig::from_toml(toml_chat).unwrap().provider_alias(),
            "engine-curator"
        );
        assert_eq!(
            EngineConfig::from_toml(toml_embed)
                .unwrap()
                .provider_alias(),
            "engine-embed"
        );
    }

    #[test]
    fn parses_mmproj_path() {
        let toml = r#"
[engine]
model_path  = "/opt/gradatum/models/qwen3-35b.gguf"
model_kind  = "chat"
port        = 8080
mmproj_path = "/opt/gradatum/models/mmproj-F16.gguf"
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.mmproj_path,
            Some(std::path::PathBuf::from(
                "/opt/gradatum/models/mmproj-F16.gguf"
            )),
            "mmproj_path parsé depuis le TOML"
        );
    }

    #[test]
    fn mmproj_path_default_is_none() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(c.mmproj_path.is_none(), "défaut mmproj_path = None");
    }

    #[test]
    fn validate_rejects_mmproj_outside_prefix() {
        // model_path valide (sous /opt/gradatum/models/) requis pour atteindre la branche mmproj.
        // On crée les 2 fichiers : le modèle sous le bon préfixe, le mmproj hors préfixe.
        let model = "/tmp/gradatum-engine-mmproj-test-model.gguf";
        let mmproj = "/tmp/gradatum-engine-mmproj-test-proj.gguf";
        let _ = std::fs::write(model, b"fake");
        let _ = std::fs::write(mmproj, b"fake");
        // model_path hors préfixe échouera AVANT mmproj — donc on teste mmproj isolément
        // via un model_path réel sous le préfixe n'est pas garanti en CI. On vérifie plutôt
        // que la fonction de validation mmproj rejette un chemin hors préfixe directement.
        let result = super::validate_model_prefix(std::path::Path::new(mmproj));
        let _ = std::fs::remove_file(model);
        let _ = std::fs::remove_file(mmproj);
        assert!(
            result.is_err(),
            "validate_model_prefix doit rejeter un chemin hors /opt/gradatum/models/"
        );
    }

    // --- speculative decoding (spec_type / draft_model_path) ---

    #[test]
    fn parses_spec_type_and_draft_model_path() {
        let toml = r#"
[engine]
model_path       = "/opt/gradatum/models/qwen3-27b.gguf"
model_kind       = "chat"
port             = 8080
spec_type        = "draft-mtp"
draft_model_path = "/opt/gradatum/models/qwen3-0.6b-draft.gguf"
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.spec_type,
            Some(SpecType::DraftMtp),
            "spec_type draft-mtp parsé"
        );
        assert_eq!(
            c.draft_model_path.as_deref(),
            Some("/opt/gradatum/models/qwen3-0.6b-draft.gguf"),
            "draft_model_path parsé"
        );
    }

    #[test]
    fn spec_fields_default_is_none() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(
            c.spec_type.is_none(),
            "défaut spec_type = None (rétrocompat)"
        );
        assert!(
            c.draft_model_path.is_none(),
            "défaut draft_model_path = None (rétrocompat)"
        );
        assert!(
            c.spec_draft_n_max.is_none(),
            "défaut spec_draft_n_max = None (rétrocompat)"
        );
        assert!(
            c.spec_draft_p_min.is_none(),
            "défaut spec_draft_p_min = None (rétrocompat)"
        );
    }

    #[test]
    fn parses_spec_draft_p_min() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nspec_type=\"draft-mtp\"\ndraft_model_path=\"/opt/gradatum/models/d.gguf\"\nspec_draft_p_min=0.75\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.spec_draft_p_min,
            Some(0.75),
            "spec_draft_p_min=0.75 parsé"
        );
    }

    #[test]
    fn spec_draft_p_min_with_spec_type_ok() {
        // Bornes incluses 0.0 et 1.0 valides quand spec_type est défini.
        for p in [0.0_f32, 0.5, 1.0] {
            assert!(
                validate_spec_draft_p_min(Some(&SpecType::DraftMtp), Some(p)).is_ok(),
                "spec_draft_p_min={p} avec spec_type défini → OK"
            );
        }
        // None (avec ou sans spec_type) est toujours valide.
        assert!(
            validate_spec_draft_p_min(Some(&SpecType::NgramSimple), None).is_ok(),
            "spec_draft_p_min=None → OK (défaut llama-server)"
        );
        assert!(
            validate_spec_draft_p_min(None, None).is_ok(),
            "les deux None → OK"
        );
    }

    #[test]
    fn spec_draft_p_min_without_spec_type_rejected() {
        let result = validate_spec_draft_p_min(None, Some(0.5));
        assert!(
            result.is_err(),
            "spec_draft_p_min sans spec_type doit être rejeté (config morte)"
        );
        assert!(
            result.unwrap_err().to_string().contains("spec_type"),
            "message doit citer spec_type"
        );
    }

    #[test]
    fn spec_draft_p_min_out_of_range_or_nan_rejected() {
        // Hors bornes 0.0..=1.0 + NaN + infini.
        for p in [-0.1_f32, 1.1, f32::NAN, f32::INFINITY] {
            assert!(
                validate_spec_draft_p_min(Some(&SpecType::DraftMtp), Some(p)).is_err(),
                "spec_draft_p_min={p} doit être rejeté (borne/finitude)"
            );
        }
    }

    #[test]
    fn parses_spec_draft_n_max() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nspec_type=\"draft-mtp\"\ndraft_model_path=\"/opt/gradatum/models/d.gguf\"\nspec_draft_n_max=2\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.spec_draft_n_max, Some(2), "spec_draft_n_max=2 parsé");
    }

    #[test]
    fn spec_draft_n_max_with_spec_type_ok() {
        // Bornes incluses 1 et 16 valides quand spec_type est défini.
        for n in [1_u32, 8, 16] {
            assert!(
                validate_spec_draft_n_max(Some(&SpecType::DraftMtp), Some(n)).is_ok(),
                "spec_draft_n_max={n} avec spec_type défini → OK"
            );
        }
        // None (avec ou sans spec_type) est toujours valide.
        assert!(
            validate_spec_draft_n_max(Some(&SpecType::NgramSimple), None).is_ok(),
            "spec_draft_n_max=None → OK (défaut llama-server)"
        );
        assert!(
            validate_spec_draft_n_max(None, None).is_ok(),
            "les deux None → OK"
        );
    }

    #[test]
    fn spec_draft_n_max_without_spec_type_rejected() {
        let result = validate_spec_draft_n_max(None, Some(2));
        assert!(
            result.is_err(),
            "spec_draft_n_max sans spec_type doit être rejeté (config morte)"
        );
        assert!(
            result.unwrap_err().to_string().contains("spec_type"),
            "message doit citer spec_type"
        );
    }

    #[test]
    fn spec_draft_n_max_out_of_range_rejected() {
        // 0 et 17 hors bornes 1..=16.
        assert!(
            validate_spec_draft_n_max(Some(&SpecType::DraftMtp), Some(0)).is_err(),
            "spec_draft_n_max=0 doit être rejeté (borne basse)"
        );
        assert!(
            validate_spec_draft_n_max(Some(&SpecType::DraftMtp), Some(17)).is_err(),
            "spec_draft_n_max=17 doit être rejeté (borne haute)"
        );
    }

    #[test]
    fn spec_type_ngram_variants_parse() {
        for (raw, expected) in [
            ("ngram-simple", SpecType::NgramSimple),
            ("ngram-map-k", SpecType::NgramMapK),
            ("ngram-map-k4v", SpecType::NgramMapK4v),
            ("ngram-mod", SpecType::NgramMod),
            ("ngram-cache", SpecType::NgramCache),
            ("draft-simple", SpecType::DraftSimple),
            ("draft-eagle3", SpecType::DraftEagle3),
        ] {
            let toml = format!(
                "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nspec_type=\"{raw}\"\n"
            );
            let c = EngineConfig::from_toml(&toml).unwrap();
            assert_eq!(c.spec_type, Some(expected), "spec_type '{raw}' parsé");
        }
    }

    #[test]
    fn rejects_unknown_spec_type() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nspec_type=\"draft-bogus\"\n";
        assert!(
            EngineConfig::from_toml(toml).is_err(),
            "spec_type hors ensemble fermé doit être rejeté à la désérialisation"
        );
    }

    #[test]
    fn spec_decoding_coherent_combinations_ok() {
        // Aucun spec.
        assert!(
            validate_spec_decoding(None, None).is_ok(),
            "les deux None → OK"
        );
        // ngram-* sans draft model.
        assert!(
            validate_spec_decoding(Some(&SpecType::NgramSimple), None).is_ok(),
            "ngram-* sans draft model → OK"
        );
        // draft-* AVEC draft model.
        assert!(
            validate_spec_decoding(
                Some(&SpecType::DraftMtp),
                Some("/opt/gradatum/models/d.gguf")
            )
            .is_ok(),
            "draft-* + draft model → OK"
        );
    }

    #[test]
    fn spec_decoding_draft_without_model_rejected() {
        let result = validate_spec_decoding(Some(&SpecType::DraftMtp), None);
        assert!(
            result.is_err(),
            "draft-mtp sans draft_model_path doit être rejeté"
        );
        assert!(
            result.unwrap_err().to_string().contains("draft-mtp"),
            "message doit citer la stratégie"
        );
    }

    #[test]
    fn spec_decoding_model_without_spec_type_rejected() {
        let result = validate_spec_decoding(None, Some("/opt/gradatum/models/d.gguf"));
        assert!(
            result.is_err(),
            "draft_model_path sans spec_type doit être rejeté (choix strict)"
        );
    }

    #[test]
    fn spec_decoding_model_with_ngram_rejected() {
        // draft model fourni avec une stratégie ngram-* = config morte → rejet.
        let result = validate_spec_decoding(
            Some(&SpecType::NgramCache),
            Some("/opt/gradatum/models/d.gguf"),
        );
        assert!(
            result.is_err(),
            "draft_model_path + ngram-* doit être rejeté (draft model jamais utilisé)"
        );
    }

    #[test]
    fn validate_rejects_draft_model_path_outside_prefix() {
        // Même garantie que mmproj : un draft_model_path hors /opt/gradatum/models/ est rejeté.
        // On teste la fonction de validation de préfixe directement (model_path 'x' échouerait avant).
        let draft = "/tmp/gradatum-engine-draft-test.gguf";
        let _ = std::fs::write(draft, b"fake");
        let result = super::validate_model_prefix(std::path::Path::new(draft));
        let _ = std::fs::remove_file(draft);
        assert!(
            result.is_err(),
            "validate_model_prefix doit rejeter un draft_model_path hors /opt/gradatum/models/"
        );
    }

    #[test]
    fn body_limit_bytes_default_is_32_mib() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.body_limit_bytes,
            32 * 1024 * 1024,
            "défaut body_limit_bytes = 32 MiB (images vision base64 dépassent 1 MiB)"
        );
    }

    #[test]
    fn parses_body_limit_bytes() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=8388608\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.body_limit_bytes, 8_388_608,
            "body_limit_bytes custom = 8 MiB parsé"
        );
    }

    #[test]
    fn validate_rejects_body_limit_over_cap() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=268435457\n"; // 256 MiB + 1
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = c.validate();
        assert!(
            result.is_err(),
            "body_limit_bytes > 256 MiB doit être rejeté"
        );
        assert!(
            result.unwrap_err().to_string().contains("256 MiB"),
            "message doit citer le plafond"
        );
    }

    #[test]
    fn validate_accepts_body_limit_at_cap_boundary() {
        // 256 MiB pile = accepté côté body_limit (échouera ensuite sur model_path 'x' inexistant — c'est attendu,
        // on vérifie juste que le message d'erreur n'est PAS celui du cap).
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=268435456\n"; // 256 MiB
        let c = EngineConfig::from_toml(toml).unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(
            !err.contains("256 MiB"),
            "256 MiB pile ne doit PAS être rejeté par le cap (erreur = model_path)"
        );
    }
}
