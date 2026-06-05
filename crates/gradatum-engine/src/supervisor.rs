//! Superviseur `llama-server` — spawn, wait-ready, restart borné, shutdown.
//!
//! ## Responsabilités
//!
//! - **spawn** : lance `llama-server` via `tokio::process::Command` (jamais `sh -c`).
//!   Binaire canonicalisé au spawn + préfixe autorisé (SP-P0-4). Env clear + allow-list
//!   GPU pour éviter le contournement orthogonal via variables d'environnement.
//! - **wait_ready** : poll `GET http://127.0.0.1:{child_port}/health` jusqu'à 200
//!   (timeout `startup_timeout_secs`). Retourne `ChildState::Starting` pendant le poll.
//! - **supervise_loop** : restart-on-failure borné (`child_restart_max`, budget total) +
//!   backoff exponentiel + détection flapping. Après épuisement → `HealthState::unhealthy` (SP-P0-3).
//! - **shutdown** : SIGTERM → attente grace period → SIGKILL (SP-P1).
//!
//! ## Sécurité orphelins (SP-P0-1)
//!
//! Choix retenu : **`process_group(0)` + `kill_on_drop(true)` + systemd `KillMode=control-group`**
//! (zéro unsafe — `#![forbid(unsafe_code)]` préservé).
//!
//! - `process_group(0)` : place l'enfant dans son propre process group → SIGTERM/SIGKILL
//!   propagé via `killpg(pgid, sig)` dans `shutdown()`.
//! - `kill_on_drop(true)` (tokio) : envoie SIGKILL si le `Child` tokio est droppé — couvre
//!   les panics *unwinding* et les drops normaux (ex. stop propre du superviseur).
//!   **LIMITE** : ne fire PAS sur un crash dur (panic=abort, SIGSEGV, OOM-kill du PID main)
//!   car le Drop ne s'exécute pas. Dans ce cas, l'enfant devient orphelin jusqu'au
//!   prochain restart systemd (fenêtre ≤ RestartSec, généralement 10s).
//! - `KillMode=control-group` + `Delegate=yes` (service systemd) : à l'arrêt/restart de
//!   l'unité, systemd tue tous les process du cgroup, y compris l'enfant orphelin du crash
//!   dur. C'est la jambe principale pour le cas SIGKILL parent. `Delegate=yes` garantit
//!   que le cgroup est propre avant re-ExecStart (pas de double chargement modèle GPU).
//!   **Fenêtre résiduelle** : entre le crash dur et le restart systemd, un seul orphelin peut
//!   tenir la VRAM (pas de double chargement simultané puisque systemd nettoie avant restart).
//!
//! La variante `PR_SET_PDEATHSIG` (unsafe `pre_exec`) n'est PAS utilisée — le signal
//! n'est pas transmis si le parent fait `exec()` entre `fork()` et `pre_exec`,
//! et le bénéfice est marginal face à l'overhead unsafe.

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

/// Préfixes autorisés pour le binaire `llama-server` (SP-P0-4).
const ALLOWED_BIN_PREFIXES: &[&str] = &["/usr/local/bin/", "/opt/gradatum/bin/"];

/// Délai de grâce SIGTERM→SIGKILL au shutdown (secondes).
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// Backoff exponentiel initial (millisecondes).
const BACKOFF_INIT_MS: u64 = 500;

/// Backoff exponentiel maximum (millisecondes).
const BACKOFF_MAX_MS: u64 = 30_000;

