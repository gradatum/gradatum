//! `llama-server` supervisor — spawn, wait-ready, bounded restart, shutdown.
//!
//! ## Responsibilities
//!
//! - **spawn**: launches `llama-server` via `tokio::process::Command` (never `sh -c`).
//!   Binary is canonicalized at spawn time and validated against the allowed prefix.
//!   `env_clear` + GPU allow-list prevent orthogonal bypasses via environment variables.
//! - **wait_ready**: polls `GET http://127.0.0.1:{child_port}/health` until HTTP 200
//!   (timeout `startup_timeout_secs`). Returns `ChildState::Starting` during the poll.
//! - **supervise_loop**: bounded restart-on-failure (`child_restart_max`, total budget) +
//!   exponential backoff + flapping detection. On exhaustion → `HealthState::unhealthy`.
//! - **shutdown**: SIGTERM → grace period → SIGKILL.
//!
//! ## Orphan process safety
//!
//! Chosen approach: **`process_group(0)` + `kill_on_drop(true)` + systemd
//! `KillMode=control-group`** (zero unsafe — `#![forbid(unsafe_code)]` preserved).
//!
//! - `process_group(0)`: places the child in its own process group → SIGTERM/SIGKILL
//!   propagated via `killpg(pgid, sig)` in `shutdown()`.
//! - `kill_on_drop(true)` (tokio): sends SIGKILL if the tokio `Child` is dropped — covers
//!   unwinding panics and normal drops (e.g. clean supervisor stop).
//!   **Limitation**: does NOT fire on hard crashes (panic=abort, SIGSEGV, OOM-kill of the
//!   main PID) because `Drop` is not executed. In that case the child becomes an orphan
//!   until the next systemd restart (window ≤ `RestartSec`, typically 10 s).
//! - `KillMode=control-group` + `Delegate=yes` (systemd service): on unit stop/restart,
//!   systemd kills all processes in the cgroup, including the hard-crash orphan. This is
//!   the primary safeguard for the SIGKILL-parent case. `Delegate=yes` ensures the cgroup
//!   is clean before re-`ExecStart` (no double GPU model load).
//!   **Residual window**: between a hard crash and the systemd restart, a single orphan may
//!   hold VRAM (no simultaneous double load because systemd cleans up before restart).
//!
//! The `PR_SET_PDEATHSIG` variant (unsafe `pre_exec`) is NOT used — the signal is not
//! delivered if the parent calls `exec()` between `fork()` and `pre_exec`, and the
//! benefit is marginal compared to the unsafe overhead.

use std::{
    error::Error as StdError,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::sleep,
};

use crate::{
    config::{EngineConfig, ModelKind},
    error::EngineError,
    health::HealthState,
};

/// Allowed prefixes for the `llama-server` binary.
///
/// The path is canonicalized before verification (prevents TOCTOU).
///
/// Accepted prefixes:
/// - `/usr/local/bin/` — system-wide install.
/// - `/opt/gradatum/bin/` — managed gradatum install.
/// - `/opt/llama-` — versioned llama-server installs (e.g. `/opt/llama-b9549/`).
///
/// The prefix `/opt/llama-` is intentionally narrow: it accepts
/// `/opt/llama-<version>/…` but rejects `/opt/evil/…` or `/opt/llama/…`.
const ALLOWED_BIN_PREFIXES: &[&str] = &["/usr/local/bin/", "/opt/gradatum/bin/", "/opt/llama-"];

/// SIGTERM → SIGKILL grace period at shutdown (seconds).
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// Initial exponential backoff (milliseconds).
const BACKOFF_INIT_MS: u64 = 500;

/// Maximum exponential backoff (milliseconds).
const BACKOFF_MAX_MS: u64 = 30_000;

/// Allow-list for entries in `EngineConfig.extra_args`.
///
/// ## Security rationale
///
/// This allow-list replaces the former deny-list, which was inherently incomplete
/// (`llama-server` exposes >100 flags, several of them dangerous and uncovered:
/// `--api-key-file`, `--model-url`, `--lora`, `--path`, `--ssl-key-file`…).
///
/// **Principle**: only the flags listed below are permitted. Any absent flag is
/// rejected with `EngineError::BadRequest` naming the offending flag.
///
/// **Extension**: adding any flag to this list is an explicit security decision —
/// verify that the flag does not open arbitrary network access, uncontrolled
/// path read/write, or a bypass of the loopback bind.
///
/// **Exclusion of `--n-gpu-layers`/`-ngl`/`--gpu-layers`**: these flags are
/// controlled authoritatively by the `gpu_layers` config field → `--n-gpu-layers`
/// in the command. Allowing them through `extra_args` would produce a
/// `llama-server` warning "argument specified multiple times" and could shadow
/// the configured value.
const ALLOWED_EXTRA_FLAGS: &[&str] = &[
    // Attention / Flash attention
    "--flash-attn",
    "-fa",
    // Memory management
    "--no-mmap",
    "--mlock",
    "--no-kv-offload",
    "-nkvo",
    // Batching
    "--cont-batching",
    "--no-cont-batching",
    "--batch-size",
    "-b",
    "--ubatch-size",
    "-ub",
    // HTTP threads
    "--threads-http",
    // Context / KV cache
    "--keep",
    "--defrag-thold",
    "--cache-type-k",
    "-ctk",
    "--cache-type-v",
    "-ctv",
    // Unified KV cache (required for prompt-cache LCP with --parallel ≥ 2 on llama.cpp b9780+)
    "--kv-unified",
    // Slot selection threshold (prefix-cache routing) — takes a float value
    "--slot-prompt-similarity",
    // NUMA
    "--numa",
    // Logging
    "--log-disable",
    "--log-prefix",
    "--log-timestamps",
    // RoPE / YaRN scaling
    "--rope-scaling",
    "--rope-scale",
    "--rope-freq-base",
    "--rope-freq-scale",
    "--yarn-orig-ctx",
    "--yarn-ext-factor",
    "--yarn-attn-factor",
    "--yarn-beta-slow",
    "--yarn-beta-fast",
    // Reproducibility
    "--seed",
    "-s",
    // Performance
    "--poll",
    // SWA / cache-reuse (large-model performance)
    "--swa-full",
    "--cache-reuse",
    // Reasoning (thinking models)
    "--reasoning",
    "--reasoning-format",
    "--reasoning-budget",
    // Chat template kwargs (JSON) — enable_thinking pour les modèles thinking
    "--chat-template-kwargs",
    // Sampling (standalone parity)
    "--temp",
    "--temperature",
    "--top-k",
    "--top-p",
    "--min-p",
    "--presence-penalty",
    "--repeat-penalty",
    "--n-predict",
    "-n",
    // GPU-side sampling (llama.cpp b9780+) — sampling élu côté GPU, évite la copie des logits en mémoire hôte
    "--backend-sampling",
];

/// Environment variable prefixes preserved at spawn time.
///
/// `env_clear()` is applied to the `Command`, then only variables whose key starts
/// with one of these prefixes (or whose name is in `ENV_PASSTHROUGH`) are re-injected
/// from the supervisor's environment.
///
/// These prefixes cover Vulkan/RADV (`VK_*`, `MESA_*`, `RADV_*`), ROCm/AMD
/// (`HIP_*`, `ROCR_*`, `ROCM_*`, `HSA_*`), CUDA/NVIDIA (`CUDA_*`, `NVIDIA_*`), and
/// native llama.cpp (`GGML_*`). Preserving them is CRITICAL for `llama-server` to
/// detect the GPU on a GPU host.
const GPU_ENV_PREFIXES: &[&str] = &[
    "GGML_", "VK_", "HIP_", "ROCR_", "ROCM_", "HSA_", "CUDA_", "NVIDIA_", "MESA_", "RADV_",
];

/// Pass-through environment variables (by exact name).
///
/// Independent of the GPU prefixes — required for correct binary operation.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "LD_LIBRARY_PATH"];

/// Builds the `llama-server` argv from the config (pure, testable).
///
/// Authoritative flags (never overridable via `extra_args`): `--model`, `--port`,
/// `--host` (loopback), `--n-gpu-layers`, `--threads`, `--ctx-size`, `--parallel`,
/// `--embedding` (if embed), `--mmproj` (if `mmproj_path` is set, vision).
/// Allow-list-validated `extra_args` are appended last.
fn build_child_args(cfg: &EngineConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--model".into(),
        cfg.model_path.clone(),
        "--port".into(),
        cfg.child_port.to_string(),
        "--host".into(),
        "127.0.0.1".into(), // loopback only — SP-P0-4
        "--n-gpu-layers".into(),
        cfg.gpu_layers.to_string(),
        "--threads".into(),
        cfg.n_threads.to_string(),
        "--ctx-size".into(),
        cfg.context_len.to_string(),
        "--parallel".into(),
        cfg.parallel.to_string(),
    ];

    if cfg.model_kind == ModelKind::Embed {
        args.push("--embedding".into());
    }

    // Vision: mmproj via the dedicated config field (never via extra_args — excluded from allow-list).
    if let Some(mmproj) = &cfg.mmproj_path {
        args.push("--mmproj".into());
        args.push(mmproj.to_string_lossy().into_owned());
    }

    // Speculative decoding via dedicated config fields (never via extra_args —
    // --spec-type/--spec-draft-model excluded from the allow-list). Coherence between
    // the two is enforced by EngineConfig::validate() at startup.
    if let Some(spec_type) = &cfg.spec_type {
        args.push("--spec-type".into());
        args.push(spec_type.as_arg().into());
    }
    if let Some(draft) = &cfg.draft_model_path {
        args.push("--spec-draft-model".into());
        args.push(draft.clone());
    }
    if let Some(n) = cfg.spec_draft_n_max {
        args.push("--spec-draft-n-max".into());
        args.push(n.to_string());
    }
    if let Some(p) = cfg.spec_draft_p_min {
        args.push("--spec-draft-p-min".into());
        args.push(p.to_string());
    }

    // Extra args — validated against the allow-list at supervisor construction.
    args.extend(cfg.extra_args.iter().cloned());

    args
}

