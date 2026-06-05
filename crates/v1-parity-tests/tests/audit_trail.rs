//! v1-parity : Audit trail sérialisation — 2 tests (D5 / spec §0.4 A3)
//!
//! Parité avec spec §2.9 AuditEvent (Q8 enum typée, ExtraFields, serde).
//! Domaine : sérialisation variante Embedded, round-trip DriftDetected.

mod common;

use chrono::Utc;
use gradatum_core::audit::{AuditEvent, AuditEventType};
use gradatum_core::author::AuthorRef;
use gradatum_core::identity::{ContentHash, NoteId};
use serde_json::Value;

// --- 1. audit_event_embedded_variant_no_payload_field ---

/// Vérifie que `AuditEventType::Embedded` est sérialisée avec le tag `"type": "embedded"`
/// et PAS de champ `"payload"` (décision Q8 — pas de `payload: Value` générique).
#[tokio::test]
async fn audit_event_embedded_variant_no_payload_field() {
    let event = AuditEvent {
        note_id: NoteId::new(),
        event_type: AuditEventType::Embedded {
            embedder_id: "bge-small-en-v1.5".into(),
            model_version: "phase1".into(),
            dim: 384,
        },
        actor: AuthorRef::human("test-agent"),
        occurred_at: Utc::now(),
        extra: Default::default(),
        correlation_id: None,
    };

    let json = serde_json::to_value(&event).expect("sérialisation AuditEvent::Embedded");

    // Le champ "type" doit être "embedded" (serde tag)
    assert_eq!(
        json["event_type"]["type"],
        Value::String("embedded".into()),
        "Le tag serde doit être 'embedded'"
    );

    // Pas de champ "payload" (décision Q8)
    assert!(
        json["event_type"].get("payload").is_none(),
        "AuditEvent ne doit PAS avoir de champ 'payload' générique (décision Q8)"
    );

    // Les champs structurés doivent être présents
    assert_eq!(json["event_type"]["embedder_id"], "bge-small-en-v1.5");
    assert_eq!(json["event_type"]["dim"], 384);
}

// --- 2. audit_event_drift_detected_round_trip ---

/// Sérialise puis désérialise un `AuditEvent::DriftDetected` → round-trip sans perte
/// des champs `stored_hash` et `computed_hash`.
#[tokio::test]
async fn audit_event_drift_detected_round_trip() {
    let fm = common::minimal_frontmatter("main");
    let stored_hash = ContentHash::compute(&fm, "Corps original.");
    let computed_hash = ContentHash::compute(&fm, "Corps modifié — drift.");

    let note_id = NoteId::new();

    let event = AuditEvent {
        note_id,
        event_type: AuditEventType::DriftDetected {
            stored_hash,
            computed_hash,
        },
        actor: AuthorRef::human("vault-drift-scanner"),
        occurred_at: Utc::now(),
        extra: Default::default(),
        correlation_id: None,
    };

    // Sérialise → JSON
    let json_str = serde_json::to_string(&event).expect("sérialisation DriftDetected");

    // Désérialise depuis JSON
    let event2: AuditEvent =
        serde_json::from_str(&json_str).expect("désérialisation DriftDetected");

    // Vérifie l'intégrité du round-trip
    assert_eq!(
        event2.note_id, note_id,
        "NoteId doit survivre au round-trip"
    );

    match &event2.event_type {
        AuditEventType::DriftDetected {
            stored_hash: sh,
            computed_hash: ch,
        } => {
            assert_eq!(
                sh.hex(),
                stored_hash.hex(),
                "stored_hash doit survivre au round-trip"
            );
            assert_eq!(
                ch.hex(),
                computed_hash.hex(),
                "computed_hash doit survivre au round-trip"
            );
        }
        _ => panic!("Variante inattendue après round-trip — DriftDetected attendu"),
    }
}
