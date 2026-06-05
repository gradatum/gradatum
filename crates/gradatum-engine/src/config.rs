//! Configuration `gradatum-engine` — parse `conf.d/70-engine.toml`.
//!
//! ## Sources de configuration
//!
//! 1. Fichier TOML local (`EngineConfig::load_local(path)`) via figment.
//! 2. Variables d'environnement préfixées `GRADATUM_ENGINE_` (override).
//! 3. Source centrale `/api/v1/config/:binary` = provider figment différé
//!    (endpoint inexistant en v0.3.x — design-only, Seam 3).
//!
//! ## Sécurité (P1-6)
//!
//! La validation de `model_path` (canonicalisation + vérification du préfixe
//! `/opt/gradatum/models/`) et de `bind_addr` (refus des wildcards 0.0.0.0/::)
//! sont effectuées par [`EngineConfig::validate()`].
//! `load_local()` parse et désérialise uniquement — appeler `validate()` ensuite
//! (le binaire le fait systématiquement après `load_local()`).
//! `load_local()` seul ne valide PAS le chemin du modèle ni l'adresse de bind.
use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// Type de modèle chargé — détermine le contexte d'inférence (P2-3).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// Modèle de génération de texte (chat/completions).
    Chat,
    /// Modèle d'embedding (embeddings).
    Embed,
}

/// Seam Runtime (spec §Seams).
///
/// - `LlamaServer` : superviseur d'un sous-process `llama-server` natif (PIVOT v2).
///   Remplace `Llamacpp` (FFI) — **BREAKING CHANGE config** : renommer `llamacpp` → `llamaserver`
///   dans les fichiers TOML déployés (ou omettre `runtime` pour utiliser le défaut).
/// - `Onnx` : DIFFÉRÉ — la valeur est reconnue et parsée, mais le binaire renvoie
///   une erreur explicite (branche en place, pas d'implémentation).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// Superviseur `llama-server` natif — PIVOT v2 (remplace FFI llama-cpp-2).
    #[default]
    LlamaServer,
    /// Backend ONNX Runtime — DIFFÉRÉ (spike design-only).
    Onnx,
}

