//! Tests de régression serde JSON pour [`CurateSpec`] — D-12 P2.
//!
//! Vérifie la compatibilité forward/backward entre les payloads alpha.15
//! (champs `note_id` + `tenant_id` seuls) et les payloads v0.2.0
//! (champs étendus `title`/`body`/`author`/`tags`/`section_hint`).
//!
//! # Contexte
//!
//! Phase 1.2 a étendu `CurateSpec` avec 5 champs optionnels annotés
//! `#[serde(default)]`. Les `JobRecord` stockés en base pendant alpha.15
//! n'ont PAS ces champs en JSON — ils doivent se désérialiser en
//! `None`/`[]` (valeurs par défaut) sans erreur.
//!
//! # Référence
//!
//! - D-12 P2 : dette technique v0.2.0
//! - §11 E-27 : statut tests régression bincode CurateSpec

use gradatum_core::{CurateSpec, Job};
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — forward-compat : payload alpha.15 (champs minimal) → defaults appliqués
// ─────────────────────────────────────────────────────────────────────────────

/// Simule un payload JSON produit par alpha.15 (note_id + tenant_id uniquement).
///
/// Les champs `title`, `body`, `author`, `tags`, `section_hint` sont ABSENTS
/// du JSON → `#[serde(default)]` doit produire `None` / `vec![]`.
#[test]
fn curate_spec_forward_compat_alpha15_payload() {
    let note_id = Ulid::new();

    // Payload minimal alpha.15 : uniquement note_id + tenant_id
    let json = format!(r#"{{"note_id":"{}","tenant_id":"main"}}"#, note_id);

    let spec: CurateSpec =
        serde_json::from_str(&json).expect("CurateSpec doit désérialiser payload alpha.15");

    // Champs présents dans alpha.15
    assert_eq!(spec.note_id, note_id, "note_id doit être préservé");
    assert_eq!(spec.tenant_id, "main", "tenant_id doit être préservé");

    // Champs ajoutés en Phase 1.2 — doivent être None/[] (defaults)
    assert!(
        spec.title.is_none(),
        "title doit être None sur payload alpha.15 (champ absent)"
    );
    assert!(
        spec.body.is_none(),
        "body doit être None sur payload alpha.15 (champ absent)"
    );
    assert!(
        spec.author.is_none(),
        "author doit être None sur payload alpha.15 (champ absent)"
    );
    assert!(
        spec.tags.is_empty(),
        "tags doit être [] sur payload alpha.15 (champ absent)"
    );
    assert!(
        spec.section_hint.is_none(),
        "section_hint doit être None sur payload alpha.15 (champ absent)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — backward-compat : payload v0.2.0 (champs étendus) → round-trip
// ─────────────────────────────────────────────────────────────────────────────

/// Sérialise un `CurateSpec` v0.2.0 avec champs étendus, re-désérialise, vérifie équivalence.
///
/// Garantit que la struct complète (title + body + author + tags + section_hint)
/// survit à un cycle serde sans perte.
#[test]
fn curate_spec_backward_compat_v020_full_roundtrip() {
    let note_id = Ulid::new();
    let spec = CurateSpec {
        note_id,
        tenant_id: "main".to_string(),
        title: Some("Titre de test".to_string()),
        body: Some("Corps Markdown de la note.".to_string()),
        author: Some("main-agent".to_string()),
        tags: vec!["smoke".to_string(), "compat".to_string()],
        section_hint: Some("experiments".to_string()),
    };

    let json = serde_json::to_string(&spec).expect("CurateSpec doit être sérialisable v0.2.0");
    let back: CurateSpec =
        serde_json::from_str(&json).expect("CurateSpec v0.2.0 doit être désérialisable");

    assert_eq!(back.note_id, note_id);
    assert_eq!(back.tenant_id, "main");
    assert_eq!(back.title, Some("Titre de test".to_string()));
    assert_eq!(back.body, Some("Corps Markdown de la note.".to_string()));
    assert_eq!(back.author, Some("main-agent".to_string()));
    assert_eq!(back.tags, vec!["smoke".to_string(), "compat".to_string()]);
    assert_eq!(back.section_hint, Some("experiments".to_string()));
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — edge case : payload tronqué → erreur explicite (pas panic)
// ─────────────────────────────────────────────────────────────────────────────

/// Un payload JSON corrompu ou tronqué doit produire une erreur serde explicite.
///
/// Garantit l'absence de panic — l'erreur doit être propageable via `?`.
#[test]
fn curate_spec_truncated_payload_returns_error_not_panic() {
    let payloads = [
        "",                               // vide
        "{",                              // JSON tronqué (accolade non fermée)
        r#"{"note_id":"INVALIDE_ULID"}"#, // ULID invalide
        r#"{"tenant_id":"main"}"#,        // note_id manquant (champ requis)
    ];

    for payload in &payloads {
        let result: Result<CurateSpec, _> = serde_json::from_str(payload);
        assert!(
            result.is_err(),
            "Le payload `{payload}` devrait produire une erreur de désérialisation, pas Ok"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4 — compat dans JobRecord : CurateSpec embeddé dans Job::Curate
// ─────────────────────────────────────────────────────────────────────────────

/// Un `Job::Curate` wrappant un `CurateSpec` alpha.15 se désérialise correctement.
///
/// Simule la lecture d'un `Job` tel qu'il est stocké dans le champ `spec.kind`
/// d'un `JobRecord` en JSON en base (colonne `record` TEXT).
#[test]
fn job_curate_with_alpha15_spec_deserializes() {
    let note_id = Ulid::new();

    // Format JSON du variant Job::Curate tel que serde le produit avec tag+content
    let json = format!(
        r#"{{"type":"Curate","data":{{"note_id":"{}","tenant_id":"main"}}}}"#,
        note_id
    );

    let job: Job = serde_json::from_str(&json)
        .expect("Job::Curate avec spec alpha.15 doit se désérialiser sans erreur");

    match job {
        Job::Curate(spec) => {
            assert_eq!(spec.note_id, note_id);
            assert_eq!(spec.tenant_id, "main");
            assert!(
                spec.title.is_none(),
                "title doit être None (payload alpha.15)"
            );
            assert!(
                spec.body.is_none(),
                "body doit être None (payload alpha.15)"
            );
            assert!(spec.tags.is_empty(), "tags doit être [] (payload alpha.15)");
        }
        other => panic!("Attendu Job::Curate, reçu {other:?}"),
    }
}
