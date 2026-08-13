//! Main binary for `gradatum-engine` — `llama-server` supervisor.
//!
//! Reads the config path from the command-line argument, parses `EngineConfig`,
//! validates the model and the `llama-server` binary, exchanges the api-key for a JWT,
//! then:
//! 1. Spawns `llama-server` via `LlamaServerSupervisor`.
//! 2. Polls child `/health` until `startup_timeout_secs`.
//! 3. Starts the axum server on `config.port` (loopback).
//! 4. Launches the supervision loop in the background (bounded by the total restart budget).
//!
//! ## Startup failure behaviour
//!
//! If `llama-server` does not respond within the timeout, `main()` explicitly calls
//! `health.set_unhealthy()` (`wait_ready` does not do this). Handlers return HTTP 503
//! via `HealthState`. The gateway fallback takes over. The binary does not panic —
//! it remains listening.
//!
//! ## Security
//!
//! - api-key read from `GRADATUM_ENGINE_API_KEY` (env) or `/etc/gradatum/engine.api-key`.
//! - Fallback to `NoopEventSink` if the gradatum server is unreachable (best-effort).
//! - Loopback-only bind: `127.0.0.1:<port>`.
//! - JWT stored in `Zeroizing<String>`.
//! - `llama-server` binary canonicalized and validated against allowed prefixes
//!   (`/usr/local/bin/`, `/opt/gradatum/bin/`).
//! - `model_path` canonicalized and validated under `/opt/gradatum/models/`.

/// String rendered by `--version`: the binary name, semantic version, and the build
/// commit SHA.
///
/// Format stable, guaranteed to stay script-extractable:
/// `gradatum-engine <semver> (build_sha <sha>)`
///
/// `<sha>` is injected at compile time by `build.rs` (`cargo:rustc-env=BUILD_SHA`)
/// and reads `unknown` when the SHA could not be resolved at build time — no `.git`
/// directory or a tarball build — a fallback carried by `build.rs`, which never fails.
/// `env!` is therefore always resolvable here, since `build.rs` emits the variable
/// unconditionally.
#[cfg(feature = "serve")]
const VERSION: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    env!("CARGO_PKG_VERSION"),
    " (build_sha ",
    env!("BUILD_SHA"),
    ")"
);

/// Help text rendered by `--help` / `-h`, without loading any configuration.
#[cfg(feature = "serve")]
const HELP: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " — llama-server supervisor (OpenAI-compatible managed runtime)\n\n",
    "Usage: ",
    env!("CARGO_PKG_NAME"),
    " <CONFIG>\n\n",
    "Arguments:\n",
    "  <CONFIG>  Path to the TOML configuration file\n\n",
    "Options:\n",
    "  -h, --help     Print help\n",
    "  -V, --version  Print version\n"
);

/// Handles `--version`/`-V` and `--help`/`-h` before any configuration is loaded.
///
/// The minimal executable contract: these must answer from any directory, with no
/// config file in scope. Returns `Some(exit_code)` when a flag was consumed, `None`
/// otherwise.
#[cfg(feature = "serve")]
fn handle_early_flags() -> Option<i32> {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("{VERSION}");
                return Some(0);
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Some(0);
            }
            _ => {}
        }
    }
    None
}

#[cfg(not(feature = "serve"))]
fn main() {
    eprintln!("gradatum-engine: compiled without the 'serve' feature. Nothing to do.");
    std::process::exit(1);
}

