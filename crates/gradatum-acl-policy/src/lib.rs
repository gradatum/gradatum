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
use gradatum_core::scope::AgentId;
use gradatum_core::trust::TrustContext;
use serde::Deserialize;

/// Crate version, sourced from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Types publics ────────────────────────────────────────────────────────────

/// ACL operation being evaluated.
///
/// `#[non_exhaustive]` (A3, gel API v1.0.0): new operations are expected in C1-C3a.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AclOp {
    /// Read access to a locus.
    Read,
    /// Write access to a locus.
    Write,
}

/// Result of an ACL evaluation.
///
/// `#[non_exhaustive]` (A3, gel API v1.0.0): fail-closed consumers must treat any
/// unknown future decision as a denial (`_` arm ⇒ deny).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum AclError {
    /// Invalid TOML preset.
    #[error("invalid TOML preset: {0}")]
    Toml(#[from] toml::de::Error),
    /// Invalid glob pattern.
    #[error("invalid glob pattern: {0}")]
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

    /// Returns `true` if `agent` is declared as a consumer in the **loaded** preset.
    ///
    /// This is the referential-integrity probe behind the `api-key create` guard and the
    /// boot reconciliation: `api_keys.owner` and `consumer.identity` are joined by nothing
    /// but string equality, so a key minted for an undeclared identity authenticates and is
    /// then denied everywhere by [`AclEngine::evaluate`] step 2 — indistinguishable from an
    /// outage. Asking the question *before* the key exists turns that into a refusal at the
    /// point where the operator can still fix the typo.
    ///
    /// **"Loaded" is the operative word**: the answer reflects what [`AclEngine::evaluate`]
    /// will actually match against, not what the preset file happens to contain. An identity
    /// declared only in a `[[consumer-template]]` block (which [`AclPreset`] does not read)
    /// is *not* known here — and would not be granted anything at runtime either. The two
    /// answers agree by construction, which is the only property that makes this guard
    /// meaningful.
    ///
    /// The relation is checked in one direction only — *does this key point at a declared
    /// identity?* A declared identity holding no key is a normal state (a consumer whose
    /// credential has not been minted yet, or was deliberately revoked) and is never
    /// reported.
    #[must_use]
    pub fn has_identity(&self, agent: &AgentId) -> bool {
        self.consumers.iter().any(|c| c.identity == agent.as_str())
    }

    /// Evaluates access for a trust context, an operation, and a locus.
    ///
    /// # Evaluation order (descending priority)
    ///
    /// 1. `Unauthenticated` → `DenyImplicit`.
    /// 2. Unknown identity → `DenyImplicit`.
    /// 3. Tenant isolation (P1 #3) → `DenyExplicit` if the locus prefix
    ///    belongs to a different tenant than the token's `tenant_id`.
    ///    The `tenant_vault_grants` system (higher level) can override this.
    /// 4. Personal-classified short-circuit → `DenyExplicit`.
    /// 5. Deny pattern (negation `!`) matched → `DenyExplicit`.
    /// 6. Allow pattern matched → `Allow`.
    /// 7. Otherwise → `DenyImplicit`.
    #[must_use]
    pub fn evaluate(&self, trust: &TrustContext, op: AclOp, locus: &str) -> AclDecision {
        // Step 1: Unauthenticated → immediate denial.
        let (identity, token_tenant) = match trust {
            TrustContext::Unauthenticated => return AclDecision::DenyImplicit,
            TrustContext::BearerToken { sub, tenant_id, .. } => {
                (sub.as_str(), Some(tenant_id.as_str()))
            }
            TrustContext::Studio { user, .. } => (user.as_str(), None),
            TrustContext::Mtls { cn, .. } => (cn.as_str(), None),
            // TrustContext est #[non_exhaustive] (A3) : toute variante future est
            // refusée par défaut (fail-closed) tant qu'elle n'est pas câblée ici.
            _ => return AclDecision::DenyImplicit,
        };

        // Step 2: unknown consumer → implicit denial (default deny).
        let Some(c) = self.consumers.iter().find(|c| c.identity == identity) else {
            return AclDecision::DenyImplicit;
        };

        // Step 3: tenant isolation (P1 #3) — défense en profondeur.
        //
        // Filet de sécurité : si le token porte un tenant_id et que le locus est
        // un **tenant root** (un seul segment, sans slash — ex: "main", "tenant-c"),
        // on refuse si le segment ne correspond pas au tenant du token.
        //
        // Un locus multi-segments (ex: "project-a/backend", "tenant-b/decisions/x")
        // n'est PAS un tenant root — c'est un locus de vault/projet, dont le premier
        // segment est un vault_id et non un identifiant de tenant. La comparaison
        // directe avec le tenant_id du token serait incorrecte (un vault "project-a"
        // peut exister sous le tenant "main"). L'autorisation de ces loci est gérée
        // par les patterns glob (étapes 5-6) et le système `tenant_vault_grants`
        // (niveau supérieur, autorité).
        //
        // Le système `tenant_vault_grants` peut accorder des exceptions explicites
        // pour les loci de tenant root également — cette garde n'est qu'un filet
        // supplémentaire, pas un remplacement.
        if let Some(tid) = token_tenant
            && !locus.contains('/')
            && !locus.is_empty()
            && locus != tid
        {
            return AclDecision::DenyExplicit;
        }

        // Step 4: personal-classified short-circuit.
        if !c.sees_personal_classified && locus.contains("personal-classified") {
            return AclDecision::DenyExplicit;
        }

        let (allow, deny) = match op {
            AclOp::Read => (&c.read_allow, &c.read_deny),
            AclOp::Write => (&c.write_allow, &c.write_deny),
        };

        // Step 5: deny-wins — negation takes priority over allow.
        if deny.is_match(locus) {
            return AclDecision::DenyExplicit;
        }

        // Step 6: explicit allow.
        if allow.is_match(locus) {
            return AclDecision::Allow;
        }

        // Step 7: implicit default — no pattern matched.
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

    /// Preset de référence : deux identités déclarées, dont une sans clé au monde réel.
    const PRESET: &str = r#"
[[consumer]]
identity = "engine"
read_patterns = ["main/**"]
write_patterns = []

[[consumer]]
identity = "validator"
read_patterns = ["main/**"]
write_patterns = []
"#;

    /// `has_identity` répond `true` sur une identité déclarée, `false` sinon.
    #[test]
    fn has_identity_distingue_declaree_et_inconnue() {
        let engine = AclEngine::from_preset_str(PRESET).expect("preset valide");
        assert!(engine.has_identity(&AgentId::new("engine")));
        assert!(!engine.has_identity(&AgentId::new("gemini-agent")));
    }

    /// Une identité déclarée SANS clé reste connue — la relation n'est vérifiée
    /// que dans le sens clé → identité.
    ///
    /// Discriminant : `validator` est le cas mesuré sur le parc (4 identités de
    /// `bearer.toml` sans clé active). Une implémentation qui aurait joint les deux
    /// sens répondrait `false` ici et transformerait un état nominal en anomalie.
    #[test]
    fn has_identity_ne_penalise_pas_une_identite_sans_cle() {
        let engine = AclEngine::from_preset_str(PRESET).expect("preset valide");
        assert!(
            engine.has_identity(&AgentId::new("validator")),
            "une identité sans clé émise reste une identité déclarée"
        );
    }

    /// Le préfixe/suffixe n'est PAS un match — l'égalité est exacte, comme dans `evaluate`.
    ///
    /// Discriminant : c'est la propriété qui aligne la garde sur le refus runtime.
    /// Un `contains`/`starts_with` rendrait la garde plus permissive que l'ACL, donc
    /// menteuse — elle laisserait passer la clé que le serveur refusera.
    #[test]
    fn has_identity_exige_une_egalite_exacte() {
        let engine = AclEngine::from_preset_str(PRESET).expect("preset valide");
        for near_miss in ["engin", "engine2", "engine-", "-engine", "Engine"] {
            assert!(
                !engine.has_identity(&AgentId::new(near_miss)),
                "{near_miss:?} n'est pas `engine` — l'égalité doit être exacte"
            );
        }
    }

    /// Un preset vide (fallback DENY-ALL du serveur) ne connaît aucune identité.
    #[test]
    fn has_identity_est_faux_sur_preset_vide() {
        let engine = AclEngine::from_preset_str("").expect("preset vide valide");
        assert!(!engine.has_identity(&AgentId::new("engine")));
    }

    /// Une identité déclarée seulement en `[[consumer-template]]` n'est PAS connue.
    ///
    /// Discriminant : `AclPreset` ne désérialise que `consumer`. La garde doit refléter
    /// ce que `evaluate` matche réellement, pas le contenu brut du fichier — sinon elle
    /// autoriserait une clé que le serveur refuse ensuite en silence.
    #[test]
    fn has_identity_ignore_les_consumer_template() {
        let preset = r#"
[[consumer-template]]
identity = "templated"
read_patterns = ["main/**"]
write_patterns = []
"#;
        let engine = AclEngine::from_preset_str(preset).expect("preset valide");
        assert!(!engine.has_identity(&AgentId::new("templated")));
        // Témoin : le runtime refuserait bien cette identité (même verdict, deux chemins).
        let trust = TrustContext::BearerToken {
            kid: "k1".to_owned(),
            aud: "gradatum".to_owned(),
            sub: AgentId::new("templated"),
            scopes: vec!["vault_read".to_owned()],
            tenant_id: gradatum_core::scope::TenantId::new("main"),
            jti: None,
        };
        assert_eq!(
            engine.evaluate(&trust, AclOp::Read, "main/decisions"),
            AclDecision::DenyImplicit
        );
    }
}
