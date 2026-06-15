//! Tower [`Layer`] implementing the warden L0.

use std::sync::Arc;

use tower::Layer;

use crate::config::WardenConfig;
use crate::error::WardenError;
use crate::service::{WardenService, WardenState};

/// Tower layer that inserts [`WardenService`] into the middleware stack.
///
/// Constructed via [`WardenLayer::new`].
///
/// # Example
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
    /// Builds a [`WardenLayer`] from a [`WardenConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`WardenError::InvalidRateLimit`] if `per_minute == 0` or `burst == 0`.
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
