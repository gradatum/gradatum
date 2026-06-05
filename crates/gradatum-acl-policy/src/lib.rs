//! ACL policy engine — deny-wins, chargement depuis preset TOML.
//!
//! Caveats absorbés :
//! - **B2** : default deny — toute requête sans match explicite retourne [`AclDecision::DenyImplicit`].
//! - **B3** : personal-classified court-circuit — si `sees_personal_classified == false`
//!   ET le locus contient la chaîne `"personal-classified"`, la décision est
//!   [`AclDecision::DenyExplicit`] avant même l'évaluation des patterns.
//!
//! ## Logique d'évaluation (par ordre de priorité décroissante)
//!
//! 1. `TrustContext::Unauthenticated` → [`AclDecision::DenyImplicit`] immédiat.
//! 2. Identité inconnue (aucun consumer matching) → [`AclDecision::DenyImplicit`].
//! 3. B3 : personal-classified bypass → [`AclDecision::DenyExplicit`].
//! 4. Pattern de négation (`!glob`) match → [`AclDecision::DenyExplicit`] (**deny-wins**).
//! 5. Pattern d'allow match → [`AclDecision::Allow`].
//! 6. Sinon → [`AclDecision::DenyImplicit`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use globset::{Glob, GlobSet, GlobSetBuilder};
use gradatum_core::trust::TrustContext;
use serde::Deserialize;

/// Crate version (depuis `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Types publics ────────────────────────────────────────────────────────────

/// Opération ACL évaluée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclOp {
    /// Lecture d'un locus.
    Read,
    /// Écriture dans un locus.
    Write,
}

/// Résultat d'une évaluation ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclDecision {
    /// Accès accordé.
    Allow,
    /// Refus explicite — pattern de négation matché ou court-circuit B3.
    DenyExplicit,
    /// Refus implicite — aucun pattern allow ne matche (default deny B2).
    DenyImplicit,
}

/// Erreurs de chargement d'un preset ACL.
#[derive(Debug, thiserror::Error)]
pub enum AclError {
    /// Preset TOML invalide.
    #[error("preset TOML invalide : {0}")]
    Toml(#[from] toml::de::Error),
    /// Pattern glob invalide.
    #[error("pattern glob invalide : {0}")]
    Glob(#[from] globset::Error),
}

// ── Sérialisation TOML ───────────────────────────────────────────────────────

/// Structure de désérialisation d'un preset TOML.
///
/// La clé de table attendue est `[[consumer]]`.
#[derive(Debug, Deserialize)]
pub struct AclPreset {
    /// Liste des consumers concrets définis dans le preset.
    #[serde(default)]
    pub consumer: Vec<ConsumerEntry>,
}

/// Entrée consumer dans le preset TOML.
///
/// Champ `identity` (alias `id` pour rétro-compatibilité avec le format Phase 0) :
/// correspond à `TrustContext::BearerToken.sub`,
/// `TrustContext::Studio.user`, ou `TrustContext::Mtls.cn`.
#[derive(Debug, Deserialize, Clone)]
pub struct ConsumerEntry {
    /// Identité du consumer — doit être unique dans le preset.
    /// Alias `id` accepté pour la rétro-compatibilité Phase 0.
    #[serde(alias = "id")]
    pub identity: String,
    /// Patterns glob de lecture. Un préfixe `!` indique une négation (deny-wins).
    pub read_patterns: Vec<String>,
    /// Patterns glob d'écriture. Un préfixe `!` indique une négation (deny-wins).
    pub write_patterns: Vec<String>,
    /// Si `false` (défaut), tout accès à un locus contenant `"personal-classified"`
    /// est refusé explicitement (caveat B3).
    #[serde(default)]
    pub sees_personal_classified: bool,
}

// ── Moteur ACL ───────────────────────────────────────────────────────────────

/// Moteur d'évaluation ACL compilé.
///
/// Charger via [`AclEngine::from_preset_str`], puis appeler [`AclEngine::evaluate`]
/// pour chaque requête.
pub struct AclEngine {
    consumers: Vec<CompiledConsumer>,
}

/// Représentation interne compilée d'un consumer (GlobSets préconstruits).
struct CompiledConsumer {
    identity: String,
    read_allow: GlobSet,
    read_deny: GlobSet,
    write_allow: GlobSet,
    write_deny: GlobSet,
    sees_personal_classified: bool,
}

impl AclEngine {
    /// Charge et compile un preset ACL depuis une chaîne TOML.
    ///
    /// # Erreurs
    ///
    /// Retourne [`AclError::Toml`] si le TOML est invalide,
    /// [`AclError::Glob`] si un pattern glob ne peut pas être compilé.
    pub fn from_preset_str(toml_str: &str) -> Result<Self, AclError> {
        let preset: AclPreset = toml::from_str(toml_str)?;
        let consumers = preset
            .consumer
            .into_iter()
            .map(compile_consumer)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { consumers })
    }

    /// Évalue l'accès pour un contexte de confiance, une opération et un locus.
    ///
    /// # Ordre d'évaluation (priorité décroissante)
    ///
    /// 1. `Unauthenticated` → `DenyImplicit`.
    /// 2. Identité inconnue → `DenyImplicit`.
    /// 3. B3 : personal-classified non autorisé → `DenyExplicit`.
    /// 4. Pattern deny (négation `!`) matché → `DenyExplicit`.
    /// 5. Pattern allow matché → `Allow`.
    /// 6. Sinon → `DenyImplicit`.
    #[must_use]
    pub fn evaluate(&self, trust: &TrustContext, op: AclOp, locus: &str) -> AclDecision {
        // Étape 1 : Unauthenticated → refus immédiat.
        let identity = match trust {
            TrustContext::Unauthenticated => return AclDecision::DenyImplicit,
            TrustContext::BearerToken { sub, .. } => sub.as_str(),
            TrustContext::Studio { user, .. } => user.as_str(),
            TrustContext::Mtls { cn, .. } => cn.as_str(),
        };

        // Étape 2 : consumer inconnu → refus implicite (default deny B2).
        let Some(c) = self.consumers.iter().find(|c| c.identity == identity) else {
            return AclDecision::DenyImplicit;
        };

        // Étape 3 : court-circuit B3 personal-classified.
        if !c.sees_personal_classified && locus.contains("personal-classified") {
            return AclDecision::DenyExplicit;
        }

        let (allow, deny) = match op {
            AclOp::Read => (&c.read_allow, &c.read_deny),
            AclOp::Write => (&c.write_allow, &c.write_deny),
        };

        // Étape 4 : deny-wins — la négation prime sur l'allow (caveat B2).
        if deny.is_match(locus) {
            return AclDecision::DenyExplicit;
        }

        // Étape 5 : allow explicite.
        if allow.is_match(locus) {
            return AclDecision::Allow;
        }

        // Étape 6 : défaut implicite — aucun pattern ne matche (B2).
        AclDecision::DenyImplicit
    }
}

// ── Helpers internes ─────────────────────────────────────────────────────────

/// Compile un [`ConsumerEntry`] en [`CompiledConsumer`] avec GlobSets préconstruits.
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

/// Scinde une liste de patterns en deux GlobSets : allow (sans préfixe) et deny (préfixe `!`).
///
/// Le préfixe `!` est le marqueur de négation (deny-wins).
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
