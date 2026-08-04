//! HARDEN-CONFIG défaut 1 — `revocation_store` doit refuser toute valeur inconnue.
//!
//! Invariant visé : une valeur invalide (coquille, casse erronée) doit **empêcher le
//! démarrage**. Avant durcissement, toute troisième valeur franchissait la garde
//! [`gradatum_auth::revocation::boot_guard_check`] (qui ne rejette que la chaîne exacte
//! `"memory"`) ET sélectionnait `InMemoryRevocationStore` dans `main.rs` (qui ne
//! sélectionne SQLite que sur la chaîne exacte `"sqlite"`) : les révocations de tokens
//! étaient alors perdues à chaque redémarrage, signalées par un simple `warn!`.

use std::io::Write as _;

use gradatum_server::config::{ConfigError, RevocationStoreKind, ServerConfig};

/// Écrit un `server.toml` minimal et le charge via le chemin de production
/// [`ServerConfig::load`] (defaults figment → TOML → env).
fn load_from_toml(body: &str) -> Result<ServerConfig, ConfigError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("server.toml");
    let mut f = std::fs::File::create(&path).expect("create server.toml");
    f.write_all(body.as_bytes()).expect("write server.toml");
    f.sync_all().expect("sync server.toml");
    ServerConfig::load(Some(&path))
}

#[test]
fn unknown_revocation_store_value_is_rejected_at_load() {
    // `"Memory"` n'est ni `"sqlite"` ni `"memory"` : c'est exactement la troisième
    // valeur qui franchissait la garde tout en activant le store DEV.
    let err = load_from_toml("[auth]\nrevocation_store = \"Memory\"\n")
        .expect_err("une valeur revocation_store inconnue doit refuser le démarrage");
    let msg = err.to_string();
    assert!(
        msg.contains("revocation_store") || msg.contains("unknown variant"),
        "le message d'erreur doit désigner le champ fautif, obtenu : {msg}"
    );
}

#[test]
fn typo_revocation_store_value_is_rejected_at_load() {
    // Coquille plausible : `"mem"`. Même classe de défaut que `"Memory"`.
    load_from_toml("[auth]\nrevocation_store = \"mem\"\n")
        .expect_err("une coquille revocation_store doit refuser le démarrage");
}

#[test]
fn live_config_shape_still_loads_and_serialises_to_sqlite() {
    // Forme de la config LIVE : `revocation_store = "sqlite"`. Doit charger, et la
    // représentation sérialisée doit rester la chaîne `"sqlite"` — c'est cette
    // représentation qui est écrite dans `server.toml` et lue par la garde.
    let cfg = load_from_toml("[auth]\nrevocation_store = \"sqlite\"\n")
        .expect("la forme de config LIVE doit charger sans erreur");
    assert_eq!(
        serde_json::to_value(cfg.auth.revocation_store).expect("serialise revocation_store"),
        serde_json::Value::String("sqlite".to_string()),
        "la représentation sur le fil doit rester `sqlite` (invariance LIVE)"
    );
}

#[test]
fn memory_store_shape_still_loads_and_serialises_to_memory() {
    // `"memory"` reste une valeur VALIDE (utilisée par les tests worker/curator_config
    // et par server_bind_validation) : le durcissement refuse l'inconnu, pas le connu.
    let cfg = load_from_toml("[auth]\nrevocation_store = \"memory\"\n")
        .expect("`memory` doit rester une valeur acceptée");
    assert_eq!(
        serde_json::to_value(cfg.auth.revocation_store).expect("serialise revocation_store"),
        serde_json::Value::String("memory".to_string()),
    );
}

#[test]
fn as_str_agrees_with_serde_for_every_variant() {
    // `as_str()` est l'unique pont vers `boot_guard_check`, dont la signature `&str` fait
    // partie de l'API publiée de `gradatum-auth`. Si cette conversion divergeait de la
    // représentation serde, la garde discriminerait une chaîne que la config ne produit
    // jamais — soit exactement l'asymétrie que ce lot supprime. Ce test la verrouille.
    for kind in [RevocationStoreKind::Sqlite, RevocationStoreKind::Memory] {
        assert_eq!(
            serde_json::to_value(kind).expect("serialise variant"),
            serde_json::Value::String(kind.as_str().to_string()),
            "as_str() diverge de la représentation serde pour {kind:?}"
        );
    }
}

#[test]
fn memory_store_is_still_refused_by_the_boot_guard_on_non_loopback() {
    // La garde reste la défense C2 : `memory` explicite + bind non-loopback = refus.
    // Le durcissement ne l'affaiblit pas, il garantit qu'aucune autre valeur ne l'atteint.
    gradatum_auth::revocation::boot_guard_check(false, RevocationStoreKind::Memory.as_str())
        .expect_err("memory + bind non-loopback doit rester refusé (caveat C2)");
    gradatum_auth::revocation::boot_guard_check(false, RevocationStoreKind::Sqlite.as_str())
        .expect("sqlite doit rester accepté sur bind non-loopback");
}
