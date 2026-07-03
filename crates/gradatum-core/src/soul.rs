//! Structural validator for soul notes (vault section `identity`, since v0.7.3).
//!
//! Module `soul` — distinct from the core `identity` module (`NoteId`/`ContentHash`).
//! Deterministic, LLM-free. Bypasses the category gatekeeper on the server side.

use thiserror::Error;

const REQUIRED_SECTIONS: [&str; 3] = ["INVARIANTS", "GATES", "NARRATIVE"];

/// Reasons a soul note fails the structural schema (`INVARIANTS`/`GATES`/`NARRATIVE`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SoulSchemaError {
    /// A required `## SECTION` header is absent.
    #[error("soul: section ## {0} manquante")]
    MissingSection(&'static str),
    /// A required section is present but empty.
    #[error("soul: section ## {0} vide")]
    EmptySection(&'static str),
    /// The `INVARIANTS` section lacks the mandatory `INV-CANARY` line.
    #[error("soul: INV-CANARY absent de ## INVARIANTS")]
    MissingCanaryInvariant,
    /// A forbidden dynamic field was found in the body (breaks byte-stability).
    #[error("soul: champ dynamique interdit dans le body: {0}")]
    DynamicFieldInBody(String),
}

/// A parsed soul note: its three mandatory sections, trimmed.
pub struct SoulDoc {
    /// `INVARIANTS` body — machine-checkable predicates.
    pub invariants: String,
    /// `GATES` body — declarative, non-blocking orientations.
    pub gates: String,
    /// `NARRATIVE` body — persona/tone, LLM-facing.
    pub narrative: String,
}

/// Extracts the body of a `## NAME` section, up to the next `## ` line or EOF.
fn extract_section(body: &str, name: &str) -> Option<String> {
    let header = format!("## {name}");
    let mut out: Vec<&str> = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        if line.trim_start().starts_with("## ") {
            if in_section {
                break; // next section reached
            }
            in_section = line.trim() == header;
            continue;
        }
        if in_section {
            out.push(line);
        }
    }
    if in_section || !out.is_empty() {
        Some(out.join("\n").trim().to_string())
    } else {
        None
    }
}

/// Returns the first forbidden dynamic-field line, if any (C8 byte-stability guard).
fn has_dynamic_field(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("updated_at:") || l.starts_with("version:"))
        .map(str::to_string)
}

/// Detects the `extends:` directive on the first non-empty line after the H1 heading.
///
/// The directive `extends: identity/main` marks a child soul that inherits from a parent.
/// Detection is **bounded**: only the **first non-empty line** within a window of ≤5 lines
/// after the H1 is tested, which prevents false positives on `extends:` appearing in
/// prose inside the NARRATIVE section or elsewhere in the body.
///
/// # Rust vs shell divergence (TOML frontmatter)
///
/// This function reads the **raw markdown** as stored in the vault (note body).
/// The vault **strips TOML frontmatter** before persisting to `body_text`
/// (see `Vault::write_note_inner` → `strip_frontmatter`), so `soul_extends` never
/// encounters a `---` / `+++` block — it operates on pure markdown.
///
/// Shell scripts that call `vault_read` via MCP may receive soul notes serialised with
/// TOML frontmatter if the serialisation path includes it. Those scripts are
/// **tolerant** and ignore unrecognised frontmatter lines. The Rust `soul_extends` is
/// **intolerant** to pre-H1 frontmatter: a non-empty, non-H1 line before the H1
/// returns `None` (see the pre-condition: `if !line.trim().is_empty() { return None }`).
///
/// This divergence is **not triggered in practice** because the vault guarantees that
/// `body_text` contains pure markdown without TOML frontmatter. It is documented here
/// for future evolutions (alternative serialisation, testing a corpus without prior
/// stripping).
///
/// # Exemples
///
/// ```rust
/// # use gradatum_core::soul::soul_extends;
/// let body = "# identity/backend\nextends: identity/main\n\n## NARRATIVE\nTu es Backend.\n";
/// assert_eq!(soul_extends(body), Some("identity/main".to_string()));
///
/// // extends: en NARRATIVE → ignoré (pas en 1ère ligne non-vide post-H1)
/// let prose = "# identity/x\n## INVARIANTS\nINV-CANARY | x\n## NARRATIVE\nextends: notre approche.\n";
/// assert_eq!(soul_extends(prose), None);
/// ```
///
/// # Returns
/// - `Some(parent)` — the directive value (e.g. `"identity/main"`).
/// - `None` — directive absent, body has no H1, or the ≤5-line window was exhausted.
pub fn soul_extends(body: &str) -> Option<String> {
    let mut lines = body.lines();
    // Étape 1 : sauter le H1 (`# ...`) et toute ligne vide précédant le H1.
    // Si une ligne non-vide non-H1 est rencontrée avant le H1 → pas de H1 canonique → None.
    for line in lines.by_ref() {
        if line.starts_with("# ") {
            break; // H1 consommé
        }
        if !line.trim().is_empty() {
            // Première ligne non-vide hors H1 avant le H1 → corps atypique.
            return None;
        }
        // Ligne vide avant le H1 → continuer à chercher le H1.
    }
    // Étape 2 : dans une fenêtre de ≤5 lignes post-H1, tester la 1ère ligne non-vide.
    for line in lines.take(5) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // sauter les séparateurs vides entre H1 et extends
        }
        // 1ère ligne non-vide trouvée : doit être `extends:` ou la détection s'arrête.
        return if let Some(rest) = trimmed.strip_prefix("extends:") {
            let parent = rest.trim();
            if parent.is_empty() {
                None
            } else {
                Some(parent.to_string())
            }
        } else {
            None // 1ère ligne non-vide ≠ extends: → stop (bornage P1-2)
        };
    }
    None // fenêtre ≤5 épuisée sans extends:
}