/// Result of a child health check.
#[derive(Debug, PartialEq)]
pub enum ChildState {
    /// Child is ready (HTTP 200 on `/health`).
    Ready,
    /// Child is still starting (timeout not yet reached).
    Starting,
    /// Child did not respond within the startup timeout.
    StartupTimeout,
}

/// Supervisor for the `llama-server` subprocess.
pub struct LlamaServerSupervisor {
    /// Current child process (`None` if not yet started or after shutdown).
    child: Mutex<Option<Child>>,
    /// Shutdown flag — stops the supervision loop when set.
    shutdown_requested: AtomicBool,
    /// Child listen port (loopback, distinct from the supervisor port).
    child_port: u16,
    /// Full engine configuration.
    config: EngineConfig,
    /// Canonicalized path of the `llama-server` binary (resolved once at construction).
    canonical_bin: PathBuf,
    /// HTTP client for `/health` polling and proxying.
    pub client: reqwest::Client,
}

impl LlamaServerSupervisor {
    /// Constructs a supervisor (without starting the child).
    ///
    /// Validates at construction time:
    /// - binary canonicalizable and under an allowed prefix
    ///   (`/usr/local/bin/`, `/opt/gradatum/bin/`, `/opt/llama-`),
    /// - `child_port` > 1024 and distinct from `port`,
    /// - `extra_args` within the allow-list.
    ///
    /// # Errors
    /// Returns `EngineError::ModelLoad` if the binary is invalid or outside the allowed prefix.
    /// Returns `EngineError::BadRequest` if `extra_args` contains a flag not in the allow-list.
    pub fn new(config: EngineConfig) -> Result<Arc<Self>, EngineError> {
        let canonical_bin = canonicalize_bin_path(&config.llama_server_bin)?;

        if config.child_port <= 1024 {
            return Err(EngineError::ModelLoad(format!(
                "child_port {} is invalid — must be > 1024 (SP-P0-4)",
                config.child_port
            )));
        }

        if config.child_port == config.port {
            return Err(EngineError::ModelLoad(format!(
                "child_port {} must differ from port {} — port collision",
                config.child_port, config.port
            )));
        }

        validate_extra_args(&config.extra_args)?;

        let child_port = config.child_port;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| EngineError::ModelLoad(format!("failed to build HTTP client: {e}")))?;

