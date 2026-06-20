//! Tests d'intégration pour [`EnvSecretsProvider`] — mutation d'env nécessitant `unsafe`.
//!
//! Ces tests sont déplacés ici (crate d'intégration séparé) depuis `src/secrets.rs`
//! pour préserver `#![forbid(unsafe_code)]` dans la lib.
//!
//! En Rust 2024, `std::env::set_var` et `remove_var` sont `unsafe` car non-thread-safe.
//! Un crate d'intégration n'hérite pas du `#![forbid]` de la lib — l'`unsafe` est donc
//! autorisé ici, uniquement dans le contexte des tests.

use gradatum_core::secrets::SecretsProvider;
use gradatum_core::secrets::{EnvSecretsProvider, SecretsError};
use secrecy::ExposeSecret;

/// Recalcule le nom de variable d'environnement canonique pour une clé.
///
/// Duplique la logique de `EnvSecretsProvider::env_var_name` (pub(crate)), qui n'est pas
/// accessible depuis un crate d'intégration. La logique est triviale et stable.
fn env_var_name(key: &str) -> String {
    let upper = key.to_uppercase().replace('-', "_");
    format!("GRADATUM_SECRET_{upper}")
}

/// Verrou de sérialisation des tests qui mutent l'env.
///
/// `std::env::set_var` / `remove_var` ne sont pas thread-safe (Rust 2024 RFC).
/// Ce mutex garantit l'exécution séquentielle des tests env de ce fichier.
/// Les autres crates ou threads ne mutent pas les mêmes variables (noms uniques).
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn env_provider_retourne_la_valeur_presente() {
    let _guard = ENV_MUTEX.lock().unwrap();

    let key = "test-key-env-present";
    let var = env_var_name(key);

    // SAFETY: mutation d'env en test mono-thread (sérialisé par ENV_MUTEX),
    // aucune autre lecture concurrente de cette variable dans ce processus de test.
    unsafe { std::env::remove_var(&var) };
    // SAFETY: idem.
    unsafe { std::env::set_var(&var, "valeur-test") };

    let provider = EnvSecretsProvider;
    let result = provider.get(key).expect("doit réussir");
    assert_eq!(result.expose_secret(), b"valeur-test");

    // Nettoyage.
    // SAFETY: idem.
    unsafe { std::env::remove_var(&var) };
}

#[test]
fn env_provider_retourne_not_found_si_absent() {
    let _guard = ENV_MUTEX.lock().unwrap();

    let key = "test-key-env-absent-xyz";
    let var = env_var_name(key);

    // SAFETY: mutation d'env en test mono-thread (sérialisé par ENV_MUTEX),
    // aucune autre lecture concurrente de cette variable dans ce processus de test.
    unsafe { std::env::remove_var(&var) };

    let provider = EnvSecretsProvider;
    let err = provider.get(key).expect_err("doit échouer");
    assert!(
        matches!(err, SecretsError::NotFound { .. }),
        "attendu NotFound, obtenu: {err:?}"
    );
}