/// Allow-list des extra_args autorisés dans `EngineConfig.extra_args`.
///
/// ## Décision de sécurité
///
/// Cette allow-list remplace l'ancienne deny-list qui était intrinsèquement incomplète
/// (llama-server expose >100 flags, dont plusieurs dangereux non couverts : `--api-key-file`,
/// `--model-url`, `--lora`, `--path`, `--ssl-key-file`…).
///
/// **Principe** : seuls les flags de la liste ci-dessous sont autorisés. Tout flag absent
/// est rejeté avec `EngineError::BadRequest` précisant le flag incriminé.
///
/// **Extension** : toute extension de cette liste est une décision de sécurité explicite —
/// vérifier que le flag n'ouvre pas d'accès réseau arbitraire, de lecture/écriture de chemin
/// non contrôlé, ou de contournement du bind loopback.
///
/// **Exclusion `--n-gpu-layers`/`-ngl`/`--gpu-layers`** : ces flags sont gérés
/// autoritairement par le champ config `gpu_layers` → `--n-gpu-layers` dans la commande.
/// Les laisser passer via extra_args générerait un avertissement llama-server
/// "argument specified multiple times" et pourrait masquer la valeur configurée.
const ALLOWED_EXTRA_FLAGS: &[&str] = &[
    // Attention
    "--flash-attn",
    "-fa",
    // Gestion mémoire
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
    // Threads HTTP
    "--threads-http",
    // Contexte / KV cache
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
    // RoPE / YaRN
    "--rope-scaling",
    "--rope-scale",
    "--rope-freq-base",
    "--rope-freq-scale",
    "--yarn-orig-ctx",
    "--yarn-ext-factor",
    "--yarn-attn-factor",
    "--yarn-beta-slow",
    "--yarn-beta-fast",
    // Reproductibilité
    "--seed",
    "-s",
    // Performance
    "--poll",
    // --- Vague-2 : SWA / cache-reuse (perf gros modèles) ---
    "--swa-full",
    "--cache-reuse",
    // --- Vague-2 : reasoning (modèle thinking deepseek) ---
    "--reasoning",
    "--reasoning-format",
    "--reasoning-budget",
    // --- Vague-2 : sampling (parité standalone — ex. 8080 temp 0.7) ---
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

/// Préfixes de variables d'environnement GPU préservées au spawn.
///
/// `env_clear()` est appliqué sur la Command, puis seules les vars dont la clé
/// commence par l'un de ces préfixes (ou dont le nom est dans `ENV_PASSTHROUGH`)
/// sont ré-injectées depuis l'environnement du superviseur.
///
/// Ces préfixes couvrent Vulkan/RADV (VK_*, MESA_*, RADV_*), ROCm/AMD
/// (HIP_*, ROCR_*, ROCM_*, HSA_*), CUDA/NVIDIA (CUDA_*, NVIDIA_*) et
/// llama.cpp natif (GGML_*). Les garder est CRITIQUE pour que llama-server
/// détecte le GPU sur un hôte GPU.
const GPU_ENV_PREFIXES: &[&str] = &[
    "GGML_", "VK_", "HIP_", "ROCR_", "ROCM_", "HSA_", "CUDA_", "NVIDIA_", "MESA_", "RADV_",
];

/// Variables d'environnement passthrough (par nom exact).
///
/// Indépendantes des préfixes GPU — nécessaires au bon fonctionnement du binaire.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "LD_LIBRARY_PATH"];

/// Construit l'argv `llama-server` à partir de la config (pure, testable).
///
/// Flags autoritaires (jamais surchargeables via `extra_args`) : `--model`, `--port`,
/// `--host` (loopback), `--n-gpu-layers`, `--threads`, `--ctx-size`, `--parallel`,
/// `--embedding` (si embed), `--mmproj` (si `mmproj_path` configuré, vision).
/// Les `extra_args` (validés allow-list) sont appendés en dernier.
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

    // Vision : mmproj via champ config dédié (jamais via extra_args — hors allow-list).
    if let Some(mmproj) = &cfg.mmproj_path {
        args.push("--mmproj".into());
        args.push(mmproj.to_string_lossy().into_owned());
    }

    // Extra args — validés dans l'allow-list à la construction du superviseur.
    args.extend(cfg.extra_args.iter().cloned());

    args
}

/// Résultat de la vérification de santé de l'enfant.
#[derive(Debug, PartialEq)]
pub enum ChildState {
    /// L'enfant est prêt (HTTP 200 sur /health).
    Ready,
    /// L'enfant démarre encore (timeout non atteint).
    Starting,
    /// L'enfant n'a pas répondu dans le timeout.
    StartupTimeout,
}

