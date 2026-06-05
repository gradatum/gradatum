//! Vendoring inline des types partagés (anti-fuite OSS).
//!
//! Ce module contient les types nécessaires portés depuis la bibliothèque
//! partagée privée. En vendorisant ici, la crate `gradatum-gateway` n'a
//! aucune dépendance vers un registre Cargo privé — compatible OSS.
//!
//! Seuls les types réellement utilisés dans ce crate sont inclus.

pub mod chat;
pub mod circuit_breaker;
pub mod embeddings;
pub mod error;
pub mod provider;
pub mod streaming;
