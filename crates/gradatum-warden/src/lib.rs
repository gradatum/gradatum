//! # gradatum-warden
//!
//! Couche réseau L0 pour Gradatum : filtrage IP CIDR + rate limiting per-IP + bypass loopback.
//!
//! ## Composants publics
//!
//! - [`WardenLayer`] : implémentation tower [`Layer`] à monter sur un routeur Axum.
//! - [`WardenConfig`] : configuration complète du warden (sérialisable TOML/JSON).
//! - [`WardenError`] : erreur de construction (config invalide).
//! - [`WardenDecision`] : décision interne par requête (observable dans les tests).
//!
//! ## Garantie bypass loopback
//!
//! Contrairement à `tower_governor` (où `error_handler` terminait la chaîne avec `Body::empty()`),
//! le warden appelle toujours `inner.call(req)` pour les requêtes bypass/allow.
//! Le body retourné est celui du handler métier réel — jamais un body vide synthétique.
//!
//! ## Exemple
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
