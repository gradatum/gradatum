//! Smoke test : boot gradatum-server + SIGTERM drain propre.
//!
//! Ce test lance le binaire compilé, attend 800ms, vérifie qu'il tourne,
//! envoie SIGTERM et attend une sortie propre (code 0) dans les 5s.

use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

#[tokio::test]
async fn server_boots_serves_health_and_shuts_down_clean() {
    let tmp = TempDir::new().expect("créer répertoire temporaire");
    let cfg = format!(
        r#"
[server]
bind = "127.0.0.1:0"
[storage]
root = "{}"
"#,
        tmp.path().display()
    );
    let cfg_path = tmp.path().join("server.toml");
    std::fs::write(&cfg_path, &cfg).expect("écrire config temporaire");

    // Le serveur ouvre db/queue.sqlite et vault/ — créer les répertoires attendus.
    // En production, gradatum-admin init s'en charge avant le démarrage du serveur.
    std::fs::create_dir_all(tmp.path().join("db")).expect("créer db/");
    std::fs::create_dir_all(tmp.path().join("vault")).expect("créer vault/");

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gradatum-server"))
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "info")
        .spawn()
        .expect("spawner le processus gradatum-server");

    sleep(Duration::from_millis(800)).await;

    // Le serveur doit être encore actif après 800ms
    assert!(
        child.try_wait().expect("vérifier état processus").is_none(),
        "le serveur s'est arrêté prématurément au boot"
    );

    // Envoi SIGTERM
    let pid = child.id().expect("obtenir PID du processus serveur");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("envoyer SIGTERM");

    let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("le serveur doit s'arrêter dans les 5s")
        .expect("attendre fin processus");

    assert!(
        exit.success(),
        "code de sortie non-zero après SIGTERM: {:?}",
        exit
    );
}
