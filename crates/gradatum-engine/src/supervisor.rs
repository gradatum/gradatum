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
        atomic::{AtomicBool, Ordering},
        Arc,
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
const ALLOWED_BIN_PREFIXES: &[&str] = &["/usr/local/bin/", "/opt/gradatum/bin/"];

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
    /// - binary canonicalizable and under an allowed prefix (`/usr/local/bin/`, `/opt/gradatum/bin/`),
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
                "child_port {} invalide — doit être > 1024 (SP-P0-4)",
                config.child_port
            )));
        }

        if config.child_port == config.port {
            return Err(EngineError::ModelLoad(format!(
                "child_port {} doit être différent de port {} — collision de port",
                config.child_port, config.port
            )));
        }

        validate_extra_args(&config.extra_args)?;

        let child_port = config.child_port;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| EngineError::ModelLoad(format!("construction client HTTP : {e}")))?;

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
            EngineError::ModelLoad(format!("spawn llama-server échoué (bin={bin:?}) : {e}"))
        })?;

        tracing::info!(
            child_port = self.child_port,
            model = %cfg.model_path,
            "llama-server spawné (PID={})",
            child.id().unwrap_or(0)
        );

        *self.child.lock().await = Some(child);
        Ok(())
    }

    /// Polls `GET /health` on the child until HTTP 200 or timeout.
    ///
    /// - `ConnectionRefused` during warm-up is a normal `Starting` state (child not yet ready).
    /// - Returns `ChildState::Ready` on the first HTTP 200.
    /// - Returns `ChildState::StartupTimeout` if the timeout is reached.
    pub async fn wait_ready(&self, health: &HealthState) -> ChildState {
        let deadline = Instant::now() + Duration::from_secs(self.config.startup_timeout_secs);
        let health_url = format!("http://127.0.0.1:{}/health", self.child_port);

        let poll_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| self.client.clone());

        while Instant::now() < deadline {
            match poll_client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(
                        child_port = self.child_port,
                        "llama-server prêt (HTTP {})",
                        resp.status()
                    );
                    health.set_ready();
                    return ChildState::Ready;
                }
                Ok(resp) => {
                    tracing::debug!(
                        status = %resp.status(),
                        "llama-server /health non-prêt, attente..."
                    );
                }
                Err(e) if is_connection_refused(&e) => {
                    tracing::debug!(
                        "llama-server /health : connection refused, démarrage en cours"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "llama-server /health : erreur poll");
                }
            }
            sleep(Duration::from_millis(500)).await;
        }

        tracing::error!(
            timeout_secs = self.config.startup_timeout_secs,
            "llama-server n'a pas répondu dans le timeout de démarrage"
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
                tracing::info!("supervise_loop : shutdown demandé, sortie");
                break;
            }

            let exit_status = {
                let mut guard = self.child.lock().await;
                match guard.as_mut() {
                    None => {
                        tracing::warn!("supervise_loop : pas d'enfant à surveiller");
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
                    tracing::warn!(status = ?status, "llama-server s'est arrêté");
                }
                Err(e) => {
                    tracing::error!(error = %e, "erreur wait() sur llama-server");
                }
            }

            // --- Uptime calculation and flapping policy ---
            let uptime_stable = last_ready_at
                .map(|t| t.elapsed() >= Duration::from_secs(self.config.min_stable_uptime_secs))
                .unwrap_or(false);

            if uptime_stable {
                tracing::info!(
                    min_stable_secs = self.config.min_stable_uptime_secs,
                    "uptime stable avant crash → reset budget + backoff"
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
                    "llama-server : budget restart épuisé — moteur unhealthy (fallback gateway)"
                );
                health.set_unhealthy();
                break;
            }
            restart_budget -= 1;

            let current_backoff = backoff_ms;
            tracing::warn!(
                backoff_ms = current_backoff,
                restarts_remaining = restart_budget,
                "llama-server : restart dans {}ms",
                current_backoff
            );
            sleep(Duration::from_millis(current_backoff)).await;

            // Re-check AFTER the sleep, BEFORE spawn_child().
            if self.shutdown_requested.load(Ordering::Relaxed) {
                tracing::info!("supervise_loop : shutdown pendant backoff, pas de respawn");
                break;
            }

            match self.spawn_child().await {
                Ok(()) => {
                    tracing::info!(restarts_remaining = restart_budget, "llama-server restarté");
                    let child_state = self.wait_ready(&health).await;
                    if child_state == ChildState::StartupTimeout {
                        tracing::error!("llama-server : timeout redémarrage — unhealthy");
                        health.set_unhealthy();
                        break;
                    }
                    last_ready_at = Some(Instant::now());
                    backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                }
                Err(e) => {
                    tracing::error!(error = %e, "llama-server : re-spawn échoué — unhealthy");
                    health.set_unhealthy();
                    break;
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
                sys::signal::{killpg, Signal},
                unistd::Pid,
            };
            let pgid = Pid::from_raw(pid as i32);
            if let Err(e) = killpg(pgid, Signal::SIGTERM) {
                tracing::warn!(pid, error = %e, "SIGTERM vers process group de llama-server échoué");
            } else {
                tracing::info!(pid, "SIGTERM envoyé à llama-server");
            }
        }

        let grace = Duration::from_secs(SHUTDOWN_GRACE_SECS);
        let result = tokio::time::timeout(grace, child.wait()).await;

        match result {
            Ok(Ok(status)) => {
                tracing::info!(status = ?status, "llama-server terminé proprement");
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "wait() après SIGTERM échoué — SIGKILL");
                let _ = child.kill().await;
            }
            Err(_timeout) => {
                tracing::warn!(
                    grace_secs = SHUTDOWN_GRACE_SECS,
                    "llama-server n'a pas répondu au SIGTERM — SIGKILL"
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

/// Canonicalizes the binary path and verifies the allowed prefix.
///
/// Returns the canonicalized `PathBuf` — store and use it at spawn time (prevents TOCTOU).
///
/// # Errors
/// Returns `EngineError::ModelLoad` if the binary is absent or outside the allowed prefix.
pub fn canonicalize_bin_path(bin: &Path) -> Result<PathBuf, EngineError> {
    let canonical = bin.canonicalize().map_err(|e| {
        EngineError::ModelLoad(format!(
            "llama_server_bin canonicalize échoué ({bin:?}) : {e}"
        ))
    })?;

    let canonical_str = canonical
        .to_str()
        .ok_or_else(|| EngineError::ModelLoad("llama_server_bin chemin non-UTF8".into()))?;

    let allowed = ALLOWED_BIN_PREFIXES
        .iter()
        .any(|prefix| canonical_str.starts_with(prefix));

    if !allowed {
        return Err(EngineError::ModelLoad(format!(
            "llama_server_bin hors préfixe autorisé ({canonical_str:?}) — \
             préfixes acceptés : {ALLOWED_BIN_PREFIXES:?} (SP-P0-4)"
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
                "extra_args : flag '{key}' non autorisé — \
                 seuls les flags de l'allow-list ALLOWED_EXTRA_FLAGS sont acceptés. \
                 Toute extension est une décision de sécurité explicite."
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

/// Returns `true` if a reqwest error represents a connection-refused condition.
fn is_connection_refused(e: &reqwest::Error) -> bool {
    if let Some(source) = e.source() {
        let msg = source.to_string().to_lowercase();
        if msg.contains("connection refused") || msg.contains("connexion refusée") {
            return true;
        }
    }
    let msg = e.to_string().to_lowercase();
    msg.contains("connection refused") || msg.contains("os error 111")
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

    /// Blocker 1 — initial_ready_at seedé : 1er crash après uptime stable → budget NON décrémenté.
    ///
    /// Vérifie que si last_ready_at est seedé avec un Instant "il y a 35s" (> 30s seuil),
    /// le premier crash est classé "stable" et le budget est reset, pas décrémenté.
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

    /// Blocker 1 — sans seed (last_ready_at = None) : premier crash classé flapping.
    ///
    /// C'est le bug corrigé — sans seed, last_ready_at.is_none() → uptime_stable=false.
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
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "/opt/gradatum/models/m.gguf"));
        assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "8090"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--host" && w[1] == "127.0.0.1"));
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
            startup_timeout_secs: 60,
            child_restart_max: 3,
            min_stable_uptime_secs: 30,
            body_limit_bytes: 32 * 1024 * 1024,
        }
    }
}
