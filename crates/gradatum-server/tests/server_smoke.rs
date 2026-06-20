//! Smoke test : boot gradatum-server + SIGTERM drain propre.
//!
//! Ce test lance le binaire compilé, attend la ligne `listening on <addr>` sur stdout
//! (readiness déterministe — pas de sleep arbitraire), poll `/health` jusqu'à HTTP 200,
//! envoie SIGTERM et attend une sortie propre (code 0) dans les 5s.

use std::io::BufRead as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;

/// Lit les lignes stdout du processus enfant jusqu'à trouver `listening on <addr>`.
///
/// Retourne l'adresse `SocketAddr` extraite ou panics si aucune ligne correspondante
/// n'arrive dans le délai imparti (`timeout`).
///
/// # Panics
/// Panics si le timeout expire avant la ligne attendue, ou si stdout n'est pas disponible.
fn wait_for_listening_addr(
    stdout: std::process::ChildStdout,
    timeout: Duration,
) -> (SocketAddr, std::process::ChildStdout) {
    use std::io::BufReader;

    let deadline = std::time::Instant::now() + timeout;
    let mut reader = BufReader::new(stdout);

    loop {
        let mut line = String::new();
        // BufReader::read_line est bloquant — acceptable dans un helper de test.
        match reader.read_line(&mut line) {
            Ok(0) => panic!("stdout du serveur fermé avant 'listening on'"),
            Ok(_) => {}
            Err(e) => panic!("erreur lecture stdout serveur : {e}"),
        }

        if let Some(addr) = parse_listening_addr(line.trim()) {
            // SAFETY : décomposer le BufReader pour récupérer le ChildStdout sous-jacent.
            // into_inner() est infaillible sur BufReader<R>.
            return (addr, reader.into_inner());
        }

        if std::time::Instant::now() > deadline {
            panic!(
                "le serveur n'a pas émis 'listening on' dans les {}s",
                timeout.as_secs()
            );
        }
    }
}

/// Parse `listening on 127.0.0.1:<port>` ou `listening on [::1]:<port>`.
fn parse_listening_addr(line: &str) -> Option<SocketAddr> {
    // Format : "listening on <addr>" — l'adresse est le dernier token.
    let addr_str = line.strip_prefix("listening on ")?;
    addr_str.parse().ok()
}

#[tokio::test]
async fn server_boots_serves_health_and_shuts_down_clean() {
    let tmp = TempDir::new().expect("créer répertoire temporaire");
    let cfg = format!(
        r#"
[server]
bind = "127.0.0.1:0"
metrics_bind = "127.0.0.1:0"
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

    // Lancer le serveur avec stdout capturé pour lire l'adresse bound.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-server"))
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "error") // réduire le bruit — seul stdout nous importe
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawner le processus gradatum-server");

    // Extraire stdout avant d'appeler wait_for_listening_addr (take() consomme l'Option).
    let child_stdout = child
        .stdout
        .take()
        .expect("stdout piped — disponible après spawn");

    // Lire "listening on <addr>" depuis stdout (bloquant dans un spawn_blocking pour ne pas
    // bloquer le runtime tokio sous-jacent du test).
    let addr = tokio::task::spawn_blocking(move || {
        // Timeout 10s : suffisant même sur CI surchargé.
        let (addr, _stdout) = wait_for_listening_addr(child_stdout, Duration::from_secs(10));
        addr
    })
    .await
    .expect("spawn_blocking wait_for_listening_addr");

    // Poll GET /health jusqu'à HTTP 200 (timeout 5s, interval 100ms).
    // Utilise reqwest async (feature rustls déjà active dans les dev-deps workspace).
    let health_url = format!("http://{addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("construire client HTTP");

    let start = std::time::Instant::now();
    loop {
        match client.get(&health_url).send().await {
            Ok(r) if r.status().is_success() => break,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(5) {
            // Tuer le process avant de paniquer pour éviter les orphelins.
            let _ = child.kill();
            panic!("GET /health non-200 dans les 5s (addr={addr})");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Le serveur répond : il est vivant.
    // Vérifier quand même que le process est actif (pas d'exit anticipé).
    assert!(
        child.try_wait().expect("vérifier état processus").is_none(),
        "le serveur s'est arrêté prématurément après le boot"
    );

    // Envoi SIGTERM.
    let pid = child.id() as i32;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("envoyer SIGTERM");

    // Attendre la sortie propre (code 0) dans les 5s.
    // Utiliser spawn_blocking car child.wait() est bloquant (std::process::Child).
    let exit = tokio::task::spawn_blocking(move || {
        // Timeout manuel : boucler avec try_wait + sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().expect("try_wait après SIGTERM") {
                Some(status) => return status,
                None => {
                    if std::time::Instant::now() > deadline {
                        panic!("le serveur ne s'est pas arrêté dans les 5s après SIGTERM");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    })
    .await
    .expect("spawn_blocking wait exit");

    assert!(
        exit.success(),
        "code de sortie non-zero après SIGTERM: {:?}",
        exit
    );
}
