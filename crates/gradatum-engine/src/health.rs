//! Warm-up state and backend self-reporting for the supervisor.
//!
//! [`HealthState`] is thread-safe via an atomic — clonable via `Arc`.
//! The `/health` endpoint exposes a [`HealthSnapshot`] JSON.
//!
//! ## Backend self-report
//!
//! With the `llama-server` supervisor, the "compiled backend" is no longer fixed at
//! Rust binary build time — it depends on the spawned `llama-server` subprocess.
//! The `backend` field returns `"llama-server"` (the actual runtime identifier).
//!
//! ## States
//!
//! - `starting`: the supervisor is waiting for `llama-server` to be ready.
//! - `ok`: `llama-server` responded HTTP 200 on `/health`.
//! - `unhealthy`: restart budget exhausted — the gateway fallback takes over.
//!
//! ## Event-log telemetry state
//!
//! Orthogonal to the warm-up state above: [`TelemetryStatus`] reports whether the
//! event-log sink is active or has *folded* onto the inert `NoopEventSink`, and — when
//! folded — *why*. A folded engine still serves inference correctly, so it is **not**
//! `unhealthy`; the telemetry state is a separate, dedicated field
//! ([`HealthSnapshot::event_log`]) so a probe can see a silent fallback without reading
//! the process logs.
//!
//! This state is decided **once at startup** and never changes without a restart: an
//! engine that folds onto the inert sink has no runtime code path to re-establish the
//! event-log connection (the lazy JWT refresh only exists inside a *live* `HttpEventSink`,
//! which is only built when the initial exchange succeeds). This one-way property is why
//! a stale credential kept the event-log silent for ten days before an audit found it.
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

/// State of the engine's event-log telemetry: active, or folded onto the inert sink
/// (with the reason for the fold).
///
/// A folded engine keeps serving inference — this is orthogonal to
/// [`HealthSnapshot::status`]. The three fold reasons demand different operational
/// responses, so they are distinct variants rather than a single "disabled" state:
///
/// - [`NotConfigured`](Self::NotConfigured): nominal in dev/test — no action.
/// - [`Unreachable`](Self::Unreachable): transient — may recover on the next restart.
/// - [`Unauthorized`](Self::Unauthorized): an identity/credential problem that will
///   **never** recover on its own and requires human action (this is the case that
///   lasted ten days).
///
/// Decided at startup and immutable for the process lifetime — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryStatus {
    /// Event-log active — events are being posted to the gradatum server.
    Active,
    /// `gradatum_url` not configured — event-log intentionally disabled (nominal in dev/test).
    NotConfigured,
    /// Server unreachable at startup (transport-level failure) — transient, may recover on restart.
    Unreachable,
    /// The api-key→JWT exchange was refused with HTTP 401 — an identity problem that
    /// requires human action and will not recover without intervention + restart.
    Unauthorized,
    /// The exchange failed for another reason (non-401 HTTP status, or a malformed
    /// response) — kept distinct so a non-401 failure is never mislabelled as transient.
    Failed,
}

impl TelemetryStatus {
    /// Stable label exposed in the `/health` JSON (`event_log` field), and safe for
    /// logs and metrics. Never rename without migrating any consumer that matches on it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            TelemetryStatus::Active => "active",
            TelemetryStatus::NotConfigured => "not_configured",
            TelemetryStatus::Unreachable => "folded_unreachable",
            TelemetryStatus::Unauthorized => "folded_unauthorized",
            TelemetryStatus::Failed => "folded_error",
        }
    }

    /// Returns `true` only when the event-log is active (not folded onto the inert sink).
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, TelemetryStatus::Active)
    }
}

/// JSON snapshot exposed by `/health`.
// `#[non_exhaustive]` (F-245) : la structure n'est plus constructible par littéral
// chez un consommateur externe — tout ajout de champ futur est additif.
#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct HealthSnapshot {
    /// `"ok"` when ready, `"starting"` during startup, `"unhealthy"` when the restart budget is exhausted.
    pub status: &'static str,
    /// Alias of the loaded model.
    pub model: String,
    /// Backend runtime — `"llama-server"`.
    pub backend: &'static str,
    /// Warm-up state: `"loading"`, `"ready"`, or `"unhealthy"`.
    pub warm_up_state: &'static str,
    /// Event-log telemetry state — the [`TelemetryStatus::label`] value. Distinct from
    /// `status`: a folded event-log (`"folded_unauthorized"`, `"folded_unreachable"`,
    /// `"not_configured"`, `"folded_error"`) leaves `status` at `"ok"` because the engine
    /// still serves inference. `"active"` means events are being posted.
    pub event_log: &'static str,
}