/// Configuration d'une instance `gradatum-engine`.
///
/// Chaque instance correspond à un modèle chargé (curator chat ou embed).
/// Les champs sont wrappés dans `[engine]` dans le TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    /// Chemin vers le fichier GGUF. Canonicalisé + validé préfixe au runtime (P1-6).
    pub model_path: String,
    /// Type de modèle : `chat` ou `embed`.
    pub model_kind: ModelKind,
    /// Runtime d'inférence (défaut : `llamacpp`). Seam 2.
    #[serde(default)]
    pub runtime: RuntimeKind,
    /// Stratégie de warm-up : `"eager"` (load puis ready) ou `"lazy"` (ready après première requête).
    #[serde(default = "default_warmup")]
    pub warm_up: String,
    /// Nombre de couches à offloader sur GPU (0 = CPU pur).
    #[serde(default)]
    pub gpu_layers: u32,
    /// Nombre de threads CPU pour l'inférence.
    #[serde(default = "default_threads")]
    pub n_threads: u32,
    /// Taille du contexte KV en tokens.
    #[serde(default = "default_ctx")]
    pub context_len: u32,
    /// Port TCP d'écoute du serveur engine (interface définie par `bind_addr`).
    pub port: u16,

    /// Adresse de bind du serveur engine.
    ///
    /// Défaut `None` = `127.0.0.1` (loopback, comportement loopback-only préservé).
    ///
    /// Pour exposer l'engine sur le réseau LAN (ex. cutover curator/embed),
    /// configurer avec l'IP unicast routable spécifique de l'interface LAN —
    /// jamais `0.0.0.0` ni `::` (wildcards refusés par `validate()` — C1 fail-closed).
    ///
    /// Exemple TOML : `bind_addr = "203.0.113.5"` (IP de test — utiliser l'IP LAN réelle).
    #[serde(default)]
    pub bind_addr: Option<IpAddr>,

    /// Port TCP du listener `/metrics` (loopback-only, distinct de `port`).
    ///
    /// `/metrics` expose des métriques Prometheus internes — ne doit jamais être
    /// accessible sur le LAN. Ce port est bindé sur `127.0.0.1` indépendamment
    /// de `bind_addr`. Défaut : `port + 1`.
    #[serde(default)]
    pub metrics_port: Option<u16>,
    /// Timeout d'inférence en secondes (P1-2).
    /// Dépasse → `EngineError::Timeout` (code 504) → gateway déclenche le fallback.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Nombre maximum de tokens générés par requête chat (M3 — défaut 512).
    /// Borné à `[1, 65535]` par la logique du binaire.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// URL de base du serveur gradatum (pour l'event-log et l'échange JWT).
    /// Doit être loopback : `http://127.0.0.1:19090` (P2-4 anti-SSRF).
    #[serde(default = "default_gradatum_url")]
    pub gradatum_url: String,

    // -------------------------------------------------------------------------
    // Champs PIVOT v2 — superviseur llama-server (spec §Nouveaux champs)
    // -------------------------------------------------------------------------
    /// Chemin vers le binaire `llama-server`.
    ///
    /// Canonicalisé + préfixe autorisé (`/usr/local/bin/` ou `/opt/gradatum/bin/`)
    /// à la construction du superviseur (SP-P0-4).
    #[serde(default = "default_llama_server_bin")]
    pub llama_server_bin: PathBuf,

    /// Port TCP d'écoute du sous-process `llama-server` (loopback, distinct de `port`).
    ///
    /// Doit être > 1024. Le superviseur écoute sur `port`, l'enfant sur `child_port`.
    /// Le proxy reqwest route vers `127.0.0.1:child_port`.
    #[serde(default = "default_child_port")]
    pub child_port: u16,

    /// Nombre de slots parallèles `--parallel` passés à `llama-server`.
    ///
    /// `llama-server` gère sa propre concurrence — le Mutex FFI est supprimé (P0-2 caduc).
    #[serde(default = "default_parallel")]
    pub parallel: u32,

    /// Arguments pass-through supplémentaires pour `llama-server`.
    ///
    /// Passés tels quels après les args dérivés de la config. Exemple : `["--flash-attn"]`.
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Chemin vers le projecteur multimodal (mmproj GGUF) pour les modèles vision.
    ///
    /// `None` = pas de vision. Si `Some`, le chemin est canonicalisé + validé sous
    /// `/opt/gradatum/models/` par `validate()` (même contrainte que `model_path`, P1-6),
    /// puis injecté en `--mmproj <path>` dans la commande `llama-server`.
    ///
    /// **Jamais via `extra_args`** : `--mmproj` reste hors `ALLOWED_EXTRA_FLAGS` —
    /// la vision passe exclusivement par ce champ dédié.
    #[serde(default)]
    pub mmproj_path: Option<PathBuf>,

    /// Timeout de démarrage de `llama-server` en secondes.
    ///
    /// Le superviseur poll `/health` jusqu'à ce timeout. Dépassé → unhealthy.
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,

    /// Nombre maximum de restarts **total** (budget, pas rate-limit) (SP-P0-3).
    ///
    /// Décrémenté à chaque crash. Remis au max uniquement si l'enfant a été stable
    /// ≥ `min_stable_uptime_secs` avant le crash (évite le flapping). `0` = aucun
    /// restart autorisé → unhealthy au 1er crash.
    /// Après épuisement → `HealthState::Unhealthy` → fallback gateway.
    #[serde(default = "default_restart_max")]
    pub child_restart_max: u32,

    /// Durée minimale en état `ready` avant de considérer un crash comme « stable »
    /// et de remettre le budget restart + backoff à zéro (P1-b anti-flapping).
    ///
    /// Si l'enfant crashe en moins de cette durée après avoir atteint `ready`,
    /// le crash est compté comme flapping : le budget est consommé sans reset et
    /// le backoff continue d'escalader. Défaut : 30s.
    #[serde(default = "default_min_stable_uptime")]
    pub min_stable_uptime_secs: u64,

    /// Limite de taille du corps des requêtes sur le port principal, en octets.
    ///
    /// Remplace le `DefaultBodyLimit::max(1MB)` codé en dur (vague-1). Les images
    /// vision encodées base64 dépassent 1 MiB → défaut élargi à 32 MiB. Une requête
    /// au-delà reçoit `413 Payload Too Large`.
    #[serde(default = "default_body_limit")]
    pub body_limit_bytes: usize,
}

