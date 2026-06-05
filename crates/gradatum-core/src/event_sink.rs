//! Contrat d'émission d'événements engine, agnostique du transport.
//!
//! Anti-cycle : `gradatum-engine` dépend de ce trait, l'impl HTTP (`HttpEventSink`)
//! est câblée au binaire. `gradatum-core` ne dépend d'aucun crate réseau.
//!
//! ## Invariant de confidentialité
//!
//! Les variants de `EngineEvent` ne doivent **jamais** contenir le contenu des
//! prompts ou des réponses générées (P2-2). Seules les métadonnées structurelles
//! (route, modèle, latence) sont autorisées.
use async_trait::async_trait;
use std::sync::Mutex;

/// Événement structurel émis par une instance `gradatum-engine`.
///
/// `#[non_exhaustive]` : nouveaux variants ajoutés sans breaking change (P2-6).
///
/// # Invariant de confidentialité
/// Aucun variant ne doit exposer le contenu d'un prompt ou d'une réponse.
/// Les tests doivent vérifier que seuls les événements `RequestServed`
/// sont postés vers l'event-log ; les événements de lifecycle restent locaux.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// Moteur démarré, port annoncé.
    EngineStarted {
        /// Alias du modèle chargé.
        model: String,
        /// Port TCP d'écoute.
        port: u16,
    },
    /// Modèle chargé en mémoire (warm-up non encore terminé).
    ModelLoaded {
        /// Alias du modèle chargé.
        model: String,
    },
    /// Warm-up complet — le moteur peut servir des requêtes.
    WarmUpComplete {
        /// Alias du modèle.
        model: String,
    },
    /// Requête traitée avec succès — seul variant posté vers l'event-log.
    ///
    /// `provider` = alias gateway (ex. `"engine-curator"`).
    /// `model` = nom du modèle chargé (ex. `"qwen3-4b"`).
    RequestServed {
        /// Route HTTP (ex. `/v1/chat/completions`).
        route: String,
        /// Alias du modèle.
        model: String,
        /// Provider gateway (ex. `"engine-curator"`).
        provider: String,
        /// Latence end-to-end en millisecondes.
        latency_ms: u64,
    },
    /// Erreur interne du moteur (non fatale).
    EngineError {
        /// Catégorie d'erreur (ex. `"inference"`, `"timeout"`).
        kind: String,
    },
    /// Le moteur est en cours d'arrêt.
    EngineStopping {
        /// Alias du modèle en cours d'arrêt.
        model: String,
    },
}

/// Émetteur d'événements engine.
///
/// L'implémentation réelle est `HttpEventSink` (crate `gradatum-engine`).
/// L'implémentation de test est `InMemorySink`.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Émet un événement. Best-effort — les implémentations ne doivent
    /// jamais paniquer ni bloquer indéfiniment.
    async fn emit(&self, event: EngineEvent);
}

/// Sink de test : capture les événements en mémoire.
///
/// Usage : tests unitaires et d'intégration uniquement. Thread-safe.
#[derive(Default)]
pub struct InMemorySink {
    events: Mutex<Vec<EngineEvent>>,
}

impl InMemorySink {
    /// Retourne une copie de tous les événements capturés.
    pub fn snapshot(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .expect("InMemorySink: lock poison — ne devrait pas arriver en test")
            .clone()
    }
}

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
        assert!(events
            .iter()
            .all(|e| !matches!(e, EngineEvent::RequestServed { .. })));
    }
}
