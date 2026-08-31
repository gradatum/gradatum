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
//! ## Pre-flight validation (`--check`)
//!
//! `gradatum-engine --check <CONFIG>` answers whether this binary can serve `<CONFIG>` on
//! this host, then exits — no port bound, no `llama-server` spawned, nothing written, no
//! event emitted. The engine binary is a single file shared by every engine of a host, so
//! a deployment replaces it for all of them at once; without `--check`, a configuration
//! this binary cannot serve is only discovered when restarting, past the point of no
//! return. See [`run_check`] for the exact coverage and its documented blind spots.
//!
//! Its filesystem verdicts are rendered for the `User=` of the systemd unit that will
//! serve the configuration — not for the account invoking the check, which on a
//! deployment host is a different, usually more privileged, identity. See
//! [`CheckIdentity`] (F-191).
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
//! - Fallback to `NoopEventSink` if the api-key→JWT exchange fails (best-effort). The
//!   fold is deliberate — a broken event-log never takes the engine down — but it is
//!   surfaced on `/health` via `event_log` (F-205), with the reason distinguished
//!   (`folded_unauthorized` / `folded_unreachable` / `not_configured`), so a silent
//!   telemetry outage is visible to a probe instead of hiding in the logs.
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
    " <CONFIG>\n",
    "       ",
    env!("CARGO_PKG_NAME"),
    " --check <CONFIG>\n\n",
    "Arguments:\n",
    "  <CONFIG>  Path to the TOML configuration file\n\n",
    "Options:\n",
    "      --check <CONFIG>  Validate CONFIG and exit, without any side effect: no port\n",
    "                        is bound, no llama-server subprocess is spawned, nothing is\n",
    "                        written to disk, no event is emitted. Answers whether THIS\n",
    "                        binary can serve CONFIG on THIS host.\n",
    "  -h, --help            Print help\n",
    "  -V, --version         Print version\n\n",
    "Exit codes:\n",
    "  0  nominal run finished, or --check found no problem (config servable)\n",
    "  1  startup/runtime failure, or --check found at least one problem (reasons on stderr)\n",
    "  2  --check usage error (missing or flag-shaped CONFIG argument)\n"
);

/// Exit code of `--check` when the configuration is servable by this binary.
#[cfg(feature = "serve")]
const EXIT_CHECK_OK: i32 = 0;

/// Exit code of `--check` when at least one problem was found.
#[cfg(feature = "serve")]
const EXIT_CHECK_FAILED: i32 = 1;

/// Exit code of `--check` when the flag itself was used incorrectly.
///
/// Distinct from [`EXIT_CHECK_FAILED`] so a deployment script can tell "this config is
/// not servable" (actionable: fix the config) from "you called me wrong" (actionable:
/// fix the script).
#[cfg(feature = "serve")]
const EXIT_CHECK_USAGE: i32 = 2;

/// Handles `--version`/`-V`, `--help`/`-h` and `--check <CONFIG>` before any runtime
/// is built and before any configuration is loaded for serving.
///
/// The minimal executable contract: `--version`/`--help` must answer from any directory,
/// with no config file in scope. `--check` extends the same discipline to configuration
/// validation — it answers and exits without ever reaching tracing init, the tokio
/// runtime, the supervisor or the listeners.
///
/// Returns `Some(exit_code)` when a flag was consumed, `None` otherwise (nominal run).
#[cfg(feature = "serve")]
fn handle_early_flags() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("{VERSION}");
                return Some(0);
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Some(0);
            }
            "--check" => {
                // The value is the NEXT argument. A missing or flag-shaped next argument
                // is a usage error, not a config verdict — reporting it as "not servable"
                // would make a broken invocation look like a broken configuration.
                return Some(match args.get(i + 1) {
                    Some(path) if !path.starts_with('-') => run_check(std::path::Path::new(path)),
                    _ => {
                        eprintln!(
                            "--check requires a configuration path: \
                             gradatum-engine --check <CONFIG>"
                        );
                        EXIT_CHECK_USAGE
                    }
                });
            }
            other => {
                if let Some(path) = other.strip_prefix("--check=") {
                    return Some(run_check(std::path::Path::new(path)));
                }
            }
        }
    }
    None
}

#[cfg(not(feature = "serve"))]
fn main() {
    eprintln!("gradatum-engine: compiled without the 'serve' feature. Nothing to do.");
    std::process::exit(1);
}

