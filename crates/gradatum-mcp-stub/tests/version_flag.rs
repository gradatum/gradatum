//! Tests d'intégration du traitement CLI `--version` / `--help`.
//!
//! Propriété gardée : ces flags répondent **sans** aucune variable d'environnement
//! `GRADATUM_*`. Avant le correctif, `main` appelait `StubHandler::from_env()` avant
//! tout traitement d'arguments — `--version` échouait donc sur
//! `Error: StubHandler initialization from env` au lieu d'imprimer une version.
//!
//! Le binaire est lancé avec un environnement **entièrement vidé** (`env_clear`) :
//! c'est la preuve la plus forte que le chemin `from_env()` n'est jamais atteint pour
//! ces flags. Le chemin absolu du binaire est fourni par cargo via
//! `CARGO_BIN_EXE_<name>`, donc l'absence de `PATH` est sans effet.

use std::process::Command;

/// Chemin absolu du binaire compilé, injecté par cargo pour les tests d'intégration.
const BIN: &str = env!("CARGO_BIN_EXE_gradatum-mcp-stub");

#[test]
fn version_flag_responds_without_any_env() {
    let output = Command::new(BIN)
        .arg("--version")
        .env_clear()
        .output()
        .expect("le binaire gradatum-mcp-stub doit être exécutable");

    assert!(
        output.status.success(),
        "--version doit sortir en succès (rc=0) sans environnement — statut : {:?}, stderr : {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("sortie --version en UTF-8");
    let line = stdout.trim();

    // Format exact partagé par les 5 autres binaires : `<nom> <semver> (build_sha <sha>)`.
    assert!(
        line.starts_with("gradatum-mcp-stub "),
        "--version doit préfixer le nom du binaire, trouvé : {line:?}"
    );
    assert!(
        line.contains(&format!("gradatum-mcp-stub {}", env!("CARGO_PKG_VERSION"))),
        "--version doit porter la version du paquet, trouvé : {line:?}"
    );
    assert!(
        line.contains(" (build_sha ") && line.ends_with(')'),
        "--version doit porter le segment '(build_sha <sha>)', trouvé : {line:?}"
    );
}

#[test]
fn short_version_flag_matches_long() {
    // `-V` est l'alias court fourni par clap ; il doit rendre exactement la même
    // chaîne que `--version`, toujours sans environnement.
    let long = Command::new(BIN)
        .arg("--version")
        .env_clear()
        .output()
        .expect("exécution --version");
    let short = Command::new(BIN)
        .arg("-V")
        .env_clear()
        .output()
        .expect("exécution -V");

    assert!(long.status.success() && short.status.success());
    assert_eq!(
        long.stdout, short.stdout,
        "--version et -V doivent produire une sortie identique"
    );
}

#[test]
fn help_flag_responds_without_any_env() {
    let output = Command::new(BIN)
        .arg("--help")
        .env_clear()
        .output()
        .expect("le binaire gradatum-mcp-stub doit être exécutable");

    assert!(
        output.status.success(),
        "--help doit sortir en succès (rc=0) sans environnement — statut : {:?}, stderr : {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("sortie --help en UTF-8");
    // clap documente les variables d'environnement dans le long_about.
    assert!(
        stdout.contains("GRADATUM_SERVER_URL"),
        "--help doit documenter la configuration par environnement, trouvé : {stdout:?}"
    );
}
