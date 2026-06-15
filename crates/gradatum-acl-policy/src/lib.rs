//! ACL policy engine — deny-wins, loaded from a TOML preset.
//!
//! Evaluation invariants:
//! - **Default deny**: any request with no explicit match returns [`AclDecision::DenyImplicit`].
//! - **Personal-classified short-circuit**: when `sees_personal_classified == false`
//!   and the locus contains the string `"personal-classified"`, the decision is
//!   [`AclDecision::DenyExplicit`] before pattern evaluation begins.
//!
//! ## Evaluation logic (descending priority order)
//!
//! 1. `TrustContext::Unauthenticated` → [`AclDecision::DenyImplicit`] immediately.
//! 2. Unknown identity (no matching consumer) → [`AclDecision::DenyImplicit`].
//! 3. Personal-classified short-circuit → [`AclDecision::DenyExplicit`].
//! 4. Negation pattern (`!glob`) matches → [`AclDecision::DenyExplicit`] (**deny-wins**).
//! 5. Allow pattern matches → [`AclDecision::Allow`].
//! 6. Otherwise → [`AclDecision::DenyImplicit`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use globset::{Glob, GlobSet, GlobSetBuilder};
use gradatum_core::trust::TrustContext;
use serde::Deserialize;

/// Crate version, sourced from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Types publics ────────────────────────────────────────────────────────────

/// ACL operation being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclOp {
    /// Read access to a locus.
    Read,
    /// Write access to a locus.
    Write,
}

/// Result of an ACL evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclDecision {
    /// Access granted.
    Allow,
    /// Explicit denial — a negation pattern matched, or the personal-classified short-circuit fired.
    DenyExplicit,
    /// Implicit denial — no allow pattern matched (default deny).
    DenyImplicit,
}