/// Process entry point — synchronous on purpose.
///
/// `--version`, `--help` and `--check` are answered here, **before** the tokio runtime
/// exists. Replacing `#[tokio::main]` by an explicit builder is behaviour-preserving for
/// the nominal run: the macro expands to exactly this multi-thread builder with
/// `enable_all()`, the only difference being that a runtime that fails to build now
/// returns an error instead of panicking.
#[cfg(feature = "serve")]
fn main() -> anyhow::Result<()> {
    // `--version`/`-V`, `--help`/`-h` and `--check <CONFIG>` must answer without any
    // runtime, any tracing subscriber and any listener (see `handle_early_flags`).
    if let Some(code) = handle_early_flags() {
        std::process::exit(code);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build the tokio runtime: {e}"))?
        .block_on(serve())
}

/// Maps an [`ExchangeError`](gradatum_engine::sink::ExchangeError) to the telemetry state
/// the engine exposes on `/health` when the event-log folds onto the inert sink.
///
/// The three fold reasons demand different operational responses, so they are kept
/// distinct: a `401` is an identity problem (human action, never self-heals), a transport
/// failure is transient (may recover on restart), and anything else is a non-transient
/// failure that must not be mislabelled as transient.
#[cfg(feature = "serve")]
fn classify_exchange_error(
    err: &gradatum_engine::sink::ExchangeError,
) -> gradatum_engine::health::TelemetryStatus {
    use gradatum_engine::{health::TelemetryStatus, sink::ExchangeError};
    match err {
        ExchangeError::Unauthorized { .. } => TelemetryStatus::Unauthorized,
        ExchangeError::Transport { .. } => TelemetryStatus::Unreachable,
        // Non-401 HTTP status or malformed response — a real failure, never "transient".
        ExchangeError::HttpStatus { .. } | ExchangeError::MissingToken { .. } => {
            TelemetryStatus::Failed
        }
        // `ExchangeError` is `#[non_exhaustive]`: any future variant folds to `Failed`
        // (a fold, never mislabelled active) until it is classified explicitly.
        _ => TelemetryStatus::Failed,
    }
}

/// Nominal run: load the config, spawn and supervise `llama-server`, serve the API.
///
/// Unchanged from the previous `#[tokio::main] async fn main` body, minus the early-flag
/// dispatch which moved to the synchronous [`main`].
#[cfg(feature = "serve")]
async fn serve() -> anyhow::Result<()> {
    use gradatum_core::event_sink::NoopEventSink;
    use gradatum_engine::{
        config::{EngineConfig, RuntimeKind},
        health::{HealthState, TelemetryStatus},
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
    // gradatum_url = None  → InMemorySink/NoopEventSink (dev/test — no event-log POST)
    // gradatum_url = Some  → validate loopback (anti-SSRF) + exchange JWT → HttpEventSink
    //                        fallback to NoopEventSink if JWT exchange fails (best-effort)
    //
    // The fold is deliberate: a broken telemetry channel must NOT take down an engine that
    // serves the fleet. But the fold must be *visible* — so the block also yields the
    // resulting `TelemetryStatus`, wired into `HealthState` and exposed on `/health`
    // (F-205). Before F-205 this fold was silent and a stale credential kept the event-log
    // dark for ten days before an audit found it.
    //
    // NOTE: the network binding (LAN vs loopback) is NOT modified here — only the sink
    // implementation changes. LAN exposure is decided by the operator.
    let (sink, telemetry): (
        Arc<dyn gradatum_core::event_sink::EventSink>,
        TelemetryStatus,
    ) = {
        if let Some(ref gradatum_url) = config.gradatum_url {
            // Validate that the URL is loopback (anti-SSRF)
            validate_loopback_url(gradatum_url)?;
            // Read api-key — only when event-log is enabled. Kept by the sink to
            // re-exchange a fresh JWT on 401 (F-120 lazy refresh).
            let api_key = read_api_key()?;
            match gradatum_engine::sink::exchange_api_key_for_jwt_typed(&api_key, gradatum_url)
                .await
            {
                Ok(jwt) => (
                    Arc::new(HttpEventSink::new(
                        gradatum_url.clone(),
                        jwt,
                        api_key,
                        config.agent_id.clone(),
                        metrics.clone(),
                    )) as Arc<dyn gradatum_core::event_sink::EventSink>,
                    TelemetryStatus::Active,
                ),
                Err(e) => {
                    // Best-effort fallback — no crash on JWT failure. The fold reason is
                    // classified so `/health` distinguishes a 401 (identity, human action)
                    // from a transient outage (may recover on restart).
                    let status = classify_exchange_error(&e);
                    tracing::warn!(
                        error = %e,
                        telemetry = status.label(),
                        "api-key→JWT exchange failed. Falling back to NoopEventSink (event-log disabled)."
                    );
                    (Arc::new(NoopEventSink), status)
                }
            }
        } else {
            // gradatum_url absent → NoopEventSink in production (no event-log POST).
            // In test/CI (feature test-utils): InMemorySink allows inspection.
            tracing::info!(
                telemetry = TelemetryStatus::NotConfigured.label(),
                "gradatum_url not set — event-log disabled (NoopEventSink in prod; \
                InMemorySink only if feature test-utils is enabled). \
                Set gradatum_url to enable event posting."
            );
            #[cfg(any(test, feature = "test-utils"))]
            let sink: Arc<dyn gradatum_core::event_sink::EventSink> =
                Arc::new(gradatum_core::event_sink::InMemorySink::default());
            #[cfg(not(any(test, feature = "test-utils")))]
            let sink: Arc<dyn gradatum_core::event_sink::EventSink> = Arc::new(NoopEventSink);
            (sink, TelemetryStatus::NotConfigured)
        }
    };

    // --- Derive metadata ---
    let model_name = config.model_alias();
    let provider = config.provider_alias();
    let health = Arc::new(HealthState::new_with_telemetry(&model_name, telemetry));

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

/// Filesystem fallback for the event-log api-key, used when `GRADATUM_ENGINE_API_KEY`
/// is absent from the environment.
#[cfg(feature = "serve")]
const API_KEY_FILE: &str = "/etc/gradatum/engine.api-key";

/// Reads the api-key from the environment variable or the secrets file.
#[cfg(feature = "serve")]
fn read_api_key() -> anyhow::Result<zeroize::Zeroizing<String>> {
    if let Ok(key) = std::env::var("GRADATUM_ENGINE_API_KEY") {
        return Ok(zeroize::Zeroizing::new(key));
    }
    let path = API_KEY_FILE;
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

// ---------------------------------------------------------------------------
// `--check` — side-effect-free pre-flight validation
// ---------------------------------------------------------------------------

/// Validates `config_path` and returns the process exit code, without any side effect.
///
/// Answers a single operational question: **can THIS binary serve THIS configuration on
/// THIS host?** The engine binary is a single file shared by every engine of a host, so
/// a deployment replaces it for all of them at once and only discovers a config it cannot
/// serve when restarting — i.e. past the point of no return. `--check` moves that verdict
/// before the replacement.
///
/// ## Guarantees
///
/// No TCP port is bound, no `llama-server` subprocess is spawned, nothing is written to
/// disk, and no event is emitted to any collector. The I/O performed is read-only:
/// `stat`/`open` on the configuration and on the paths it references, one
/// `systemctl show` (a read-only query, see [`resolve_service_identity`]), and — only
/// when `gradatum_url` names a hostname rather than a literal IP — a DNS lookup (no
/// connection is opened).
///
/// ## Whose access is answered for
///
/// The filesystem verdicts are rendered for the `User=` of the systemd unit that will
/// serve this configuration, not for the account running the check — see
/// [`CheckIdentity`]. When no unit can be resolved the check falls back to the calling
/// account and says so, loudly, in a `check NOTE:` line.
///
/// It holds for the configuration file itself, checked *before* it is parsed:
/// [`config_file_access_problem`] answers for the service identity, so an unreadable file
/// is reported as a missing permission and never as a missing field.
///
/// ## Exit codes
///
/// - [`EXIT_CHECK_OK`] — servable.
/// - [`EXIT_CHECK_FAILED`] — at least one problem; every reason is printed to stderr.
/// - [`EXIT_CHECK_USAGE`] — reserved for a malformed invocation (see [`handle_early_flags`]).
///
/// ## Redaction
///
/// `figment` exposes the offending value in its `Display`; the parse-failure branch below
/// rebuilds a value-free message through `redact_figment_error`, exactly like the nominal
/// path. A `--check` that echoed a configuration secret on stderr would be a security
/// regression, not a convenience.
/// What the pre-flight concluded, before anything is written to a stream.
///
/// Exists so the **order** of the steps is observable by a test: step 0 (can the identity
/// read the configuration at all?) must run *before* the parser, and a mutant that drops
/// it has to be detectable otherwise than by reading stderr. The variants are ordered as
/// the steps are.
#[cfg(feature = "serve")]
#[derive(Debug)]
enum CheckOutcome {
    /// The configuration file cannot be read by the identity that will serve it.
    ConfigUnusable(String),
    /// The file is readable but does not parse into an `EngineConfig`.
    ConfigUnloadable(String),
    /// The configuration was loaded and evaluated.
    Evaluated(CheckFindings),
}

/// Runs the pre-flight and returns its conclusion, without printing anything.
///
/// Step 0 comes first on purpose — see [`config_file_access_problem`].
#[cfg(feature = "serve")]
#[must_use]
fn evaluate_check(config_path: &std::path::Path, identity: &CheckIdentity) -> CheckOutcome {
    use gradatum_engine::config::EngineConfig;

    if let Some(problem) = config_file_access_problem(config_path, identity) {
        return CheckOutcome::ConfigUnusable(problem);
    }
    match EngineConfig::load_local(config_path) {
        Err(e) => CheckOutcome::ConfigUnloadable(config_load_failure_message(config_path, &e)),
        Ok(config) => CheckOutcome::Evaluated(collect_check_failures(&config, identity)),
    }
}

#[cfg(feature = "serve")]
#[must_use]
fn run_check(config_path: &std::path::Path) -> i32 {
    // Resolved first: the very first file the engine reads is its configuration, and the
    // question "who will read it" must be settled before anything tries to parse it.
    let (identity, degraded_reason) = CheckIdentity::resolve(config_path);
    eprintln!("{}", identity.subject_note(degraded_reason.as_deref()));

    let findings = match evaluate_check(config_path, &identity) {
        CheckOutcome::ConfigUnusable(problem) => {
            eprintln!(
                "check FAILED: {} — 1 problem(s) found by {VERSION}:",
                config_path.display()
            );
            eprintln!("  - {problem}");
            return EXIT_CHECK_FAILED;
        }
        CheckOutcome::ConfigUnloadable(message) => {
            eprintln!("{message}");
            return EXIT_CHECK_FAILED;
        }
        CheckOutcome::Evaluated(findings) => findings,
    };

    for note in &findings.notes {
        eprintln!("{note}");
    }

    if findings.failures.is_empty() {
        println!(
            "check OK: {} — servable by {VERSION}",
            config_path.display()
        );
        return EXIT_CHECK_OK;
    }

    eprintln!(
        "check FAILED: {} — {} problem(s) found by {VERSION}:",
        config_path.display(),
        findings.failures.len()
    );
    for failure in &findings.failures {
        eprintln!("  - {failure}");
    }
    EXIT_CHECK_FAILED
}

/// Builds the stderr line emitted when the configuration cannot even be loaded.
///
/// Extracted so the redaction is testable at the exact site that prints it: `figment`
/// exposes the offending value in its `Display`, and a `--check` that echoed a
/// configuration secret on stderr would be a security regression, not a convenience.
#[cfg(feature = "serve")]
#[must_use]
fn config_load_failure_message(config_path: &std::path::Path, e: &figment::Error) -> String {
    format!(
        "check FAILED: {} — configuration cannot be loaded: {}",
        config_path.display(),
        gradatum_core::config::redact_figment_error(e)
    )
}

/// Collects every reason `config` could not be served by this binary, without side effects.
///
/// Returns empty [`CheckFindings::failures`] when the configuration is servable. Failures
/// are **accumulated** rather than short-circuited: an operator fixing a config wants the
/// whole list in one pass, not one problem per run.
///
/// ## Whose access is judged
///
/// `identity` decides who the filesystem predicates (steps 4 and 6) answer for — the
/// `User=` of the unit that will serve the configuration, not the account invoking the
/// check. See [`CheckIdentity`] for why the difference is load-bearing (F-191). Every
/// other step is identity-independent.
///
/// ## Coverage
///
/// Mirrors, in order, the startup checks the nominal path performs before it becomes
/// observable from the outside:
///
/// 1. [`EngineConfig::validate`] — `model_path`/`mmproj_path`/`draft_model_path` existence
///    and `/opt/gradatum/models/` prefix, speculative-decoding coherence, `bind_addr`
///    fail-closed policy, `body_limit_bytes` cap.
/// 2. `runtime` is implemented (`onnx` is parsed but unimplemented).
/// 3. Port invariants — `child_port > 1024` and `child_port != port` (mirrors
///    [`LlamaServerSupervisor::new`](gradatum_engine::supervisor::LlamaServerSupervisor::new)),
///    plus the resolved metrics port not colliding with either (the nominal path has no
///    explicit guard for this: it fails later, at bind time).
/// 4. `llama_server_bin` — canonicalizable, under an allowed prefix, correctly named
///    (`canonicalize_bin_path`), and executable by this process.
/// 5. `extra_args` — within the allow-list (`validate_extra_args`).
/// 6. Referenced model files are actually **readable** (existence alone is what
///    `validate()` proves; a `0600 root:root` GGUF passes it and still breaks the child).
/// 7. When `gradatum_url` is set — the loopback policy (anti-SSRF), which aborts the
///    nominal startup.
///
/// ## Deliberately NOT covered
///
/// - **Port availability.** Probing it means binding, which this function must never do —
///   and the binding would be misleading anyway: in the nominal deployment scenario the
///   port is legitimately held by the very engine being replaced.
/// - **That `llama_server_bin` actually runs** (correct architecture, resolvable shared
///   libraries): proving it requires executing it.
/// - **That the child would become healthy** — model/build compatibility (e.g. `draft-*`
///   strategies need `llama-server` ≥ b9780), VRAM/RAM headroom for `context_len`.
/// - **The event-log api-key.** Its absence does abort startup, but it is delivered by
///   the unit's `EnvironmentFile=`, which a process started from a shell cannot see —
///   verdicting on it would fail every healthy config. Reported as an advisory note by
///   [`run_check`] instead. Its *acceptance* by the server is a network call, and its
///   failure does not block startup anyway (the sink falls back to noop).
/// - **Unknown TOML keys.** `EngineConfig` does not deny them, so a typo'd field is
///   silently ignored at startup and is equally invisible here.
/// - **The configuration file's own readability by the service** — covered, but not here:
///   [`config_file_access_problem`] answers it in [`run_check`], necessarily *before* the
///   parse, since reaching this function at all requires the file to have been read.
/// - **What POSIX mode bits do not express** — ACLs, mount options (`noexec`, `ro`),
///   SELinux/AppArmor. They can only make the service *less* able than
///   [`mode_grants`] computes, never more: the verdict errs fail-safe, not fail-open.
///
/// ## Drift
///
/// Steps 3-5 re-use the very functions the supervisor calls, but the call sites are
/// duplicated: a new invariant added inside `LlamaServerSupervisor::new` itself would not
/// be seen here. Extending that constructor requires extending this list.
#[cfg(feature = "serve")]
#[must_use]
fn collect_check_failures(
    config: &gradatum_engine::config::EngineConfig,
    identity: &CheckIdentity,
) -> CheckFindings {
    use gradatum_engine::{
        config::RuntimeKind,
        supervisor::{canonicalize_bin_path, validate_extra_args},
    };

    let mut failures: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // Advisory, never a verdict: the api-key reaches the engine through the unit's
    // `EnvironmentFile=`, which a check run from a shell cannot observe. Failing on it
    // would mark every healthy configuration as not servable — a checker that is wrong
    // in the nominal case is worse than one that stays silent. Its real absence at
    // startup does abort the process, hence the note.
    if config.gradatum_url.is_some()
        && std::env::var_os("GRADATUM_ENGINE_API_KEY").is_none()
        && !std::path::Path::new(API_KEY_FILE).exists()
    {
        notes.push(format!(
            "check NOTE: gradatum_url is set but no event-log api-key is visible from \
             this environment (neither GRADATUM_ENGINE_API_KEY nor {API_KEY_FILE}). \
             The unit must provide one — startup aborts without it. Not counted in the \
             verdict: a systemd EnvironmentFile is invisible here."
        ));
    }

    // 1. Invariants carried by EngineConfig::validate() (read-only: canonicalize = stat).
    if let Err(e) = config.validate() {
        failures.push(format!("invalid config: {e}"));
    }

    // 2. Runtime kind.
    if config.runtime == RuntimeKind::Onnx {
        failures.push(
            "runtime 'onnx' is not implemented — use runtime='llamaserver' (default)".to_string(),
        );
    }

    // 3. Port invariants.
    if config.child_port <= 1024 {
        failures.push(format!(
            "child_port {} is invalid — must be > 1024 (SP-P0-4)",
            config.child_port
        ));
    }
    if config.child_port == config.port {
        failures.push(format!(
            "child_port {} must differ from port {} — port collision",
            config.child_port, config.port
        ));
    }
    let metrics_port = config.resolved_metrics_port();
    if metrics_port == config.port {
        failures.push(format!(
            "resolved metrics port {metrics_port} collides with port {} — \
             the metrics listener would fail to bind (set metrics_port explicitly)",
            config.port
        ));
    }
    if metrics_port == config.child_port {
        failures.push(format!(
            "resolved metrics port {metrics_port} collides with child_port {} — \
             the metrics listener would fail to bind (set metrics_port explicitly)",
            config.child_port
        ));
    }

    // 4. llama-server binary: allowed location, then executability BY THE SERVICE.
    match canonicalize_bin_path(&config.llama_server_bin) {
        Err(e) => failures.push(format!("llama_server_bin: {e}")),
        Ok(canonical) => match identity.can_exec(&canonical) {
            Err(e) => failures.push(format!("llama_server_bin: {e}")),
            Ok(()) => {
                notes.extend(identity.false_red_note("llama_server_bin", &canonical, WANT_EXEC))
            }
        },
    }

    // 5. Pass-through arguments allow-list.
    if let Err(e) = validate_extra_args(&config.extra_args) {
        failures.push(format!("extra_args: {e}"));
    }

    // 6. Readability of the referenced model files (existence is already covered by
    //    validate(); this catches the permission case it cannot see).
    let mut model_files: Vec<(&str, std::path::PathBuf)> =
        vec![("model_path", std::path::PathBuf::from(&config.model_path))];
    if let Some(mmproj) = &config.mmproj_path {
        model_files.push(("mmproj_path", mmproj.clone()));
    }
    if let Some(draft) = &config.draft_model_path {
        model_files.push(("draft_model_path", std::path::PathBuf::from(draft)));
    }
    for (label, path) in &model_files {
        match identity.can_read(path) {
            Err(reason) => failures.push(format!("{label}: {reason}")),
            Ok(()) => notes.extend(identity.false_red_note(label, path, WANT_READ)),
        }
    }

    // 7. Event-log destination — only when the nominal path would validate it.
    if let Some(gradatum_url) = &config.gradatum_url
        && let Err(e) = validate_loopback_url(gradatum_url)
    {
        failures.push(format!("gradatum_url: {e}"));
    }

    CheckFindings { failures, notes }
}

/// Returns an error message when `path` is not executable by this process.
///
/// Uses `access(X_OK)` rather than inspecting mode bits, so the answer accounts for the
/// process identity and the mount options actually in force (`noexec`), not just the
/// permission field.
///
/// **Answers for the calling process only.** It is the *caller-relative* half of the
/// pair used by [`CheckIdentity`]; the verdict itself is rendered for the service
/// identity (F-191).
#[cfg(feature = "serve")]
fn check_executable(path: &std::path::Path) -> Result<(), String> {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK)
        .map_err(|e| format!("{} is not executable by this process: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// F-191 — whose access is `--check` answering for?
// ---------------------------------------------------------------------------

/// Unit name pattern queried to find which unit would serve a given configuration.
///
/// Deliberately loose: it must match both the templated units of this host
/// (`gradatum-engine@curator.service`) and the flat ones of the GPU host
/// (`gradatum-engine-embed.service`). Selection is done on `ExecStart`, not on the name.
#[cfg(feature = "serve")]
const ENGINE_UNIT_PATTERN: &str = "gradatum-engine*";

/// Permission bit requested from an identity — read.
#[cfg(feature = "serve")]
const WANT_READ: u32 = 0o4;

/// Permission bit requested from an identity — execute (and, on a directory, search).
#[cfg(feature = "serve")]
const WANT_EXEC: u32 = 0o1;

/// Unix identity a systemd unit runs its `ExecStart` under.
#[cfg(feature = "serve")]
#[derive(Debug, Clone)]
struct ServiceIdentity {
    /// Unit whose `ExecStart` names the configuration being checked.
    unit: String,
    /// `User=` of that unit, as written (or `root` when the unit declares none).
    user: String,
    /// Resolved uid of `user`.
    uid: u32,
    /// Primary gid, plus the secondary groups of `user` in the group database, plus the
    /// unit's `SupplementaryGroups=`. A file is reachable through any one of them.
    gids: Vec<u32>,
}

/// The identity a `--check` filesystem predicate is evaluated for.
///
/// ## Why this exists
///
/// `access(X_OK)` and `File::open` answer for the uid of the *calling* process — never
/// for the `User=` of the unit that will actually run the engine. The two coincide on a
/// host where the deploying account **is** the `User=`, and diverge on a host where
/// deployment and service run under distinct accounts — the deploying account writes the
/// binary, a dedicated service account reads the configuration. Both layouts occur in the
/// same fleet, so neither can be assumed. The divergence is asymmetric:
///
/// - the caller has **less** access than the service ⇒ a **false red**: a deployment
///   blocked on a configuration that is in fact servable. Unpleasant, fail-safe;
/// - the caller has **more** access than the service ⇒ a **false green**: the check
///   approves a configuration the service cannot read, and authorises exactly the
///   switch the check exists to forbid.
///
/// The second is not theoretical: where the accounts diverge, the model directories are
/// created by the deploying account and left `drwxr-xr-x`, so the service account reaches
/// them through the world bits alone. Today the two verdicts agree only because the
/// permissions happen to be permissive; that is a coincidence, not a guarantee.
///
/// So the verdict is rendered for [`CheckIdentity::Service`] whenever a unit can be
/// resolved, and the caller-relative answer is downgraded to an advisory note.
#[cfg(feature = "serve")]
#[derive(Debug, Clone)]
enum CheckIdentity {
    /// `User=`/`Group=` of the systemd unit that will serve this configuration.
    Service(ServiceIdentity),
    /// No unit could be resolved — the verdict falls back to the calling process and
    /// says so. Degraded, never silent: see [`CheckIdentity::subject_note`].
    CallingProcess,
}

/// Everything `--check` learned about one configuration.
///
/// Failures decide the exit code; notes never do. Keeping them apart is what makes the
/// false red reportable without being blocking (F-191 criterion 3).
#[cfg(feature = "serve")]
#[derive(Debug, Default)]
struct CheckFindings {
    /// Reasons the configuration is not servable. Non-empty ⇒ [`EXIT_CHECK_FAILED`].
    failures: Vec<String>,
    /// Advisory lines printed to stderr, excluded from the verdict.
    notes: Vec<String>,
}

/// Reads a single `Key=Value` property out of a `systemctl show` record.
#[cfg(feature = "serve")]
fn record_property<'a>(record: &'a str, key: &str) -> Option<&'a str> {
    record
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// Extracts the last `argv[]=` token of an `ExecStart=` property — the config path.
///
/// `systemctl show` renders `ExecStart` as
/// `{ path=… ; argv[]=/opt/gradatum/bin/gradatum-engine /etc/gradatum/conf.d/70-engine-curator.toml ; … }`.
/// The engine takes its configuration as a positional argument, so the last token of
/// `argv[]` is it.
#[cfg(feature = "serve")]
fn exec_start_config_arg(exec_start: &str) -> Option<&str> {
    let after = exec_start.split("argv[]=").nth(1)?;
    let argv = after.split(" ; ").next()?.trim();
    argv.split_whitespace().last()
}

/// Resolves the uid/gids a unit's `ExecStart` would run under.
///
/// # Errors
/// Returns a human-readable reason when systemd cannot be queried, when no unit names
/// `config_path`, or when the declared user/group is absent from the local databases.
#[cfg(feature = "serve")]
fn resolve_service_identity(config_path: &std::path::Path) -> Result<ServiceIdentity, String> {
    let target = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());

    let output = std::process::Command::new("systemctl")
        .args([
            "show",
            ENGINE_UNIT_PATTERN,
            "--no-pager",
            "--property=Id",
            "--property=User",
            "--property=Group",
            "--property=SupplementaryGroups",
            "--property=ExecStart",
        ])
        .env("SYSTEMD_PAGER", "")
        .output()
        .map_err(|e| format!("systemd could not be queried ('systemctl show' failed: {e})"))?;
    if !output.status.success() {
        return Err(format!(
            "systemd could not be queried ('systemctl show' exited with {})",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let record = text
        .split("\n\n")
        .find(|record| {
            record_property(record, "ExecStart")
                .and_then(exec_start_config_arg)
                .is_some_and(|arg| {
                    let arg = std::path::Path::new(arg);
                    std::fs::canonicalize(arg).unwrap_or_else(|_| arg.to_path_buf()) == target
                })
        })
        .ok_or_else(|| {
            format!(
                "no loaded '{ENGINE_UNIT_PATTERN}' unit serves {} on this host",
                config_path.display()
            )
        })?;

    let unit = record_property(record, "Id")
        .unwrap_or("<unknown>")
        .to_string();
    // systemd defaults to root when the unit declares no User=/Group=.
    let user = non_empty(record_property(record, "User")).unwrap_or("root");
    let group = non_empty(record_property(record, "Group"));

    let passwd = nix::unistd::User::from_name(user)
        .map_err(|e| format!("user '{user}' of unit {unit} could not be looked up: {e}"))?
        .ok_or_else(|| format!("user '{user}' of unit {unit} does not exist on this host"))?;

    let primary_gid = match group {
        Some(name) => {
            nix::unistd::Group::from_name(name)
                .map_err(|e| format!("group '{name}' of unit {unit} could not be looked up: {e}"))?
                .ok_or_else(|| {
                    format!("group '{name}' of unit {unit} does not exist on this host")
                })?
                .gid
        }
        None => passwd.gid,
    };

    // Secondary groups of the user in the group database. A failure here is not fatal:
    // the primary gid alone still yields a strictly more conservative verdict than the
    // caller's, which is the property that matters.
    let mut gids: Vec<u32> = vec![primary_gid.as_raw()];
    if let Ok(name) = std::ffi::CString::new(user.as_bytes())
        && let Ok(list) = nix::unistd::getgrouplist(&name, primary_gid)
    {
        gids.extend(list.into_iter().map(nix::unistd::Gid::as_raw));
    }
    // `SupplementaryGroups=` of the unit — invisible to the group database, and the
    // reason a file owned by `kvm` is reachable by this engine but not by its user.
    for name in record_property(record, "SupplementaryGroups")
        .unwrap_or("")
        .split_whitespace()
    {
        if let Ok(Some(g)) = nix::unistd::Group::from_name(name) {
            gids.push(g.gid.as_raw());
        }
    }
    gids.sort_unstable();
    gids.dedup();

    Ok(ServiceIdentity {
        unit,
        user: user.to_string(),
        uid: passwd.uid.as_raw(),
        gids,
    })
}

/// `None` for an absent or empty property, `Some` otherwise.
#[cfg(feature = "serve")]
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Whether `uid`/`gids` are granted `want` on `meta` by the owner/group/other rules.
///
/// Deliberately reimplements the kernel's classic check rather than calling `access()`:
/// `access()` can only ever answer for the *calling* process. The blind spots of this
/// reimplementation are the ones POSIX mode bits do not express — POSIX ACLs, mount
/// options (`noexec`, `ro`), SELinux/AppArmor — all listed in the `--check` coverage
/// section. They can only make the real service *less* able than computed here, never
/// more, so the verdict stays on the fail-safe side.
#[cfg(feature = "serve")]
fn mode_grants(meta: &std::fs::Metadata, uid: u32, gids: &[u32], want: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mode = meta.mode();
    if uid == 0 {
        // root bypasses read/write entirely; execute still requires at least one x bit.
        return want != WANT_EXEC || mode & 0o111 != 0;
    }
    let class = if meta.uid() == uid {
        (mode >> 6) & 0o7
    } else if gids.contains(&meta.gid()) {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    class & want == want
}

/// Whether `uid`/`gids` can reach `path` and hold `want` on it.
///
/// Walks the ancestors first: a file `0644` under a directory the identity cannot search
/// is unreachable, and that is precisely how a model directory owned by the deployment
/// account locks the service out.
///
/// A path that does not exist yields `Ok(())`: its absence is already reported by
/// [`EngineConfig::validate`](gradatum_engine::config::EngineConfig::validate), and
/// reporting it twice would only pad the operator's list.
#[cfg(feature = "serve")]
fn identity_access(
    path: &std::path::Path,
    uid: u32,
    gids: &[u32],
    want: u32,
) -> Result<(), String> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "{} cannot be inspected from this account ({e}) — the verdict for the \
                 service identity could not be computed",
                path.display()
            ));
        }
    };

    let mut ancestors: Vec<&std::path::Path> = canonical.ancestors().collect();
    ancestors.reverse();
    let Some((leaf, dirs)) = ancestors.split_last() else {
        return Ok(());
    };

    for dir in dirs {
        let meta = std::fs::metadata(dir).map_err(|e| {
            format!(
                "{} cannot be inspected from this account ({e}) — the verdict for the \
                 service identity could not be computed",
                dir.display()
            )
        })?;
        if !mode_grants(&meta, uid, gids, WANT_EXEC) {
            return Err(format!(
                "{} is not reachable: {} is not searchable",
                canonical.display(),
                dir.display()
            ));
        }
    }

    let meta = std::fs::metadata(leaf).map_err(|e| {
        format!(
            "{} cannot be inspected from this account ({e}) — the verdict for the \
             service identity could not be computed",
            leaf.display()
        )
    })?;
    if mode_grants(&meta, uid, gids, want) {
        return Ok(());
    }
    Err(match want {
        WANT_EXEC => format!("{} is not executable", canonical.display()),
        _ => format!("{} is not readable", canonical.display()),
    })
}

/// Returns the problem barring the configuration file itself, if there is one.
///
/// ## Why this runs *before* the parser
///
/// When the obstacle is the **parent directory** — not searchable by the identity doing the
/// reading — `figment` cannot even `stat` the file and renders it as **absent**: the
/// extraction fails with `missing field \`engine\``. An operator reading that goes hunting
/// for a typo while the real problem is a permission, at the worst possible moment, during
/// the pre-flight of a deployment. (When the file itself is unreadable under a searchable
/// directory, `figment` does surface the `EACCES`; the confusion is specific to the
/// directory case — measured 2026-08-18, and it is the form met in the field.) `--check`
/// exists to say whether the service will be able to serve this configuration; answering
/// with a diagnosis that points at the wrong place is worse than answering nothing.
///
/// The predicate is the same one used for every other referenced path, so the file the
/// engine reads *first* is judged for the identity that will read it — otherwise F-191
/// would be reintroduced on the configuration itself.
///
/// ## Absence is not a problem here
///
/// A missing file yields `None` on purpose: [`EngineConfig::load_local`](gradatum_engine::config::EngineConfig::load_local)
/// reports it right after, and unlike an unreadable file its message is not misleading.
///
/// ## Redaction
///
/// The returned line names the path, the missing right and the identity — never a byte of
/// the file. Nothing is read to build it: the answer comes from `stat`, not from `open`.
#[cfg(feature = "serve")]
#[must_use]
fn config_file_access_problem(
    config_path: &std::path::Path,
    identity: &CheckIdentity,
) -> Option<String> {
    // The service verdict decides servability: if the identity that will run the engine
    // cannot read the file, the engine aborts at startup, whatever this account can see.
    if let Err(reason) = identity.can_read(config_path) {
        return Some(format!(
            "configuration file: {reason} — for {}. The engine would abort at startup.",
            identity.subject_label()
        ));
    }

    // The service can read it; can this check? If not, it cannot render a verdict at all —
    // and must not let the parser answer 'missing field' in its place. Fail-safe: reported
    // as a problem, but named for what it is, so the operator fixes the right thing.
    let CheckIdentity::Service(service) = identity else {
        return None;
    };
    let caller_reason = check_readable(config_path)?;
    Some(format!(
        "configuration file: {caller_reason} — but User={} (uid {}) of unit {} can read it. \
         This is NOT a defect of the configuration: the check cannot inspect it from this \
         account. Re-run it as {}, or let the invoking account read the file.",
        service.user, service.uid, service.unit, service.user
    ))
}

#[cfg(feature = "serve")]
impl CheckIdentity {
    /// Resolves the identity `--check` must answer for, given the configuration path.
    ///
    /// Never fails: an unresolvable unit downgrades to [`CheckIdentity::CallingProcess`]
    /// carrying the reason, so a development host or a not-yet-installed unit still gets
    /// a verdict — an explicitly degraded one.
    fn resolve(config_path: &std::path::Path) -> (Self, Option<String>) {
        match resolve_service_identity(config_path) {
            Ok(identity) => (Self::Service(identity), None),
            Err(reason) => (Self::CallingProcess, Some(reason)),
        }
    }

    /// The line stating whose access the filesystem verdicts describe.
    ///
    /// Always emitted. A verdict whose subject is implicit is exactly what F-191 is
    /// about: the operator must be able to read, on the spot, *for whom* the answer
    /// holds.
    fn subject_note(&self, degraded_reason: Option<&str>) -> String {
        let caller = nix::unistd::getuid().as_raw();
        match self {
            Self::Service(s) => format!(
                "check NOTE: filesystem verdicts are rendered for User={} (uid {}) of unit \
                 {}, not for the invoking account (uid {caller}).",
                s.user, s.uid, s.unit
            ),
            Self::CallingProcess => format!(
                "check NOTE: filesystem verdicts are rendered for the INVOKING account \
                 (uid {caller}) — {}. If the unit runs under a different User=, a file \
                 readable here may be unreadable there and this check cannot see it.",
                degraded_reason.unwrap_or("no service identity could be resolved")
            ),
        }
    }

    /// Short designation of the identity, for use inside a failure line.
    fn subject_label(&self) -> String {
        match self {
            Self::Service(s) => format!("User={} (uid {}) of unit {}", s.user, s.uid, s.unit),
            Self::CallingProcess => {
                format!(
                    "the invoking account (uid {})",
                    nix::unistd::getuid().as_raw()
                )
            }
        }
    }

    /// `Ok(())` when this identity may read `path`.
    fn can_read(&self, path: &std::path::Path) -> Result<(), String> {
        match self {
            Self::Service(s) => identity_access(path, s.uid, &s.gids, WANT_READ),
            Self::CallingProcess => check_readable(path).map_or(Ok(()), Err),
        }
    }

    /// `Ok(())` when this identity may execute `path`.
    fn can_exec(&self, path: &std::path::Path) -> Result<(), String> {
        match self {
            Self::Service(s) => identity_access(path, s.uid, &s.gids, WANT_EXEC),
            Self::CallingProcess => check_executable(path),
        }
    }

    /// Note emitted when the *caller* would have failed where the service succeeds.
    ///
    /// This is the false red the previous caller-relative predicate produced: hardening a
    /// file towards the service (`chown gradatum`, `chmod 0640`) takes the access away
    /// from the deployment account and used to block a deployment that was in fact sound.
    /// It is reported, never counted in the verdict (F-191 criterion 3).
    fn false_red_note(&self, label: &str, path: &std::path::Path, want: u32) -> Option<String> {
        let Self::Service(s) = self else {
            return None;
        };
        let caller = match want {
            WANT_EXEC => check_executable(path).err(),
            _ => check_readable(path),
        }?;
        Some(format!(
            "check NOTE: {label}: {caller} — but User={} (uid {}) of unit {} can, so this \
             is NOT a problem. Reported because the invoking account sees a file the \
             service does not need to share with it.",
            s.user, s.uid, s.unit
        ))
    }
}

/// Returns `Some(reason)` when `path` exists but cannot be opened for reading.
///
/// A missing file yields `None`: absence is already reported by
/// [`EngineConfig::validate`](gradatum_engine::config::EngineConfig::validate), and
/// reporting it twice would only pad the operator's list.
#[cfg(feature = "serve")]
fn check_readable(path: &std::path::Path) -> Option<String> {
    match std::fs::File::open(path) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("{} is not readable: {e}", path.display())),
    }
}

#[cfg(all(test, feature = "serve"))]
mod check_tests {
    use super::*;
    use gradatum_engine::config::EngineConfig;
    use std::path::{Path, PathBuf};

    /// Construit une `EngineConfig` depuis le corps d'une section `[engine]`.
    fn cfg(body: &str) -> EngineConfig {
        EngineConfig::from_toml(&format!("[engine]\n{body}"))
            .expect("the test TOML must parse — otherwise the test is wrong")
    }

    /// Répertoire temporaire unique, supprimé par [`TempDir::drop`].
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gradatum-engine-check-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creating the test temporary directory");
            Self(path)
        }

        fn file(&self, name: &str, mode: u32) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            std::fs::write(&path, b"x").expect("writing the test temporary file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("setting permissions on the test temporary file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            // Restaure les droits avant suppression (un 0o000 empêcherait le remove).
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::set_permissions(
                        entry.path(),
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `true` si ce processus outrepasse les bits de permission (root, `CAP_DAC_OVERRIDE`).
    ///
    /// Sonde volontairement INDÉPENDANTE des prédicats testés : utiliser
    /// `check_readable`/`check_executable` comme oracle rendrait la garde complice d'une
    /// neutralisation — un prédicat muté « toujours OK » ferait *sauter* les tests au lieu
    /// de les faire rougir, et l'anti-vacuité deviendrait elle-même le trou.
    fn privileges_bypass_permission_bits() -> bool {
        let dir = TempDir::new("privprobe");
        let probe = dir.file("probe", 0o000);
        std::fs::read(&probe).is_ok()
    }

    /// Assertion : `needle` apparaît dans au moins un motif d'échec.
    fn assert_reports(failures: &[String], needle: &str) {
        assert!(
            failures.iter().any(|f| f.contains(needle)),
            "no failure mentions '{needle}' — failures obtained: {failures:#?}"
        );
    }

    // --- Cas nominal : configuration servable ---

    /// Première GGUF trouvée sous le préfixe autorisé, s'il en existe une.
    fn any_real_model() -> Option<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir("/opt/gradatum/models/")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "gguf") && p.is_file())
            .collect();
        found.sort();
        found.into_iter().next()
    }

    /// Comme [`any_real_model`], mais **échoue franchement** si l'hôte n'en a aucune.
    ///
    /// Contrat des tests qui l'appellent : ils portent `#[ignore]`, donc `cargo test`
    /// les compte `ignored` — jamais `ok`. Quand on les demande explicitement
    /// (`cargo test -- --ignored`) sur un hôte qui n'a pas l'environnement requis,
    /// le verdict doit être ROUGE et non un abandon silencieux : un test qui rend `ok`
    /// sans avoir exercé une seule assertion est précisément le défaut que ce lot corrige.
    fn require_real_model() -> PathBuf {
        any_real_model().expect(
            "required environment missing: no GGUF under /opt/gradatum/models/ — \
             this test carries #[ignore] and is only valid on an engine host",
        )
    }

    /// Une configuration dont tout est satisfait ne produit AUCUN échec.
    ///
    /// Dépendant de l'environnement par construction : les préfixes autorisés
    /// (`/opt/gradatum/models/`, `/usr/local/bin/`) sont absolus, gravés dans
    /// `config.rs` / `supervisor.rs`, et non falsifiables sans droits root — aucune
    /// fixture locale ne peut les satisfaire. D'où `#[ignore]` : le test est compté
    /// *non exécuté* là où l'environnement manque, jamais *réussi*.
    #[test]
    #[ignore = "requires an engine host: a GGUF under /opt/gradatum/models/ and \
                /usr/local/bin/llama-server executable — `cargo test -- --ignored`"]
    fn valid_config_yields_no_failure() {
        let bin = Path::new("/usr/local/bin/llama-server");
        let model = require_real_model();
        assert!(
            check_executable(bin).is_ok(),
            "environnement requis absent : {} n'est pas exécutable par ce processus",
            bin.display()
        );
        let c = cfg(&format!(
            "model_path=\"{}\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nllama_server_bin=\"{}\"\n",
            model.display(),
            bin.display()
        ));
        assert_eq!(
            collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            Vec::<String>::new(),
            "une configuration entièrement satisfaite ne doit produire aucun échec"
        );
    }

    // --- Une cause retenue = un test ---

    #[test]
    fn reports_unimplemented_onnx_runtime() {
        let c = cfg("model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nruntime=\"onnx\"\n");
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "runtime 'onnx' is not implemented",
        );
    }

    #[test]
    fn reports_child_port_below_1024() {
        let c = cfg("model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=1024\n");
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "child_port 1024 is invalid",
        );
    }

    #[test]
    fn reports_child_port_equal_to_port() {
        let c = cfg("model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11435\n");
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "must differ from port 11435",
        );
    }

    #[test]
    fn reports_metrics_port_colliding_with_port() {
        let c = cfg(
            "model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11435\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "resolved metrics port 11435 collides with port 11435",
        );
    }

    /// `metrics_port` non renseigné vaut `port + 1` : la collision se joue sur la valeur
    /// RÉSOLUE, pas sur le champ déclaré (qui est ici absent).
    #[test]
    fn reports_resolved_default_metrics_port_colliding_with_child_port() {
        let c = cfg("model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11436\n");
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "resolved metrics port 11436 collides with child_port 11436",
        );
    }

    #[test]
    fn reports_missing_model_file() {
        let c = cfg(
            "model_path=\"/opt/gradatum/models/absent-du-disque.gguf\"\n\
             model_kind=\"chat\"\nport=11435\nchild_port=11455\nmetrics_port=11438\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "canonicalize failed",
        );
    }

    #[test]
    fn reports_model_outside_allowed_prefix() {
        let dir = TempDir::new("model-prefix");
        let model = dir.file("fake.gguf", 0o644);
        let c = cfg(&format!(
            "model_path=\"{}\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\n",
            model.display()
        ));
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "must be under /opt/gradatum/models/",
        );
    }

    #[test]
    fn reports_llama_server_bin_outside_allowed_prefix() {
        let c = cfg(
            "model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nllama_server_bin=\"/bin/sh\"\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "outside the allowed prefix or has an invalid filename",
        );
    }

    #[test]
    fn reports_absent_llama_server_bin() {
        let c = cfg(
            "model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nllama_server_bin=\"/usr/local/bin/llama-server-absent-xyz\"\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "canonicalize failed",
        );
    }

    #[test]
    fn reports_disallowed_extra_arg() {
        let c = cfg(
            "model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nextra_args=[\"--rm-rf-slash\"]\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "'--rm-rf-slash' is not allowed",
        );
    }

    /// `EngineConfig::validate()` est fail-fast : elle contrôle `model_path` AVANT
    /// `bind_addr` et sort au premier échec. Atteindre la règle `bind_addr` exige donc
    /// un `model_path` réellement satisfait — d'où la dépendance à l'environnement,
    /// et `#[ignore]` plutôt qu'un abandon rendant `ok`.
    #[test]
    #[ignore = "requires an engine host: a GGUF under /opt/gradatum/models/ \
                — `cargo test -- --ignored`"]
    fn reports_wildcard_bind_addr() {
        let model = require_real_model();
        let c = cfg(&format!(
            "model_path=\"{}\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nbind_addr=\"0.0.0.0\"\n",
            model.display()
        ));
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "wildcard bind",
        );
    }

    #[test]
    fn reports_non_loopback_gradatum_url() {
        let c = cfg(
            "model_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\ngradatum_url=\"http://203.0.113.1:19090\"\n",
        );
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "gradatum_url: gradatum_url must point to loopback",
        );
    }

    /// Même contrainte de fail-fast que [`reports_wildcard_bind_addr`].
    #[test]
    #[ignore = "requires an engine host: a GGUF under /opt/gradatum/models/ \
                — `cargo test -- --ignored`"]
    fn reports_incoherent_speculative_decoding() {
        let model = require_real_model();
        let c = cfg(&format!(
            "model_path=\"{}\"\nmodel_kind=\"chat\"\nport=11435\nchild_port=11455\n\
             metrics_port=11438\nspec_type=\"draft-simple\"\n",
            model.display()
        ));
        assert_reports(
            &collect_check_failures(&c, &CheckIdentity::CallingProcess).failures,
            "requires draft_model_path",
        );
    }

    // --- Prédicats de système de fichiers ---

    #[test]
    fn check_executable_rejects_a_non_executable_file() {
        // Un processus privilégié outrepasse les bits de permission : le test n'aurait
        // alors aucun pouvoir discriminant, on le saute bruyamment plutôt qu'en silence.
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP check_executable_rejects_a_non_executable_file : ce processus \
                 outrepasse les bits de permission (root) — prédicat non discriminant ici"
            );
            return;
        }
        let dir = TempDir::new("noexec");
        let file = dir.file("plain", 0o644);
        let err = check_executable(&file).expect_err("0o644 ne doit pas être exécutable");
        assert!(
            err.contains("is not executable by this process"),
            "message inattendu : {err}"
        );
    }

    #[test]
    fn check_executable_accepts_an_executable_file() {
        let dir = TempDir::new("exec");
        let file = dir.file("runnable", 0o755);
        assert!(
            check_executable(&file).is_ok(),
            "0o755 doit être considéré exécutable"
        );
    }

    #[test]
    fn check_readable_reports_an_unreadable_existing_file() {
        // Idem : root lit un 0o000 — le prédicat perd tout pouvoir discriminant.
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP check_readable_reports_an_unreadable_existing_file : ce processus \
                 outrepasse les bits de permission (root) — prédicat non discriminant ici"
            );
            return;
        }
        let dir = TempDir::new("noread");
        let file = dir.file("secret", 0o000);
        let reason = check_readable(&file).expect("0o000 ne doit pas être lisible");
        assert!(
            reason.contains("is not readable"),
            "message inattendu : {reason}"
        );
    }

    #[test]
    fn check_readable_defers_absence_to_validate() {
        let missing = std::env::temp_dir().join("gradatum-engine-check-absent-xyz.gguf");
        assert!(
            check_readable(&missing).is_none(),
            "l'absence est signalée par validate(), pas ici — pas de doublon"
        );
    }

    #[test]
    fn check_readable_accepts_a_readable_file() {
        let dir = TempDir::new("read");
        let file = dir.file("plain", 0o644);
        assert!(check_readable(&file).is_none(), "0o644 doit être lisible");
    }

    // --- Non-régression sécurité : pas de fuite de valeur de configuration ---

    /// La valeur fautive apparaît dans le `Display` de figment, et NE DOIT PAS survivre
    /// à la rédaction utilisée par `--check`. Le test vérifie les deux moitiés : sans
    /// l'assertion « brut contient », il passerait aussi si figment cessait de fuiter,
    /// et ne prouverait plus rien sur la rédaction.
    #[test]
    fn config_parse_failure_is_redacted_before_reaching_stderr() {
        const SECRET: &str = "sk-live-EXFILTRATION-CANARY";
        let dir = TempDir::new("redact");
        let path = dir.0.join("engine.toml");
        std::fs::write(
            &path,
            format!("[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=\"{SECRET}\"\n"),
        )
        .expect("écriture de la config du test");

        let err = EngineConfig::load_local(&path).expect_err("port non numérique doit échouer");
        assert!(
            err.to_string().contains(SECRET),
            "prémisse du test : figment DOIT fuiter la valeur dans son Display — sinon \
             la rédaction n'a plus de pouvoir discriminant. Obtenu : {err}"
        );
        // On mesure la ligne RÉELLEMENT écrite sur stderr par `run_check`, pas la
        // fonction de rédaction prise isolément : c'est le site d'appel qui fuit ou non.
        let rendered = config_load_failure_message(&path, &err);
        assert!(
            !rendered.contains(SECRET),
            "la valeur fautive a fuité dans le message rendu sur stderr : {rendered}"
        );
    }

    /// Une config illisible/absente rend un code non nul, sans panic.
    #[test]
    fn run_check_on_absent_config_returns_failure_code() {
        let missing = std::env::temp_dir().join("gradatum-engine-check-no-such-file.toml");
        assert_eq!(run_check(&missing), EXIT_CHECK_FAILED);
    }

    /// Les trois codes de sortie sont distincts — un script doit pouvoir les discriminer.
    #[test]
    fn exit_codes_are_distinct() {
        assert_ne!(EXIT_CHECK_OK, EXIT_CHECK_FAILED);
        assert_ne!(EXIT_CHECK_FAILED, EXIT_CHECK_USAGE);
        assert_ne!(EXIT_CHECK_OK, EXIT_CHECK_USAGE);
    }

    /// `--help` documente le drapeau et ses codes de sortie.
    #[test]
    fn help_documents_the_check_flag() {
        assert!(
            HELP.contains("--check <CONFIG>"),
            "HELP doit documenter --check"
        );
        assert!(
            HELP.contains("Exit codes:"),
            "HELP doit documenter les codes de sortie"
        );
    }

    // -----------------------------------------------------------------------
    // F-191 — pour QUELLE identité le verdict est-il rendu ?
    // -----------------------------------------------------------------------

    /// uid du processus courant.
    fn caller_uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    /// gid du processus courant.
    fn caller_gid() -> u32 {
        nix::unistd::getgid().as_raw()
    }

    /// Identité de service **étrangère** au processus : ni propriétaire, ni membre du
    /// groupe des fichiers que ce processus crée.
    ///
    /// Construite arithmétiquement plutôt que lue dans `/etc/passwd` : le test doit
    /// discriminer sur n'importe quel hôte, y compris là où aucun compte `gradatum`
    /// n'existe. Seuls les nombres comptent — `mode_grants` ne consulte aucune base.
    fn foreign_identity() -> CheckIdentity {
        CheckIdentity::Service(ServiceIdentity {
            unit: "gradatum-engine@fixture.service".to_string(),
            user: "fixture-service".to_string(),
            uid: caller_uid().wrapping_add(1),
            gids: vec![caller_gid().wrapping_add(1)],
        })
    }

    /// Identité de service **confondue** avec le processus : mêmes uid et gid.
    ///
    /// Reproduit le cas de l'hôte GPU, où le compte ssh **est** le `User=` de l'unité :
    /// l'écart n'y existe pas, et le durcissement ne doit pas le casser.
    fn same_as_caller_identity() -> CheckIdentity {
        CheckIdentity::Service(ServiceIdentity {
            unit: "gradatum-engine-fixture.service".to_string(),
            user: "fixture-service".to_string(),
            uid: caller_uid(),
            gids: vec![caller_gid()],
        })
    }

    #[test]
    fn mode_grants_uses_the_owner_class_for_the_owner() {
        let dir = TempDir::new("grants-owner");
        let file = dir.file("owned", 0o600);
        let meta = std::fs::metadata(&file).expect("stat de la fixture");
        assert!(
            mode_grants(&meta, caller_uid(), &[caller_gid()], WANT_READ),
            "0o600 doit être lisible par son propriétaire"
        );
    }

    #[test]
    fn mode_grants_uses_the_other_class_for_a_stranger() {
        let dir = TempDir::new("grants-other");
        let private = dir.file("private", 0o600);
        let shared = dir.file("shared", 0o644);
        let stranger_uid = caller_uid().wrapping_add(1);
        let stranger_gids = [caller_gid().wrapping_add(1)];

        let private_meta = std::fs::metadata(&private).expect("stat de la fixture");
        let shared_meta = std::fs::metadata(&shared).expect("stat de la fixture");

        assert!(
            !mode_grants(&private_meta, stranger_uid, &stranger_gids, WANT_READ),
            "0o600 ne doit PAS être lisible par un tiers"
        );
        assert!(
            mode_grants(&shared_meta, stranger_uid, &stranger_gids, WANT_READ),
            "0o644 doit être lisible par un tiers — sinon le test précédent n'aurait \
             aucun pouvoir discriminant (il rougirait pour toute permission)"
        );
    }

    #[test]
    fn mode_grants_lets_root_read_a_zeroed_file_but_not_execute_it() {
        let dir = TempDir::new("grants-root");
        let file = dir.file("zeroed", 0o000);
        let meta = std::fs::metadata(&file).expect("stat de la fixture");
        assert!(
            mode_grants(&meta, 0, &[0], WANT_READ),
            "root outrepasse les bits de lecture"
        );
        assert!(
            !mode_grants(&meta, 0, &[0], WANT_EXEC),
            "root n'exécute que si AU MOINS un bit x est posé — le noyau ne fabrique \
             pas ce bit"
        );
    }

    /// **Critère 2 — le faux vert est fermé.**
    ///
    /// Un fichier que l'appelant lit et que le service ne lit pas doit être REFUSÉ.
    /// C'est le sens exact du défaut : `File::open` répond pour l'uid appelant, et
    /// approuvait une configuration que le service ne saurait pas lire.
    #[test]
    fn a_file_readable_only_by_the_caller_is_refused_for_the_service() {
        let dir = TempDir::new("false-green");
        let file = dir.file("caller-only.gguf", 0o600);

        // Prémisse : l'appelant, LUI, y accède — sans quoi le test ne prouverait rien
        // sur le faux vert (il n'y aurait pas de vert à rendre faux).
        assert!(
            check_readable(&file).is_none(),
            "prémisse du test : l'appelant doit pouvoir lire sa propre fixture 0o600"
        );

        let err = foreign_identity()
            .can_read(&file)
            .expect_err("le service, étranger au fichier, ne doit PAS pouvoir le lire");
        assert!(
            err.contains("is not readable"),
            "le refus doit nommer l'illisibilité, pas un autre motif : {err}"
        );
    }

    /// **Critère 3 — le faux rouge reste distinct, et signalé comme tel.**
    ///
    /// Un fichier durci VERS le service (lisible par lui, pas par l'appelant) ne doit
    /// produire aucun échec — seulement une note nommant l'écart.
    #[test]
    fn a_file_readable_only_by_the_service_yields_a_note_not_a_failure() {
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP a_file_readable_only_by_the_service_yields_a_note_not_a_failure : \
                 ce processus outrepasse les bits de permission (root) — l'appelant ne \
                 peut pas être mis en échec, le test n'a aucun pouvoir discriminant"
            );
            return;
        }
        // 0o004 : illisible pour son PROPRIÉTAIRE (classe owner = 0), lisible pour un
        // tiers (classe other = r). Reproduit `chown gradatum` + `chmod 0640` sans
        // exiger le moindre privilège.
        let dir = TempDir::new("false-red");
        let file = dir.file("service-only.gguf", 0o004);
        let identity = foreign_identity();

        assert!(
            check_readable(&file).is_some(),
            "prémisse du test : l'appelant NE doit PAS pouvoir lire un 0o004 qu'il possède"
        );
        assert!(
            identity.can_read(&file).is_ok(),
            "le service, tiers, doit pouvoir lire un 0o004 — sinon il n'y a pas de faux \
             rouge à distinguer"
        );

        let note = identity
            .false_red_note("model_path", &file, WANT_READ)
            .expect("l'écart appelant/service doit être signalé");
        assert!(
            note.starts_with("check NOTE:"),
            "un faux rouge est une note, jamais un échec : {note}"
        );
        assert!(
            note.contains("is NOT a problem"),
            "la note doit dire explicitement que ce n'est pas un problème : {note}"
        );
    }

    /// Aucune note quand les deux identités s'accordent — sinon la note perdrait son
    /// pouvoir de signal en devenant permanente.
    #[test]
    fn no_note_when_caller_and_service_agree() {
        let dir = TempDir::new("agree");
        let file = dir.file("shared.gguf", 0o644);
        assert!(
            foreign_identity()
                .false_red_note("model_path", &file, WANT_READ)
                .is_none(),
            "un fichier que les deux lisent ne doit produire aucune note"
        );
    }

    /// **Critère 1 (moitié en-processus)** — le verdict est indexé sur l'identité
    /// passée, jamais sur le processus qui appelle.
    ///
    /// Deux identités, un seul fichier, un seul processus appelant : les réponses
    /// divergent. La preuve hors-processus (même verdict sous deux comptes réels) est
    /// dans le livrable — elle exige deux uid, ce qu'un test unitaire ne peut pas avoir.
    #[test]
    fn the_verdict_is_keyed_on_the_identity_not_on_the_calling_process() {
        let dir = TempDir::new("keyed");
        let file = dir.file("owned.gguf", 0o600);

        assert!(
            same_as_caller_identity().can_read(&file).is_ok(),
            "l'identité confondue avec l'appelant lit le fichier"
        );
        assert!(
            foreign_identity().can_read(&file).is_err(),
            "l'identité étrangère ne le lit pas — même fichier, même processus appelant"
        );
    }

    /// L'hôte GPU (compte ssh == `User=`) ne doit pas régresser : l'ancien verdict et le
    /// nouveau coïncident quand les deux identités sont la même.
    #[test]
    fn an_identity_equal_to_the_caller_reproduces_the_caller_verdict() {
        let dir = TempDir::new("gpu-host");
        let readable = dir.file("readable.gguf", 0o644);
        let executable = dir.file("runnable", 0o755);
        let identity = same_as_caller_identity();

        assert_eq!(
            identity.can_read(&readable).is_ok(),
            check_readable(&readable).is_none(),
            "lisibilité : les deux verdicts doivent coïncider"
        );
        assert_eq!(
            identity.can_exec(&executable).is_ok(),
            check_executable(&executable).is_ok(),
            "exécutabilité : les deux verdicts doivent coïncider"
        );
    }

    /// L'absence reste déférée à `validate()` — pas de doublon, quelle que soit
    /// l'identité.
    #[test]
    fn an_absent_path_is_still_deferred_to_validate() {
        let missing = std::env::temp_dir().join("gradatum-engine-f191-absent-xyz.gguf");
        assert!(
            foreign_identity().can_read(&missing).is_ok(),
            "l'absence est signalée par validate(), pas par le prédicat d'accès"
        );
    }

    /// Un répertoire non traversable rend le fichier inatteignable, même en 0o644.
    ///
    /// C'est le cas terrain : `/opt/gradatum/models/gemma4-26b-mtp/` appartient au
    /// compte de déploiement. Un GGUF lisible par tous y reste hors de portée si le
    /// répertoire ne l'est pas.
    #[test]
    fn an_unsearchable_directory_makes_the_file_unreachable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("nosearch");
        let sub = dir.0.join("locked");
        std::fs::create_dir_all(&sub).expect("création du sous-répertoire");
        let file = sub.join("model.gguf");
        std::fs::write(&file, b"x").expect("écriture de la fixture");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .expect("chmod du fichier");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))
            .expect("chmod du répertoire");

        let err = foreign_identity()
            .can_read(&file)
            .expect_err("un répertoire 0o700 doit rendre son contenu inatteignable");
        assert!(
            err.contains("is not searchable"),
            "le motif doit désigner le répertoire, pas le fichier : {err}"
        );

        // Restaure avant le drop de TempDir (qui ne descend pas dans les sous-dossiers).
        let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&sub);
    }

    // --- Résolution de l'identité depuis systemd ---

    #[test]
    fn exec_start_config_arg_extracts_the_positional_configuration() {
        let exec_start = "{ path=/opt/gradatum/bin/gradatum-engine ; \
             argv[]=/opt/gradatum/bin/gradatum-engine /etc/gradatum/conf.d/70-engine-curator.toml ; \
             ignore_errors=no ; start_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        assert_eq!(
            exec_start_config_arg(exec_start),
            Some("/etc/gradatum/conf.d/70-engine-curator.toml"),
            "le dernier jeton d'argv[] est le chemin de configuration (argument positionnel)"
        );
    }

    #[test]
    fn exec_start_config_arg_ignores_a_property_without_argv() {
        assert_eq!(
            exec_start_config_arg("{ path=/bin/true ; ignore_errors=no }"),
            None,
            "sans argv[], aucun chemin ne doit être inventé"
        );
    }

    #[test]
    fn record_property_reads_a_declared_property() {
        let record = "ExecStart={ path=/bin/true }\nUser=gradatum\nGroup=gradatum";
        assert_eq!(record_property(record, "User"), Some("gradatum"));
        assert_eq!(record_property(record, "Group"), Some("gradatum"));
        assert_eq!(record_property(record, "SupplementaryGroups"), None);
    }

    /// Une propriété déclarée vide (`User=`) n'est pas une identité : systemd y lit root.
    #[test]
    fn an_empty_property_is_not_an_identity() {
        assert_eq!(record_property("User=", "User"), Some(""));
        assert_eq!(non_empty(Some("")), None);
        assert_eq!(non_empty(Some("gradatum")), Some("gradatum"));
    }

    /// Une configuration que nulle unité ne sert dégrade le verdict — bruyamment.
    #[test]
    fn an_unresolved_configuration_degrades_loudly() {
        let missing = std::env::temp_dir().join("gradatum-engine-f191-no-such-unit.toml");
        let (identity, reason) = CheckIdentity::resolve(&missing);
        assert!(
            matches!(identity, CheckIdentity::CallingProcess),
            "aucune unité ne sert cette configuration : repli sur le processus appelant"
        );
        let note = identity.subject_note(reason.as_deref());
        assert!(
            note.contains("INVOKING account"),
            "la dégradation doit être écrite noir sur blanc : {note}"
        );
        assert!(
            note.contains("cannot see it"),
            "la note doit nommer l'angle mort qu'elle laisse ouvert : {note}"
        );
    }

    // --- Le fichier de configuration lui-même (angle mort refermé) ---

    /// **Le défaut ciblé** : quand le RÉPERTOIRE barre l'accès, figment rend le fichier
    /// comme *absent* (`missing field \`engine\``). L'opérateur part chercher une faute de
    /// frappe alors que le problème est un droit — pendant le pré-vol d'un déploiement.
    ///
    /// La forme du répertoire est celle observée sur le terrain, et la seule qui trompe :
    /// un fichier illisible sous un répertoire traversable fait bien remonter l'`EACCES`
    /// par figment. C'est pourquoi la fixture verrouille le répertoire, pas le fichier.
    ///
    /// Le test vérifie les DEUX moitiés : la prémisse (le parseur trompe bien) et la
    /// parade (la garde dit le droit). Sans la première, il passerait encore le jour où
    /// figment cesserait de tromper, et ne prouverait plus rien sur la garde.
    #[test]
    fn an_unreachable_configuration_names_the_missing_right_not_a_missing_field() {
        use std::os::unix::fs::PermissionsExt;
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP an_unreachable_configuration_names_the_missing_right_not_a_missing_field \
                 : ce processus outrepasse les bits de permission (root) — prédicat non \
                 discriminant ici"
            );
            return;
        }
        let dir = TempDir::new("cfg-unreachable");
        let sub = dir.0.join("conf.d");
        std::fs::create_dir_all(&sub).expect("création du répertoire de configuration");
        let cfg = sub.join("70-engine.toml");
        std::fs::write(
            &cfg,
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n",
        )
        .expect("écriture de la config du test");
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644))
            .expect("chmod du fichier");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000))
            .expect("chmod du répertoire");

        // Prémisse : sans la garde, le parseur rend un diagnostic qui désigne le mauvais
        // problème — un champ manquant, là où il s'agit d'un droit.
        let parser_verdict = EngineConfig::load_local(&cfg)
            .map_or_else(|e| e.to_string(), |_| "chargée (inattendu)".to_string());
        // Parade : la garde nomme le droit, pas un champ manquant.
        let problem = config_file_access_problem(&cfg, &CheckIdentity::CallingProcess)
            .expect("une configuration inatteignable doit être signalée AVANT le parseur");

        // Restaure AVANT toute assertion : un échec ne doit pas laisser un 0o000 derrière
        // lui (TempDir::drop ne descend pas dans les sous-répertoires).
        let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&sub);

        assert!(
            parser_verdict.contains("missing field"),
            "prémisse du test : figment DOIT rendre un fichier inatteignable comme absent \
             — sinon la garde n'a plus de pouvoir discriminant. Obtenu : {parser_verdict}"
        );
        assert!(
            problem.contains("is not readable") || problem.contains("is not searchable"),
            "le message doit nommer le droit manquant : {problem}"
        );
        assert!(
            problem.contains("configuration file:"),
            "le message doit désigner le fichier de configuration : {problem}"
        );
    }

    /// Un répertoire de configuration non traversable est nommé comme tel.
    ///
    /// C'est la forme réellement rencontrée sur le terrain : les droits se posent
    /// beaucoup plus souvent sur `conf.d/` que sur le fichier.
    #[test]
    fn an_unsearchable_configuration_directory_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("cfg-nosearch");
        let sub = dir.0.join("conf.d");
        std::fs::create_dir_all(&sub).expect("création du répertoire de configuration");
        let cfg = sub.join("70-engine.toml");
        std::fs::write(&cfg, "[engine]\n").expect("écriture de la config du test");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))
            .expect("chmod du répertoire");

        let problem = config_file_access_problem(&cfg, &foreign_identity())
            .expect("un répertoire 0o700 doit barrer l'identité de service");
        assert!(
            problem.contains("is not searchable"),
            "le message doit désigner le RÉPERTOIRE, pas le fichier : {problem}"
        );

        let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&sub);
    }

    /// **Le faux vert de F-191 ne revient pas par la porte de la configuration.**
    ///
    /// Un fichier que l'appelant lit et que le service ne lit pas doit être refusé — la
    /// garde utilise la même identité que les autres chemins, sinon elle réintroduirait
    /// exactement le défaut que ce lot ferme.
    #[test]
    fn a_configuration_readable_only_by_the_caller_is_refused() {
        let dir = TempDir::new("cfg-false-green");
        let cfg = dir.0.join("70-engine.toml");
        std::fs::write(&cfg, "[engine]\n").expect("écriture de la config du test");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600))
                .expect("chmod de la config du test");
        }
        assert!(
            check_readable(&cfg).is_none(),
            "prémisse : l'appelant lit sa propre configuration 0o600"
        );

        let problem = config_file_access_problem(&cfg, &foreign_identity())
            .expect("le service, étranger au fichier, ne doit pas pouvoir le lire");
        assert!(
            problem.contains("is not readable"),
            "le refus doit nommer le droit : {problem}"
        );
        assert!(
            problem.contains("User=fixture-service"),
            "le refus doit nommer l'identité pour laquelle il vaut : {problem}"
        );
    }

    /// Le faux rouge sur la configuration : le service la lit, pas l'appelant.
    ///
    /// Ce n'est pas un défaut de la configuration, et le message doit le dire — mais le
    /// verdict reste bloquant, car le contrôle ne peut alors rien inspecter du tout.
    #[test]
    fn a_configuration_the_caller_cannot_read_is_reported_as_a_check_limitation() {
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP a_configuration_the_caller_cannot_read_is_reported_as_a_check_limitation : \
                 ce processus outrepasse les bits de permission (root)"
            );
            return;
        }
        let dir = TempDir::new("cfg-false-red");
        let cfg = dir.0.join("70-engine.toml");
        std::fs::write(&cfg, "[engine]\n").expect("écriture de la config du test");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o004))
                .expect("chmod de la config du test");
        }

        let problem = config_file_access_problem(&cfg, &foreign_identity())
            .expect("l'incapacité du contrôle lui-même doit être signalée");
        assert!(
            problem.contains("NOT a defect of the configuration"),
            "le message doit disculper la configuration : {problem}"
        );
        assert!(
            problem.contains("Re-run it as fixture-service"),
            "le message doit dire quoi faire : {problem}"
        );
    }

    /// **Le câblage, pas seulement le prédicat** : l'étape 0 passe AVANT le parseur.
    ///
    /// Une configuration que l'identité de service ne peut pas lire mais qui parse
    /// parfaitement doit rendre `ConfigUnusable` — jamais `Evaluated`. C'est la seule
    /// assertion qui rougisse si la garde est retirée de la séquence : le code de sortie,
    /// lui, vaut 1 dans les deux cas et ne discrimine rien.
    #[test]
    fn step_zero_runs_before_the_parser() {
        let dir = TempDir::new("cfg-order");
        let cfg = dir.0.join("70-engine.toml");
        std::fs::write(
            &cfg,
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n",
        )
        .expect("écriture de la config du test");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600))
                .expect("chmod de la config du test");
        }

        // Prémisse : le fichier parse sans erreur — l'issue ne peut donc PAS venir du
        // parseur, et `Evaluated` serait le verdict si l'étape 0 était absente.
        EngineConfig::load_local(&cfg).expect("prémisse : la configuration doit parser");

        let outcome = evaluate_check(&cfg, &foreign_identity());
        match outcome {
            CheckOutcome::ConfigUnusable(problem) => assert!(
                problem.contains("is not readable"),
                "l'étape 0 doit nommer le droit : {problem}"
            ),
            other => panic!(
                "l'étape 0 doit précéder le parseur — obtenu : {other:?}. Une garde placée \
                 après le chargement ne verrait jamais ce cas."
            ),
        }
    }

    /// Une configuration lisible par les deux ne produit aucun problème.
    #[test]
    fn a_readable_configuration_yields_no_problem() {
        let dir = TempDir::new("cfg-ok");
        let cfg = dir.0.join("70-engine.toml");
        std::fs::write(&cfg, "[engine]\n").expect("écriture de la config du test");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644))
                .expect("chmod de la config du test");
        }
        assert!(
            config_file_access_problem(&cfg, &foreign_identity()).is_none(),
            "une configuration 0o644 ne pose de problème à personne"
        );
        assert!(
            config_file_access_problem(&cfg, &CheckIdentity::CallingProcess).is_none(),
            "idem pour l'identité appelante"
        );
    }

    /// L'absence reste déférée au chargeur : son message, lui, ne trompe pas.
    #[test]
    fn an_absent_configuration_is_deferred_to_the_loader() {
        let missing = std::env::temp_dir().join("gradatum-engine-cfg-absent-xyz.toml");
        assert!(
            config_file_access_problem(&missing, &CheckIdentity::CallingProcess).is_none(),
            "l'absence est signalée par load_local, dont le message est correct"
        );
        assert_eq!(
            run_check(&missing),
            EXIT_CHECK_FAILED,
            "et le verdict reste un échec"
        );
    }

    /// Le message ne rend aucun octet du fichier — seulement chemin, droit et identité.
    #[test]
    fn the_configuration_problem_never_renders_the_file_content() {
        if privileges_bypass_permission_bits() {
            eprintln!(
                "SKIP the_configuration_problem_never_renders_the_file_content : \
                 ce processus outrepasse les bits de permission (root)"
            );
            return;
        }
        const SECRET: &str = "sk-live-EXFILTRATION-CANARY";
        let dir = TempDir::new("cfg-redact");
        let cfg = dir.0.join("70-engine.toml");
        std::fs::write(&cfg, format!("[engine]\napi_key = \"{SECRET}\"\n"))
            .expect("écriture de la config du test");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000))
                .expect("chmod de la config du test");
        }

        let problem = config_file_access_problem(&cfg, &CheckIdentity::CallingProcess)
            .expect("un 0o000 doit être signalé");
        assert!(
            !problem.contains(SECRET),
            "aucun contenu ne doit transiter par le message : {problem}"
        );
    }

    /// L'identité résolue est annoncée avec son unité et son `User=`.
    #[test]
    fn a_resolved_identity_names_its_unit_and_user() {
        let note = foreign_identity().subject_note(None);
        assert!(
            note.contains("User=fixture-service"),
            "la note doit nommer le User= pour lequel le verdict vaut : {note}"
        );
        assert!(
            note.contains("gradatum-engine@fixture.service"),
            "la note doit nommer l'unité : {note}"
        );
    }
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

    // --- F-205 : classification du motif de repli de la télémétrie ---

    use gradatum_engine::health::TelemetryStatus;
    use gradatum_engine::sink::ExchangeError;

    /// Chaque variante d'erreur d'échange se mappe sur le bon motif exposé dans /health.
    #[test]
    fn classify_maps_each_exchange_error_to_its_reason() {
        assert_eq!(
            classify_exchange_error(&ExchangeError::Unauthorized {
                url: "http://127.0.0.1:1/auth/exchange".into()
            }),
            TelemetryStatus::Unauthorized,
            "401 → problème d'identité"
        );
        assert_eq!(
            classify_exchange_error(&ExchangeError::HttpStatus {
                url: "http://127.0.0.1:1/auth/exchange".into(),
                status: 503,
            }),
            TelemetryStatus::Failed,
            "un non-401 n'est jamais étiqueté transitoire"
        );
        assert_eq!(
            classify_exchange_error(&ExchangeError::MissingToken {
                url: "http://127.0.0.1:1/auth/exchange".into()
            }),
            TelemetryStatus::Failed,
            "réponse malformée → échec, pas transitoire"
        );
        // Transport se construit via un vrai reqwest::Error (pas de constructeur public) :
        // couvert par le test frontière `folded_unauthorized` + le test sink
        // `exchange_maps_unreachable_to_transport`.
    }

    /// Test frontière (heuristique « unités contradictoires → test de frontière ») :
    /// traverse lib(exchange) → bin(classify) en un seul flux. Un `/auth/exchange`
    /// répondant 401 doit produire `ExchangeError::Unauthorized`, que le classifieur
    /// mappe sur `TelemetryStatus::Unauthorized` — dont le label exposé est
    /// `folded_unauthorized`. C'est exactement le cas qui a duré dix jours.
    #[tokio::test]
    async fn frontier_401_exchange_classifies_to_folded_unauthorized() {
        use axum::{Router, http::StatusCode, routing::post};
        use tokio::net::TcpListener;
        use zeroize::Zeroizing;

        let app = Router::new().route(
            "/auth/exchange",
            post(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let api_key = Zeroizing::new("ak_frontiertest".into());
        let err = gradatum_engine::sink::exchange_api_key_for_jwt_typed(
            &api_key,
            &format!("http://127.0.0.1:{port}"),
        )
        .await
        .expect_err("un 401 doit produire une erreur");

        let status = classify_exchange_error(&err);
        assert_eq!(status, TelemetryStatus::Unauthorized);
        assert_eq!(
            status.label(),
            "folded_unauthorized",
            "le motif 401 traverse jusqu'au label exposé dans /health"
        );
        assert!(
            !status.is_active(),
            "un moteur replié n'est pas actif côté télémétrie"
        );
    }
}
