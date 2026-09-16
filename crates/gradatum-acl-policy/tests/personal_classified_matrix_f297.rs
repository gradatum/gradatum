//! F-297 critère 2 — matrice `personal-classified` : le disjoncteur devient une
//! négation de motif, évaluée par segment de locus (plus par sous-chaîne).
//!
//! Deux volets :
//! 1. Une matrice `(identité × opération × locus)` snapshotée (`insta`) sur les deux
//!    presets livrés (`gradatum-admin/presets/{hierarchical,flat}.toml`) et la fixture
//!    synthétique locale. La décision effective (allow/deny) est l'axe primaire ; la
//!    variante d'`AclDecision` est l'axe secondaire. AVANT/APRÈS : la décision effective
//!    reste identique sur les positions de segment et les loci existants ; seules les
//!    variantes par affixe (`…-2026`, `my-…`) basculent, endossé par la spec F-160 l.257.
//! 2. Des assertions ciblées sur les quatre positions de segment, les variantes par
//!    affixe, et la garde de migration `RemovedPresetField`.

use gradatum_acl_policy::{AclDecision, AclEngine, AclError, AclOp};
use gradatum_core::section::Section;
use gradatum_core::trust::{StudioScope, TrustContext};

const HIERARCHICAL: &str = include_str!("../../gradatum-admin/presets/hierarchical.toml");
const FLAT: &str = include_str!("../../gradatum-admin/presets/flat.toml");
const FIXTURE: &str = include_str!("fixtures/hierarchical.toml");

fn hierarchical() -> AclEngine {
    AclEngine::from_preset_str(HIERARCHICAL).expect("preset hierarchical valide")
}

/// Contexte de confiance modèle : `admin`/`studio` sont des identités Studio (sans
/// `tenant_id`, donc hors garde d'isolation tenant, comme au runtime) ; toute autre
/// identité est un porteur Bearer sur le tenant `main`.
fn trust_for(identity: &str) -> TrustContext {
    if identity == "admin" || identity == "studio" {
        TrustContext::Studio {
            user: identity.into(),
            scope: StudioScope::Admin,
            step_up_until: None,
        }
    } else {
        TrustContext::BearerToken {
            kid: format!("kid-{identity}"),
            aud: "gradatum".into(),
            sub: identity.into(),
            scopes: vec!["vault_read".into(), "vault_write".into()],
            tenant_id: "main".into(),
            jti: None,
        }
    }
}

fn op_str(op: AclOp) -> &'static str {
    match op {
        AclOp::Read => "read",
        AclOp::Write => "write",
        _ => "op?",
    }
}

fn decision_str(d: &AclDecision) -> &'static str {
    match d {
        AclDecision::Allow => "ALLOW",
        AclDecision::DenyExplicit => "DENY(explicit)",
        AclDecision::DenyImplicit => "DENY(implicit)",
        _ => "DENY(unknown)",
    }
}

/// Jeu de loci : une position par classe `personal-classified` + témoins, PLUS tous les
/// loci existants du parc (vaults énumérés par commande lecture seule le 2026-09-14 —
/// `sqlite3 -readonly … "select distinct vault_id from notes"` → `code-gradatum`,
/// `default`, `main`, `test`) croisés avec `Section::ALL` (15) et la pseudo-section
/// `timeline`.
fn loci() -> Vec<String> {
    let mut v = vec![
        // Une position par classe de segment.
        "personal-classified".to_string(),
        "main/personal-classified".to_string(),
        "personal-classified/decisions".to_string(),
        "main/personal-classified/sub".to_string(),
        // Variantes par affixe (ce qui cesse d'être refusé — spec F-160 l.257).
        "main/personal-classified-2026".to_string(),
        "main/my-personal-classified".to_string(),
        // Témoins.
        "main/main".to_string(),
        "default".to_string(),
    ];
    for vault in ["code-gradatum", "default", "main", "test"] {
        for sec in Section::ALL {
            v.push(format!("{vault}/{}", sec.as_str()));
        }
        v.push(format!("{vault}/timeline"));
    }
    v.sort();
    v.dedup();
    v
}

/// Rend la matrice `identité | op | locus => décision` triée, pour un snapshot stable.
fn render_matrix(engine: &AclEngine, identities: &[&str]) -> String {
    let loci = loci();
    let mut lines = Vec::new();
    for id in identities {
        let trust = trust_for(id);
        for op in [AclOp::Read, AclOp::Write] {
            for locus in &loci {
                let d = engine.evaluate(&trust, op, locus);
                lines.push(format!(
                    "{id} | {} | {locus} => {}",
                    op_str(op),
                    decision_str(&d)
                ));
            }
        }
    }
    lines.join("\n")
}

