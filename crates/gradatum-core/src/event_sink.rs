//! Transport-agnostic engine event emission contract.
//!
//! Anti-cycle: `gradatum-engine` depends on this trait; the HTTP implementation
//! (`HttpEventSink`) is wired at the binary level. `gradatum-core` has no network dependencies.
//!
//! ## Confidentiality invariant
//!
//! Variants of `EngineEvent` must **never** contain prompt content or generated responses.
//! Only structural metadata (route, model, latency) is permitted.
use async_trait::async_trait;
#[cfg(any(test, feature = "test-utils"))]
use std::sync::Mutex;

/// Structural event emitted by a `gradatum-engine` instance.
///
/// `#[non_exhaustive]`: new variants may be added without a breaking change.
///
/// # Confidentiality invariant
/// No variant may expose prompt content or generated responses.
/// Tests must verify that only `RequestServed` events are posted to the event-log;
/// lifecycle events remain local.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// Engine started, listening port announced.
    EngineStarted {
        /// Loaded model alias.
        model: String,
        /// TCP listening port.
        port: u16,
    },
    /// Model loaded into memory (warm-up not yet complete).
    ModelLoaded {
        /// Loaded model alias.
        model: String,
    },
    /// Warm-up complete — the engine can serve requests.
    WarmUpComplete {
        /// Model alias.
        model: String,
    },
    /// Request served — the only variant posted to the event-log.
    ///
    /// `provider` = gateway alias (e.g. `"engine-curator"`).
    /// `model` = name of the loaded model (e.g. `"qwen3-4b"`).
    /// `status_code` = actual HTTP status code returned by `llama-server` (e.g. 200, 400, 503).
    RequestServed {
        /// HTTP route (e.g. `/v1/chat/completions`).
        route: String,
        /// Model alias.
        model: String,
        /// Gateway provider (e.g. `"engine-curator"`).
        provider: String,
        /// End-to-end latency in milliseconds.
        latency_ms: u64,
        /// Actual HTTP status code returned by the downstream llama-server.
        ///
        /// Never hard-coded to 200 — reflects the observable status of the downstream request.
        status_code: u16,
    },
    /// Non-fatal internal engine error.
    EngineError {
        /// Error category (e.g. `"inference"`, `"timeout"`).
        kind: String,
    },
    /// Engine is shutting down.
    EngineStopping {
        /// Alias of the model being stopped.
        model: String,
    },
}

/// Engine event emitter.
///
/// The production implementation is `HttpEventSink` (crate `gradatum-engine`).
/// The test implementation is `InMemorySink`.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Emits an event. Best-effort — implementations must never panic or block indefinitely.
    async fn emit(&self, event: EngineEvent);
}

/// No-op sink: silently discards all events.
///
/// Used as a fallback when the gradatum server is unreachable —
/// preserves engine startup without blocking inference (best-effort degraded mode).
#[derive(Default)]
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&self, _event: EngineEvent) {
        // Intentionally empty — best-effort degraded mode.
    }
}

/// In-memory sink: captures events for inspection.
///
/// For unit and integration tests **only**. Thread-safe.
/// In production, use [`NoopEventSink`] as the fallback.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Default)]
pub struct InMemorySink {
    events: Mutex<Vec<EngineEvent>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl InMemorySink {
    /// Returns a snapshot copy of all captured events.
    pub fn snapshot(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .expect("InMemorySink: lock poison — ne devrait pas arriver en test")
            .clone()
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl EventSink for InMemorySink {
    async fn emit(&self, event: EngineEvent) {
        self.events
            .lock()
            .expect("InMemorySink: lock poison — ne devrait pas arriver en test")
            .push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_sink_captures_events() {
        let sink = InMemorySink::default();
        sink.emit(EngineEvent::ModelLoaded {
            model: "qwen3-4b".into(),
        })
        .await;
        sink.emit(EngineEvent::RequestServed {
            route: "/v1/chat/completions".into(),
            model: "qwen3-4b".into(),
            provider: "engine-curator".into(),
            latency_ms: 42,
            status_code: 200,
        })
        .await;
        let got = sink.snapshot();
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], EngineEvent::ModelLoaded { .. }));
        assert!(matches!(
            got[1],
            EngineEvent::RequestServed { latency_ms: 42, .. }
        ));
    }

    #[tokio::test]
    async fn in_memory_sink_empty_by_default() {
        let sink = InMemorySink::default();
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn engine_event_non_exhaustive_lifecycle() {
        // Vérifie que les variants de lifecycle sont distincts de RequestServed
        let sink = InMemorySink::default();
        sink.emit(EngineEvent::EngineStarted {
            model: "test".into(),
            port: 11435,
        })
        .await;
        sink.emit(EngineEvent::WarmUpComplete {
            model: "test".into(),
        })
        .await;
        sink.emit(EngineEvent::EngineStopping {
            model: "test".into(),
        })
        .await;
        let events = sink.snapshot();
        assert_eq!(events.len(), 3);
        // Aucun RequestServed parmi les lifecycle events
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::RequestServed { .. }))
        );
    }
}
