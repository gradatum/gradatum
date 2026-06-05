//! Module helpers partagés pour la suite v1-parity-tests.
//!
//! # Modules disponibles
//!
//! - [`diff_json_strip_tenant`] — comparaison JSON avec suppression des champs
//!   gradatum-only (tenant_id, _gradatum_*) pour la parité L2 avec le legacy vault v1.6.2.
//!

pub mod diff_json_strip_tenant;

// Ré-exporter les fonctions publiques au niveau du module helpers pour ergonomie.
pub use diff_json_strip_tenant::{diff_json, strip_tenant};
