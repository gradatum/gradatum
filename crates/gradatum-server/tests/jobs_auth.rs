//! Tests V4 auth conditionnelle jobs — RETIRÉS Phase 4.2bis (P1-4).
//!
//! `require_jwt_jobs_endpoint` était un flag fantôme : présent dans config + AppState
//! mais jamais lu par les handlers `jobs_v2.rs` → sécurité dangereuse par fausse assurance.
//!
//! Décision P1-4 (code audit Phase 4.2 / Backend Phase 4.2bis) :
//! - Flag retiré de `AuthConfig` + `AppState`.
//! - v0.2.0 Bronze invariant réseau privé : endpoints jobs ouverts sans auth conditionnelle.
//! - Auth granulaire F-45 multi-user JWT planifiée v1.0.0 Gold.
//! - Spec §11 E-21 documente cet écart.
//!
//! Tous les scénarios de ce module sont devenus sans objet après le retrait du flag.