/// Parses and validates a soul note body against the structural schema.
///
/// # Behaviour under the `extends:` directive
///
/// - **Root soul** (no `extends:`): `INVARIANTS` + `GATES` + `NARRATIVE` are required;
///   `INV-CANARY` is mandatory inside `INVARIANTS`.
/// - **Child soul** (with `extends:` as the first non-empty line after the H1):
///   only `NARRATIVE` is required; `INVARIANTS`/`GATES` are optional (inherited from
///   the parent); `INV-CANARY` is not required for the child.
///
/// In both cases, the dynamic-field guard (byte-stability) is always checked.
/// The `scope:` field on an `INVARIANTS` line is accepted and ignored (byte-stable,
/// statically versioned in the body).
///
/// # Errors
/// Returns [`SoulSchemaError`] when a required section is missing/empty, the canary
/// invariant is absent (root soul only), or a dynamic field would break byte-stability.
pub fn parse_soul(body: &str) -> Result<SoulDoc, SoulSchemaError> {
    // Byte-stability : interdit dans tous les cas (enfant ou racine).
    // Vérifié en premier pour court-circuiter rapidement les corps malformés.
    if let Some(t) = has_dynamic_field(body) {
        return Err(SoulSchemaError::DynamicFieldInBody(t));
    }

    let is_child = soul_extends(body).is_some();
    let required: &[&str] = if is_child {
        // Âme enfant : seule NARRATIVE est obligatoire.
        &["NARRATIVE"]
    } else {
        // Âme racine : les trois sections sont obligatoires.
        &REQUIRED_SECTIONS
    };

    for &s in required {
        match extract_section(body, s) {
            None => return Err(SoulSchemaError::MissingSection(s)),
            Some(c) if c.is_empty() => return Err(SoulSchemaError::EmptySection(s)),
            _ => {}
        }
    }

    // INV-CANARY : requis uniquement pour les âmes racines.
    // Anchored match: require a line that *starts* with "INV-CANARY" (after trimming
    // leading whitespace).  A bare `.contains("INV-CANARY")` would accept prose like
    // "# no INV-CANARY present" or a comment referencing the token — false positive.
    if !is_child {
        let invariants = extract_section(body, "INVARIANTS")
            .expect("INVARIANTS existe : vérifiée dans la boucle required ci-dessus");
        if !invariants
            .lines()
            .any(|l| l.trim_start().starts_with("INV-CANARY"))
        {
            return Err(SoulSchemaError::MissingCanaryInvariant);
        }
    }

    Ok(SoulDoc {
        // unwrap_or_default : safe — section vide = "" pour un enfant sans INVARIANTS/GATES.
        invariants: extract_section(body, "INVARIANTS").unwrap_or_default(),
        gates: extract_section(body, "GATES").unwrap_or_default(),
        narrative: extract_section(body, "NARRATIVE").unwrap_or_default(),
    })
}

