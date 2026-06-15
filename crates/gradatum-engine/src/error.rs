//! Error taxonomy for `gradatum-engine`.
//!
//! Each variant exposes an HTTP status code via `status()` and a stable label via
//! `kind()` — used in metrics and logs. The HTTP mapping follows REST conventions
//! (503 = service unavailable, 504 = timeout, 400 = bad request, 500 = internal error).
use thiserror::Error;

/// Inference engine errors.
///
/// All variants are convertible to an HTTP response via `status()`.
/// The `kind()` label is stable (used in Prometheus and logs) — never rename
/// without migrating dashboards.
#[derive(Debug, Error)]
pub enum EngineError {
    /// GGUF model load failure (invalid path, OOM, corrupted format).
    #[error("model load: {0}")]
    ModelLoad(String),

    /// Error during inference (decode failure, sampler, FFI).
    #[error("inference: {0}")]
    Inference(String),

    /// Configurable timeout exceeded (`timeout_secs` in `EngineConfig`).
    /// The gateway may switch to its fallback on this HTTP 504 response.
    #[error("timeout")]
    Timeout,

    /// Insufficient memory to allocate the context or KV-cache.
    #[error("out of memory")]
    Oom,

    /// Invalid request (malformed body, input too long, `max_tokens` out of bounds).
    #[error("bad request: {0}")]
    BadRequest(String),
}

impl EngineError {
    /// Returns the HTTP status code associated with this error.
    ///
    /// - `ModelLoad` / `Inference` → 500 Internal Server Error
    /// - `Timeout` → 504 Gateway Timeout (allows the gateway to trigger its fallback)
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

    /// Returns the stable label for metrics and logs.
    ///
    /// Values: `"model_load"`, `"inference"`, `"timeout"`, `"oom"`, `"bad_request"`.
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
