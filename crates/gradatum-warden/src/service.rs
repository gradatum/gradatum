//! Service tower implémentant la logique warden.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::ConnectInfo;
use http::{Request, Response, StatusCode};
use tower::Service;

use crate::config::WardenConfig;
use crate::error::WardenDecision;
use crate::ip::ip_allowed;
use crate::rate::PerIpRateLimiter;

/// État partagé du warden entre les instances du service.
#[derive(Debug, Clone)]
pub(crate) struct WardenState {
    pub(crate) config: Arc<WardenConfig>,
    pub(crate) rate_limiter: Option<PerIpRateLimiter>,
}

impl WardenState {
    /// Construit le state depuis la config.
    ///
    /// Retourne une erreur si la config de rate limit est invalide.
    pub(crate) fn new(config: Arc<WardenConfig>) -> Result<Self, crate::error::WardenError> {
        let rate_limiter = if config.enabled {
            Some(PerIpRateLimiter::new(
                config.rate_limit_per_minute,
                config.rate_limit_burst,
            )?)
        } else {
            None
        };
        Ok(Self {
            config,
            rate_limiter,
        })
    }
}

/// Service tower qui applique la logique warden sur chaque requête.
///
/// Générique sur `S` (service inner) et `ReqBody` (type de body HTTP).
///
/// Ordre de décision :
/// 1. Warden désactivé → `Allow` direct.
/// 2. ConnectInfo absent → `Allow` (fail-open — la connexion vient de l'intérieur de tower).
/// 3. `bypass_loopback=true` et IP loopback → `Bypass` (appel direct `inner.call(req)`).
/// 4. Filtre IP CIDR → `DenyIp` (403) si non autorisée.
/// 5. Rate limit → `DenyRateLimit` (429 + retry-after) si dépassé.
/// 6. Sinon → `Allow`.
#[derive(Debug, Clone)]
pub struct WardenService<S> {
    pub(crate) inner: S,
    pub(crate) state: Arc<WardenState>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for WardenService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    ReqBody: Send + 'static,
    ResBody: Default + Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let state = self.state.clone();
        // Cloner le service pour la future — requis par tower convention.
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Warden désactivé → pass-through.
            if !state.config.enabled {
                return inner.call(req).await;
            }

            // Lire l'IP depuis ConnectInfo dans les extensions.
            // ConnectInfo absent = fail-open (logique interne tower, pas une connexion externe).
            let peer_ip: Option<IpAddr> = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());

            let ip = match peer_ip {
                Some(ip) => ip,
                None => {
                    // Pas de ConnectInfo — fail-open (décision Backend documentée :
                    // les appels internes tower sans ConnectInfo ne doivent pas être bloqués).
                    tracing::debug!("warden: ConnectInfo absent — fail-open");
                    return inner.call(req).await;
                }
            };

            // Décision warden.
            let decision = evaluate_decision(&state, ip);

            match decision {
                WardenDecision::Allow | WardenDecision::Bypass => {
                    // Cas critiques : appeler le service inner pour obtenir le vrai body handler.
                    inner.call(req).await
                }
                WardenDecision::DenyIp => {
                    tracing::warn!(ip = %ip, "warden: IP refusée (filtre CIDR)");
                    let resp = Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(ResBody::default())
                        .expect("réponse 403 statique — ne peut pas échouer");
                    Ok(resp)
                }
                WardenDecision::DenyRateLimit => {
                    // Calculer retry-after AVANT de retourner la réponse.
                    let retry_secs = state
                        .rate_limiter
                        .as_ref()
                        .map(|rl| rl.wait_time_secs(ip))
                        .unwrap_or(1);
                    tracing::debug!(ip = %ip, retry_secs, "warden: rate limit dépassé");
                    let resp = Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("retry-after", retry_secs.to_string())
                        .body(ResBody::default())
                        .expect("réponse 429 statique — ne peut pas échouer");
                    Ok(resp)
                }
            }
        })
    }
}

/// Évalue la décision warden pour une IP donnée.
///
/// Ordre : bypass loopback → filtre IP → rate limit → allow.
/// Séparé du service pour faciliter les tests unitaires.
fn evaluate_decision(state: &WardenState, ip: IpAddr) -> WardenDecision {
    // 1. Bypass loopback.
    if state.config.bypass_loopback && ip.is_loopback() {
        return WardenDecision::Bypass;
    }

    // 2. Filtre IP CIDR.
    if !ip_allowed(ip, &state.config.ip_allow, &state.config.ip_deny) {
        return WardenDecision::DenyIp;
    }

    // 3. Rate limit.
    if let Some(rl) = &state.rate_limiter {
        if !rl.check(ip) {
            return WardenDecision::DenyRateLimit;
        }
    }

    WardenDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(config: WardenConfig) -> Arc<WardenState> {
        Arc::new(WardenState::new(Arc::new(config)).expect("config valide"))
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_bypassed_when_enabled() {
        let cfg = WardenConfig {
            bypass_loopback: true,
            ..WardenConfig::default()
        };
        let state = make_state(cfg);
        assert_eq!(
            evaluate_decision(&state, ip("127.0.0.1")),
            WardenDecision::Bypass
        );
    }

    #[test]
    fn loopback_rate_limited_when_bypass_disabled() {
        let cfg = WardenConfig {
            bypass_loopback: false,
            rate_limit_per_minute: 60,
            rate_limit_burst: 1,
            ..WardenConfig::default()
        };
        let state = make_state(cfg);
        // Premier jeton : Allow.
        assert_eq!(
            evaluate_decision(&state, ip("127.0.0.1")),
            WardenDecision::Allow
        );
        // Burst épuisé : DenyRateLimit.
        assert_eq!(
            evaluate_decision(&state, ip("127.0.0.1")),
            WardenDecision::DenyRateLimit
        );
    }

    #[test]
    fn ip_deny_returns_deny_ip() {
        let cfg = WardenConfig {
            bypass_loopback: false,
            ip_deny: vec!["10.0.0.0/8".parse().unwrap()],
            ..WardenConfig::default()
        };
        let state = make_state(cfg);
        assert_eq!(
            evaluate_decision(&state, ip("10.0.0.1")),
            WardenDecision::DenyIp
        );
    }

    #[test]
    fn rate_limit_exceeded_returns_deny_rate_limit() {
        let cfg = WardenConfig {
            bypass_loopback: false,
            rate_limit_per_minute: 60,
            rate_limit_burst: 2,
            ..WardenConfig::default()
        };
        let state = make_state(cfg);
        let test_ip = ip("192.0.2.50");
        // Vider le burst.
        evaluate_decision(&state, test_ip);
        evaluate_decision(&state, test_ip);
        // 3e : dépassé.
        assert_eq!(
            evaluate_decision(&state, test_ip),
            WardenDecision::DenyRateLimit
        );
    }

    #[test]
    fn disabled_warden_allows_all() {
        let cfg = WardenConfig {
            enabled: false,
            ..WardenConfig::default()
        };
        let state = make_state(cfg);
        // evaluate_decision n'est pas appelée quand enabled=false (court-circuit dans call()),
        // mais le rate_limiter est None donc on vérifie indirectement.
        assert!(state.rate_limiter.is_none());
    }
}