/// Validates a soul note body, discarding the parsed document.
///
/// # Errors
/// See [`parse_soul`].
pub fn validate_soul(body: &str) -> Result<(), SoulSchemaError> {
    parse_soul(body).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "\
## INVARIANTS
INV-CANARY | REQUIRED | response.prefix matches ^\\(TODAY\\):
INV-LANG | REQUIRED | response.language == fr

## GATES
GATE-PIPELINE | multi_step OR service_live -> invoke gov-pipeline-agents

## NARRATIVE
Tu es le Général en Chef. Ton: direct, FR.
";

    #[test]
    fn parse_good_soul_ok() {
        let doc = parse_soul(GOOD).expect("valid soul");
        assert!(doc.invariants.contains("INV-CANARY"));
        assert!(!doc.gates.trim().is_empty());
        assert!(doc.narrative.contains("Général"));
    }

    #[test]
    fn missing_gates_section_rejected() {
        let bad = "## INVARIANTS\nINV-CANARY | REQUIRED | x\n## NARRATIVE\ny\n";
        assert!(matches!(
            validate_soul(bad),
            Err(SoulSchemaError::MissingSection("GATES"))
        ));
    }

    #[test]
    fn missing_canary_invariant_rejected() {
        let bad = "## INVARIANTS\nINV-LANG | REQUIRED | x\n## GATES\ng\n## NARRATIVE\nn\n";
        assert!(matches!(
            validate_soul(bad),
            Err(SoulSchemaError::MissingCanaryInvariant)
        ));
    }

    #[test]
    fn dynamic_field_in_body_rejected() {
        let bad = "## INVARIANTS\nINV-CANARY | REQUIRED | x\nupdated_at: 2026-06-27\n## GATES\ng\n## NARRATIVE\nn\n";
        assert!(matches!(
            validate_soul(bad),
            Err(SoulSchemaError::DynamicFieldInBody(_))
        ));
    }

    // ── Tests Task 4 — extends + champ scope (v0.7.3 Slice A item 2) ──────────

    /// Cas 1 — Âme enfant avec `extends:` : INVARIANTS/GATES optionnels, NARRATIVE seule requise.
    ///
    /// Le flag `extends: identity/main` en 1ère ligne non-vide post-H1 relaxe la validation :
    /// l'enfant n'a pas besoin de dupliquer INVARIANTS/GATES (ils viennent du parent).
    #[test]
    fn soul_with_extends_allows_missing_invariants_gates() {
        let child = "# identity/backend\nextends: identity/main\n\n## NARRATIVE\nTu es Backend. Rust, API, DB.\n";
        validate_soul(child)
            .expect("soul enfant avec extends doit être valide sans INVARIANTS/GATES");
    }

    /// Cas 2 — Âme enfant avec `extends:` mais sans `## NARRATIVE` → Err MissingSection.
    ///
    /// Même avec extends, NARRATIVE reste obligatoire (persona propre à l'agent).
    #[test]
    fn soul_with_extends_still_requires_narrative() {
        let child_no_narrative = "# identity/backend\nextends: identity/main\n";
        assert!(
            matches!(
                validate_soul(child_no_narrative),
                Err(SoulSchemaError::MissingSection("NARRATIVE"))
            ),
            "soul enfant sans ## NARRATIVE doit être rejeté même avec extends"
        );
    }

    /// Cas 3 — Corps enfant sans `extends:` et sans INVARIANTS → comportement inchangé.
    ///
    /// Régression : la validation actuelle (INVARIANTS/GATES/NARRATIVE + INV-CANARY requis)
    /// ne doit PAS être relaxée pour les corps sans directive `extends:`.
    #[test]
    fn soul_without_extends_unchanged() {
        let child_no_extends = "# identity/backend\n\n## NARRATIVE\nTu es Backend.\n";
        assert!(
            matches!(
                validate_soul(child_no_extends),
                Err(SoulSchemaError::MissingSection("INVARIANTS"))
            ),
            "soul sans extends doit rester strict : INVARIANTS manquante = Err"
        );
    }

    /// Cas 4 — `soul_extends` parse correctement la directive `extends:`.
    ///
    /// Le helper public `soul_extends` retourne `Some("identity/main")` sur la 1ère
    /// ligne non-vide post-H1 qui commence par `extends:`.
    #[test]
    fn soul_extends_helper_parses_parent() {
        let body = "# identity/backend\nextends: identity/main\n## NARRATIVE\nTu es Backend.\n";
        assert_eq!(
            soul_extends(body),
            Some("identity/main".to_string()),
            "soul_extends doit extraire la valeur de la directive extends:"
        );
    }

    /// Cas 5 — `soul_extends` ignore les occurrences de `extends:` en prose (P1-2 council).
    ///
    /// La détection est bornée à la 1ère ligne non-vide post-H1 (≤5 lignes post-H1).
    /// Un `extends:` dans NARRATIVE ou après une 1ère ligne non-vide autre ne déclenche PAS
    /// la détection (anti-faux-positif).
    #[test]
    fn soul_extends_ignores_prose_extends() {
        // `extends:` apparaît dans NARRATIVE mais PAS en 1ère ligne non-vide post-H1.
        let body = "# identity/backend\n## INVARIANTS\nINV-CANARY | REQUIRED | x\n## NARRATIVE\nTu utilises extends: notre approche.\n";
        assert_eq!(
            soul_extends(body),
            None,
            "soul_extends ne doit pas détecter extends: en prose dans NARRATIVE ou après une autre 1ère ligne"
        );
    }

    /// Cas 6 — Le champ `scope:` sur une ligne INVARIANT est accepté et ignoré (byte-stable).
    ///
    /// Les lignes `INV-CANARY | REQUIRED | x | scope:main-only` sont légales (Option A P0 council).
    /// `scope:` n'est PAS un champ dynamique interdit (C8) : il est statique, versionnée dans le body.
    #[test]
    fn soul_accepts_scope_field() {
        let body_with_scope = "\
## INVARIANTS
INV-CANARY | REQUIRED | response.prefix matches ^(TODAY): | scope:main-only
INV-LANG | REQUIRED | response.language == fr | scope:shared

## GATES
ORIENT-DELEGATE | tâche de code -> déléguer

## NARRATIVE
Tu es le Général en Chef.
";
        validate_soul(body_with_scope)
            .expect("soul avec champ scope: sur les lignes INVARIANT doit être valide");
    }
}