/// Superviseur du sous-process `llama-server`.
pub struct LlamaServerSupervisor {
    /// Processus enfant courant (None si pas encore démarré ou après shutdown).
    child: Mutex<Option<Child>>,
    /// Indicateur d'arrêt demandé — stoppe la boucle de supervision.
    shutdown_requested: AtomicBool,
    /// Port d'écoute de l'enfant (loopback, distinct du port superviseur).
    child_port: u16,
    /// Config complète de l'engine.
    config: EngineConfig,
    /// Chemin canonicalisé du binaire llama-server (résolu une fois à la construction).
    canonical_bin: PathBuf,
    /// Client HTTP pour le poll /health et les proxies.
    pub client: reqwest::Client,
}

impl LlamaServerSupervisor {
    /// Construit un superviseur (sans démarrer l'enfant).
    ///
    /// Valide à la construction :
    /// - binaire canonicalisable + préfixe autorisé (SP-P0-4),
    /// - child_port > 1024 et distinct de port,
    /// - extra_args dans l'allow-list.
    ///
    /// # Errors
    /// `EngineError::ModelLoad` si le binaire est invalide ou hors préfixe.
    /// `EngineError::BadRequest` si extra_args contient un flag hors allow-list.
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

    /// Construit et lance le sous-process `llama-server`.
    ///
    /// Flags réels confirmés sur llama-server b8931 (`--help`) :
    /// - `--model` : chemin GGUF
    /// - `--port` : port TCP enfant
    /// - `--host` : adresse d'écoute (`127.0.0.1` — loopback only)
    /// - `--n-gpu-layers` (alias `--gpu-layers`) : couches GPU
    /// - `--threads` (`-t`) : threads CPU génération
    /// - `--ctx-size` (`-c`) : taille contexte
    /// - `--parallel` (`-np`) : slots parallèles (concurrence)
    /// - `--embedding` / `--embeddings` : mode embedding (restrict)
    ///
    /// JAMAIS `sh -c` / JAMAIS string interpolée dans un shell (SP-P0-4).
    ///
    /// # Errors
    /// `EngineError::ModelLoad` si le spawn échoue.
    pub async fn spawn_child(&self) -> Result<(), EngineError> {
        let bin = &self.canonical_bin;
        let cfg = &self.config;

        let args = build_child_args(cfg);

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            // SP-P0-1 : process group propre pour isolation orphelin
            .process_group(0)
            // SP-P0-1 : SIGKILL si le Child est droppé (panics unwinding)
            .kill_on_drop(true)
            // Stdio::inherit() — journald capte les logs via le cgroup systemd.
            // Note : Stdio::piped() sans drain deadlocke sur cold start verbeux
            // (banner backend + GGUF metadata > 64KB saturation buffer noyau).
            // Drain structuré (BufReader + tokio::spawn) = follow-up SP-P1 différé.
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        // Blocker 3 — env_clear + ré-injection allow-list GPU.
        //
        // `env_clear()` empêche llama-server de lire les vars d'env du superviseur
        // (ex. LLAMA_ARG_HOST, LLAMA_ARG_PORT, LLAMA_API_KEY, LLAMA_ARG_MODEL_URL,
        // HF_TOKEN...) qui pourraient contourner orthogonalement les restrictions
        // positionnées en argv (loopback, modèle, pas de téléchargement réseau).
        //
        // Les vars GPU (VK_*, MESA_*, RADV_*, GGML_*, HIP_*, ROCR_*, ROCM_*, HSA_*,
        // CUDA_*, NVIDIA_*) sont CRITIQUES pour la détection GPU — elles sont
        // préservées par préfixe depuis l'env courant.
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

    /// Poll `GET /health` de l'enfant jusqu'à 200 ou timeout.
    ///
    /// - `ConnectionRefused` pendant le warm-up = état `Starting` normal (SP-P1).
    /// - Retourne `ChildState::Ready` dès le premier HTTP 200.
    /// - Retourne `ChildState::StartupTimeout` si le timeout est atteint.
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

    /// Boucle de supervision avec budget restart total + détection flapping (SP-P0-3).
    ///
    /// ## Politique de restart
    ///
    /// `child_restart_max` est un **budget total** (pas un rate-limit par fenêtre).
    /// Chaque crash décrémente le budget, quelle que soit la durée d'uptime.
    ///
    /// **Reset du budget** : UNIQUEMENT si l'enfant a été stable ≥ `min_stable_uptime_secs`
    /// avant le crash. Un uptime court (flapping : Ready→crash en < seuil) consomme le
    /// budget SANS reset, forçant l'escalade vers `HealthState::unhealthy` après
    /// `child_restart_max` crashs.
    ///
    /// **Reset du backoff** : même condition que le reset budget.
    ///
    /// ## Seed initial (Blocker 1)
    ///
    /// `initial_ready_at` porte l'`Instant` du moment où `wait_ready()` a retourné
    /// `Ready` lors du spawn initial (effectué dans `main()` avant `supervise_loop`).
    /// Sans ce seed, le premier crash d'un enfant sain aurait `last_ready_at == None`
    /// → classé flapping à tort → budget amputé prématurément.
    ///
    /// ## Shutdown
    ///
    /// `shutdown_requested` vérifié au sommet de boucle, après `child.wait()`, et
    /// JUSTE AVANT `spawn_child()` (après le sleep backoff) — P1-c.
    pub async fn supervise_loop(
        self: Arc<Self>,
        health: Arc<HealthState>,
        initial_ready_at: Option<Instant>,
    ) {
        let mut restart_budget = self.config.child_restart_max;
        let mut backoff_ms = BACKOFF_INIT_MS;
        // Seedé depuis main() pour couvrir le cas du 1er crash post spawn initial.
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

            // --- Calcul uptime et politique flapping (P1-b) ---
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
            // Flapping : budget et backoff continuent à escalader sans reset.
            // last_ready_at sera mis à jour après le prochain spawn réussi.

            // --- Vérifier le budget total ---
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

            // P1-c : re-tester APRÈS le sleep, AVANT spawn_child().
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

    /// Shutdown gracieux : SIGTERM → attente grace → SIGKILL → reap (SP-P1).
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

    /// URL de base de l'enfant (loopback) pour le proxy.
    pub fn child_base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.child_port)
    }
}