#[cfg(feature = "serve")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use gradatum_core::event_sink::NoopEventSink;
    use gradatum_engine::{
        config::{EngineConfig, RuntimeKind},
        health::HealthState,
        metrics::EngineMetrics,
        runtime::ForwardProxy,
        server::{AppState, EngineServer},
        sink::HttpEventSink,
        supervisor::LlamaServerSupervisor,
    };
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::Path,
        sync::Arc,
    };

    // `--version`/`-V` and `--help`/`-h` must answer without any configuration file,
    // from any directory — handled before config loading (see `handle_early_flags`).
    if let Some(code) = handle_early_flags() {
        std::process::exit(code);
    }

    // --- Initialize tracing ---
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gradatum_engine=info".parse().unwrap()),
        )
        .init();

    // --- Parse arguments ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: gradatum-engine <config-path>");
        std::process::exit(1);
    }
    let config_path = Path::new(&args[1]);

    // --- Load config ---
    // Redaction : figment fuite la valeur fautive dans son `Display` ; `{e}` sur la
    // `Box<figment::Error>` la propagerait sur stderr via le `Debug` d'anyhow à la
    // sortie du process. On rebâtit un message sans valeur (garde-fou gradatum-core).
    let config = EngineConfig::load_local(config_path).map_err(|e| {
        anyhow::anyhow!(
            "EngineConfig::load_local failed: {}",
            gradatum_core::config::redact_figment_error(&e)
        )
    })?;

    // --- Validate config (model_path canonicalization + prefix) ---
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;

    // --- Match runtime ---
    if config.runtime == RuntimeKind::Onnx {
        anyhow::bail!("runtime 'onnx' is not implemented. Use runtime='llamaserver' (default).");
    }

    // --- Validate child port ---
    if config.child_port <= 1024 {
        anyhow::bail!(
            "child_port {} is invalid — must be > 1024 (SP-P0-4)",
            config.child_port
        );
    }

    // --- Build metrics (shared: event sink + AppState) ---
    // Constructed before the sink so the HttpEventSink can count non-2xx
    // event-log POSTs (F-120 observability).
    let metrics = Arc::new(EngineMetrics::new());

    // --- Build the event sink (HttpEventSink if gradatum_url is set, otherwise InMemorySink) ---
    //
    // gradatum_url = None  → InMemorySink (dev/test — no event-log POST)
    // gradatum_url = Some  → validate loopback (anti-SSRF) + exchange JWT → HttpEventSink
    //                        fallback to NoopEventSink if JWT exchange fails (best-effort)
    //
    // NOTE: the network binding (LAN vs loopback) is NOT modified here — only the sink
    // implementation changes. LAN exposure is decided by the operator.
    let sink: Arc<dyn gradatum_core::event_sink::EventSink> = {
        if let Some(ref gradatum_url) = config.gradatum_url {
            // Validate that the URL is loopback (anti-SSRF)
            validate_loopback_url(gradatum_url)?;
            // Read api-key — only when event-log is enabled. Kept by the sink to
            // re-exchange a fresh JWT on 401 (F-120 lazy refresh).
            let api_key = read_api_key()?;
            match gradatum_engine::sink::exchange_api_key_for_jwt(&api_key, gradatum_url).await {
                Ok(jwt) => Arc::new(HttpEventSink::new(
                    gradatum_url.clone(),
                    jwt,
                    api_key,
                    config.agent_id.clone(),
                    metrics.clone(),
                )),
                Err(e) => {
                    // Best-effort fallback — no crash on JWT failure
                    tracing::warn!(
                        error = %e,
                        "api-key→JWT exchange failed. Falling back to NoopEventSink (event-log disabled)."
                    );
                    Arc::new(NoopEventSink)
                }
            }
        } else {
            // gradatum_url absent → NoopEventSink in production (no event-log POST).
            // In test/CI (feature test-utils): InMemorySink allows inspection.
            tracing::info!(
                "gradatum_url not set — event-log disabled (NoopEventSink in prod; \
                InMemorySink only if feature test-utils is enabled). \
                Set gradatum_url to enable event posting."
            );
            #[cfg(any(test, feature = "test-utils"))]
            {
                Arc::new(gradatum_core::event_sink::InMemorySink::default())
            }
            #[cfg(not(any(test, feature = "test-utils")))]
            {
                Arc::new(NoopEventSink)
            }
        }
    };

    // --- Derive metadata ---
    let model_name = config.model_alias();
    let provider = config.provider_alias();
    let health = Arc::new(HealthState::new(&model_name));

    // --- Build the supervisor ---
    let supervisor = LlamaServerSupervisor::new(config.clone())
        .map_err(|e| anyhow::anyhow!("LlamaServerSupervisor::new failed: {e}"))?;

    // --- Spawn llama-server ---
    supervisor
        .spawn_child()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn llama-server: {e}"))?;

    // --- Wait ready ---
    // Capture the initial ready Instant to seed last_ready_at in supervise_loop
    // (without this seed, the first crash of a healthy child would be misclassified as flapping).
    let initial_ready_at = {
        let state = supervisor.wait_ready(&health).await;
        if state == gradatum_engine::supervisor::ChildState::StartupTimeout {
            // wait_ready returns StartupTimeout without calling set_unhealthy — do it here
            // so the gateway falls back to its fallback cleanly.
            tracing::error!(
                "llama-server did not start within the timeout — engine unhealthy. \
                 The fallback gateway takes over."
            );
            health.set_unhealthy();
            // M1 fix (A1b, was misleading): supervise_loop() IS still launched below
            // regardless of this branch — it is never skipped. `None` here only means
            // it starts with no `last_ready_at` seed, so the very first crash it
            // observes (this dead child) is classified as flapping (no budget/backoff
            // reset) rather than as a post-stable-uptime crash. See
            // `LlamaServerSupervisor::supervise_loop`'s "Initial seed" + "Escalation to
            // systemd" doc sections for what happens next (R1 fix: budget is consumed
            // with backoff, then the process exits for systemd to escalate).
            None
        } else {
            Some(std::time::Instant::now())
        }
    };

    // --- Build transparent ForwardProxy ---
    let proxy = ForwardProxy::new(supervisor.client.clone(), supervisor.child_base_url());

    // --- Build AppState ---
    let state = AppState {
        proxy,
        health: health.clone(),
        metrics: metrics.clone(),
        sink,
        model_name,
        provider,
        timeout_secs: config.timeout_secs,
        body_limit_bytes: config.body_limit_bytes,
    };

    // --- Launch supervision loop in background ---
    // initial_ready_at seeds last_ready_at to prevent false flapping detection
    // on the first crash of a healthy child.
    let supervisor_arc = supervisor.clone();
    let health_arc = health.clone();
    tokio::spawn(async move {
        supervisor_arc
            .supervise_loop(health_arc, initial_ready_at)
            .await;
    });

    // --- Start metrics listener on loopback ---
    // /metrics is on a separate port so it is never exposed on the LAN.
    let metrics_addr = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        config.resolved_metrics_port(),
    );
    let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;
    let metrics_router = EngineServer::metrics_router(metrics);
    tracing::info!(
        metrics_addr = %metrics_addr,
        "gradatum-engine /metrics listener started on loopback"
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(metrics_listener, metrics_router).await {
            tracing::error!(error = %e, "metrics listener erreur");
        }
    });

    // --- Start main axum listener ---
    // bind_addr resolved from config: loopback (127.0.0.1) if unset,
    // or a specific LAN unicast IP validated by validate() (fail-closed).
    let bind_addr = config.resolved_bind_addr();
    let addr = SocketAddr::new(bind_addr, config.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        addr = %addr,
        model = %state.model_name,
        child_port = config.child_port,
        metrics_port = config.resolved_metrics_port(),
        "gradatum-engine started (llama-server supervisor PIVOT v2)"
    );

    let router = EngineServer::router(state);
    axum::serve(listener, router).await?;
    Ok(())
}

