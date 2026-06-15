//! Tests du registre de schémas d'overrides embarqués.
//!
//! Vérifie que les 4 fichiers TOML sont bien chargés depuis `include_dir!`,
//! correctement parsés, et que les fonctions stubs retournent `Ok`.

use gradatum_core::schema_registry::{lookup, SchemaStatus};

/// metadata-v1 est le schéma stable — tous ses champs doivent être présents.
#[test]
fn lookup_metadata_v1_returns_struct() {
    let schema = lookup("metadata", 1).expect("metadata-v1 doit être embarqué");

    assert_eq!(schema.override_type, "metadata");
    assert_eq!(schema.schema_version, 1);
    assert_eq!(schema.owner_crate, "gradatum-vault");
    assert_eq!(schema.phase, 1);
    assert_eq!(schema.status, SchemaStatus::Stable);

    // Vérification des champs attendus.
    assert!(
        schema.fields.contains_key("status"),
        "champ 'status' manquant dans metadata-v1"
    );
    assert!(
        schema.fields.contains_key("tags_add"),
        "champ 'tags_add' manquant dans metadata-v1"
    );
    assert!(
        schema.fields.contains_key("section"),
        "champ 'section' manquant dans metadata-v1"
    );
    assert!(
        schema.fields.contains_key("author_override"),
        "champ 'author_override' manquant dans metadata-v1"
    );
    assert!(
        schema.fields.contains_key("status_reason"),
        "champ 'status_reason' manquant dans metadata-v1"
    );

    // status_reason a un max_len = 500.
    assert_eq!(
        schema.fields["status_reason"].max_len,
        Some(500),
        "max_len de status_reason doit être 500"
    );
}

/// Les lookups inconnus doivent retourner `None` (pas de panic).
#[test]
fn lookup_unknown_returns_none() {
    assert!(
        lookup("nonexistent", 1).is_none(),
        "type inconnu doit retourner None"
    );
    assert!(
        lookup("metadata", 99).is_none(),
        "version inconnue doit retourner None"
    );
}

/// acl-v1 est un stub expérimental — vérifie que le schéma est bien chargé et marqué experimental.
#[test]
fn lookup_phase2_stub_acl_v1() {
    let schema = lookup("acl", 1).expect("acl-v1 stub doit être embarqué");

    assert_eq!(schema.override_type, "acl");
    assert_eq!(schema.phase, 2);
    assert_eq!(schema.status, SchemaStatus::Experimental);
    assert_eq!(schema.owner_crate, "gradatum-acl-policy");
}

/// index-v1 est un stub expérimental (owner `gradatum-index`).
#[test]
fn lookup_phase3_stub_index_v1() {
    let schema = lookup("index", 1).expect("index-v1 stub doit être embarqué");

    assert_eq!(schema.phase, 3);
    assert_eq!(schema.status, SchemaStatus::Experimental);
    assert_eq!(schema.owner_crate, "gradatum-index");
}

/// score-v1 est un stub expérimental (owner `gradatum-curator`).
#[test]
fn lookup_phase4_stub_score_v1() {
    let schema = lookup("score", 1).expect("score-v1 stub doit être embarqué");

    assert_eq!(schema.phase, 4);
    assert_eq!(schema.status, SchemaStatus::Experimental);
    assert_eq!(schema.owner_crate, "gradatum-curator");
}

/// `validate_payload` accepte n'importe quoi (stub — validation complète planifiée à v0.5.0).
#[test]
fn validate_payload_phase1_stub_accepts_anything() {
    let result = gradatum_core::schema_registry::validate_payload("metadata", 1, "anything = true");
    assert!(
        result.is_ok(),
        "validate_payload stub doit toujours retourner Ok"
    );

    // Teste aussi un override_type inconnu — le stub ne valide pas.
    let result =
        gradatum_core::schema_registry::validate_payload("unknown-type", 99, "garbage toml!!!");
    assert!(result.is_ok(), "validate_payload stub n'échoue jamais");
}

/// `migrate_payload` retourne le payload inchangé (stub — migration complète planifiée à v0.5.0).
#[test]
fn migrate_payload_phase1_stub_returns_input() {
    let input = "section = \"decisions\"\nstatus = \"live\"";
    let result = gradatum_core::schema_registry::migrate_payload("metadata", 1, 2, input).unwrap();
    assert_eq!(
        result, input,
        "migrate_payload Phase 1 doit retourner l'input tel quel"
    );
}
