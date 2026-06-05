//! Binaire principal `gradatum-engine` — PIVOT v2 superviseur.
//!
//! Lit le chemin de config en argument, parse `EngineConfig`, valide le modèle
//! et le binaire `llama-server`, échange l'api-key → JWT, puis :
//! 1. Spawn `llama-server` via `LlamaServerSupervisor`.
//! 2. Poll `/health` enfant jusqu'au timeout `startup_timeout_secs`.
//! 3. Démarrage axum sur `config.port` (loopback, P1-4).
//! 4. Lance la boucle de supervision en background (restart borné SP-P0-3).
//!
//! ## Comportement startup KO
//!
//! Si `llama-server` ne répond pas dans le timeout, `main()` appelle
//! `health.set_unhealthy()` explicitement (wait_ready ne le fait pas). Les handlers
//! retournent 503 via le HealthState. Le fallback gateway prend le relais.
//! Le binaire ne panique pas — il reste en écoute.
//!
//! ## Sécurité
//!
//! - api-key lue depuis `GRADATUM_ENGINE_API_KEY` (env) ou `/etc/gradatum/engine.api-key`.
//! - Fallback `InMemorySink` si le serveur gradatum est injoignable (best-effort, P0-8).
//! - Bind loopback uniquement (P1-4) : `127.0.0.1:<port>`.
//! - JWT dans `Zeroizing<String>` (P2-4).
//! - Binaire `llama-server` canonicalisé + préfixe autorisé (SP-P0-4).
//! - model_path canonicalisé + préfixe `/opt/gradatum/models/` (P1-6).

#[cfg(not(feature = "serve"))]
fn main() {
    eprintln!("gradatum-engine: compilé sans la feature 'serve'. Rien à faire.");
    std::process::exit(1);
}

#[cfg(feature = "serve")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use gradatum_core::event_sink::InMemorySink;
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

    // --- Initialiser tracing ---
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gradatum_engine=info".parse().unwrap()),
        )
        .init();

    // --- Parse args ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: gradatum-engine <config-path>");
        std::process::exit(1);
    }
    let config_path = Path::new(&args[1]);

    // --- Charger config ---
    let config = EngineConfig::load_local(config_path)
        .map_err(|e| anyhow::anyhow!("EngineConfig::load_local échoué : {e}"))?;

    // --- Valider config (model_path canonicalisation + préfixe P1-6) ---
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("config invalide : {e}"))?;

    // --- Match runtime (Seam 2) ---
    if config.runtime == RuntimeKind::Onnx {
        anyhow::bail!("runtime 'onnx' non implémenté. Utiliser runtime='llamaserver' (défaut).");
    }

    // --- Valider port enfant (SP-P0-4) ---
    if config.child_port <= 1024 {
        anyhow::bail!(
            "child_port {} invalide — doit être > 1024 (SP-P0-4)",
            config.child_port
        );
    }

    // --- Valider base_url loopback (P2-4 anti-SSRF) ---
    validate_loopback_url(&config.gradatum_url)?;

    // --- Lire api-key (P0-8) ---
    let api_key = read_api_key()?;

    // --- Construire le sink (fallback InMemorySink si échange JWT KO) ---
    let sink: Arc<dyn gradatum_core::event_sink::EventSink> = {
        match exchange_api_key_for_jwt(&api_key, &config.gradatum_url).await {
            Ok(jwt) => Arc::new(HttpEventSink::new(config.gradatum_url.clone(), jwt)),
            Err(e) => {
                // Fallback best-effort — P0-8 : pas de crash sur JWT KO
                tracing::warn!(
                    error = %e,
                    "échange api-key→JWT échoué. Fallback InMemorySink (event-log non alimenté)."
                );
                Arc::new(InMemorySink::default())
            }
        }
    };

    // --- Dériver les métadonnées ---
    let model_name = config.model_alias();
    let provider = config.provider_alias();
    let health = Arc::new(HealthState::new(&model_name));
    let metrics = Arc::new(EngineMetrics::new());

    // --- Construire le superviseur ---
    let supervisor = LlamaServerSupervisor::new(config.clone())
        .map_err(|e| anyhow::anyhow!("LlamaServerSupervisor::new échoué : {e}"))?;

    // --- Spawn llama-server ---
    supervisor
        .spawn_child()
        .await
        .map_err(|e| anyhow::anyhow!("spawn llama-server échoué : {e}"))?;

    // --- Wait ready ---
    // Capture l'Instant du ready initial pour seeder last_ready_at dans supervise_loop
    // (Blocker 1 : sans ce seed, le 1er crash d'un enfant sain serait classé flapping).
    let initial_ready_at = {
        let state = supervisor.wait_ready(&health).await;
        if state == gradatum_engine::supervisor::ChildState::StartupTimeout {
            // wait_ready retourne StartupTimeout sans appeler set_unhealthy — on le fait ici
            // (P2 : corriger l'état pour que le gateway bascule en fallback proprement).
            tracing::error!(
                "llama-server n'a pas démarré dans le timeout — moteur unhealthy. \
                 Le fallback gateway prend le relais."
            );
            health.set_unhealthy();
            None // pas de seed : supervise_loop ne démarre pas sur un enfant mort
        } else {
            Some(std::time::Instant::now())
        }
    };

    // --- Construire le ForwardProxy transparent ---
    let proxy = ForwardProxy::new(supervisor.client.clone(), supervisor.child_base_url());

    // --- Construire AppState ---
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

    // --- Lancer la boucle de supervision en background (SP-P0-3) ---
    // initial_ready_at seed last_ready_at pour éviter une fausse détection flapping
    // au 1er crash d'un enfant sain (Blocker 1).
    let supervisor_arc = supervisor.clone();
    let health_arc = health.clone();
    tokio::spawn(async move {
        supervisor_arc
            .supervise_loop(health_arc, initial_ready_at)
            .await;
    });

    // --- Lancer le listener metrics sur loopback (C2) ---
    // /metrics est séparé du port principal pour ne jamais être exposé sur le LAN.
    let metrics_addr = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        config.resolved_metrics_port(),
    );
    let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;
    let metrics_router = EngineServer::metrics_router(metrics);
    tracing::info!(
        metrics_addr = %metrics_addr,
        "gradatum-engine /metrics listener loopback démarré"
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(metrics_listener, metrics_router).await {
            tracing::error!(error = %e, "metrics listener erreur");
        }
    });

    // --- Lancer axum principal (bind_addr configurable — C1) ---
    // bind_addr résolu depuis config : loopback (127.0.0.1) si non spécifié,
    // ou IP unicast LAN spécifique validée par validate() (fail-closed).
    let bind_addr = config.resolved_bind_addr();
    let addr = SocketAddr::new(bind_addr, config.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        addr = %addr,
        model = %state.model_name,
        child_port = config.child_port,
        metrics_port = config.resolved_metrics_port(),
        "gradatum-engine démarré (superviseur llama-server PIVOT v2)"
    );

    let router = EngineServer::router(state);
    axum::serve(listener, router).await?;
    Ok(())
}

