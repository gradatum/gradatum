//! Tests d'intégration — JsonlFileSink + content_hash_jcs (P2.0b T10 — caveat C4).
//!
//! Vérifie :
//! 1. Écriture JSONL + permissions mode 0640 (Unix).
//! 2. Rotation à la frontière de jour UTC.
//! 3. Canonicalisation JCS RFC 8785 (ordre champs invariant).

use chrono::TimeZone as _;
use gradatum_core::audit::http::{
    AuditSink as _, HttpAuditActor, HttpAuditEvent, content_hash_jcs,
};
use gradatum_server::audit_jsonl::JsonlFileSink;
use serde_json::json;
use std::os::unix::fs::PermissionsExt as _;

/// Construit un `HttpAuditEvent` minimal pour les tests.
fn make_event(ts: chrono::DateTime<chrono::Utc>, note_id: &str) -> HttpAuditEvent {
    HttpAuditEvent {
        ts,
        event: "vault_write".into(),
        actor: HttpAuditActor {
            kid: "k1".into(),
            sub: "test-agent".into(),
            aud: "gradatum".into(),
        },
        tenant_id: "main".into(),
        locus: "decisions/test-note".into(),
        note_id: Some(note_id.into()),
        content_hash: Some("sha256:abc123".into()),
        outcome: "admitted".into(),
        curator: None,
        request_id: "req_test_1".into(),
    }
}

/// Vérifie que JsonlFileSink crée le fichier avec mode 0640 et écrit un JSON valide.
#[tokio::test]
async fn writes_jsonl_with_mode_0640() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sink = JsonlFileSink::new(tmp.path().to_path_buf());
    let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();

    sink.record(make_event(ts, "01HXYZAUDITWRITE"))
        .await
        .unwrap();

    let path = tmp.path().join("audit.2026-05-05.jsonl");
    assert!(
        path.is_file(),
        "Le fichier audit.2026-05-05.jsonl doit exister"
    );

    // Vérification mode Unix 0640
    let mode = path.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o640,
        "Le fichier audit doit avoir les permissions 0640, obtenu : {mode:o}"
    );

    // Vérification contenu JSON valide
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(content.trim()).expect("Le contenu doit être du JSON valide");

    assert_eq!(
        parsed["note_id"], "01HXYZAUDITWRITE",
        "note_id doit être préservé tel quel"
    );
    assert_eq!(parsed["event"], "vault_write");
    assert_eq!(parsed["outcome"], "admitted");
    assert_eq!(parsed["tenant_id"], "main");
}

/// Vérifie que deux événements sur deux jours distincts produisent deux fichiers séparés.
#[tokio::test]
async fn rotates_on_day_boundary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sink = JsonlFileSink::new(tmp.path().to_path_buf());

    let day1 = chrono::Utc.with_ymd_and_hms(2026, 5, 5, 23, 59, 0).unwrap();
    let day2 = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 0, 1, 0).unwrap();

    sink.record(make_event(day1, "note-day1")).await.unwrap();
    sink.record(make_event(day2, "note-day2")).await.unwrap();

    let file_day1 = tmp.path().join("audit.2026-05-05.jsonl");
    let file_day2 = tmp.path().join("audit.2026-05-06.jsonl");

    assert!(
        file_day1.is_file(),
        "audit.2026-05-05.jsonl doit exister après événement jour 1"
    );
    assert!(
        file_day2.is_file(),
        "audit.2026-05-06.jsonl doit exister après franchissement minuit"
    );

    // Vérifier que chaque fichier contient le bon événement
    let content_day1 = tokio::fs::read_to_string(&file_day1).await.unwrap();
    let parsed_day1: serde_json::Value = serde_json::from_str(content_day1.trim()).unwrap();
    assert_eq!(parsed_day1["note_id"], "note-day1");

    let content_day2 = tokio::fs::read_to_string(&file_day2).await.unwrap();
    let parsed_day2: serde_json::Value = serde_json::from_str(content_day2.trim()).unwrap();
    assert_eq!(parsed_day2["note_id"], "note-day2");
}

/// Vérifie que content_hash_jcs produit le même hash pour deux JSON équivalents
/// (champs dans des ordres différents) — propriété fondamentale JCS RFC 8785.
#[test]
fn content_hash_jcs_canonical() {
    // Deux valeurs JSON structurellement équivalentes mais avec ordre de champs différent.
    let a = json!({"section": "decisions", "tags": ["a", "b"], "title": "ma-note"});
    let b = json!({"title": "ma-note", "tags": ["a", "b"], "section": "decisions"});

    let hash_a = content_hash_jcs(&a).expect("JCS doit réussir sur a");
    let hash_b = content_hash_jcs(&b).expect("JCS doit réussir sur b");

    assert_eq!(
        hash_a, hash_b,
        "JCS doit produire le même hash pour deux JSON équivalents (ordre indépendant)"
    );

    // Vérifier le format du hash
    assert!(
        hash_a.starts_with("sha256:"),
        "Le hash doit commencer par 'sha256:'"
    );
    assert_eq!(
        hash_a.len(),
        7 + 64, // "sha256:" + 64 hex chars
        "Le hash sha256 doit faire 64 caractères hex"
    );

    // Vérifier que deux valeurs différentes produisent des hashes différents
    let c = json!({"section": "decisions", "tags": ["a", "c"]});
    let hash_c = content_hash_jcs(&c).expect("JCS doit réussir sur c");
    assert_ne!(
        hash_a, hash_c,
        "Des valeurs différentes doivent avoir des hashes différents"
    );
}

/// Vérifie que plusieurs événements sur le même jour s'accumulent dans le même fichier.
#[tokio::test]
async fn multiple_events_same_day_appended() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sink = JsonlFileSink::new(tmp.path().to_path_buf());

    let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 7, 10, 0, 0).unwrap();

    for i in 0..5 {
        let note_id = format!("note-{i}");
        sink.record(make_event(ts, &note_id)).await.unwrap();
    }

    let path = tmp.path().join("audit.2026-05-07.jsonl");
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        5,
        "5 événements doivent produire 5 lignes JSONL"
    );

    // Chaque ligne doit être du JSON valide
    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("Ligne {i} invalide : {e}"));
        assert_eq!(parsed["note_id"], format!("note-{i}"));
    }
}