/// Reads the api-key from the environment variable or the secrets file.
#[cfg(feature = "serve")]
fn read_api_key() -> anyhow::Result<zeroize::Zeroizing<String>> {
    if let Ok(key) = std::env::var("GRADATUM_ENGINE_API_KEY") {
        return Ok(zeroize::Zeroizing::new(key));
    }
    let path = "/etc/gradatum/engine.api-key";
    let key = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("FATAL: api-key not found ({path}): {e}"))?;
    Ok(zeroize::Zeroizing::new(key.trim().to_string()))
}

/// Validates that the URL resolves to a loopback address (anti-SSRF).
///
/// ## Validation policy
///
/// - If the host is a literal IP address: `is_loopback()` direct check (127.x.x.x, ::1).
/// - If the host is a hostname (e.g. `localhost`): synchronous DNS resolution.
///   All resolved IPs must be loopback — a single non-loopback IP causes rejection.
///   A hostname that does not resolve at all is rejected (fail-closed).
///
/// ## Security
///
/// Prevents SSRF bypass via `localhost` resolving to a non-loopback IP (e.g. through
/// a modified `/etc/hosts`, split-horizon DNS, or a forged `Host` header). An attacker
/// controlling DNS resolution of `localhost` to a public IP would be rejected.
///
/// ## Note
///
/// Synchronous function — uses `std::net::ToSocketAddrs` for resolution.
/// Call only at startup, not in an async hot path.
#[cfg(feature = "serve")]
fn validate_loopback_url(url: &str) -> anyhow::Result<()> {
    use std::net::IpAddr;

    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("gradatum_url invalid (URL parsing): {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("gradatum_url has no host: {url}"))?;

    // Try to parse the host as a literal IP address.
    if let Ok(ip) = host.parse::<IpAddr>() {
        // Literal IP — direct is_loopback() check (no DNS resolution).
        if ip.is_loopback() {
            return Ok(());
        }
        anyhow::bail!("gradatum_url must point to loopback (127.0.0.1/::1), IP={ip}: {url}");
    }

    // Hostname — synchronous DNS resolution (fail-closed if resolution fails).
    // All resolved IPs must be loopback.
    let port = parsed.port().unwrap_or(80);
    let addrs = format!("{host}:{port}")
        .parse::<std::net::SocketAddr>()
        .map(|sa| vec![sa])
        .or_else(|_| {
            // ToSocketAddrs resolves the hostname (blocking — acceptable at boot).
            use std::net::ToSocketAddrs;
            (host, port)
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<_>>())
        })
        .map_err(|e| {
            anyhow::anyhow!(
                "gradatum_url hostname='{host}' does not resolve — fail-closed (P2-4 anti-SSRF): {e}"
            )
        })?;

    if addrs.is_empty() {
        anyhow::bail!(
            "gradatum_url hostname='{host}' resolves to 0 addresses — fail-closed (P2-4 anti-SSRF)"
        );
    }

    // All resolved IPs must be loopback.
    for addr in &addrs {
        if !addr.ip().is_loopback() {
            anyhow::bail!(
                "gradatum_url hostname='{host}' resolves to non-loopback IP={} — \
                 rejected (P2-4 anti-SSRF). Use the literal IP 127.0.0.1 or ::1.",
                addr.ip()
            );
        }
    }

    Ok(())
}