fn default_warmup() -> String {
    "eager".into()
}
fn default_threads() -> u32 {
    8
}
fn default_ctx() -> u32 {
    32_768
}
fn default_timeout() -> u64 {
    120
}
fn default_max_tokens() -> u32 {
    512
}
fn default_gradatum_url() -> String {
    "http://127.0.0.1:19090".into()
}
fn default_llama_server_bin() -> PathBuf {
    PathBuf::from("/usr/local/bin/llama-server")
}
fn default_child_port() -> u16 {
    11436
}
fn default_parallel() -> u32 {
    4
}
fn default_startup_timeout() -> u64 {
    60
}
fn default_restart_max() -> u32 {
    3
}
fn default_min_stable_uptime() -> u64 {
    30
}
fn default_body_limit() -> usize {
    32 * 1024 * 1024 // 32 MiB
}

/// Wrapper pour la désérialisation figment/toml (`[engine]` section).
#[derive(Deserialize)]
struct Wrapper {
    engine: EngineConfig,
}

/// Valide qu'une adresse IP n'est pas un wildcard, broadcast, ni multicast (C1 fail-closed).
///
/// ## Politique
///
/// - REJETÉ : `0.0.0.0` (IPv4 unspecified), `::` (IPv6 unspecified),
///   `::ffff:0.0.0.0` (IPv4-mapped unspecified — binde toutes les interfaces IPv4 sur
///   Linux avec `net.ipv6.bindv6only=0`), `255.255.255.255` (IPv4 broadcast),
///   `::ffff:255.255.255.255` (broadcast mappé), tout multicast.
///   Le principe : une IP unicast précise, choisie explicitement par l'opérateur.
/// - AUTORISÉ : loopback (127.x.x.x, ::1, ::ffff:127.0.0.1), unicast routable spécifique
///   (IP LAN fixe configurée par l'opérateur), IPv4-mapped unicast (ex. `::ffff:203.0.113.5`).
///
/// ## IPv4-mapped
///
/// Pour toute adresse V6, `to_ipv4_mapped()` est appelé en premier. Si un V4 est extrait,
/// les règles V4 s'appliquent (unspecified, broadcast, multicast). Cela couvre
/// `::ffff:0.0.0.0` (mapped unspecified) et `::ffff:255.255.255.255` (mapped broadcast)
/// sans liste noire exhaustive.
///
/// # Errors
/// `anyhow::Error` si l'adresse est interdite, avec un message explicatif.
fn validate_bind_addr(addr: IpAddr) -> Result<(), anyhow::Error> {
    match addr {
        IpAddr::V4(v4) => check_v4_addr(v4, &addr.to_string())?,
        IpAddr::V6(v6) => {
            // Décoder le V4-mapped avant tout autre check (stable depuis Rust 1.63).
            // ::ffff:X.X.X.X → applique les mêmes règles que l'IPv4 X.X.X.X.
            if let Some(v4_mapped) = v6.to_ipv4_mapped() {
                check_v4_addr(v4_mapped, &addr.to_string())?;
            } else {
                // V6 pur (non-mappé)
                if v6.is_unspecified() {
                    anyhow::bail!(
                        "bind_addr '::' interdit (C1 fail-closed) — \
                         bind wildcard IPv6 expose l'engine sur toutes les interfaces. \
                         Utiliser ::1 (loopback) ou l'IP unicast LAN spécifique."
                    );
                }
                if v6.is_multicast() {
                    anyhow::bail!(
                        "bind_addr '{addr}' interdit — adresse multicast IPv6 non autorisée."
                    );
                }
            }
        }
    }
    Ok(())
}

