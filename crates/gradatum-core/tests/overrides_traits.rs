//! Tests des traits Override : Overridable, OverridePayload, FrontmatterPatch, OverrideScope.
//!
//! Objectif : prouver que les shapes de traits compilent et que le round-trip serde
//! des types de scope est correct.

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::overrides::{FrontmatterPatch, Overridable, OverridePayload};

/// Dummy type concret pour vérifier que les Associated Types compilent.
#[derive(serde::Serialize, serde::Deserialize)]
struct DummyMetadataOverride;

impl OverridePayload for DummyMetadataOverride {
    const OVERRIDE_TYPE: &'static str = "metadata";
    const SCHEMA_VERSION: u32 = 1;
}

impl Overridable for DummyMetadataOverride {
    type Patch = FrontmatterPatch;
    type Output = Frontmatter;

    /// Résolution identité — utilisé uniquement pour vérifier que le trait compile.
    fn resolve(base: &Frontmatter, _patch: &FrontmatterPatch) -> Frontmatter {
        base.clone()
    }
}

/// Vérification de compilation : les Associated Types sont bien contraignables.
///
/// Le fait que ce test COMPILE prouve que la shape du trait est correcte.
#[test]
fn overridable_associated_types_compile() {
    // Accéder au type associé suffit à prouver la shape.
    let _patch: <DummyMetadataOverride as Overridable>::Patch = FrontmatterPatch::default();
    let _ = _patch;
}

/// Vérification des constantes discriminant + schema_version.
#[test]
fn override_payload_const_discriminant() {
    assert_eq!(DummyMetadataOverride::OVERRIDE_TYPE, "metadata");
    assert_eq!(DummyMetadataOverride::SCHEMA_VERSION, 1);
}

/// Round-trip serde JSON de `OverrideScope::Vault`.
///
/// Vérifie le format `{ "kind": "vault", "id": "main" }` produit par `serde(tag, content)`.
#[test]
fn override_scope_serde_roundtrip_vault() {
    use gradatum_core::scope::{OverrideScope, VaultId};

    let scope = OverrideScope::Vault(VaultId::new("main"));
    let json = serde_json::to_string(&scope).unwrap();
    assert!(
        json.contains("\"kind\":\"vault\""),
        "attendu kind=vault, got: {json}"
    );
    assert!(
        json.contains("\"id\":\"main\""),
        "attendu id=main, got: {json}"
    );

    let back: OverrideScope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, scope, "round-trip OverrideScope::Vault");
}

/// Round-trip serde JSON de `OverrideScope::Locus`.
#[test]
fn override_scope_serde_roundtrip_locus() {
    use gradatum_core::scope::{LocusId, OverrideScope, VaultId};

    let scope = OverrideScope::Locus {
        vault: VaultId::new("main"),
        locus: LocusId::new("decisions"),
    };
    let json = serde_json::to_string(&scope).unwrap();
    assert!(
        json.contains("\"kind\":\"locus\""),
        "attendu kind=locus, got: {json}"
    );

    let back: OverrideScope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, scope, "round-trip OverrideScope::Locus");
}

/// Vérification que `FrontmatterPatch::default()` est vide (tous les champs `None`/vides).
///
/// Un patch vide appliqué sur un frontmatter doit idempotent (aucune modification).
#[test]
fn frontmatter_patch_default_is_empty() {
    let patch = FrontmatterPatch::default();
    assert!(patch.section.is_none());
    assert!(patch.tags_add.is_empty());
    assert!(patch.tags_remove.is_empty());
    assert!(patch.status.is_none());
    assert!(patch.author_override.is_none());
    assert!(patch.status_reason.is_none());
}

/// Round-trip TOML de `FrontmatterPatch` (via serde_json car plus flexible que TOML
/// pour les types avec champs skip_serializing_if).
///
/// `OverridePayload::to_toml/from_toml` est testé sur un type table TOML-compatible
/// (pas une unit struct — TOML ne supporte pas les unit structs sans champs).
#[test]
fn frontmatter_patch_serde_roundtrip() {
    use gradatum_core::status::NoteStatus;
    use gradatum_core::tag::Tag;

    let patch = FrontmatterPatch {
        status: Some(NoteStatus::Live),
        tags_add: vec![Tag::new("validated").unwrap()],
        ..Default::default()
    };

    // Vérification via serde_json (round-trip complet).
    let json = serde_json::to_string(&patch).unwrap();
    let back: FrontmatterPatch = serde_json::from_str(&json).unwrap();
    assert_eq!(patch.status, back.status);
    assert_eq!(patch.tags_add.len(), back.tags_add.len());
    assert_eq!(patch.tags_add[0].as_str(), back.tags_add[0].as_str());
    // tags_remove vide → absent en sérialisation → Vec vide après désérialisation.
    assert!(back.tags_remove.is_empty());
    // section et author_override non renseignés → None.
    assert!(back.section.is_none());
    assert!(back.author_override.is_none());
}

/// Vérification que `OverridePayload::to_toml`/`from_toml` fonctionnent sur un type
/// TOML-compatible concret (table avec champs), pas une unit struct.
#[test]
fn override_payload_toml_roundtrip_on_table_type() {
    use gradatum_core::overrides::OverridePayload;
    use gradatum_core::status::NoteStatus;

    /// Payload concret minimal pour le test TOML.
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct ConcretePayload {
        override_type: String,
        version: u32,
    }

    impl OverridePayload for ConcretePayload {
        const OVERRIDE_TYPE: &'static str = "concrete";
        const SCHEMA_VERSION: u32 = 1;
    }

    let payload = ConcretePayload {
        override_type: "concrete".into(),
        version: 1,
    };

    let toml_str = payload
        .to_toml()
        .expect("sérialisation TOML ConcretePayload");
    let back = ConcretePayload::from_toml(&toml_str).expect("désérialisation TOML ConcretePayload");

    assert_eq!(payload, back);
    assert!(toml_str.contains("override_type"));
    assert!(toml_str.contains("version = 1"));
    let _ = NoteStatus::Live; // silence lint
}
