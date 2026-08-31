//! Mesure la version du moteur SQLite embarqué (F-145 — montée rusqlite).
//!
//! `rusqlite` est compilé avec la feature `bundled` : ce test interroge la VRAIE
//! version du moteur (`SELECT sqlite_version()`), jamais une déduction depuis le
//! numéro de crate. C'est l'instrument de la mesure AVANT/APRÈS exigée par la
//! carte F-145 (jalon 2.1.0) et la preuve que la montée ne fait pas RECULER le
//! moteur embarqué des bases de production.

use rusqlite::Connection;

#[test]
fn sqlite_engine_version_is_measurable_and_semver_shaped() {
    let conn = Connection::open_in_memory().expect("in-memory connection must open");
    let version: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("sqlite_version() must answer");

    // Affiché par `--nocapture` ; c'est la valeur que porte le CHANGELOG.
    eprintln!("SQLITE_ENGINE_VERSION={version}");

    // x.y.z — le moteur doit être semver-shapé (parité format production).
    let parts: Vec<&str> = version.split('.').collect();
    assert!(parts.len() >= 2, "sqlite_version() = {version:?}");
    assert!(
        parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
        "sqlite_version() = {version:?} contient des caractères non numériques"
    );

    // Garde anti-recul du moteur embarqué (vigilance F-145) : le moteur ne doit JAMAIS
    // revenir sous 3.46.0 — la version en place avant la montée rusqlite du 2026-08-26.
    // Une substitution de moteur silencieuse (3.46.0 → 3.45.1) est le défaut qui a fait
    // écarter la tentative précédente de montée ; ce test la rend impossible.
    let major: u32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    assert!(
        (major, minor, patch) >= (3, 46, 0),
        "moteur SQLite {version} < plancher 3.46.0 — montée rusqlite interdite si le moteur recule"
    );
}