/// Canonicalise un chemin et vérifie qu'il est sous `/opt/gradatum/models/` (P1-6).
///
/// Utilisé pour `model_path` et `mmproj_path`. Le fichier doit exister
/// (canonicalize échoue sinon).
///
/// # Errors
/// `anyhow::Error` si le chemin est inaccessible ou hors préfixe.
fn validate_model_prefix(path: &std::path::Path) -> Result<(), anyhow::Error> {
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize échoué pour {} : {e}", path.display()))?;
    if !canonical.starts_with("/opt/gradatum/models/") {
        anyhow::bail!(
            "chemin doit être sous /opt/gradatum/models/ (P1-6) : {}",
            canonical.display()
        );
    }
    Ok(())
}

/// Vérifie les contraintes sur une adresse IPv4 (helper interne).
///
/// `display` est la représentation textuelle de l'adresse originale (peut être
/// une notation IPv4-mapped comme `::ffff:0.0.0.0`) pour le message d'erreur.
fn check_v4_addr(v4: std::net::Ipv4Addr, display: &str) -> Result<(), anyhow::Error> {
    if v4.is_unspecified() {
        anyhow::bail!(
            "bind_addr '{display}' interdit (C1 fail-closed) — \
             bind wildcard expose l'engine sur toutes les interfaces. \
             Utiliser 127.0.0.1 (loopback) ou l'IP unicast LAN spécifique."
        );
    }
    if v4.is_broadcast() {
        anyhow::bail!("bind_addr '{display}' interdit — adresse broadcast non autorisée.");
    }
    if v4.is_multicast() {
        anyhow::bail!("bind_addr '{display}' interdit — adresse multicast non autorisée.");
    }
    Ok(())
}

