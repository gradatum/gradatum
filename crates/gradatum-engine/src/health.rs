//! État de warm-up et self-report backend du superviseur.
//!
//! [`HealthState`] est thread-safe via `AtomicBool` — clonable via `Arc`.
//! L'endpoint `/health` expose un [`HealthSnapshot`] JSON.
//!
//! ## PIVOT v2 — backend self-report
//!
//! Avec le PIVOT v2 (superviseur `llama-server`), le "backend compilé" n'est plus
//! figé à la construction du binaire Rust — il dépend du sous-process `llama-server`
//! démarré. Le champ `backend` retourne `"llama-server"` (identifiant du runtime réel).
//!
//! ## États
//!
//! - `starting` : le superviseur attend que `llama-server` soit prêt.
//! - `ok` : `llama-server` a répondu HTTP 200 sur `/health`.
//! - `unhealthy` : restart_max épuisé — le fallback gateway prend le relais.
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

/// Snapshot JSON exposé par `/health`.
#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    /// `"ok"` si prêt, `"starting"` si démarrage, `"unhealthy"` si restart_max épuisé.
    pub status: &'static str,
    /// Alias du modèle chargé.
    pub model: String,
    /// Runtime backend — `"llama-server"` (PIVOT v2).
    pub backend: &'static str,
    /// État du warm-up : `"loading"`, `"ready"`, ou `"unhealthy"`.
    pub warm_up_state: &'static str,
}

/// États internes encodés sur u8 pour l'atomique.
mod state {
    pub const STARTING: u8 = 0;
    pub const READY: u8 = 1;
    pub const UNHEALTHY: u8 = 2;
}

/// État de warm-up partagé entre le superviseur et les handlers.
pub struct HealthState {
    model: String,
    state: AtomicU8,
}

/// Backend runtime supervisé — PIVOT v2.
///
/// Retourne `"llama-server"` (le binaire natif llama.cpp supervisé).
/// Distinct d'Ollama — c'est le binaire `llama-server` brut de llama.cpp.
pub const fn compiled_backend() -> &'static str {
    "llama-server"
}

impl HealthState {
    /// Crée un nouvel état de warm-up pour le modèle `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.into(),
            state: AtomicU8::new(state::STARTING),
        }
    }

    /// Passe l'état en `ready` — appelé après que `llama-server` a répondu HTTP 200
    /// sur son endpoint `/health` (wait_ready OK).
    pub fn set_ready(&self) {
        self.state.store(state::READY, Ordering::SeqCst);
    }

    /// Passe l'état en `unhealthy` — appelé quand le restart_max est épuisé.
    ///
    /// Le gateway détecte cet état et bascule sur le fallback.
    pub fn set_unhealthy(&self) {
        self.state.store(state::UNHEALTHY, Ordering::SeqCst);
    }

    /// Retourne `true` si l'état est `ready`.
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::SeqCst) == state::READY
    }

    /// Retourne un snapshot de l'état courant.
    pub fn snapshot(&self) -> HealthSnapshot {
        let s = self.state.load(Ordering::SeqCst);
        let (status, warm_up_state) = match s {
            state::READY => ("ok", "ready"),
            state::UNHEALTHY => ("unhealthy", "unhealthy"),
            _ => ("starting", "loading"),
        };
        HealthSnapshot {
            status,
            model: self.model.clone(),
            backend: compiled_backend(),
            warm_up_state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_transitions_warmup() {
        let h = HealthState::new("qwen3-4b");
        assert_eq!(h.snapshot().warm_up_state, "loading");
        h.set_ready();
        let s = h.snapshot();
        assert_eq!(s.warm_up_state, "ready");
        assert_eq!(s.model, "qwen3-4b");
    }

    #[test]
    fn health_reports_llama_server_backend() {
        // PIVOT v2 : backend = "llama-server" (plus de feature compile-time)
        assert_eq!(compiled_backend(), "llama-server");
        assert_eq!(HealthState::new("x").snapshot().backend, "llama-server");
    }

    #[test]
    fn health_status_field() {
        let h = HealthState::new("test");
        assert_eq!(h.snapshot().status, "starting");
        h.set_ready();
        assert_eq!(h.snapshot().status, "ok");
    }

    #[test]
    fn health_unhealthy_state() {
        let h = HealthState::new("test");
        h.set_unhealthy();
        let s = h.snapshot();
        assert_eq!(s.status, "unhealthy");
        assert_eq!(s.warm_up_state, "unhealthy");
        assert!(!h.is_ready(), "unhealthy ≠ ready");
    }

    #[test]
    fn health_is_ready_only_when_ready() {
        let h = HealthState::new("test");
        assert!(!h.is_ready(), "starting n'est pas ready");
        h.set_ready();
        assert!(h.is_ready(), "ready est ready");
        h.set_unhealthy();
        assert!(!h.is_ready(), "unhealthy n'est pas ready");
    }
}
