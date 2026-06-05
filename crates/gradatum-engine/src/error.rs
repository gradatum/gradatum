//! Taxonomie d'erreurs `gradatum-engine`.
//!
//! Chaque variant expose un code HTTP via `status()` et un label stable via
//! `kind()` — utilisé dans les métriques et les logs. Le mapping HTTP est
//! aligné sur les conventions REST (503 = service unavailable, 504 = timeout,
//! 400 = bad request, 500 = internal error).
use thiserror::Error;

/// Erreurs du moteur d'inférence.
///
/// Tous les variants sont convertibles en réponse HTTP via `status()`.
/// Le label `kind()` est stable (utilisé dans prometheus + logs) — ne jamais
/// renommer sans migration de dashboards.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Échec de chargement du modèle GGUF (chemin invalide, OOM, format corrompu).
    #[error("model load: {0}")]
    ModelLoad(String),

    /// Erreur pendant l'inférence (decode échoué, sampler, FFI).
    #[error("inference: {0}")]
    Inference(String),

    /// Dépassement du timeout configurable (`timeout_secs` dans `EngineConfig`).
    /// Le gateway peut basculer sur le fallback après ce code 504.
    #[error("timeout")]
    Timeout,

    /// Mémoire insuffisante pour allouer le contexte ou le KV-cache.
    #[error("out of memory")]
    Oom,

    /// Requête invalide (corps malformé, input trop long, max_tokens hors borne).
    #[error("bad request: {0}")]
    BadRequest(String),
}

impl EngineError {
    /// Code de statut HTTP associé à cette erreur.
    ///
    /// - `ModelLoad` / `Inference` → 500 Internal Server Error
    /// - `Timeout` → 504 Gateway Timeout (permet au gateway de déclencher le fallback)
    /// - `Oom` → 503 Service Unavailable
    /// - `BadRequest` → 400 Bad Request
    pub fn status(&self) -> u16 {
        match self {
            EngineError::ModelLoad(_) | EngineError::Inference(_) => 500,
            EngineError::Timeout => 504,
            EngineError::Oom => 503,
            EngineError::BadRequest(_) => 400,
        }
    }

    /// Label stable pour les métriques et logs.
    ///
    /// Valeurs : `"model_load"`, `"inference"`, `"timeout"`, `"oom"`, `"bad_request"`.
    pub fn kind(&self) -> &'static str {
        match self {
            EngineError::ModelLoad(_) => "model_load",
            EngineError::Inference(_) => "inference",
            EngineError::Timeout => "timeout",
            EngineError::Oom => "oom",
            EngineError::BadRequest(_) => "bad_request",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_maps_to_http_status() {
        assert_eq!(EngineError::Timeout.status(), 504);
        assert_eq!(EngineError::Oom.status(), 503);
        assert_eq!(EngineError::BadRequest("x".into()).status(), 400);
        assert_eq!(EngineError::ModelLoad("x".into()).status(), 500);
        assert_eq!(EngineError::Inference("x".into()).status(), 500);
    }

    #[test]
    fn error_kind_label_stable() {
        assert_eq!(EngineError::Timeout.kind(), "timeout");
        assert_eq!(EngineError::Oom.kind(), "oom");
        assert_eq!(EngineError::BadRequest("".into()).kind(), "bad_request");
        assert_eq!(EngineError::ModelLoad("".into()).kind(), "model_load");
        assert_eq!(EngineError::Inference("".into()).kind(), "inference");
    }

    #[test]
    fn error_display_contains_message() {
        let e = EngineError::BadRequest("input trop long".into());
        assert!(e.to_string().contains("input trop long"));
        let t = EngineError::Timeout;
        assert!(t.to_string().contains("timeout"));
    }
}
