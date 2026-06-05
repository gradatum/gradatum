//! gradatum-worker — API de bibliothèque pour les tests d'intégration.
//!
//! Expose les modules internes pour permettre leur test depuis `tests/`.
//!
//! # Note compatibility Phase 2
//!
//! Les modules `dispatch`, `curator_loader` et `leader` sont conservés pour
//! compatibilité avec les tests d'intégration existants. Ils ne sont plus
//! utilisés directement par le binaire v0.2.0 qui utilise le Monitor Apalis.

// Modules conservés pour rétro-compatibilité tests d'intégration
// (non utilisés par le binaire v0.2.0 — dispatcher remplacé par Monitor Apalis)
#[allow(dead_code)]
pub mod curator_loader;
#[allow(dead_code)]
pub mod dispatch;
#[allow(dead_code)]
pub mod leader;

pub mod apalis_backend;
pub mod apalis_handlers;
pub mod metrics;
pub mod monitor;
pub mod schedules;

// ── Re-exports pour les tests d'intégration ──────────────────────────────────

/// Construit le [`gradatum_curator::CuratorPipeline`] depuis la section `[curator]`
/// du fichier TOML serveur. Exposé pour les tests d'intégration.
///
/// Délègue à [`curator_loader::build_curator_pipeline`].
pub use curator_loader::build_curator_pipeline;

/// Config curator désérialisée depuis le TOML serveur.
/// Exposée pour les tests d'intégration vérifiant la propagation des champs gating.
pub use curator_loader::WorkerCuratorConfig;

/// Config Apalis (workers + schedules).
pub use monitor::ApalisConfig;

/// Registre Prometheus partagé entre les workers.
pub use metrics::WorkerMetrics;