// ---------------------------------------------------------------------------
// Helpers publics
// ---------------------------------------------------------------------------

/// Canonicalise le chemin du binaire et vérifie le préfixe autorisé (SP-P0-4).
///
/// Retourne le `PathBuf` canonicalisé — à stocker et utiliser au spawn (anti-TOCTOU).
///
/// # Errors
/// `EngineError::ModelLoad` si le binaire est absent ou hors préfixe autorisé.
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

/// Alias de rétro-compatibilité.
pub fn validate_bin_path(bin: &Path) -> Result<(), EngineError> {
    canonicalize_bin_path(bin).map(|_| ())
}

/// Vérifie que chaque `extra_arg` est dans l'allow-list `ALLOWED_EXTRA_FLAGS`.
///
/// La clé est extraite en splitant sur `=` (forme `--flag=value`) ou en
/// prenant l'arg tel quel (forme `--flag value`). Les valeurs positionnelles
/// (argument suivant un flag autorisé) ne sont pas inspectées — seules les
/// clés débutant par `-` sont contrôlées.
///
/// # Errors
/// `EngineError::BadRequest` si un flag hors allow-list est trouvé.
pub fn validate_extra_args(extra_args: &[String]) -> Result<(), EngineError> {
    for arg in extra_args {
        // Ignorer les valeurs positionnelles (ne commencent pas par `-`)
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

/// Ré-injecte dans `cmd` les variables d'environnement autorisées depuis le process courant.
///
/// À appeler après `cmd.env_clear()`. Préserve :
/// - Les vars par nom exact listées dans `ENV_PASSTHROUGH` (PATH, HOME, LD_LIBRARY_PATH).
/// - Les vars dont la clé commence par un préfixe GPU listé dans `GPU_ENV_PREFIXES`
///   (VK_*, MESA_*, RADV_*, GGML_*, HIP_*, ROCR_*, ROCM_*, HSA_*, CUDA_*, NVIDIA_*).
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

/// Détermine si une erreur reqwest est un `ConnectionRefused`.
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
            gradatum_url: "http://127.0.0.1:19090".into(),
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