impl EngineConfig {
    /// Parse depuis une chaîne TOML brute — usage tests unitaires uniquement.
    ///
    /// # Errors
    /// Retourne une erreur si le TOML est invalide ou si un champ requis manque.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        Ok(toml::from_str::<Wrapper>(s)?.engine)
    }

    /// Valide la config après parse.
    ///
    /// Doit être appelé après `load_local()` ou `from_toml()`. Le binaire le fait
    /// systématiquement. Les consommateurs directs de `load_local()` doivent aussi l'appeler.
    ///
    /// ## Validations effectuées
    ///
    /// - `model_path` canonicalisable (fichier accessible) + sous `/opt/gradatum/models/` (P1-6).
    /// - `bind_addr` : REJETÉ si `0.0.0.0` (IPv4 unspecified), `::` (IPv6 unspecified),
    ///   ou toute adresse multicast (C1 fail-closed — jamais de bind wildcard).
    ///   AUTORISÉ : loopback (127.x.x.x, ::1) ou unicast routable spécifique.
    ///
    /// # Errors
    /// `anyhow::Error` si le chemin est invalide, hors préfixe, ou si `bind_addr` est un wildcard.
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        // --- body_limit_bytes : borne supérieure anti-DoS (256 MiB) ---
        const MAX_BODY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
        if self.body_limit_bytes > MAX_BODY_LIMIT_BYTES {
            anyhow::bail!(
                "body_limit_bytes {} dépasse le plafond {} (256 MiB)",
                self.body_limit_bytes,
                MAX_BODY_LIMIT_BYTES
            );
        }

        // --- model_path (P1-6) ---
        validate_model_prefix(std::path::Path::new(&self.model_path))
            .map_err(|e| anyhow::anyhow!("model_path : {e}"))?;

        // --- mmproj_path (P1-6, même contrainte que model_path) ---
        if let Some(mmproj) = &self.mmproj_path {
            validate_model_prefix(mmproj).map_err(|e| anyhow::anyhow!("mmproj_path : {e}"))?;
        }

        // --- bind_addr : fail-closed (C1) ---
        if let Some(addr) = self.bind_addr {
            validate_bind_addr(addr)?;
        }

        Ok(())
    }

    /// Retourne l'adresse de bind résolue : `bind_addr` si configuré, sinon `127.0.0.1`.
    ///
    /// Garantit le fallback loopback quand `bind_addr` est absent.
    pub fn resolved_bind_addr(&self) -> IpAddr {
        self.bind_addr
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    }

    /// Retourne le port metrics résolu : `metrics_port` si configuré, sinon `port + 1`.
    ///
    /// Le listener metrics est toujours bindé sur `127.0.0.1` (loopback-only).
    pub fn resolved_metrics_port(&self) -> u16 {
        self.metrics_port
            .unwrap_or_else(|| self.port.saturating_add(1))
    }

    /// Charge la config depuis un fichier TOML local + override env `GRADATUM_ENGINE_*`.
    ///
    /// Seam 3 : source locale via figment (bronze F-53). La source centrale
    /// (`/api/v1/config/:binary`) est un provider figment supplémentaire différé.
    ///
    /// **Note sécurité** : cette méthode parse et désérialise uniquement. Elle ne valide
    /// PAS `model_path`. Appeler [`EngineConfig::validate()`] après pour les garanties P1-6.
    ///
    /// # Errors
    /// Retourne `figment::Error` si le fichier est absent, malformé, ou si un
    /// champ requis manque.
    pub fn load_local(path: &std::path::Path) -> Result<Self, Box<figment::Error>> {
        use figment::{
            providers::{Env, Format, Toml},
            Figment,
        };
        let w: Wrapper = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("GRADATUM_ENGINE_"))
            .extract()
            .map_err(Box::new)?;
        Ok(w.engine)
    }

    /// Alias du modèle — dérivé du nom de fichier GGUF pour l'event-log.
    ///
    /// Ex : `/opt/gradatum/models/qwen3-4b.gguf` → `"qwen3-4b"`.
    pub fn model_alias(&self) -> String {
        std::path::Path::new(&self.model_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("engine")
            .to_string()
    }

    /// Alias provider gateway — dérivé du `model_kind`.
    ///
    /// Ex : `ModelKind::Chat` → `"engine-curator"`, `ModelKind::Embed` → `"engine-embed"`.
    pub fn provider_alias(&self) -> String {
        match self.model_kind {
            ModelKind::Chat => "engine-curator".into(),
            ModelKind::Embed => "engine-embed".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_instance() {
        let toml = r#"
[engine]
model_path  = "/opt/models/qwen3-4b.gguf"
model_kind  = "chat"
warm_up     = "eager"
gpu_layers  = 0
n_threads   = 8
context_len = 32768
port        = 11435
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.model_kind, ModelKind::Chat);
        assert_eq!(c.gpu_layers, 0);
        assert_eq!(c.port, 11435);
        assert_eq!(
            c.runtime,
            RuntimeKind::LlamaServer,
            "défaut runtime = llamaserver (PIVOT v2)"
        );
        assert_eq!(c.timeout_secs, 120, "défaut timeout = 120s");
        assert_eq!(
            c.gradatum_url, "http://127.0.0.1:19090",
            "défaut gradatum_url"
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"vision\"\nport=1\n";
        assert!(EngineConfig::from_toml(toml).is_err());
    }

    #[test]
    fn parses_onnx_runtime_seam() {
        // Seam design-only : la config ONNX PARSE (la branche existe), le wiring
        // binaire la refusera explicitement.
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"embed\"\nruntime=\"onnx\"\nport=11436\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.runtime, RuntimeKind::Onnx);
    }

    #[test]
    fn parses_llamaserver_runtime() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nruntime=\"llamaserver\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.runtime,
            RuntimeKind::LlamaServer,
            "runtime=llamaserver parsé"
        );
    }

    #[test]
    fn parses_pivot_v2_fields() {
        let toml = r#"
[engine]
model_path       = "/opt/gradatum/models/qwen3-4b.gguf"
model_kind       = "chat"
port             = 11435
child_port       = 11436
parallel         = 4
startup_timeout_secs = 90
child_restart_max    = 5
extra_args       = ["--flash-attn"]
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.child_port, 11436, "child_port parsé");
        assert_eq!(c.parallel, 4, "parallel parsé");
        assert_eq!(c.startup_timeout_secs, 90, "startup_timeout_secs parsé");
        assert_eq!(c.child_restart_max, 5, "child_restart_max parsé");
        assert_eq!(c.extra_args, vec!["--flash-attn"], "extra_args parsé");
    }

    #[test]
    fn defaults_pivot_v2_fields() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.llama_server_bin,
            std::path::PathBuf::from("/usr/local/bin/llama-server"),
            "défaut llama_server_bin"
        );
        assert_eq!(c.child_port, 11436, "défaut child_port");
        assert_eq!(c.parallel, 4, "défaut parallel");
        assert_eq!(c.startup_timeout_secs, 60, "défaut startup_timeout_secs");
        assert_eq!(c.child_restart_max, 3, "défaut child_restart_max");
        assert!(c.extra_args.is_empty(), "défaut extra_args = vide");
        assert_eq!(
            c.min_stable_uptime_secs, 30,
            "défaut min_stable_uptime_secs = 30s"
        );
    }

    #[test]
    fn parses_min_stable_uptime_secs() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nmin_stable_uptime_secs=60\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.min_stable_uptime_secs, 60,
            "min_stable_uptime_secs parsé depuis TOML"
        );
    }

    /// validate() doit rejeter un model_path hors /opt/gradatum/models/.
    ///
    /// /tmp est toujours présent sur Linux (canonicalize réussit) mais hors préfixe.
    #[test]
    fn validate_rejects_model_path_outside_prefix() {
        // Écrire un fichier réel dans /tmp pour que canonicalize() réussisse
        let tmp_path = "/tmp/gradatum-engine-test-model.gguf";
        let _ = std::fs::write(tmp_path, b"fake-gguf");

        let toml =
            format!("[engine]\nmodel_path=\"{tmp_path}\"\nmodel_kind=\"chat\"\nport=11435\n");
        let c = EngineConfig::from_toml(&toml).unwrap();
        let result = c.validate();
        // Nettoyage best-effort
        let _ = std::fs::remove_file(tmp_path);
        assert!(
            result.is_err(),
            "validate() doit rejeter un model_path hors /opt/gradatum/models/"
        );
    }

    #[test]
    fn validate_rejects_nonexistent_model_path() {
        let toml =
            "[engine]\nmodel_path=\"/opt/gradatum/models/does-not-exist.gguf\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = c.validate();
        assert!(
            result.is_err(),
            "validate() doit rejeter un model_path non-existant (canonicalize échoue)"
        );
    }

    // --- bind_addr C1 ---

    #[test]
    fn bind_addr_default_is_none_resolves_to_loopback() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(c.bind_addr.is_none(), "défaut bind_addr = None");
        let resolved = c.resolved_bind_addr();
        assert_eq!(
            resolved,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "bind_addr None → résolu 127.0.0.1"
        );
    }

    #[test]
    fn bind_addr_loopback_explicit_ok() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"127.0.0.1\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.bind_addr, Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
        assert!(
            validate_bind_addr(c.bind_addr.unwrap()).is_ok(),
            "127.0.0.1 doit être accepté"
        );
    }

    #[test]
    fn bind_addr_routable_unicast_accepted() {
        // 203.0.113.5 = TEST-NET-3 (RFC 5737) — IP unicast non-loopback de test
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"203.0.113.5\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(
            validate_bind_addr(c.bind_addr.unwrap()).is_ok(),
            "203.0.113.5 (unicast routable) doit être accepté"
        );
    }

    #[test]
    fn bind_addr_0_0_0_0_rejected() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"0.0.0.0\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), "0.0.0.0 doit être rejeté (C1 fail-closed)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("0.0.0.0") && msg.contains("interdit"),
            "message d'erreur doit citer 0.0.0.0 et 'interdit' : {msg}"
        );
    }

    #[test]
    fn bind_addr_ipv6_unspecified_rejected() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"::\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), ":: doit être rejeté (C1 fail-closed)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("::") && msg.contains("interdit"),
            "message d'erreur doit citer :: et 'interdit' : {msg}"
        );
    }

    #[test]
    fn bind_addr_multicast_rejected() {
        // 224.0.0.1 = adresse multicast IPv4
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbind_addr=\"224.0.0.1\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = validate_bind_addr(c.bind_addr.unwrap());
        assert!(result.is_err(), "adresse multicast doit être rejetée");
    }

    /// P0 — bypass IPv4-mapped unspecified : ::ffff:0.0.0.0 doit être REJETÉ.
    ///
    /// Sur Linux avec net.ipv6.bindv6only=0 (défaut), bind(::ffff:0.0.0.0) équivaut
    /// à bind(0.0.0.0) — écoute sur toutes les interfaces IPv4.
    #[test]
    fn bind_addr_ipv4_mapped_unspecified_rejected() {
        let addr: IpAddr = "::ffff:0.0.0.0".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "::ffff:0.0.0.0 (IPv4-mapped unspecified) doit être rejeté (P0 fail-closed)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("interdit"),
            "message d'erreur doit contenir 'interdit' : {msg}"
        );
    }

    /// P1 — broadcast IPv4 : 255.255.255.255 doit être REJETÉ.
    #[test]
    fn bind_addr_broadcast_rejected() {
        let addr: IpAddr = "255.255.255.255".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "255.255.255.255 (broadcast) doit être rejeté"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("interdit"),
            "message doit contenir 'interdit' : {msg}"
        );
    }

    /// P1 — broadcast IPv4-mapped : ::ffff:255.255.255.255 doit être REJETÉ.
    #[test]
    fn bind_addr_ipv4_mapped_broadcast_rejected() {
        let addr: IpAddr = "::ffff:255.255.255.255".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_err(),
            "::ffff:255.255.255.255 (broadcast mappé) doit être rejeté"
        );
    }

    /// Non-régression — ::ffff:203.0.113.5 (unicast mappé RFC5737) doit être ACCEPTÉ.
    ///
    /// Une adresse IPv4-mapped unicast est une IP routable valide — l'opérateur peut
    /// légitimement configurer un bind en notation mappée. Les règles unspecified/broadcast/
    /// multicast ne s'appliquent pas à 203.0.113.5.
    #[test]
    fn bind_addr_ipv4_mapped_unicast_accepted() {
        // ::ffff:203.0.113.5 = TEST-NET-3 (RFC 5737) en notation IPv4-mapped
        let addr: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        let result = validate_bind_addr(addr);
        assert!(
            result.is_ok(),
            "::ffff:203.0.113.5 (unicast mappé) doit être accepté : {result:?}"
        );
    }

    #[test]
    fn metrics_port_default_is_port_plus_one() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.resolved_metrics_port(),
            11436,
            "metrics_port défaut = port + 1"
        );
    }

    #[test]
    fn metrics_port_explicit_config() {
        let toml =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nmetrics_port=19091\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.resolved_metrics_port(),
            19091,
            "metrics_port configuré parsé"
        );
    }

    #[test]
    fn parses_timeout_secs() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\ntimeout_secs=60\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.timeout_secs, 60);
    }

    /// M3 — régression : max_tokens parsé depuis le TOML + défaut 512.
    #[test]
    fn parses_max_tokens_with_default() {
        let toml_default = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml_default).unwrap();
        assert_eq!(c.max_tokens, 512, "défaut max_tokens = 512");

        let toml_custom =
            "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\nmax_tokens=256\n";
        let c2 = EngineConfig::from_toml(toml_custom).unwrap();
        assert_eq!(c2.max_tokens, 256, "max_tokens custom parsé");
    }

    #[test]
    fn parses_gradatum_url() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\ngradatum_url=\"http://127.0.0.1:19090\"\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.gradatum_url, "http://127.0.0.1:19090");
    }

    #[test]
    fn model_alias_from_path() {
        let toml = "[engine]\nmodel_path=\"/opt/gradatum/models/qwen3-4b.gguf\"\nmodel_kind=\"chat\"\nport=1\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(c.model_alias(), "qwen3-4b");
    }

    #[test]
    fn provider_alias_chat_embed() {
        let toml_chat = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n";
        let toml_embed = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"embed\"\nport=1\n";
        assert_eq!(
            EngineConfig::from_toml(toml_chat).unwrap().provider_alias(),
            "engine-curator"
        );
        assert_eq!(
            EngineConfig::from_toml(toml_embed)
                .unwrap()
                .provider_alias(),
            "engine-embed"
        );
    }

    #[test]
    fn parses_mmproj_path() {
        let toml = r#"
[engine]
model_path  = "/opt/gradatum/models/qwen3-35b.gguf"
model_kind  = "chat"
port        = 8080
mmproj_path = "/opt/gradatum/models/mmproj-F16.gguf"
"#;
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.mmproj_path,
            Some(std::path::PathBuf::from(
                "/opt/gradatum/models/mmproj-F16.gguf"
            )),
            "mmproj_path parsé depuis le TOML"
        );
    }

    #[test]
    fn mmproj_path_default_is_none() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert!(c.mmproj_path.is_none(), "défaut mmproj_path = None");
    }

    #[test]
    fn validate_rejects_mmproj_outside_prefix() {
        // model_path valide (sous /opt/gradatum/models/) requis pour atteindre la branche mmproj.
        // On crée les 2 fichiers : le modèle sous le bon préfixe, le mmproj hors préfixe.
        let model = "/tmp/gradatum-engine-mmproj-test-model.gguf";
        let mmproj = "/tmp/gradatum-engine-mmproj-test-proj.gguf";
        let _ = std::fs::write(model, b"fake");
        let _ = std::fs::write(mmproj, b"fake");
        // model_path hors préfixe échouera AVANT mmproj — donc on teste mmproj isolément
        // via un model_path réel sous le préfixe n'est pas garanti en CI. On vérifie plutôt
        // que la fonction de validation mmproj rejette un chemin hors préfixe directement.
        let result = super::validate_model_prefix(std::path::Path::new(mmproj));
        let _ = std::fs::remove_file(model);
        let _ = std::fs::remove_file(mmproj);
        assert!(
            result.is_err(),
            "validate_model_prefix doit rejeter un chemin hors /opt/gradatum/models/"
        );
    }

    #[test]
    fn body_limit_bytes_default_is_32_mib() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.body_limit_bytes,
            32 * 1024 * 1024,
            "défaut body_limit_bytes = 32 MiB (images vision base64 dépassent 1 MiB)"
        );
    }

    #[test]
    fn parses_body_limit_bytes() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=8388608\n";
        let c = EngineConfig::from_toml(toml).unwrap();
        assert_eq!(
            c.body_limit_bytes, 8_388_608,
            "body_limit_bytes custom = 8 MiB parsé"
        );
    }

    #[test]
    fn validate_rejects_body_limit_over_cap() {
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=268435457\n"; // 256 MiB + 1
        let c = EngineConfig::from_toml(toml).unwrap();
        let result = c.validate();
        assert!(
            result.is_err(),
            "body_limit_bytes > 256 MiB doit être rejeté"
        );
        assert!(
            result.unwrap_err().to_string().contains("256 MiB"),
            "message doit citer le plafond"
        );
    }

    #[test]
    fn validate_accepts_body_limit_at_cap_boundary() {
        // 256 MiB pile = accepté côté body_limit (échouera ensuite sur model_path 'x' inexistant — c'est attendu,
        // on vérifie juste que le message d'erreur n'est PAS celui du cap).
        let toml = "[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=11435\nbody_limit_bytes=268435456\n"; // 256 MiB
        let c = EngineConfig::from_toml(toml).unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(
            !err.contains("256 MiB"),
            "256 MiB pile ne doit PAS être rejeté par le cap (erreur = model_path)"
        );
    }
}