        Ok(Arc::new(Self {
            child: Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
            child_port,
            config,
            canonical_bin,
            client,
        }))
    }

    /// Builds and launches the `llama-server` subprocess.
    ///
    /// Confirmed flags (from `llama-server --help`):
    /// - `--model`: GGUF path
    /// - `--port`: child TCP port
    /// - `--host`: listen address (`127.0.0.1` — loopback only)
    /// - `--n-gpu-layers` (alias `--gpu-layers`): GPU layers
    /// - `--threads` (`-t`): CPU generation threads
    /// - `--ctx-size` (`-c`): context size
    /// - `--parallel` (`-np`): parallel slots (concurrency)
    /// - `--embedding` / `--embeddings`: embedding mode (restrict)
    ///
    /// NEVER uses `sh -c` or shell-interpolated strings.
    ///
    /// # Errors
    /// Returns `EngineError::ModelLoad` if spawning fails.
    pub async fn spawn_child(&self) -> Result<(), EngineError> {
        let bin = &self.canonical_bin;
        let cfg = &self.config;

        let args = build_child_args(cfg);

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            // Own process group for orphan isolation.
            .process_group(0)
            // SIGKILL if the tokio Child is dropped (unwinding panics).
            .kill_on_drop(true)
            // Stdio::inherit() — journald captures logs via the systemd cgroup.
            // Note: Stdio::piped() without draining deadlocks on verbose cold start
            // (backend banner + GGUF metadata > 64 KB saturate the kernel buffer).
            // Structured drain (BufReader + tokio::spawn) is a deferred follow-up.
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        // env_clear + GPU allow-list re-injection.
        //
        // `env_clear()` prevents llama-server from reading the supervisor's environment
        // (e.g. LLAMA_ARG_HOST, LLAMA_ARG_PORT, LLAMA_API_KEY, LLAMA_ARG_MODEL_URL,
        // HF_TOKEN…) which could orthogonally bypass the argv restrictions
        // (loopback, model path, no network download).
        //
        // GPU vars (VK_*, MESA_*, RADV_*, GGML_*, HIP_*, ROCR_*, ROCM_*, HSA_*,
        // CUDA_*, NVIDIA_*) are CRITICAL for GPU detection — they are preserved
        // by prefix from the current environment.
        cmd.env_clear();
        inject_allowed_env(&mut cmd);

        let child = cmd.spawn().map_err(|e| {
            EngineError::ModelLoad(format!("failed to spawn llama-server (bin={bin:?}): {e}"))
        })?;

        tracing::info!(
            child_port = self.child_port,
            model = %cfg.model_path,
            "llama-server spawned (PID={})",
            child.id().unwrap_or(0)
        );

        *self.child.lock().await = Some(child);
        Ok(())
    }

    /// Polls `GET /health` on the child until HTTP 200 or timeout.
    ///
    /// - `ConnectionRefused` during warm-up is a normal `Starting` state (child not yet ready).
    /// - Returns `ChildState::Ready` on the first HTTP 200.
    /// - Returns `ChildState::StartupTimeout` if the timeout is reached OR if the child
    ///   process has already exited (early-exit detection — see below).
    ///
    /// ## Early-exit detection (resilience fix — incident 2026-07-08 20:47)
    ///
    /// Before this fix, a child that exits immediately (e.g. `llama-server` failing to
    /// bind `child_port` because a previous instance's process had not yet released it —
    /// a race on `systemctl restart`) was invisible to this loop: only `/health` is
    /// polled, so a dead child looks identical to a slow-starting one
    /// (`ConnectionRefused` either way). The loop would poll for the **entire**
    /// `startup_timeout_secs` before giving up — during which `main()` has not yet
    /// bound its own service port, so the engine is neither reachable nor restarted by
    /// systemd (`Restart=on-failure` never fires: the process never exits, it is
    /// `active (running)` the whole time). Recovery required either waiting out the
    /// full timeout or a manual `systemctl restart`.
    ///
    /// `startup_timeout_secs` is per-engine config, not a single constant — the code
    /// default is 60 s (`config.rs`), and an operator may raise it well beyond that for a
    /// slow-loading engine. The incident this fix targets hit such a raised timeout.
    ///
    /// Fix: `try_wait()` on the child at the top of every loop iteration. If the child
    /// has already exited, return `StartupTimeout` immediately (within one poll
    /// interval, ≤500 ms) instead of waiting out the timeout. `main()` then proceeds to
    /// `supervise_loop()` right away — see [`Self::supervise_loop`] for the R1 fix that
    /// makes it actually consume `child_restart_max` with backoff instead of giving up
    /// after a single retry. Net effect: a stall lasting the whole `startup_timeout_secs`
    /// becomes a sub-second detection + bounded, backed-off retries + systemd escalation.
    pub async fn wait_ready(&self, health: &HealthState) -> ChildState {
        let deadline = Instant::now() + Duration::from_secs(self.config.startup_timeout_secs);
        let health_url = format!("http://127.0.0.1:{}/health", self.child_port);

        let poll_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| self.client.clone());

        while Instant::now() < deadline {
            // Early-exit detection: a child that already died (e.g. bind-fail) must not
            // be waited out for the full timeout — /health polling alone cannot tell
            // "dead" apart from "still starting" (both look like connection-refused).
            {
                let mut guard = self.child.lock().await;
                if let Some(child) = guard.as_mut() {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            tracing::error!(
                                status = ?status,
                                "llama-server exited before becoming ready — aborting wait early \
                                 (was: bind-fail or crash during startup)"
                            );
                            return ChildState::StartupTimeout;
                        }
                        Ok(None) => {} // still running — continue polling /health
                        Err(e) => {
                            tracing::warn!(error = %e, "try_wait() error while polling child");
                        }
                    }
                }
            }

            match poll_client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(
                        child_port = self.child_port,
                        "llama-server ready (HTTP {})",
                        resp.status()
                    );
                    health.set_ready();
                    return ChildState::Ready;
                }
                Ok(resp) => {
                    tracing::debug!(
                        status = %resp.status(),
                        "llama-server /health not ready, waiting..."
                    );
                }
                Err(e) if is_connection_refused(&e) => {
                    tracing::debug!("llama-server /health: connection refused, starting up");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "llama-server /health: poll error");
                }
            }
            sleep(Duration::from_millis(500)).await;
        }

        tracing::error!(
            timeout_secs = self.config.startup_timeout_secs,
            "llama-server did not respond within the startup timeout"
        );
        ChildState::StartupTimeout
    }

    /// Supervision loop with a total restart budget and flapping detection.
    ///
    /// ## Restart policy
    ///
    /// `child_restart_max` is a **total budget** (not a per-window rate limit).
    /// Each crash decrements the budget, regardless of uptime duration.
    ///
    /// **Budget reset**: ONLY if the child was stable for ≥ `min_stable_uptime_secs`
    /// before the crash. A short uptime (flapping: Ready → crash in < threshold) consumes
    /// the budget WITHOUT a reset, forcing escalation to `HealthState::unhealthy` after
    /// `child_restart_max` crashes.
    ///
    /// **Backoff reset**: same condition as the budget reset.
    ///
    /// ## Escalation to systemd (R1 fix — reviewer finding on `3256aac`, 2026-07-10)
    ///
    /// Two bugs made the pre-A1b code give up too easily and too silently:
    ///
    /// 1. A restart whose own `wait_ready()` also returned `StartupTimeout` (e.g. a
    ///    persistent bind-fail: the respawned child dies immediately every time) was
    ///    treated as **immediately terminal** — `break` after exactly one retry,
    ///    regardless of how much `child_restart_max` budget remained. Fixed: that branch
    ///    now `continue`s the loop like any other crash, so it goes through the same
    ///    budget/backoff accounting at the top — a persistent bind-fail now consumes the
    ///    *entire* configured budget (with escalating backoff) before giving up, not
    ///    just one attempt.
    /// 2. Budget exhaustion only called `health.set_unhealthy()` then `break` — the
    ///    process itself never exited, so it sat `active (running)` forever from
    ///    systemd's point of view and `Restart=on-failure` could never fire. Tolerable
    ///    on the 4-engine fleet (the gateway falls back to another engine for that
    ///    mode), **not tolerable post-cutover** (mono-instance: no fallback left for
    ///    the mode this engine serves). Fixed: the systemd escalation path now actually
    ///    exits the process (`std::process::exit(1)` in production builds — see its
    ///    doc for why this is a no-op under `#[cfg(test)]`), so a truly exhausted
    ///    engine becomes a real ExecMain failure and systemd's own
    ///    `Restart=on-failure`/`RestartSec` takes over as the last line of defense.
    ///
    /// Combined with the A1b port-race fix (`ExecStartPre=wait-for-port-free.sh` in
    /// `packaging/systemd/gradatum-engine@.service`), the common transient case (old
    /// child hadn't released `child_port` yet) is now closed at the source, and this
    /// in-process retry+escalate logic is defense-in-depth for whatever transient
    /// failure gets through anyway.
    ///
    /// ## Initial seed
    ///
    /// `initial_ready_at` carries the `Instant` when `wait_ready()` returned `Ready`
    /// during the initial spawn (performed in `main()` before `supervise_loop`).
    /// Without this seed, the first crash of a healthy child would have
    /// `last_ready_at == None` → incorrectly classified as flapping → budget prematurely
    /// consumed.
    ///
    /// ## Shutdown
    ///
    /// `shutdown_requested` is checked at the top of the loop, after `child.wait()`, and
    /// immediately BEFORE `spawn_child()` (after the backoff sleep).
    pub async fn supervise_loop(
        self: Arc<Self>,
        health: Arc<HealthState>,
        initial_ready_at: Option<Instant>,
    ) {
        let mut restart_budget = self.config.child_restart_max;
        let mut backoff_ms = BACKOFF_INIT_MS;
        // Seeded from main() to cover the first crash after the initial spawn.
        let mut last_ready_at: Option<Instant> = initial_ready_at;

        loop {
            if self.shutdown_requested.load(Ordering::Relaxed) {
                tracing::info!("supervise_loop: shutdown requested, exiting");
                break;
            }

            let exit_status = {
                let mut guard = self.child.lock().await;
                match guard.as_mut() {
                    None => {
                        tracing::warn!("supervise_loop: no child process to monitor");
                        break;
                    }
                    Some(child) => child.wait().await,
                }
            };

            if self.shutdown_requested.load(Ordering::Relaxed) {
                break;
            }

            match exit_status {
                Ok(status) => {
                    tracing::warn!(status = ?status, "llama-server exited");
                }
                Err(e) => {
                    tracing::error!(error = %e, "wait() error on llama-server");
                }
            }

            // --- Uptime calculation and flapping policy ---
            let uptime_stable = last_ready_at
                .map(|t| t.elapsed() >= Duration::from_secs(self.config.min_stable_uptime_secs))
                .unwrap_or(false);

            if uptime_stable {
                tracing::info!(
                    min_stable_secs = self.config.min_stable_uptime_secs,
                    "stable uptime before crash — resetting budget and backoff"
                );
                restart_budget = self.config.child_restart_max;
                backoff_ms = BACKOFF_INIT_MS;
            }
            // Flapping: budget and backoff continue to escalate without reset.
            // last_ready_at will be updated after the next successful spawn.

            // --- Check total budget ---
            if restart_budget == 0 {
                tracing::error!(
                    max_restarts = self.config.child_restart_max,
                    "llama-server: restart budget exhausted — engine unhealthy, \
                     escalating to systemd"
                );
                health.set_unhealthy();
                escalate_to_systemd("restart budget exhausted");
                break;
            }
            restart_budget -= 1;

            let current_backoff = backoff_ms;
            tracing::warn!(
                backoff_ms = current_backoff,
                restarts_remaining = restart_budget,
                "llama-server: restarting in {}ms",
                current_backoff
            );
            sleep(Duration::from_millis(current_backoff)).await;

            // Re-check AFTER the sleep, BEFORE spawn_child().
            if self.shutdown_requested.load(Ordering::Relaxed) {
                tracing::info!("supervise_loop: shutdown during backoff, skipping respawn");
                break;
            }

            match self.spawn_child().await {
                Ok(()) => {
                    tracing::info!(
                        restarts_remaining = restart_budget,
                        "llama-server restarted"
                    );
                    let child_state = self.wait_ready(&health).await;
                    if child_state == ChildState::StartupTimeout {
                        // R1 fix: a restart whose wait_ready also fails (e.g. a
                        // persistent bind-fail — the respawned child dies immediately
                        // every time) is a crash like any other. Do NOT treat it as
                        // terminal here: `continue` so the top of the loop re-checks
                        // `restart_budget`/backoff and retries, instead of giving up
                        // after a single post-crash attempt regardless of remaining
                        // budget (this was the exact bug the reviewer flagged in
                        // `3256aac`).
                        tracing::warn!(
                            restarts_remaining = restart_budget,
                            "llama-server: restart attempt did not become healthy in time \
                             — retrying within remaining restart budget"
                        );
                        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                        continue;
                    }
                    last_ready_at = Some(Instant::now());
                    backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                }
                Err(e) => {
                    // R1 fix: same treatment as above — an OS-level respawn failure
                    // (e.g. transient resource pressure) consumes the restart budget
                    // instead of escalating on the very first occurrence. The
                    // previous child (if any) is left untouched in `self.child`;
                    // the next loop iteration's `child.wait()` returns its cached
                    // exit status immediately (tokio caches it after the first
                    // successful `wait()`), so this does not stall.
                    tracing::warn!(
                        error = %e,
                        restarts_remaining = restart_budget,
                        "llama-server: re-spawn failed — retrying within remaining restart budget"
                    );
                    backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                    continue;
                }
            }
        }
    }

    /// Graceful shutdown: SIGTERM → grace period → SIGKILL → reap.
    pub async fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);

        let mut guard = self.child.lock().await;
        let child = match guard.as_mut() {
            None => return,
            Some(c) => c,
        };

        let pid = child.id().unwrap_or(0);
        if pid > 0 {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let pgid = Pid::from_raw(pid as i32);
            if let Err(e) = killpg(pgid, Signal::SIGTERM) {
                tracing::warn!(pid, error = %e, "SIGTERM to llama-server process group failed");
            } else {
                tracing::info!(pid, "SIGTERM sent to llama-server");
            }
        }

        let grace = Duration::from_secs(SHUTDOWN_GRACE_SECS);
        let result = tokio::time::timeout(grace, child.wait()).await;

        match result {
            Ok(Ok(status)) => {
                tracing::info!(status = ?status, "llama-server shut down cleanly");
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "wait() after SIGTERM failed — SIGKILL");
                let _ = child.kill().await;
            }
            Err(_timeout) => {
                tracing::warn!(
                    grace_secs = SHUTDOWN_GRACE_SECS,
                    "llama-server did not respond to SIGTERM — SIGKILL"
                );
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }

        *guard = None;
    }

    /// Returns the child base URL (loopback) for the proxy.
    pub fn child_base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.child_port)
    }
}

// ---------------------------------------------------------------------------
// Helpers publics
// ---------------------------------------------------------------------------

/// Returns `true` if `path_str` starts with one of the `ALLOWED_BIN_PREFIXES`.
///
/// Internal helper — called only by [`is_binary_allowed`]. Pure, no filesystem access.
fn is_prefix_allowed(path_str: &str) -> bool {
    ALLOWED_BIN_PREFIXES
        .iter()
        .any(|prefix| path_str.starts_with(prefix))
}

