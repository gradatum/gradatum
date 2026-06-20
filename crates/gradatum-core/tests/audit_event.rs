//! Tests du module audit : AuditEvent + AuditEventType typed enum.
//!
//! Objectifs :
//! - Vérifier que la sérialisation JSON ne génère pas de champ `payload` générique (décision Q8).
//! - Vérifier que `extra: ExtraFields` est omis quand vide.
//! - Vérifier que `DriftDetected` transporte bien deux ContentHash.

use chrono::Utc;

use gradatum_core::audit::{AuditEvent, AuditEventType};
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::frontmatter::ExtraFields;
use gradatum_core::identity::{ContentHash, NoteId};

/// Construit un `AuditEvent` minimal pour les tests.
fn minimal_event(event_type: AuditEventType) -> AuditEvent {
    AuditEvent {
        note_id: NoteId::new(),
        event_type,
        actor: AuthorRef {
            kind: AuthorKind::Human,
            id: "operator".into(),
            display_name: None,
        },
        occurred_at: Utc::now(),
        extra: ExtraFields::empty(),
        correlation_id: None,
    }
}

/// La variante `Embedded` sérialise ses champs inline — AUCUN champ `payload` générique.
///
/// Décision Q8 : pas de `payload: serde_json::Value` dans `AuditEventType`.
#[test]
fn typed_variant_no_payload_value_field() {
    let json = serde_json::to_string(&AuditEventType::Embedded {
        embedder_id: "bge-m3".into(),
        model_version: "1.0".into(),
        dim: 1024,
    })
    .unwrap();

    assert!(
        json.contains("\"type\":\"embedded\""),
        "type kebab-case manquant, got: {json}"
    );
    assert!(
        json.contains("\"embedder_id\":\"bge-m3\""),
        "embedder_id manquant, got: {json}"
    );
    assert!(json.contains("\"dim\":1024"), "dim manquant, got: {json}");
    // Point critique : aucun champ "payload" générique.
    assert!(
        !json.contains("\"payload\""),
        "champ payload interdit (décision Q8), got: {json}"
    );
}

/// `extra: ExtraFields` vide → absent en sérialisation JSON.
/// `correlation_id: None` → absent en sérialisation JSON.
#[test]
fn extra_fields_empty_skipped_in_serialization() {
    let evt = minimal_event(AuditEventType::Created);
    let json = serde_json::to_string(&evt).unwrap();

    assert!(
        !json.contains("\"extra\""),
        "extra vide doit être omis, got: {json}"
    );
    assert!(
        !json.contains("\"correlation_id\""),
        "correlation_id None doit être omis, got: {json}"
    );
}

/// `DriftDetected` transporte deux `ContentHash` distincts — stocked vs computed.
///
/// Vérifie le round-trip serde + l'égalité après désérialisation.
#[test]
fn drift_detected_carries_two_content_hashes() {
    let stored = ContentHash([0x11; 32]);
    let computed = ContentHash([0x22; 32]);

    let evt_type = AuditEventType::DriftDetected {
        stored_hash: stored,
        computed_hash: computed,
    };

    let json = serde_json::to_string(&evt_type).unwrap();
    assert!(
        json.contains("\"type\":\"drift-detected\""),
        "type kebab-case manquant, got: {json}"
    );

    let back: AuditEventType = serde_json::from_str(&json).unwrap();
    match back {
        AuditEventType::DriftDetected {
            stored_hash,
            computed_hash,
        } => {
            assert_eq!(stored_hash, stored, "stored_hash ne correspond pas");
            assert_eq!(computed_hash, computed, "computed_hash ne correspond pas");
        }
        _ => panic!("mauvaise variante après round-trip"),
    }
}

/// Round-trip complet d'un `AuditEvent` avec `extra` rempli.
///
/// Vérifie que les champs extra sont bien préservés après round-trip JSON.
#[test]
fn audit_event_with_extra_roundtrip() {
    use gradatum_core::identity::NoteVersion;

    let mut extra = ExtraFields::empty();
    extra.insert("session_id".into(), toml::Value::String("abc123".into()));

    let evt = AuditEvent {
        note_id: NoteId::new(),
        event_type: AuditEventType::Restored {
            from_version: NoteVersion::initial(),
        },
        actor: AuthorRef::system("legacy-vault"),
        occurred_at: Utc::now(),
        extra,
        correlation_id: None,
    };

    let json = serde_json::to_string(&evt).unwrap();
    // extra est présent car non vide.
    assert!(
        json.contains("\"extra\""),
        "extra rempli doit apparaître, got: {json}"
    );
    assert!(
        json.contains("session_id"),
        "clé extra session_id manquante, got: {json}"
    );
}

/// `StatusChanged` sérialise correctement from/to et reason optionnel.
#[test]
fn status_changed_variant_serde() {
    use gradatum_core::status::NoteStatus;

    let evt_type = AuditEventType::StatusChanged {
        from: NoteStatus::Draft,
        to: NoteStatus::PendingReview,
        reason: Some("curator pipeline".into()),
    };

    let json = serde_json::to_string(&evt_type).unwrap();
    assert!(
        json.contains("\"type\":\"status-changed\""),
        "type manquant, got: {json}"
    );
    assert!(
        json.contains("\"from\":\"draft\""),
        "from manquant, got: {json}"
    );
    assert!(
        json.contains("\"to\":\"pending-review\""),
        "to manquant, got: {json}"
    );
    assert!(
        json.contains("\"reason\":\"curator pipeline\""),
        "reason manquant, got: {json}"
    );

    // Round-trip.
    let back: AuditEventType = serde_json::from_str(&json).unwrap();
    if let AuditEventType::StatusChanged { from, to, reason } = back {
        assert_eq!(from, NoteStatus::Draft);
        assert_eq!(to, NoteStatus::PendingReview);
        assert_eq!(reason, Some("curator pipeline".into()));
    } else {
        panic!("mauvaise variante");
    }
}
