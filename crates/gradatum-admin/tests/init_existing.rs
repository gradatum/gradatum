use tempfile::TempDir;

#[test]
fn init_refuses_existing_without_force() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Première init — doit réussir.
    // CWD arbitraire : le preset "hierarchical" est embarqué dans le binaire.
    let s1 = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .arg("--preset")
        .arg("hierarchical")
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir())
        .status()
        .unwrap();
    assert!(s1.success(), "première init a échoué");

    // Deuxième init sans --force — doit échouer.
    let s2 = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .arg("--preset")
        .arg("hierarchical")
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir())
        .status()
        .unwrap();
    assert!(
        !s2.success(),
        "deuxième init sans --force aurait dû échouer"
    );
}

#[test]
fn init_force_overrides() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Init initiale.
    let s1 = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .arg("--preset")
        .arg("hierarchical")
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir())
        .status()
        .unwrap();
    assert!(s1.success(), "init initiale a échoué");

    // Re-init avec --force — doit réussir.
    let s2 = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .arg("--preset")
        .arg("hierarchical")
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .arg("--force")
        .current_dir(std::env::temp_dir())
        .status()
        .unwrap();
    assert!(s2.success(), "re-init avec --force a échoué");
}