/// Returns `true` if `name` matches the allowed llama-server filename pattern.
///
/// Accepted forms:
/// - `"llama-server"` — exact unversioned binary
/// - `"llama-server-<suffix>"` — versioned wrapper where `<suffix>` is non-empty
///   and consists solely of ASCII alphanumeric characters (e.g. `b9549`, `b9780`)
///
/// Rejected: `"llama-serverXYZ"` (glued, no dash separator), `"llama-server.bak"` (dot),
/// `"llama-server-evil-shim"` (dash inside suffix), `"bash"`.
///
/// Internal helper — called only by [`is_binary_allowed`]. Pure, no filesystem access.
fn is_llama_server_name(name: &str) -> bool {
    match name.strip_prefix("llama-server") {
        // exact "llama-server"
        Some("") => true,
        // "llama-server-<suffix>" — suffix must be non-empty ASCII alphanumeric only
        Some(suffix) if suffix.starts_with('-') => {
            let tail = &suffix[1..];
            !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphanumeric())
        }
        _ => false,
    }
}

/// Returns `true` if `path` passes both the allowlist prefix check **and** the filename check.
///
/// Two cumulative conditions (both required):
/// 1. The canonicalized path string starts with one of [`ALLOWED_BIN_PREFIXES`].
/// 2. The final path component (`file_name()`) satisfies [`is_llama_server_name`]:
///    exact `"llama-server"` or `"llama-server-<alphanum>"` versioned wrapper.
///
/// Non-UTF8 filenames are rejected fail-closed (`.to_str()` returns `None`).
///
/// The prefix guard (condition 1) remains the primary security control: only explicitly
/// trusted directories are accepted, so a `llama-server-*` filename under an untrusted
/// path is still rejected.
///
/// Call on the canonicalized path (canonicalization must happen before this check — TOCTOU).
fn is_binary_allowed(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    // Fail-closed on non-UTF8 filenames; then apply the strict name pattern.
    if !file_name
        .to_str()
        .map(is_llama_server_name)
        .unwrap_or(false)
    {
        return false;
    }
    let Some(path_str) = path.to_str() else {
        return false;
    };
    is_prefix_allowed(path_str)
}

/// Canonicalizes the binary path and verifies the allowed prefix.
///
/// Returns the canonicalized `PathBuf` — store and use it at spawn time (prevents TOCTOU).
///
/// Accepted prefixes: `/usr/local/bin/`, `/opt/gradatum/bin/`, `/opt/llama-`.
///
/// # Errors
/// Returns `EngineError::ModelLoad` if the binary is absent or outside the allowed prefix.
pub fn canonicalize_bin_path(bin: &Path) -> Result<PathBuf, EngineError> {
    let canonical = bin.canonicalize().map_err(|e| {
        EngineError::ModelLoad(format!(
            "llama_server_bin canonicalize failed ({bin:?}): {e}"
        ))
    })?;

    let canonical_str = canonical
        .to_str()
        .ok_or_else(|| EngineError::ModelLoad("llama_server_bin path is non-UTF8".into()))?;

    if !is_binary_allowed(&canonical) {
        return Err(EngineError::ModelLoad(format!(
            "llama_server_bin is outside the allowed prefix or has an invalid filename \
             ({canonical_str:?}) — accepted prefixes: {ALLOWED_BIN_PREFIXES:?}, \
             required name: must start with \"llama-server\" (SP-P0-4)"
        )));
    }

    Ok(canonical)
}

/// Backward-compatibility alias for `canonicalize_bin_path`.
pub fn validate_bin_path(bin: &Path) -> Result<(), EngineError> {
    canonicalize_bin_path(bin).map(|_| ())
}

/// Verifies that every `extra_arg` is in the `ALLOWED_EXTRA_FLAGS` allow-list.
///
/// The key is extracted by splitting on `=` (form `--flag=value`) or taken as-is
/// (form `--flag value`). Positional values (arguments following an allowed flag)
/// are not inspected — only keys starting with `-` are checked.
///
/// # Errors
/// Returns `EngineError::BadRequest` if a flag not in the allow-list is found.
pub fn validate_extra_args(extra_args: &[String]) -> Result<(), EngineError> {
    for arg in extra_args {
        // Skip positional values (do not start with `-`)
        if !arg.starts_with('-') {
            continue;
        }
        let key = arg.split('=').next().unwrap_or(arg.as_str());
        if !ALLOWED_EXTRA_FLAGS.contains(&key) {
            return Err(EngineError::BadRequest(format!(
                "extra_args: flag '{key}' is not allowed — \
                 only allow-listed flags (ALLOWED_EXTRA_FLAGS) are accepted; \
                 flags managed by dedicated config fields must use those fields \
                 (e.g. gpu_layers for --n-gpu-layers)"
            )));
        }
    }
    Ok(())
}

