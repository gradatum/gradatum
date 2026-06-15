//! # gradatum-warden
//!
//! Network layer L0 for Gradatum: CIDR IP filtering, per-IP rate limiting, and loopback bypass.
//!
//! ## Public components
//!
//! - [`WardenLayer`]: tower `Layer` implementation to mount on an Axum router.
//! - [`WardenConfig`]: complete warden configuration (TOML/JSON serializable).
//! - [`WardenError`]: construction error (invalid configuration).
//! - [`WardenDecision`]: per-request warden decision (observable in tests).
//!
//! ## Loopback bypass guarantee
//!
//! Unlike `tower_governor` (where `error_handler` terminated the chain with `Body::empty()`),
//! the warden always calls `inner.call(req)` for bypass/allow requests.
//! The returned body is the real handler body — never a synthetic empty body.
//!
//! ## Example
//!
//! ```rust,ignore
//! use gradatum_warden::{WardenConfig, WardenLayer};
//!
//! let config = WardenConfig {
//!     enabled: true,
//!     rate_limit_per_minute: 60,
//!     rate_limit_burst: 10,
//!     bypass_loopback: true,
//!     ..WardenConfig::default()
//! };
//! let warden = WardenLayer::new(config).expect("config warden valide");
//! // app.layer(warden)
//! ```

pub mod config;
pub mod error;
pub mod ip;
pub mod layer;
pub mod rate;
pub mod service;

pub use config::WardenConfig;
pub use error::{WardenDecision, WardenError};
pub use layer::WardenLayer;