/// Identités concrètes déclarées dans le preset livré `hierarchical.toml`.
const HIERARCHICAL_IDENTITIES: &[&str] = &[
    "admin",
    "studio",
    "main-agent",
    "expert-prompt",
    "expert-llm",
    "expert-infra",
    "expert-clouds",
    "expert-pulse",
    "expert-monitoring",
    "expert-security",
    "engine",
    "agent-wrapper",
    "third-party-agent",
];

/// Identités concrètes de la fixture synthétique locale.
const FIXTURE_IDENTITIES: &[&str] = &[
    "admin",
    "main-agent",
    "sub-agent-a",
    "validator",
    "expert-rust",
];

#[test]
fn matrix_hierarchical_preset() {
    let engine = hierarchical();
    insta::assert_snapshot!(
        "matrix_hierarchical",
        render_matrix(&engine, HIERARCHICAL_IDENTITIES)
    );
}

#[test]
fn matrix_flat_preset() {
    let engine = AclEngine::from_preset_str(FLAT).expect("preset flat valide");
    insta::assert_snapshot!("matrix_flat", render_matrix(&engine, &["main-agent"]));
}

#[test]
fn matrix_fixture() {
    let engine = AclEngine::from_preset_str(FIXTURE).expect("fixture valide");
    insta::assert_snapshot!("matrix_fixture", render_matrix(&engine, FIXTURE_IDENTITIES));
}

/// Le refus porte sur le SEGMENT `personal-classified`, à toute position et toute
/// profondeur — `DenyExplicit` (une négation de motif a matché, deny-wins).
#[test]
fn negation_denies_personal_classified_in_every_segment_position() {
    let engine = hierarchical();
    let trust = trust_for("main-agent");
    for locus in [
        "personal-classified",           // mono-segment
        "main/personal-classified",      // suffixe
        "personal-classified/decisions", // préfixe
        "main/personal-classified/sub",  // au milieu
        "personal-classified/a/b/c",     // préfixe, profond
        "x/y/z/personal-classified",     // suffixe, profond
    ] {
        assert_eq!(
            engine.evaluate(&trust, AclOp::Read, locus),
            AclDecision::DenyExplicit,
            "{locus} doit être refusé explicitement (négation par segment)"
        );
    }
}

/// Ce qui cesse d'être refusé : les variantes par AFFIXE ne contiennent pas le segment
/// `personal-classified` — un porteur dont l'allow matche y accède (spec F-160 l.257).
#[test]
fn negation_allows_affix_variants() {
    let engine = hierarchical();
    let trust = trust_for("main-agent");
    for locus in [
        "main/personal-classified-2026",
        "main/my-personal-classified",
    ] {
        assert_eq!(
            engine.evaluate(&trust, AclOp::Read, locus),
            AclDecision::Allow,
            "{locus} est une variante par affixe, plus refusée (spec F-160 l.257)"
        );
    }
}

// ── Garde de migration : la clé retirée ne peut plus passer inaperçue ──────────
// Trois tests, dont deux de NON-déclenchement : la garde PARSE, elle ne grep pas.

/// Un `[[consumer]]` portant encore la clé `sees_personal_classified` → refus explicite.
#[test]
fn removed_preset_field_key_is_refused() {
    let preset = r#"
[[consumer]]
identity = "bot"
read_patterns = ["**"]
write_patterns = ["**"]
sees_personal_classified = false
"#;
    match AclEngine::from_preset_str(preset) {
        Err(AclError::RemovedPresetField { field }) => {
            assert_eq!(field, "sees_personal_classified");
        }
        Err(other) => panic!("attendu RemovedPresetField, obtenu {other:?}"),
        Ok(_) => panic!("la clé retirée doit être refusée par la garde de migration"),
    }
}

/// La chaîne `sees_personal_classified` dans un COMMENTAIRE (forme de
/// `hierarchical.toml:40`) ne déclenche pas la garde — preuve que la garde ne grep pas.
#[test]
fn removed_preset_field_in_comment_is_accepted() {
    let preset = r#"
# Caveat B3: sees_personal_classified=false devient une négation de motif.
[[consumer]]
identity = "bot"
read_patterns = ["!**/personal-classified", "**"]
write_patterns = ["!**/personal-classified", "**"]
"#;
    assert!(
        AclEngine::from_preset_str(preset).is_ok(),
        "un preset conforme ne doit pas être refusé pour une mention en commentaire"
    );
}

/// La chaîne `sees_personal_classified` dans une valeur `description` ne déclenche pas
/// la garde — la garde ne vérifie que la CLÉ d'une table de consommateur.
#[test]
fn removed_preset_field_in_description_is_accepted() {
    let preset = r#"
[[consumer]]
identity = "bot"
description = "note héritée mentionnant sees_personal_classified=false en prose"
read_patterns = ["**"]
write_patterns = ["**"]
"#;
    assert!(
        AclEngine::from_preset_str(preset).is_ok(),
        "un preset conforme ne doit pas être refusé pour une mention en description"
    );
}