/// Errors that can occur while loading an ACL preset.
#[derive(Debug, thiserror::Error)]
pub enum AclError {
    /// Invalid TOML preset.
    #[error("preset TOML invalide : {0}")]
    Toml(#[from] toml::de::Error),
    /// Invalid glob pattern.
    #[error("pattern glob invalide : {0}")]
    Glob(#[from] globset::Error),
}

// ── Sérialisation TOML ───────────────────────────────────────────────────────

/// Deserialization structure for a TOML preset.
///
/// Expected table key: `[[consumer]]`.
#[derive(Debug, Deserialize)]
pub struct AclPreset {
    /// List of concrete consumers defined in the preset.
    #[serde(default)]
    pub consumer: Vec<ConsumerEntry>,
}

/// Consumer entry in the TOML preset.
///
/// The `identity` field (alias `id` accepted for backwards compatibility)
/// must match `TrustContext::BearerToken.sub`,
/// `TrustContext::Studio.user`, or `TrustContext::Mtls.cn`.
#[derive(Debug, Deserialize, Clone)]
pub struct ConsumerEntry {
    /// Consumer identity — must be unique within the preset.
    /// Alias `id` accepted for backwards compatibility.
    #[serde(alias = "id")]
    pub identity: String,
    /// Read glob patterns. A `!` prefix marks a negation (deny-wins).
    pub read_patterns: Vec<String>,
    /// Write glob patterns. A `!` prefix marks a negation (deny-wins).
    pub write_patterns: Vec<String>,
    /// When `false` (default), any access to a locus containing `"personal-classified"`
    /// is explicitly denied.
    #[serde(default)]
    pub sees_personal_classified: bool,
}

// ── Moteur ACL ───────────────────────────────────────────────────────────────

/// Compiled ACL evaluation engine.
///
/// Load via [`AclEngine::from_preset_str`], then call [`AclEngine::evaluate`]
/// for each request.
pub struct AclEngine {
    consumers: Vec<CompiledConsumer>,
}

/// Internal compiled representation of a consumer with pre-built `GlobSet`s.
struct CompiledConsumer {
    identity: String,
    read_allow: GlobSet,
    read_deny: GlobSet,
    write_allow: GlobSet,
    write_deny: GlobSet,
    sees_personal_classified: bool,
}

impl AclEngine {
    /// Loads and compiles an ACL preset from a TOML string.
    ///
    /// # Errors
    ///
    /// Returns [`AclError::Toml`] if the TOML is invalid,
    /// or [`AclError::Glob`] if a glob pattern cannot be compiled.
    pub fn from_preset_str(toml_str: &str) -> Result<Self, AclError> {
        let preset: AclPreset = toml::from_str(toml_str)?;
        let consumers = preset
            .consumer
            .into_iter()
            .map(compile_consumer)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { consumers })
    }

    /// Evaluates access for a trust context, an operation, and a locus.
    ///
    /// # Evaluation order (descending priority)
    ///
    /// 1. `Unauthenticated` → `DenyImplicit`.
    /// 2. Unknown identity → `DenyImplicit`.
    /// 3. Personal-classified short-circuit → `DenyExplicit`.
    /// 4. Deny pattern (negation `!`) matched → `DenyExplicit`.
    /// 5. Allow pattern matched → `Allow`.
    /// 6. Otherwise → `DenyImplicit`.
    #[must_use]
    pub fn evaluate(&self, trust: &TrustContext, op: AclOp, locus: &str) -> AclDecision {
        // Step 1: Unauthenticated → immediate denial.
        let identity = match trust {
            TrustContext::Unauthenticated => return AclDecision::DenyImplicit,
            TrustContext::BearerToken { sub, .. } => sub.as_str(),
            TrustContext::Studio { user, .. } => user.as_str(),
            TrustContext::Mtls { cn, .. } => cn.as_str(),
        };

        // Step 2: unknown consumer → implicit denial (default deny).
        let Some(c) = self.consumers.iter().find(|c| c.identity == identity) else {
            return AclDecision::DenyImplicit;
        };

        // Step 3: personal-classified short-circuit.
        if !c.sees_personal_classified && locus.contains("personal-classified") {
            return AclDecision::DenyExplicit;
        }

        let (allow, deny) = match op {
            AclOp::Read => (&c.read_allow, &c.read_deny),
            AclOp::Write => (&c.write_allow, &c.write_deny),
        };

        // Step 4: deny-wins — negation takes priority over allow.
        if deny.is_match(locus) {
            return AclDecision::DenyExplicit;
        }

        // Step 5: explicit allow.
        if allow.is_match(locus) {
            return AclDecision::Allow;
        }

        // Step 6: implicit default — no pattern matched.
        AclDecision::DenyImplicit
    }
}

// ── Helpers internes ─────────────────────────────────────────────────────────

/// Compiles a [`ConsumerEntry`] into a [`CompiledConsumer`] with pre-built `GlobSet`s.
fn compile_consumer(c: ConsumerEntry) -> Result<CompiledConsumer, AclError> {
    let (read_allow, read_deny) = split_patterns(&c.read_patterns)?;
    let (write_allow, write_deny) = split_patterns(&c.write_patterns)?;
    Ok(CompiledConsumer {
        identity: c.identity,
        read_allow,
        read_deny,
        write_allow,
        write_deny,
        sees_personal_classified: c.sees_personal_classified,
    })
}

/// Splits a list of patterns into two `GlobSet`s: allow (no prefix) and deny (prefix `!`).
///
/// The `!` prefix is the negation marker (deny-wins).
fn split_patterns(patterns: &[String]) -> Result<(GlobSet, GlobSet), AclError> {
    let mut allow_b = GlobSetBuilder::new();
    let mut deny_b = GlobSetBuilder::new();
    for p in patterns {
        if let Some(rest) = p.strip_prefix('!') {
            deny_b.add(Glob::new(rest)?);
        } else {
            allow_b.add(Glob::new(p)?);
        }
    }
    Ok((allow_b.build()?, deny_b.build()?))
}

// ── Tests unitaires internes ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_est_definie() {
        assert!(!VERSION.is_empty());
    }
}
