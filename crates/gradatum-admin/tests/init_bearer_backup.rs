//! Tests backup atomique `bearer.toml`.
//!
//! Valide les deux scénarios du backup minimaliste :
//! 1. `bearer.toml` préexistant → backup `.bak.<ISO-TS>` créé, contenu original préservé,
//!    fichier actif réécrit avec le preset.
//! 2. Fresh install (pas de `bearer.toml`) → aucun backup créé.

use gradatum_admin::materialize_preset;
use std::fs;
use tempfile::TempDir;

/// Vérifie qu'un `bearer.toml` existant est backupé avant écrasement.
///
/// Cas terrain : `install-gradatum-services.sh --force` sur une instance LIVE avec un
/// `bearer.toml` contenant des entrées consumer customisées.
#[test]
fn materialize_preset_backups_existing_bearer_toml() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let bearer = config_dir.join("bearer.toml");

    let custom_content = r#"
# Customisation user — doit être backupé avant écrasement.
[[consumer]]
identity = "custom-user-bot"
read_patterns = ["custom/**"]
write_patterns = ["custom/**"]
sees_personal_classified = false
"#;
    fs::write(&bearer, custom_content).unwrap();

    // materialize_preset doit détecter le fichier existant, le backuper, puis écraser.
    materialize_preset(tmp.path(), "hierarchical", Some("main"))
        .expect("materialize_preset ne doit pas échouer");

    // Vérifier qu'exactement 1 fichier .bak.<TS> existe dans config/
    let backups: Vec<_> = fs::read_dir(&config_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("bearer.toml.bak.")
        })
        .collect();

    assert_eq!(backups.len(), 1, "exactement 1 backup attendu");

    // Le backup doit contenir le contenu original
    let backup_content = fs::read_to_string(backups[0].path()).unwrap();
    assert!(
        backup_content.contains("custom-user-bot"),
        "le backup doit contenir le contenu original customisé"
    );

    // Le bearer.toml actuel doit avoir été réécrit avec le preset hierarchical.
    // Le preset "hierarchical" embarqué contient l'identité "maintainer".
    let new_content = fs::read_to_string(&bearer).unwrap();
    assert!(
        new_content.contains("identity = \"maintainer\""),
        "bearer.toml doit être réécrit avec le preset hierarchical (identity maintainer)"
    );
    assert!(
        !new_content.contains("custom-user-bot"),
        "le custom consumer ne doit pas apparaître dans le bearer.toml réécrit \
        (merge consumer-aware out of scope patch.3)"
    );
}

/// Vérifie qu'aucun backup n'est créé lors d'un fresh install (pas de `bearer.toml` existant).
#[test]
fn materialize_preset_no_backup_on_fresh_install() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    // Pas de bearer.toml préexistant.
    materialize_preset(tmp.path(), "hierarchical", Some("main"))
        .expect("materialize_preset ne doit pas échouer sur fresh install");

    // Aucun fichier backup ne doit avoir été créé
    let has_backup = fs::read_dir(&config_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("bearer.toml.bak.")
        });

    assert!(!has_backup, "aucun backup attendu sur fresh install");

    // Le bearer.toml doit avoir été créé
    assert!(
        config_dir.join("bearer.toml").exists(),
        "bearer.toml doit être créé sur fresh install"
    );
}