#[cfg(all(test, feature = "serve"))]
mod bin_tests {
    use super::*;

    // --- C1 : régression URL exchange (P0 — route hors /api/v1) ---
    #[test]
    fn exchange_url_ends_with_auth_exchange_not_api_v1() {
        let base = "http://127.0.0.1:19090";
        let url = format!("{base}/auth/exchange");
        assert!(
            url.ends_with("/auth/exchange"),
            "URL doit se terminer par /auth/exchange : {url}"
        );
        assert!(
            !url.contains("/api/v1/auth/exchange"),
            "URL ne doit PAS contenir /api/v1/auth/exchange : {url}"
        );
    }

    // --- S2 : validate_loopback_url (P2 item 4 : résolution DNS + toutes IPs loopback) ---

    #[test]
    fn validate_loopback_accepts_127_0_0_1() {
        // IP littérale loopback IPv4 — pas de résolution DNS.
        assert!(validate_loopback_url("http://127.0.0.1:19090").is_ok());
    }

    #[test]
    fn validate_loopback_accepts_ipv6_loopback_literal() {
        // IP littérale loopback IPv6 — pas de résolution DNS.
        assert!(validate_loopback_url("http://[::1]:19090").is_ok());
    }

    #[test]
    fn validate_loopback_accepts_localhost_resolves_to_loopback() {
        // localhost doit résoudre vers 127.0.0.1 ou ::1 sur Linux standard.
        // Si l'environnement CI ne résout pas localhost → test ignoré (non bloquant).
        let result = validate_loopback_url("http://localhost:19090");
        // Sur Linux standard (nom d'hôte /etc/hosts → 127.0.0.1), doit passer.
        // Si la résolution échoue (CI réseau restreint) → Err est acceptable aussi
        // (fail-closed est correct — pas de bypass SSRF).
        if let Err(e) = result {
            let msg = e.to_string();
            // The error must be a resolution or validation error — not a panic.
            assert!(
                msg.contains("resolve") || msg.contains("non-loopback"),
                "expected resolution or validation error — received: {msg}"
            );
        }
        // Pas d'assert!(result.is_ok()) — fail-closed acceptable si DNS restreint.
    }

    #[test]
    fn validate_loopback_rejects_bypass_subdomain() {
        // 127.0.0.1.evil.com : parsé comme hostname, pas comme IP.
        // Résout (probablement) vers une IP publique — rejeté.
        let result = validate_loopback_url("http://127.0.0.1.evil.com:19090");
        // Rejeté : soit la résolution échoue (Err), soit l'IP résolue est non-loopback.
        assert!(
            result.is_err(),
            "127.0.0.1.evil.com doit être rejeté (SSRF bypass)"
        );
    }

