//! Tower Layer implémentant le warden L0.

use std::sync::Arc;

use tower::Layer;

use crate::config::WardenConfig;
use crate::error::WardenError;
use crate::service::{WardenService, WardenState};

/// Layer tower qui insère [`WardenService`] dans la pile de middlewares.
///
/// Construit via [`WardenLayer::new`].
///
/// # Exemple
///
/// ```rust,ignore
/// let warden = WardenLayer::new(WardenConfig::default())?;
/// let app = Router::new()
///     .route("/api/v1/...", ...)
///     .layer(warden);
/// ```
#[derive(Debug, Clone)]
pub struct WardenLayer {
    pub(crate) state: Arc<WardenState>,
}

impl WardenLayer {
    /// Construit un [`WardenLayer`] depuis une [`WardenConfig`].
    ///
    /// # Erreurs
    ///
    /// Retourne [`WardenError::InvalidRateLimit`] si `per_minute == 0` ou `burst == 0`.
    pub fn new(config: WardenConfig) -> Result<Self, WardenError> {
        let config = Arc::new(config);
        let state = Arc::new(WardenState::new(config)?);
        Ok(Self { state })
    }
}

impl<S> Layer<S> for WardenLayer {
    type Service = WardenService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WardenService {
            inner,
            state: self.state.clone(),
        }
    }
}