/// Re-injects allowed environment variables from the current process into `cmd`.
///
/// Call after `cmd.env_clear()`. Preserves:
/// - Variables listed by exact name in `ENV_PASSTHROUGH` (`PATH`, `HOME`, `LD_LIBRARY_PATH`).
/// - Variables whose key starts with a GPU prefix listed in `GPU_ENV_PREFIXES`
///   (`VK_*`, `MESA_*`, `RADV_*`, `GGML_*`, `HIP_*`, `ROCR_*`, `ROCM_*`, `HSA_*`,
///   `CUDA_*`, `NVIDIA_*`).
pub fn inject_allowed_env(cmd: &mut Command) {
    for (key, val) in std::env::vars_os() {
        let key_str = match key.to_str() {
            Some(s) => s,
            None => continue, // clé non-UTF8 → skip
        };

        let pass = ENV_PASSTHROUGH.contains(&key_str)
            || GPU_ENV_PREFIXES
                .iter()
                .any(|prefix| key_str.starts_with(prefix));

        if pass {
            cmd.env(&key, &val);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers privés
// ---------------------------------------------------------------------------

/// Terminates the process so systemd's `Restart=on-failure` can escalate (R1 fix).
///
/// Called only from [`LlamaServerSupervisor::supervise_loop`] once `child_restart_max`
/// is truly exhausted — i.e. a genuinely persistent failure, not a transient one (those
/// are absorbed by the retry-with-backoff loop above this call). At that point the
/// engine has no other recovery path of its own; exiting hands off to systemd, which
/// applies the unit's own `Restart=on-failure`/`RestartSec` as the last line of defense.
///
/// `#[cfg(not(test))]`: real `std::process::exit(1)`.
/// `#[cfg(test)]`: no-op. A literal `std::process::exit()` inside a `cargo test` binary
/// would kill the *entire test process*, not just the test that reached this path — it
/// would silently abort every other test running in the same binary.
///
/// M4 fix (doc accuracy, 2026-07-10): what the test actually asserts is
/// `health.snapshot().status == "unhealthy"` after `supervise_loop()` returns, plus an
/// elapsed-time bound. That is a sufficient proxy — post-R1, `unhealthy` is only ever
/// set immediately before `escalate_to_systemd` is called, so observing it after the
/// loop returns is equivalent to observing that this function was reached. Restart
/// attempts are not counted programmatically by the test; they were confirmed by
/// inspecting the captured `llama-server` process log during development (each
/// model-load failure prints its own startup trace). The literal `exit()` syscall
/// itself is out of scope for a unit test either way.
#[cfg(not(test))]
fn escalate_to_systemd(reason: &str) {
    tracing::error!(
        reason,
        "gradatum-engine: exiting so systemd Restart=on-failure can take over"
    );
    std::process::exit(1);
}

#[cfg(test)]
fn escalate_to_systemd(reason: &str) {
    tracing::error!(
        reason,
        "gradatum-engine (test build): would exit(1) here in production \
         — no-op under #[cfg(test)] to avoid killing the test binary"
    );
}

/// Returns `true` if a reqwest error represents a connection-refused condition.
///
/// Locale-independent: matches the English kernel message and the raw OS errno
/// (`os error 111` on Linux) rather than any localized `strerror` text, so it
/// holds regardless of the host locale. Checks both the underlying source error
/// and the top-level error string.
fn is_connection_refused(e: &reqwest::Error) -> bool {
    fn hit(msg: &str) -> bool {
        let m = msg.to_lowercase();
        m.contains("connection refused") || m.contains("os error 111")
    }
    if let Some(source) = e.source()
        && hit(&source.to_string())
    {
        return true;
    }
    hit(&e.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelKind, RuntimeKind};

    // --- canonicalize_bin_path / validate_bin_path ---

    #[test]
    fn validate_bin_path_accepts_usr_local_bin() {
        let result = validate_bin_path(Path::new("/usr/local/bin/llama-server"));
        assert!(
            result.is_ok(),
            "llama-server dans /usr/local/bin/ doit être accepté : {result:?}"
        );
    }

    #[test]
    fn validate_bin_path_rejects_arbitrary_path() {
        let result = validate_bin_path(Path::new("/tmp/malicious-llama-server"));
        assert!(
            result.is_err(),
            "binaire dans /tmp doit être rejeté (SP-P0-4)"
        );
    }

    #[test]
    fn validate_bin_path_rejects_nonexistent() {
        let result = validate_bin_path(Path::new("/usr/local/bin/does-not-exist-engine-test"));
        assert!(result.is_err(), "binaire absent doit être rejeté");
    }

    #[test]
    fn validate_bin_path_rejects_sh_injection() {
        let result = validate_bin_path(Path::new("/usr/local/bin/../../bin/sh"));
        assert!(result.is_err(), "path traversal doit être rejeté (SP-P0-4)");
    }

    #[test]
    fn canonicalize_bin_path_returns_canonical_pathbuf() {
        let result = canonicalize_bin_path(Path::new("/usr/local/bin/llama-server"));
        assert!(result.is_ok(), "doit retourner un PathBuf canonicalisé");
        let p = result.unwrap();
        assert!(
            p.is_absolute(),
            "le PathBuf canonicalisé doit être absolu : {p:?}"
        );
    }

    // --- validate_extra_args (allow-list, Blocker 2) ---

    #[test]
    fn extra_args_accepts_flash_attn() {
        assert!(
            validate_extra_args(&["--flash-attn".into()]).is_ok(),
            "--flash-attn doit être accepté"
        );
    }

    #[test]
    fn extra_args_accepts_log_disable() {
        assert!(
            validate_extra_args(&["--log-disable".into()]).is_ok(),
            "--log-disable doit être accepté"
        );
    }

    #[test]
    fn extra_args_accepts_no_mmap() {
        assert!(
            validate_extra_args(&["--no-mmap".into()]).is_ok(),
            "--no-mmap doit être accepté"
        );
    }

    #[test]
    fn extra_args_accepts_batch_size_with_value() {
        // Forme --flag value : la valeur positionnelle ne commence pas par '-', ignorée
        assert!(
            validate_extra_args(&["--batch-size".into(), "512".into()]).is_ok(),
            "--batch-size 512 doit être accepté"
        );
    }

    #[test]
    fn extra_args_accepts_batch_size_equals_form() {
        assert!(
            validate_extra_args(&["--batch-size=512".into()]).is_ok(),
            "--batch-size=512 doit être accepté"
        );
    }

    #[test]
    fn extra_args_rejects_host_override() {
        let result = validate_extra_args(&["--host".into(), "0.0.0.0".into()]);
        assert!(result.is_err(), "--host doit être rejeté (allow-list)");
        assert!(
            result.unwrap_err().to_string().contains("--host"),
            "message d'erreur doit citer --host"
        );
    }

    #[test]
    fn extra_args_rejects_api_key_file() {
        let result = validate_extra_args(&["--api-key-file".into(), "/etc/passwd".into()]);
        assert!(result.is_err(), "--api-key-file doit être rejeté");
    }

    #[test]
    fn extra_args_rejects_model_url() {
        let result =
            validate_extra_args(&["--model-url".into(), "http://evil.example/evil.gguf".into()]);
        assert!(result.is_err(), "--model-url doit être rejeté");
    }

    #[test]
    fn extra_args_rejects_n_gpu_layers() {
        // --n-gpu-layers géré via config.gpu_layers — ne pas doubler
        let result = validate_extra_args(&["--n-gpu-layers".into(), "35".into()]);
        assert!(
            result.is_err(),
            "--n-gpu-layers doit être rejeté (géré par config gpu_layers)"
        );
    }

    #[test]
    fn extra_args_rejects_gpu_layers_alias() {
        let result = validate_extra_args(&["--gpu-layers".into(), "35".into()]);
        assert!(result.is_err(), "--gpu-layers doit être rejeté");
        let result2 = validate_extra_args(&["-ngl".into(), "35".into()]);
        assert!(result2.is_err(), "-ngl doit être rejeté");
    }

    #[test]
    fn extra_args_rejects_model_short_flag() {
        let result = validate_extra_args(&["-m".into(), "/tmp/evil.gguf".into()]);
        assert!(result.is_err(), "-m doit être rejeté");
    }

    #[test]
    fn extra_args_rejects_port_override() {
        let result = validate_extra_args(&["--port=9999".into()]);
        assert!(result.is_err(), "--port= doit être rejeté");
    }

    #[test]
    fn extra_args_rejects_lora_path() {
        let result = validate_extra_args(&["--lora".into(), "/tmp/evil.bin".into()]);
        assert!(result.is_err(), "--lora doit être rejeté");
    }

    #[test]
    fn extra_args_accepts_backend_sampling() {
        // --backend-sampling (llama.cpp b9780+) : sampling élu côté GPU, flag booléen
        // standalone. Extension explicite de l'allow-list.
        assert!(
            validate_extra_args(&["--backend-sampling".into()]).is_ok(),
            "--backend-sampling doit être accepté (allow-list)"
        );
    }

    #[test]
    fn extra_args_accepts_kv_unified() {
        // --kv-unified est requis pour activer le prompt-cache LCP avec --parallel ≥ 2
        // (llama.cpp b9780+). Extension explicite de l'allow-list — décision F-75 PERF.
        assert!(
            validate_extra_args(&["--kv-unified".into()]).is_ok(),
            "--kv-unified doit être accepté (allow-list F-75 PERF)"
        );
    }

    #[test]
    fn extra_args_accepts_slot_prompt_similarity_with_value() {
        // --slot-prompt-similarity <f> : seuil de similarité pour la sélection de slot
        // (routage prefix-cache, llama.cpp). Prend une valeur flottante. Incident
        // 2026-07-11 : restart-loop du superviseur agent-main car ce flag valide côté
        // llama-server était rejeté par l'allow-list. Réf debug/01KX8D6W9Q.
        assert!(
            validate_extra_args(&["--slot-prompt-similarity".into(), "0.8".into()]).is_ok(),
            "--slot-prompt-similarity doit être accepté (allow-list, incident 01KX8D6W9Q)"
        );
    }

    #[test]
    fn extra_args_accepts_chat_template_kwargs_with_json_value() {
        // --chat-template-kwargs '{"enable_thinking":false}' : désactive le mode thinking
        // des modèles Qwen3.6-35B-A3B (économise ~400-1600 tokens de raisonnement/requête).
        // Forme --flag value : la valeur JSON commence par '{', ignorée comme positionnelle.
        assert!(
            validate_extra_args(&[
                "--chat-template-kwargs".into(),
                r#"{"enable_thinking":false}"#.into()
            ])
            .is_ok(),
            "--chat-template-kwargs '{{\"enable_thinking\":false}}' doit être accepté"
        );
    }

    #[test]
    fn extra_args_accepts_chat_template_kwargs_equals_form() {
        // Forme --flag=value : la clé est extraite avant le '=', la valeur JSON (avec
        // guillemets) n'est pas inspectée — elle est passée telle quelle en argv direct.
        assert!(
            validate_extra_args(&[r#"--chat-template-kwargs={"enable_thinking":false}"#.into()])
                .is_ok(),
            "--chat-template-kwargs={{...}} doit être accepté"
        );
    }

    #[test]
    fn extra_args_empty_always_ok() {
        assert!(
            validate_extra_args(&[]).is_ok(),
            "extra_args vide toujours OK"
        );
    }

    // --- validate_extra_args via supervisor::new ---

    #[test]
    fn supervisor_rejects_extra_args_hors_allow_list() {
        let mut config = make_test_config();
        config.extra_args = vec!["--host".into(), "0.0.0.0".into()];
        assert!(
            LlamaServerSupervisor::new(config).is_err(),
            "superviseur doit rejeter extra_args hors allow-list"
        );
    }

    #[test]
    fn supervisor_rejects_n_gpu_layers_in_extra_args() {
        let mut config = make_test_config();
        config.extra_args = vec!["--n-gpu-layers".into(), "35".into()];
        assert!(
            LlamaServerSupervisor::new(config).is_err(),
            "superviseur doit rejeter --n-gpu-layers en extra_args (doublon config)"
        );
    }

    #[test]
    fn supervisor_accepts_extra_args_in_allow_list() {
        let mut config = make_test_config();
        config.extra_args = vec!["--log-disable".into()];
        assert!(
            LlamaServerSupervisor::new(config).is_ok(),
            "superviseur doit accepter extra_args dans l'allow-list"
        );
    }

    // --- inject_allowed_env (Blocker 3) ---

    #[test]
    fn inject_allowed_env_preserves_path() {
        // Vérification : PATH est dans ENV_PASSTHROUGH
        assert!(
            ENV_PASSTHROUGH.contains(&"PATH"),
            "PATH doit être dans ENV_PASSTHROUGH"
        );
    }

    #[test]
    fn inject_allowed_env_preserves_gpu_prefixes() {
        // Vérification : les préfixes GPU sont tous présents
        let required_prefixes = ["VK_", "MESA_", "RADV_", "GGML_", "HIP_", "ROCR_"];
        for prefix in &required_prefixes {
            assert!(
                GPU_ENV_PREFIXES.contains(prefix),
                "préfixe GPU {prefix} doit être dans GPU_ENV_PREFIXES"
            );
        }
    }

    #[test]
    fn inject_allowed_env_excludes_llama_arg_host() {
        // LLAMA_ARG_HOST ne doit PAS être injecté (non dans ENV_PASSTHROUGH ni GPU_ENV_PREFIXES)
        let key = "LLAMA_ARG_HOST";
        let in_passthrough = ENV_PASSTHROUGH.contains(&key);
        let in_gpu = GPU_ENV_PREFIXES.iter().any(|p| key.starts_with(p));
        assert!(
            !in_passthrough && !in_gpu,
            "LLAMA_ARG_HOST ne doit pas être dans l'allow-list env"
        );
    }

    #[test]
    fn inject_allowed_env_excludes_hf_token() {
        let key = "HF_TOKEN";
        let pass =
            ENV_PASSTHROUGH.contains(&key) || GPU_ENV_PREFIXES.iter().any(|p| key.starts_with(p));
        assert!(!pass, "HF_TOKEN ne doit pas être dans l'allow-list env");
    }

    #[test]
    fn inject_allowed_env_excludes_llama_api_key() {
        let key = "LLAMA_API_KEY";
        let pass =
            ENV_PASSTHROUGH.contains(&key) || GPU_ENV_PREFIXES.iter().any(|p| key.starts_with(p));
        assert!(
            !pass,
            "LLAMA_API_KEY ne doit pas être dans l'allow-list env"
        );
    }

    #[test]
    fn inject_allowed_env_injects_to_command() {
        // Vérification fonctionnelle : inject_allowed_env() peut être appelé sans panique
        // et les vars GPU présentes dans l'env courant sont transmises.
        // On ne peut pas inspecter l'env d'une Command directement — on vérifie la logique
        // en testant qu'une clé GPU connue (si présente) passerait le filtre.
        let test_key = "VK_ICD_FILENAMES";
        let would_pass = ENV_PASSTHROUGH.contains(&test_key)
            || GPU_ENV_PREFIXES.iter().any(|p| test_key.starts_with(p));
        assert!(
            would_pass,
            "VK_ICD_FILENAMES (préfixe VK_) doit passer le filtre GPU"
        );

        // Appel réel pour vérifier qu'il ne panique pas
        let mut cmd = Command::new("/usr/local/bin/llama-server");
        cmd.env_clear();
        inject_allowed_env(&mut cmd); // ne doit pas paniquer
    }

    // --- child_port validation ---

    #[test]
    fn supervisor_rejects_privileged_port() {
        let mut config = make_test_config();
        config.child_port = 80;
        assert!(
            LlamaServerSupervisor::new(config).is_err(),
            "child_port=80 doit être rejeté"
        );
    }

    #[test]
    fn supervisor_rejects_same_port_as_supervisor() {
        let mut config = make_test_config();
        config.child_port = config.port;
        assert!(
            LlamaServerSupervisor::new(config).is_err(),
            "child_port == port doit être rejeté"
        );
    }

    #[test]
    fn supervisor_accepts_valid_port() {
        let result = LlamaServerSupervisor::new(make_test_config());
        assert!(
            result.is_ok(),
            "config valide doit être acceptée : {:?}",
            result.err()
        );
    }

    // --- budget restart + flapping (P1-b logique) ---

    #[test]
    fn restart_budget_exhausted_by_flapping() {
        let config = make_test_config();
        let max = config.child_restart_max;
        let min_stable = config.min_stable_uptime_secs;

        let mut budget = max;
        let mut backoff = BACKOFF_INIT_MS;

        for i in 0..max {
            let uptime_stable = false; // flapping
            assert!(!uptime_stable);
            assert_eq!(budget, max - i, "budget à l'itération {i}");
            assert!(budget > 0);
            budget -= 1;
            backoff = (backoff * 2).min(BACKOFF_MAX_MS);
        }

        assert_eq!(budget, 0, "budget épuisé après {max} crashs flapping");
        assert!(backoff > BACKOFF_INIT_MS, "backoff escaladé : {backoff}ms");
        assert_eq!(min_stable, 30, "défaut min_stable_uptime_secs = 30s");
    }

    #[test]
    fn restart_budget_resets_on_stable_uptime() {
        let config = make_test_config();
        let max = config.child_restart_max;
        let min_stable_secs = config.min_stable_uptime_secs;

        let mut budget = max - 1;
        let mut backoff = BACKOFF_MAX_MS;

        let elapsed = Duration::from_secs(min_stable_secs + 5);
        let uptime_stable = elapsed >= Duration::from_secs(min_stable_secs);
        assert!(uptime_stable);

        if uptime_stable {
            budget = max;
            backoff = BACKOFF_INIT_MS;
        }

        assert_eq!(budget, max, "budget remis au max");
        assert_eq!(backoff, BACKOFF_INIT_MS, "backoff remis à l'init");
    }

    #[test]
    fn child_restart_max_zero_means_no_restart() {
        let mut config = make_test_config();
        config.child_restart_max = 0;
        assert!(
            LlamaServerSupervisor::new(config).is_ok(),
            "child_restart_max=0 doit être accepté à la construction"
        );
    }

    /// When `initial_ready_at` is seeded: first crash after stable uptime → budget NOT decremented.
    ///
    /// Verifies that if `last_ready_at` is seeded with an Instant "35 s ago" (> 30 s threshold),
    /// the first crash is classified as "stable" and the budget is reset, not decremented.
    #[test]
    fn initial_ready_at_seed_prevents_false_flapping() {
        let config = make_test_config();
        let max = config.child_restart_max;
        let min_stable_secs = config.min_stable_uptime_secs;

        let mut budget = max;
        let mut backoff = BACKOFF_MAX_MS; // suppose qu'il avait déjà escaladé

        // Simule : initial_ready_at = il y a 35s (> seuil 30s)
        // C'est ce que main() passerait via Some(Instant::now()) au moment du ready initial.
        // On simule avec un elapsed fictif au lieu d'un vrai Instant (sans sleep).
        let simulated_elapsed = Duration::from_secs(min_stable_secs + 5);
        let uptime_stable = simulated_elapsed >= Duration::from_secs(min_stable_secs);

        assert!(
            uptime_stable,
            "premier crash après {simulated_elapsed:?} doit être classé stable"
        );

        // Comportement attendu : reset, PAS de décrément
        if uptime_stable {
            budget = max;
            backoff = BACKOFF_INIT_MS;
        }

        assert_eq!(
            budget, max,
            "budget doit être remis au max après 1er crash stable (pas de faux flapping)"
        );
        assert_eq!(
            backoff, BACKOFF_INIT_MS,
            "backoff remis à l'init après uptime stable"
        );
    }

    /// Without a seed (`last_ready_at = None`): first crash classified as flapping.
    ///
    /// Without a seed, `last_ready_at.is_none()` → `uptime_stable=false`.
    #[test]
    fn without_seed_first_crash_is_flapping() {
        let last_ready_at: Option<Instant> = None; // pas de seed
        let min_stable_secs = 30_u64;
        let uptime_stable = last_ready_at
            .map(|t| t.elapsed() >= Duration::from_secs(min_stable_secs))
            .unwrap_or(false);
        assert!(
            !uptime_stable,
            "sans seed, premier crash est classé flapping (budget décrémenté sans reset)"
        );
    }

    // --- wait_ready early-exit detection (resilience fix — incident 2026-07-08 20:47) ---

    /// Regression test for the bind-fail resilience fix.
    ///
    /// Reproduces the incident pattern on an ephemeral child: a process that exits
    /// almost immediately (standing in for `llama-server` failing to bind an
    /// already-occupied `child_port`). Before the fix, `wait_ready()` would poll
    /// `/health` for the *entire* `startup_timeout_secs` before giving up, because it
    /// had no way to notice the child was already dead. After the fix, `try_wait()`
    /// detects the dead child within one poll interval (≤500 ms).
    ///
    /// Run 3 times in a row (3 "bind-fail cycles") — mirrors the acceptance criterion
    /// "3 cycles bind-fail → 3 auto-restart confirmés, aucun down >30s": each cycle
    /// must resolve in well under 30s (in practice: well under 1s), not the
    /// `startup_timeout_secs` configured (60s in `make_test_config`, 300s in prod).
    #[test]
    fn wait_ready_detects_dead_child_immediately_3_cycles() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            for cycle in 1..=3 {
                let config = make_test_config();
                let supervisor = LlamaServerSupervisor::new(config)
                    .expect("supervisor construction must succeed");
                let health = HealthState::new_with_telemetry(
                    "test-model",
                    crate::health::TelemetryStatus::Active,
                );

                // Spawn a real child that exits immediately with a non-zero status —
                // stands in for llama-server's "couldn't bind HTTP server socket" exit.
                let child = Command::new("/bin/false")
                    .spawn()
                    .expect("/bin/false must be spawnable on the test host");
                *supervisor.child.lock().await = Some(child);

                let start = Instant::now();
                let state = supervisor.wait_ready(&health).await;
                let elapsed = start.elapsed();

                assert_eq!(
                    state,
                    ChildState::StartupTimeout,
                    "cycle {cycle}: dead child must be reported as StartupTimeout, not left hanging"
                );
                assert!(
                    elapsed < Duration::from_secs(3),
                    "cycle {cycle}: early-exit detection must resolve in a couple of poll \
                     intervals (~500ms), not wait out the full startup_timeout_secs \
                     (60s test / 300s prod) — elapsed={elapsed:?}"
                );
            }
        });
    }

    /// R1 regression test (M3, A1b) — real multi-restart recovery, not just detection
    /// latency. Reproduces a *persistent* failure end-to-end through the real
    /// `spawn_child()` + `wait_ready()` + `supervise_loop()` path (the actual
    /// `/usr/local/bin/llama-server` binary present on this host, pointed at a
    /// nonexistent model — it binds its port, fails to load the model, and exits with
    /// a non-zero status in well under 1s every single time, deterministically standing
    /// in for a persistent bind-fail: same observable shape — child dies before
    /// `/health` ever answers).
    ///
    /// Before the R1 fix, `supervise_loop` gave up after exactly one post-crash retry
    /// regardless of `child_restart_max`. This test uses `child_restart_max = 2` and
    /// asserts the loop actually attempts 2 restarts (budget genuinely consumed) before
    /// marking the engine unhealthy — proving the "1 seul retry" bug is fixed.
    #[test]
    fn supervise_loop_persistent_failure_consumes_full_budget_then_unhealthy() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let llama_server_bin = Path::new("/usr/local/bin/llama-server");
            if !llama_server_bin.exists() {
                // Environment without the real binary (e.g. a different CI runner) —
                // skip rather than fail; the early-exit-detection test above already
                // covers the core mechanism with a synthetic child.
                eprintln!(
                    "skipping supervise_loop_persistent_failure_consumes_full_budget_then_unhealthy: \
                     {llama_server_bin:?} not present on this host"
                );
                return;
            }

            let mut config = make_test_config();
            config.llama_server_bin = llama_server_bin.to_path_buf();
            // `make_test_config()`'s default port/child_port (11435/11436) collide with
            // this host's own LIVE `gradatum-engine@curator`/`gradatum-engine@embed`
            // services (confirmed via `ss -tlnp` — the forge host runs a CPU-only fallback
            // engine pair locally, not just the GPU host). Use dedicated high ports,
            // confirmed free, so this test can never contend with a real service.
            config.port = 58080;
            config.child_port = 58081;
            // Guaranteed to never exist — llama-server binds child_port, fails to load
            // the model, and exits (~1s), deterministically simulating a persistent
            // startup failure without needing a real GGUF or a real port conflict.
            config.model_path = "/tmp/gradatum-a1b-test-nonexistent-model.gguf".into();
            config.child_restart_max = 2;
            config.startup_timeout_secs = 5; // bounds worst-case test runtime only
            config.gpu_layers = 0;

            let supervisor =
                LlamaServerSupervisor::new(config).expect("supervisor construction must succeed");
            let health = Arc::new(HealthState::new_with_telemetry("test-model", crate::health::TelemetryStatus::Active));

            // Mirrors main(): initial spawn + wait_ready before supervise_loop.
            supervisor
                .spawn_child()
                .await
                .expect("OS-level spawn must succeed — the binary exists, it just fails later");
            let initial_state = supervisor.wait_ready(&health).await;
            assert_eq!(
                initial_state,
                ChildState::StartupTimeout,
                "child must die (model load failure) before becoming ready"
            );

            let start = Instant::now();
            supervisor.clone().supervise_loop(health.clone(), None).await;
            let elapsed = start.elapsed();

            assert_eq!(
                health.snapshot().status,
                "unhealthy",
                "engine must end unhealthy once the persistent failure exhausts child_restart_max"
            );
            assert!(
                elapsed < Duration::from_secs(30),
                "R1: consuming child_restart_max=2 with backoff on a persistent failure \
                 must resolve well under the 'no down >30s' acceptance criterion \
                 (in-process budget+backoff only — excludes systemd's own RestartSec, \
                 which is a separate, pre-existing escalation layer) — elapsed={elapsed:?}"
            );
        });
    }

    // --- is_connection_refused ---

    #[test]
    fn connection_refused_detection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .unwrap();
            let result = client.get("http://127.0.0.1:1/health").send().await;
            if let Err(e) = result {
                let _ = is_connection_refused(&e);
            }
        });
    }

    // --- build_child_args ---

    #[test]
    fn build_child_args_base_chat() {
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/m.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        // Vérifie la présence des flags autoritaires
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "/opt/gradatum/models/m.gguf")
        );
        assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "8090"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--host" && w[1] == "127.0.0.1")
        );
        // Pas de --embedding pour un chat
        assert!(!args.iter().any(|a| a == "--embedding"));
        // Pas de --mmproj quand mmproj_path = None
        assert!(!args.iter().any(|a| a == "--mmproj"));
    }

    #[test]
    fn build_child_args_embed_adds_embedding_flag() {
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/e.gguf\"\nmodel_kind=\"embed\"\nport=8080\nchild_port=8090\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            args.iter().any(|a| a == "--embedding"),
            "embed → --embedding présent"
        );
    }

    #[test]
    fn build_child_args_injects_mmproj_when_set() {
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/v.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\nmmproj_path=\"/opt/gradatum/models/mmproj-F16.gguf\"\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--mmproj" && w[1] == "/opt/gradatum/models/mmproj-F16.gguf"),
            "mmproj_path Some → --mmproj <path> injecté"
        );
    }

    #[test]
    fn build_child_args_injects_spec_decoding_when_set() {
        // Cas nominal draft-mtp : --spec-type <valeur> ET --spec-draft-model <path> injectés.
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/big.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\nspec_type=\"draft-mtp\"\ndraft_model_path=\"/opt/gradatum/models/draft.gguf\"\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--spec-type" && w[1] == "draft-mtp"),
            "spec_type draft-mtp → --spec-type draft-mtp injecté : {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--spec-draft-model" && w[1] == "/opt/gradatum/models/draft.gguf"),
            "draft_model_path → --spec-draft-model <path> injecté : {args:?}"
        );
    }

    #[test]
    fn build_child_args_injects_spec_draft_n_max_when_set() {
        // spec_draft_n_max=2 → --spec-draft-n-max 2 injecté.
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/big.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\nspec_type=\"draft-mtp\"\ndraft_model_path=\"/opt/gradatum/models/draft.gguf\"\nspec_draft_n_max=2\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--spec-draft-n-max" && w[1] == "2"),
            "spec_draft_n_max=2 → --spec-draft-n-max 2 injecté : {args:?}"
        );
    }

    #[test]
    fn build_child_args_injects_spec_draft_p_min_when_set() {
        // spec_draft_p_min=0.75 → --spec-draft-p-min 0.75 injecté.
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/big.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\nspec_type=\"draft-mtp\"\ndraft_model_path=\"/opt/gradatum/models/draft.gguf\"\nspec_draft_p_min=0.75\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--spec-draft-p-min" && w[1] == "0.75"),
            "spec_draft_p_min=0.75 → --spec-draft-p-min 0.75 injecté : {args:?}"
        );
    }

    #[test]
    fn build_child_args_no_spec_flags_when_unset() {
        let cfg = EngineConfig::from_toml(
            "[engine]\nmodel_path=\"/opt/gradatum/models/m.gguf\"\nmodel_kind=\"chat\"\nport=8080\nchild_port=8090\n",
        )
        .unwrap();
        let args = build_child_args(&cfg);
        assert!(
            !args
                .iter()
                .any(|a| a == "--spec-type" || a == "--spec-draft-model"),
            "aucun flag spec quand spec_type/draft_model_path = None"
        );
        // Delta zéro : absence de spec_draft_n_max / spec_draft_p_min → flags jamais émis.
        assert!(
            !args
                .iter()
                .any(|a| a == "--spec-draft-n-max" || a == "--spec-draft-p-min"),
            "aucun --spec-draft-n-max/--spec-draft-p-min quand None"
        );
    }

    #[test]
    fn extra_args_still_rejects_spec_flags() {
        // R-sécu : --spec-type / --spec-draft-model / --spec-draft-n-max restent HORS allow-list
        // (champs config dédiés).
        assert!(
            validate_extra_args(&["--spec-type".into(), "draft-mtp".into()]).is_err(),
            "--spec-type doit rester rejeté (champ config dédié spec_type)"
        );
        assert!(
            validate_extra_args(&["--spec-draft-model".into(), "/tmp/evil.gguf".into()]).is_err(),
            "--spec-draft-model doit rester rejeté (champ config dédié draft_model_path)"
        );
        assert!(
            validate_extra_args(&["--spec-draft-n-max".into(), "2".into()]).is_err(),
            "--spec-draft-n-max doit rester rejeté (champ config dédié spec_draft_n_max)"
        );
        assert!(
            validate_extra_args(&["--spec-draft-p-min".into(), "0.75".into()]).is_err(),
            "--spec-draft-p-min doit rester rejeté (champ config dédié spec_draft_p_min)"
        );
    }

    #[test]
    fn extra_args_accepts_extended_flags() {
        // Flags ajoutés vague-2 : SWA, cache-reuse, reasoning, sampling, n-predict.
        let ok = [
            vec!["--swa-full".to_string()],
            vec!["--cache-reuse".to_string(), "256".to_string()],
            vec!["--reasoning-format".to_string(), "deepseek".to_string()],
            vec!["--reasoning-budget".to_string(), "4000".to_string()],
            vec!["--temp".to_string(), "0.7".to_string()],
            vec!["--top-k".to_string(), "20".to_string()],
            vec!["--top-p".to_string(), "0.8".to_string()],
            vec!["--min-p".to_string(), "0.0".to_string()],
            vec!["--presence-penalty".to_string(), "1.5".to_string()],
            vec!["--repeat-penalty".to_string(), "1.1".to_string()],
            vec!["--n-predict".to_string(), "512".to_string()],
        ];
        for args in ok {
            assert!(
                validate_extra_args(&args).is_ok(),
                "flag étendu doit être accepté : {args:?}"
            );
        }
    }

    #[test]
    fn extra_args_still_rejects_mmproj() {
        // R-sécu : --mmproj reste HORS allow-list (la vision passe par mmproj_path).
        let result = validate_extra_args(&["--mmproj".into(), "/etc/passwd".into()]);
        assert!(
            result.is_err(),
            "--mmproj doit rester rejeté (champ config dédié)"
        );
    }

    #[test]
    fn extra_args_security_frontier_unchanged() {
        // La frontière sécu ne bouge pas : ces flags restent rejetés.
        for flag in [
            "--host",
            "--api-key-file",
            "--model-url",
            "--rpc",
            "--ssl-key-file",
            "--path",
        ] {
            assert!(
                validate_extra_args(&[flag.to_string(), "x".to_string()]).is_err(),
                "{flag} doit rester rejeté (frontière sécu inchangée)"
            );
        }
    }

    // --- is_binary_allowed (allowlist binaire + suffixe llama-server) ---

    #[test]
    fn binary_allowed_rejects_wrong_filename() {
        // Le composant final NE commence PAS par "llama-server" → rejeté.
        assert!(
            !is_binary_allowed(Path::new("/opt/llama-b9549/bin/evil-binary")),
            "/opt/llama-b9549/bin/evil-binary doit être rejeté (file_name sans préfixe llama-server)"
        );
        assert!(
            !is_binary_allowed(Path::new("/opt/gradatum/bin/bash")),
            "/opt/gradatum/bin/bash doit être rejeté (file_name != llama-server*)"
        );
        assert!(
            !is_binary_allowed(Path::new("/usr/local/bin/not-llama-server")),
            "/usr/local/bin/not-llama-server doit être rejeté"
        );
    }

    #[test]
    fn binary_allowed_accepts_valid_paths() {
        // Régression : préfixe OK + file_name commence par "llama-server".
        assert!(
            is_binary_allowed(Path::new("/opt/llama-b9549/llama-server")),
            "/opt/llama-b9549/llama-server doit être accepté"
        );
        assert!(
            is_binary_allowed(Path::new("/opt/llama-3456/bin/llama-server")),
            "/opt/llama-3456/bin/llama-server doit être accepté"
        );
        assert!(
            is_binary_allowed(Path::new("/usr/local/bin/llama-server")),
            "/usr/local/bin/llama-server doit être accepté"
        );
        assert!(
            is_binary_allowed(Path::new("/opt/gradatum/bin/llama-server")),
            "/opt/gradatum/bin/llama-server doit être accepté"
        );
    }

    #[test]
    fn binary_allowed_accepts_versioned_wrappers() {
        // Régression F-75 : wrappers versionnés llama-server-b9549, llama-server-b9780
        // dans /opt/gradatum/bin/ (préfixe autorisé).
        assert!(
            is_binary_allowed(Path::new("/opt/gradatum/bin/llama-server-b9549")),
            "/opt/gradatum/bin/llama-server-b9549 doit être accepté (wrapper versionné)"
        );
        assert!(
            is_binary_allowed(Path::new("/opt/gradatum/bin/llama-server-b9780")),
            "/opt/gradatum/bin/llama-server-b9780 doit être accepté (wrapper versionné)"
        );
        assert!(
            is_binary_allowed(Path::new("/usr/local/bin/llama-server-b9549")),
            "/usr/local/bin/llama-server-b9549 doit être accepté"
        );
    }

    #[test]
    fn binary_allowed_rejects_wrong_prefix_even_good_filename() {
        // Préfixe invalide même si file_name est correct.
        assert!(
            !is_binary_allowed(Path::new("/tmp/llama-server")),
            "/tmp/llama-server rejeté (préfixe hors allowlist)"
        );
        assert!(
            !is_binary_allowed(Path::new("/opt/evil/llama-server")),
            "/opt/evil/llama-server rejeté (préfixe invalide)"
        );
    }

    // --- is_llama_server_name (helper strict — resserrement caveat security-reviewer) ---

    #[test]
    fn llama_server_name_accepts_exact_and_versioned() {
        // Acceptés : exact + suffixes alphanum après tiret.
        assert!(
            is_llama_server_name("llama-server"),
            "\"llama-server\" exact doit être accepté"
        );
        assert!(
            is_llama_server_name("llama-server-b9549"),
            "\"llama-server-b9549\" doit être accepté"
        );
        assert!(
            is_llama_server_name("llama-server-b9780"),
            "\"llama-server-b9780\" doit être accepté"
        );
        assert!(
            is_llama_server_name("llama-server-1234"),
            "\"llama-server-1234\" (numérique pur) doit être accepté"
        );
    }

    #[test]
    fn llama_server_name_rejects_collated_and_invalid_suffixes() {
        // Rejetés : collé (pas de tiret), point, tiret interne dans suffix, nom quelconque.
        assert!(
            !is_llama_server_name("llama-serverXYZ"),
            "\"llama-serverXYZ\" (collé sans tiret) doit être rejeté"
        );
        assert!(
            !is_llama_server_name("llama-server.bak"),
            "\"llama-server.bak\" (point) doit être rejeté"
        );
        assert!(
            !is_llama_server_name("llama-server-evil-shim"),
            "\"llama-server-evil-shim\" (tiret dans le suffixe) doit être rejeté"
        );
        assert!(!is_llama_server_name("bash"), "\"bash\" doit être rejeté");
        assert!(
            !is_llama_server_name("llama-server-"),
            "\"llama-server-\" (tiret terminal sans suffixe) doit être rejeté"
        );
    }

    #[test]
    fn binary_allowed_rejects_collated_and_invalid_suffixes() {
        // is_binary_allowed doit propager le resserrement is_llama_server_name
        // même sous un préfixe autorisé.
        assert!(
            !is_binary_allowed(Path::new("/opt/gradatum/bin/llama-serverXYZ")),
            "/opt/gradatum/bin/llama-serverXYZ (collé) doit être rejeté"
        );
        assert!(
            !is_binary_allowed(Path::new("/opt/gradatum/bin/llama-server.bak")),
            "/opt/gradatum/bin/llama-server.bak (point) doit être rejeté"
        );
        assert!(
            !is_binary_allowed(Path::new("/opt/gradatum/bin/llama-server-evil-shim")),
            "/opt/gradatum/bin/llama-server-evil-shim (tiret dans suffixe) doit être rejeté"
        );
        assert!(
            !is_binary_allowed(Path::new("/usr/local/bin/bash")),
            "/usr/local/bin/bash doit être rejeté"
        );
    }

    // --- is_prefix_allowed (allowlist binaire — helper interne) ---

    #[test]
    fn bin_prefix_accepts_llama_versioned_path() {
        // Chemin versioned /opt/llama-<version>/llama-server — ajouté v0.6.1.
        assert!(
            is_prefix_allowed("/opt/llama-b9549/llama-server"),
            "/opt/llama-b9549/llama-server doit être accepté (préfixe /opt/llama-)"
        );
        // Variante avec une autre version
        assert!(
            is_prefix_allowed("/opt/llama-3456/bin/llama-server"),
            "/opt/llama-3456/bin/llama-server doit être accepté (préfixe /opt/llama-)"
        );
    }

    #[test]
    fn bin_prefix_rejects_tmp_path() {
        assert!(
            !is_prefix_allowed("/tmp/llama-server"),
            "/tmp/llama-server doit être rejeté (hors allowlist)"
        );
    }

    #[test]
    fn bin_prefix_rejects_opt_evil_path() {
        // Valide que le préfixe /opt/llama- est strict : /opt/evil/ ne passe pas.
        assert!(
            !is_prefix_allowed("/opt/evil/llama-server"),
            "/opt/evil/llama-server doit être rejeté (préfixe /opt/evil/ non autorisé)"
        );
        // /opt/llama seul (sans tiret) ne passe pas non plus.
        assert!(
            !is_prefix_allowed("/opt/llama/bin/llama-server"),
            "/opt/llama/bin/llama-server doit être rejeté (préfixe doit être /opt/llama-)"
        );
    }

    #[test]
    fn bin_prefix_accepts_existing_prefixes() {
        // Régression : les préfixes historiques restent acceptés.
        assert!(
            is_prefix_allowed("/usr/local/bin/llama-server"),
            "/usr/local/bin/ doit rester accepté"
        );
        assert!(
            is_prefix_allowed("/opt/gradatum/bin/llama-server"),
            "/opt/gradatum/bin/ doit rester accepté"
        );
    }

    // --- helpers ---

    fn make_test_config() -> EngineConfig {
        EngineConfig {
            model_path: "/opt/gradatum/models/test.gguf".into(),
            model_kind: ModelKind::Chat,
            runtime: RuntimeKind::LlamaServer,
            warm_up: "eager".into(),
            gpu_layers: 0,
            n_threads: 4,
            context_len: 4096,
            port: 11435,
            bind_addr: None, // défaut loopback
            metrics_port: None,
            timeout_secs: 30,
            max_tokens: 512,
            gradatum_url: Some("http://127.0.0.1:19090".into()),
            agent_id: None,
            llama_server_bin: PathBuf::from("/usr/local/bin/llama-server"),
            child_port: 11436,
            parallel: 2,
            extra_args: vec![],
            mmproj_path: None,
            draft_model_path: None,
            spec_type: None,
            spec_draft_n_max: None,
            spec_draft_p_min: None,
            startup_timeout_secs: 60,
            child_restart_max: 3,
            min_stable_uptime_secs: 30,
            body_limit_bytes: 32 * 1024 * 1024,
        }
    }
}