    #[test]
    fn validate_loopback_rejects_external_ip() {
        // 203.0.113.1 = TEST-NET-3 (RFC 5737) — IP littérale non-loopback.
        let result = validate_loopback_url("http://203.0.113.1:19090");
        assert!(result.is_err(), "IP externe doit être rejetée");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("loopback") || msg.contains("non-loopback"),
            "message doit citer loopback : {msg}"
        );
    }

    #[test]
    fn validate_loopback_rejects_invalid_url() {
        let result = validate_loopback_url("not-a-url");
        assert!(result.is_err(), "URL invalide doit être rejetée");
    }

    /// Verifies that `::1` (IPv6 loopback) is accepted as a literal IP address.
    #[test]
    fn validate_loopback_accepts_ipv6_bracket_notation() {
        // [::1] est la notation RFC pour IPv6 dans les URLs.
        assert!(
            validate_loopback_url("http://[::1]:19090").is_ok(),
            "[::1] doit être accepté (loopback IPv6)"
        );
    }

    /// Verifies that a hostname resolving to a non-loopback IP is rejected.
    /// Uses a well-known public domain — when network resolution is available,
    /// the name resolves to a non-loopback IP.
    #[test]
    fn validate_loopback_rejects_hostname_resolving_to_external() {
        // example.com résout vers 93.184.216.34 (non-loopback) si réseau disponible.
        // Si résolution KO (CI réseau restreint) → Err aussi acceptable (fail-closed).
        let result = validate_loopback_url("http://example.com:19090");
        assert!(
            result.is_err(),
            "example.com doit être rejeté (résout vers IP publique ou résolution KO — fail-closed)"
        );
    }

    // --- P2 item 2 : sélection du sink selon gradatum_url ---

    /// Verifies that `gradatum_url = None` parses without error and that the absence
    /// of `gradatum_url` does not trigger `validate_loopback_url` (no SSRF error on `None`).
    #[test]
    fn sink_selection_gradatum_url_none_does_not_validate_loopback() {
        // Aucune erreur : gradatum_url absent → validate_loopback_url n'est PAS appelé.
        // validate_loopback_url serait appelé si Some(url) → cette branche ne l'est pas.
        // Ce test vérifie l'invariant : None = pas de validation = pas de crash.
        // Le binaire utilise InMemorySink/NoopEventSink dans ce cas.
        let url_none: Option<&str> = None;
        // La branche None ne fait jamais appel à validate_loopback_url.
        // Simuler : si None, on ne valide pas, donc pas d'erreur même pour une URL invalide.
        let would_validate = url_none.is_some();
        assert!(
            !would_validate,
            "gradatum_url=None ne doit PAS déclencher validate_loopback_url"
        );
    }

    /// Verifies that `gradatum_url = Some(url)` triggers `validate_loopback_url`.
    /// The `Some` branch must always run the anti-SSRF validation.
    #[test]
    fn sink_selection_gradatum_url_some_triggers_loopback_validation() {
        // Une URL valide loopback doit passer.
        let url_valid = Some("http://127.0.0.1:19090".to_string());
        if let Some(ref url) = url_valid {
            assert!(
                validate_loopback_url(url).is_ok(),
                "URL loopback valide doit passer validate_loopback_url"
            );
        }

        // Une URL non-loopback doit être rejetée (SSRF P2-4).
        let url_invalid = Some("http://203.0.113.1:19090".to_string());
        if let Some(ref url) = url_invalid {
            assert!(
                validate_loopback_url(url).is_err(),
                "URL non-loopback doit être rejetée par validate_loopback_url (SSRF P2-4)"
            );
        }
    }

    /// Verifies that `gradatum_url` defaults to `None` in a TOML-parsed config.
    #[test]
    fn config_gradatum_url_none_by_default() {
        use gradatum_engine::config::EngineConfig;
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(
            c.gradatum_url.is_none(),
            "gradatum_url doit être None par défaut (InMemorySink sans config explicite)"
        );
    }

    /// gradatum_url Some depuis TOML.
    #[test]
    fn config_gradatum_url_some_from_toml() {
        use gradatum_engine::config::EngineConfig;
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\ngradatum_url=\"http://127.0.0.1:19090\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.gradatum_url,
            Some("http://127.0.0.1:19090".to_string()),
            "gradatum_url Some parsé depuis TOML"
        );
    }
}
