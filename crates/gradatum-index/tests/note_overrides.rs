//! Tests override generique — table `note_overrides`, contrainte UNIQUE, round-trip.

mod common;
use common::make_note;

use gradatum_core::scope::{OverrideScope, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

#[tokio::test]
async fn override_upsert_then_get_round_trip() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "test override",
    );
    idx.upsert_note(&note).await.unwrap();

    let scope = OverrideScope::Vault(VaultId::new("main"));
    let payload = r#"[metadata]
title = "Titre surchargé"
pinned = true
"#;

    idx.upsert_override_raw(note.id, &scope, "metadata", 1, payload)
        .await
        .unwrap();

    let result = idx
        .get_override_raw(note.id, &scope, "metadata")
        .await
        .unwrap();

    assert!(result.is_some(), "override doit exister après upsert");
    let (sv, pt) = result.unwrap();
    assert_eq!(sv, 1, "schema_version doit être 1");
    assert_eq!(pt, payload, "payload_toml doit être round-trippé intact");
}

#[tokio::test]
async fn override_get_absent_returns_none() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note("main", Section::Decisions, NoteStatus::Live, "absent");
    idx.upsert_note(&note).await.unwrap();

    let scope = OverrideScope::Vault(VaultId::new("main"));
    let result = idx
        .get_override_raw(note.id, &scope, "metadata")
        .await
        .unwrap();
    assert!(result.is_none(), "override absent doit retourner None");
}

#[tokio::test]
async fn override_unique_constraint_upserts_not_inserts() {
    // 2ème upsert sur le même (note_id, scope, type) met à jour — pas de doublon
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "test unique override",
    );
    idx.upsert_note(&note).await.unwrap();

    let scope = OverrideScope::Vault(VaultId::new("main"));
    let payload_v1 = "version = 1\n";
    let payload_v2 = "version = 2\n";

    idx.upsert_override_raw(note.id, &scope, "metadata", 1, payload_v1)
        .await
        .unwrap();
    // 2ème call — même clé, payload différent
    idx.upsert_override_raw(note.id, &scope, "metadata", 2, payload_v2)
        .await
        .unwrap();

    let result = idx
        .get_override_raw(note.id, &scope, "metadata")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        result.0, 2,
        "schema_version doit être mise à jour à 2 après le 2ème upsert"
    );
    assert_eq!(result.1, payload_v2, "payload doit être remplacé par la v2");
}

#[tokio::test]
async fn override_distinct_scope_kinds() {
    // Vault + Locus scopes distincts sur la même note — coexistent sans collision
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note("main", Section::Decisions, NoteStatus::Live, "multi scope");
    idx.upsert_note(&note).await.unwrap();

    let scope_vault = OverrideScope::Vault(VaultId::new("main"));
    let scope_locus = OverrideScope::Locus {
        vault: VaultId::new("main"),
        locus: gradatum_core::scope::LocusId::new("locus-private"),
    };

    idx.upsert_override_raw(note.id, &scope_vault, "metadata", 1, "source = \"vault\"\n")
        .await
        .unwrap();
    idx.upsert_override_raw(note.id, &scope_locus, "metadata", 1, "source = \"locus\"\n")
        .await
        .unwrap();

    let r_vault = idx
        .get_override_raw(note.id, &scope_vault, "metadata")
        .await
        .unwrap()
        .unwrap();
    let r_locus = idx
        .get_override_raw(note.id, &scope_locus, "metadata")
        .await
        .unwrap()
        .unwrap();

    assert!(r_vault.1.contains("vault"));
    assert!(r_locus.1.contains("locus"));
}