/// Internal states encoded as `u8` for the atomic.
mod state {
    pub const STARTING: u8 = 0;
    pub const READY: u8 = 1;
    pub const UNHEALTHY: u8 = 2;
}

/// Warm-up state shared between the supervisor and the handlers.
pub struct HealthState {
    model: String,
    state: AtomicU8,
    /// Event-log telemetry state, decided at startup (see [`TelemetryStatus`]). Immutable:
    /// a folded engine cannot re-establish its event-log without a restart.
    telemetry: TelemetryStatus,
}

/// Returns the supervised runtime backend identifier.
///
/// Returns `"llama-server"` (the native llama.cpp binary being supervised).
/// Distinct from Ollama — this is the raw `llama-server` binary from llama.cpp.
pub const fn compiled_backend() -> &'static str {
    "llama-server"
}

impl HealthState {
    /// Creates a new warm-up state for the given `model`, with the event-log telemetry
    /// left at [`TelemetryStatus::NotConfigured`].
    ///
    /// This is the zero-telemetry constructor: it makes no claim about the event-log
    /// channel. `NotConfigured` is the honest default — it says "no one told me whether
    /// the event-log is live", which is exactly true for a caller that constructs a
    /// `HealthState` without running the api-key→JWT exchange. Defaulting to
    /// [`Active`](TelemetryStatus::Active) would be a lie and would re-introduce the
    /// silent false-green this default avoids. A caller that knows the telemetry outcome
    /// must use [`new_with_telemetry`](Self::new_with_telemetry).
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self::new_with_telemetry(model, TelemetryStatus::NotConfigured)
    }

    /// Creates a new warm-up state for the given `model` and event-log `telemetry` state.
    ///
    /// `telemetry` is decided by the binary when it builds the event sink (active if the
    /// api-key→JWT exchange succeeded, otherwise the fold reason) and is immutable
    /// afterwards.
    #[must_use]
    pub fn new_with_telemetry(model: &str, telemetry: TelemetryStatus) -> Self {
        Self {
            model: model.into(),
            state: AtomicU8::new(state::STARTING),
            telemetry,
        }
    }

    /// Transitions to the `ready` state — called after `llama-server` responds HTTP 200
    /// on its `/health` endpoint.
    pub fn set_ready(&self) {
        self.state.store(state::READY, Ordering::SeqCst);
    }

    /// Transitions to the `unhealthy` state — called when the restart budget is exhausted.
    ///
    /// The gateway detects this state and switches to its fallback.
    pub fn set_unhealthy(&self) {
        self.state.store(state::UNHEALTHY, Ordering::SeqCst);
    }

    /// Returns `true` if the state is `ready`.
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::SeqCst) == state::READY
    }

    /// Returns a snapshot of the current state.
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
            event_log: self.telemetry.label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_transitions_warmup() {
        let h = HealthState::new_with_telemetry("qwen3-4b", TelemetryStatus::Active);
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
        assert_eq!(
            HealthState::new_with_telemetry("x", TelemetryStatus::Active)
                .snapshot()
                .backend,
            "llama-server"
        );
    }

    #[test]
    fn health_status_field() {
        let h = HealthState::new_with_telemetry("test", TelemetryStatus::Active);
        assert_eq!(h.snapshot().status, "starting");
        h.set_ready();
        assert_eq!(h.snapshot().status, "ok");
    }

    #[test]
    fn health_unhealthy_state() {
        let h = HealthState::new_with_telemetry("test", TelemetryStatus::Active);
        h.set_unhealthy();
        let s = h.snapshot();
        assert_eq!(s.status, "unhealthy");
        assert_eq!(s.warm_up_state, "unhealthy");
        assert!(!h.is_ready(), "unhealthy ≠ ready");
    }

    #[test]
    fn health_is_ready_only_when_ready() {
        let h = HealthState::new_with_telemetry("test", TelemetryStatus::Active);
        assert!(!h.is_ready(), "starting n'est pas ready");
        h.set_ready();
        assert!(h.is_ready(), "ready est ready");
        h.set_unhealthy();
        assert!(!h.is_ready(), "unhealthy n'est pas ready");
    }

    #[test]
    fn new_defaults_telemetry_to_not_configured() {
        // La signature historique `new(&str)` ne prétend rien sur l'event-log : elle
        // retombe sur NotConfigured (« personne ne m'a dit »), jamais sur Active. Un
        // défaut Active serait un mensonge et rejouerait le faux vert que F-205 ferme.
        let h = HealthState::new("qwen3-4b");
        assert_eq!(
            h.snapshot().event_log,
            "not_configured",
            "new(&str) doit retomber sur NotConfigured, pas sur un état actif mensonger"
        );
        // Le reste du comportement est identique au constructeur explicite.
        assert_eq!(h.snapshot().status, "starting");
        assert_eq!(h.snapshot().model, "qwen3-4b");
    }

    #[test]
    fn telemetry_labels_are_distinct_and_stable() {
        // Chaque motif a un label distinct — un consommateur peut discriminer les 3
        // situations (non configuré / injoignable / 401) sans lire les journaux.
        assert_eq!(TelemetryStatus::Active.label(), "active");
        assert_eq!(TelemetryStatus::NotConfigured.label(), "not_configured");
        assert_eq!(TelemetryStatus::Unreachable.label(), "folded_unreachable");
        assert_eq!(TelemetryStatus::Unauthorized.label(), "folded_unauthorized");
        assert_eq!(TelemetryStatus::Failed.label(), "folded_error");
        // Seul Active est actif.
        assert!(TelemetryStatus::Active.is_active());
        for folded in [
            TelemetryStatus::NotConfigured,
            TelemetryStatus::Unreachable,
            TelemetryStatus::Unauthorized,
            TelemetryStatus::Failed,
        ] {
            assert!(!folded.is_active(), "{folded:?} ne doit pas être actif");
        }
    }

    #[test]
    fn folded_unauthorized_engine_stays_ready_to_serve() {
        // Cœur de F-205 : un moteur dont l'échange a échoué en 401 expose le motif
        // repli-401 dans event_log, MAIS reste prêt à servir (status "ok", is_ready).
        // La télémétrie repliée ne doit JAMAIS faire passer le moteur en unhealthy —
        // ce serait remplacer un faux vert par un faux rouge.
        let h = HealthState::new_with_telemetry("qwen3-4b", TelemetryStatus::Unauthorized);
        // Avant warm-up : starting, mais le motif de repli est déjà visible.
        assert_eq!(h.snapshot().event_log, "folded_unauthorized");
        assert_eq!(h.snapshot().status, "starting");
        // Après warm-up : le moteur sert (ok / ready), et event_log reste au motif 401.
        h.set_ready();
        let s = h.snapshot();
        assert_eq!(
            s.status, "ok",
            "un event-log replié ne rend PAS le moteur unhealthy"
        );
        assert_eq!(s.warm_up_state, "ready");
        assert!(
            h.is_ready(),
            "le moteur reste prêt à servir malgré la télémétrie repliée"
        );
        assert_eq!(
            s.event_log, "folded_unauthorized",
            "le motif 401 doit rester lisible dans la surface de santé"
        );
    }

    #[test]
    fn event_log_is_orthogonal_to_status() {
        // event_log et status varient indépendamment : un moteur unhealthy (budget de
        // redémarrage épuisé) avec télémétrie active, et un moteur ok avec télémétrie
        // repliée, sont tous deux représentables.
        let active_but_unhealthy = HealthState::new_with_telemetry("m", TelemetryStatus::Active);
        active_but_unhealthy.set_unhealthy();
        let s = active_but_unhealthy.snapshot();
        assert_eq!(s.status, "unhealthy");
        assert_eq!(s.event_log, "active");

        let ok_but_folded = HealthState::new_with_telemetry("m", TelemetryStatus::Unreachable);
        ok_but_folded.set_ready();
        let s = ok_but_folded.snapshot();
        assert_eq!(s.status, "ok");
        assert_eq!(s.event_log, "folded_unreachable");
    }
}