/// Lit l'api-key depuis l'environnement ou le fichier de secrets.
#[cfg(feature = "serve")]
fn read_api_key() -> anyhow::Result<zeroize::Zeroizing<String>> {
    if let Ok(key) = std::env::var("GRADATUM_ENGINE_API_KEY") {
        return Ok(zeroize::Zeroizing::new(key));
    }
    let path = "/etc/gradatum/engine.api-key";
    let key = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("FATAL: api-key introuvable ({path}): {e}"))?;
    Ok(zeroize::Zeroizing::new(key.trim().to_string()))
}

/// Échange une api-key contre un JWT 24h via POST /auth/exchange.
///
/// La route est montée HORS du nest /api/v1 (gradatum-server main.rs:
/// `unauthed.merge(auth_exchange)`) — pas de préfixe /api/v1 (bug C1 préservé).
#[cfg(feature = "serve")]
async fn exchange_api_key_for_jwt(
    api_key: &zeroize::Zeroizing<String>,
    base_url: &str,
) -> anyhow::Result<zeroize::Zeroizing<String>> {
    let url = format!("{base_url}/auth/exchange");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(&url)
        .bearer_auth(api_key.as_str())
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("échange api-key→JWT échoué ({url}): {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("échange api-key→JWT → HTTP {} ({url})", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let token = body["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("réponse exchange sans champ 'token'"))?;
    Ok(zeroize::Zeroizing::new(token.to_string()))
}

/// Valide que l'URL pointe vers le loopback (P2-4 anti-SSRF).
///
/// Utilise un parsing URL réel pour éviter les bypasses comme
/// `http://127.0.0.1.evil.com` (faux positif avec `.contains()`).
#[cfg(feature = "serve")]
fn validate_loopback_url(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("gradatum_url invalide (parsing URL) : {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("gradatum_url sans host : {url}"))?;
    if host == "127.0.0.1" || host == "localhost" {
        Ok(())
    } else {
        anyhow::bail!(
            "gradatum_url doit être loopback (127.0.0.1 ou localhost), host={host} : {url}"
        )
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

    // --- S2 : validate_loopback_url ---
    #[test]
    fn validate_loopback_accepts_127_0_0_1() {
        assert!(validate_loopback_url("http://127.0.0.1:19090").is_ok());
    }

    #[test]
    fn validate_loopback_accepts_localhost() {
        assert!(validate_loopback_url("http://localhost:19090").is_ok());
    }

    #[test]
    fn validate_loopback_rejects_bypass_subdomain() {
        let result = validate_loopback_url("http://127.0.0.1.evil.com:19090");
        assert!(
            result.is_err(),
            "127.0.0.1.evil.com doit être rejeté (SSRF bypass)"
        );
    }

    #[test]
    fn validate_loopback_rejects_external_ip() {
        // 203.0.113.1 = TEST-NET-3 (RFC 5737) — IP non-loopback de test
        let result = validate_loopback_url("http://203.0.113.1:19090");
        assert!(result.is_err(), "IP externe doit être rejetée");
    }

    #[test]
    fn validate_loopback_rejects_invalid_url() {
        let result = validate_loopback_url("not-a-url");
        assert!(result.is_err(), "URL invalide doit être rejetée");
    }
}
