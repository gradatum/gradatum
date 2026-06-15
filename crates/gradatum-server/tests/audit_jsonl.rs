//! Tests TDD T7 — JsonlFileSink : écriture JSONL + compteur dropped_total (caveat C-aud-4).
//!
//! Ces tests vérifient :
//! 1. `write_audit_event_appends_jsonl_line` — écriture d'un événement JSONL valide.
//! 2. `dropped_total_increments_on_buffer_saturation` — compteur atomique interne
//!    incrémenté sur chaque erreur I/O (saturation ou disque plein).
//!

use gradatum_core::audit::http::{AuditSink as _, HttpAuditActor, HttpAuditEvent};
use gradatum_server::audit_jsonl::JsonlFileSink;
use tempfile::TempDir;

/// Construit un `HttpAuditEvent` minimal pour les tests T7.
fn make_audit_event(event: &str, outcome: &str) -> HttpAuditEvent {
    HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: event.into(),
        actor: HttpAuditActor {
            kid: "test-kid".into(),
            sub: "test-agent".into(),
            aud: "gradatum".into(),
        },
        tenant_id: "main".into(),
        locus: "decisions/test".into(),
        note_id: None,
        content_hash: None,
        outcome: outcome.into(),
        curator: None,
        request_id: "req-t7-test".into(),
    }
}

/// Vérifie qu'un événement JSONL est appendé dans le fichier audit du jour.
#[tokio::test]
async fn write_audit_event_appends_jsonl_line() {
    let dir = TempDir::new().unwrap();
    let sink = JsonlFileSink::new(dir.path().to_path_buf());

    let evt = make_audit_event("vault_write", "admitted");
    sink.record(evt)
        .await
        .expect("record doit réussir sur un répertoire accessible");

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let path = dir.path().join(format!("audit.{today}.jsonl"));
    let content = std::fs::read_to_string(&path).expect("fichier audit doit exister");

    assert!(
        content.contains("\"event\":\"vault_write\""),
        "le champ event doit être présent dans le JSONL"
    );
    assert!(
        content.contains("\"outcome\":\"admitted\""),
        "le champ outcome doit être présent dans le JSONL"
    );
}

/// Vérifie que `dropped_total` est incrémenté quand `record` échoue (erreur I/O).
///
/// Simule la saturation en rendant le répertoire d'audit non accessible (read-only)
/// après construction du sink. Chaque `record` suivant doit échouer et incrémenter
/// le compteur atomique interne.
///
/// Caveat C-aud-4 : le compteur est accessible via `dropped_total()` pour les fixtures de test.
#[tokio::test]
async fn dropped_total_increments_on_buffer_saturation() {
    let dir = TempDir::new().unwrap();
    let sink = JsonlFileSink::new(dir.path().to_path_buf());

    // Écrire un premier événement pour créer le fichier avec les bonnes permissions.
    let first_evt = make_audit_event("vault_write", "queued");
    sink.record(first_evt)
        .await
        .expect("premier record doit réussir");

    // Rendre le répertoire non accessible en écriture pour forcer les erreurs I/O.
    // Utilise nix/chmod pour simuler un filesystem en lecture seule.
    use std::os::unix::fs::PermissionsExt as _;
    let ro_perms = std::fs::Permissions::from_mode(0o444);
    std::fs::set_permissions(dir.path(), ro_perms)
        .expect("set_permissions doit réussir pour le test");

    // Forcer la rotation vers un nouveau jour pour éviter d'utiliser le handle déjà ouvert.
    // On crée un nouveau sink qui tente d'ouvrir un fichier dans le répertoire read-only.
    let sink_ro = JsonlFileSink::new(dir.path().join("subdir_impossible"));

    for i in 0..10 {
        let evt = make_audit_event(&format!("test_{i}"), "queued");
        // Les erreurs I/O sont attendues — record retourne Err.
        let _ = sink_ro.record(evt).await;
    }

    let dropped = sink_ro.dropped_total();
    assert!(
        dropped > 0,
        "dropped_total doit être > 0 après des erreurs I/O répétées — got {dropped}"
    );

    // Restaurer les permissions pour que TempDir puisse se nettoyer.
    let rw_perms = std::fs::Permissions::from_mode(0o755);
    let _ = std::fs::set_permissions(dir.path(), rw_perms);
}
