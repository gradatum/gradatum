//! In-memory fleet service registry.
//!
//! A process-lifetime store of the fleet members that have announced themselves via a
//! heartbeat. It holds no network surface of its own: the write path (ingress endpoint +
//! clients in each binary) is **P6b**; P6a delivers the store, its validation guards, and the
//! loopback read projection.
//!
//! ## Expiration — lazy, monotonic
//!
//! Liveness is decided by a **monotonic** clock ([`std::time::Instant`]), immune to wall-clock
//! jumps. On every read ([`RegistryStore::report`]), entries whose last heartbeat is older than
//! the TTL are dropped in place (lazy expiration): a member that stops beating disappears from
//! the projection within one TTL, without a background sweeper.
//!
//! ## Bounds (defense in depth)
//!
//! Two independent guards, both enforced at [`RegistryStore::register`] — the single write
//! boundary reused by the P6b ingress:
//!
//! 1. **Allowlist** ([`ALLOWED_SERVICES`]): the fleet is a fixed, closed set. A `service`
//!    outside it is rejected — an attacker cannot inflate the registry with arbitrary names.
//! 2. **Field caps** ([`MAX_FIELD_LEN`]) + **entry cap** ([`MAX_ENTRIES`]): bound the memory a
//!    single announcement (and the map as a whole) can consume, even though the allowlist
//!    already caps the distinct-key count.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use gradatum_dto::{RegistryEntry, RegistryReport};

// Le write-boundary (`register`/`validate`) et ses gardes (allowlist, caps, erreurs) sont
// consommés par l'ingress P6b (`registry_ingress::handle_heartbeat`) côté binaire ET par les
// tests. Seul `ttl()` reste sans appelant binaire (lu par les tests + un futur diagnostic) et
// garde donc son `allow(dead_code)` ciblé, aligné sur la convention `state::VaultRegistry`.

/// Closed allowlist of fleet service names accepted by the registry.
///
/// The parc is fixed: mutating this set is a deliberate code change, never runtime input.
/// A heartbeat carrying any other `service` is rejected at registration
/// ([`RegistryError::UnknownService`]).
pub const ALLOWED_SERVICES: [&str; 6] = [
    "server",
    "worker",
    "gateway",
    "engine-agent-main",
    "engine-curator",
    "engine-embed",
];

/// Maximum byte length of a heartbeat text field (`version`, `build_sha`) — DoS cap.
pub const MAX_FIELD_LEN: usize = 128;

/// Hard cap on the number of distinct entries.
///
/// The allowlist already bounds distinct keys to [`ALLOWED_SERVICES`]`.len()`; this cap is a
/// second, independent guard (defense in depth) should the allowlist ever widen.
pub const MAX_ENTRIES: usize = 32;

/// Default heartbeat TTL: an entry unrefreshed for longer is dropped on read.
///
/// 45 s ≫ the 15 s heartbeat cadence (tolerates two missed beats) and ≪ the external
/// monitor's 30 min watchdog window, so a dead member surfaces long before the external monitor.
pub const DEFAULT_TTL: Duration = Duration::from_secs(45);

/// One registered member, as held in the store.
///
/// `last_seen` is the **monotonic** liveness clock (drives TTL). `last_seen_ms` is the wall
/// clock at the same instant, carried through to the read projection for display only.
#[derive(Debug, Clone)]
pub struct Registration {
    /// Fleet service name — already allowlisted when this value reaches the map.
    pub service: String,
    /// Binary version last announced.
    pub version: String,
    /// Build commit SHA last announced (public), or `None`.
    pub build_sha: Option<String>,
    /// OS process id last announced.
    pub pid: u32,
    /// Monotonic instant of the last heartbeat — the TTL reference (never serialized).
    pub last_seen: Instant,
    /// Wall-clock epoch-ms of the last heartbeat — projection display only.
    pub last_seen_ms: i64,
}

/// Rejection reasons at the registry write boundary (fail-closed).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistryError {
    /// `service` is not in [`ALLOWED_SERVICES`].
    #[error("unknown service '{0}': not in the fleet allowlist")]
    UnknownService(String),
    /// A text field exceeds [`MAX_FIELD_LEN`].
    #[error("field '{field}' exceeds the maximum length of {max} bytes")]
    FieldTooLong {
        /// Name of the offending field (`"version"` or `"build_sha"`).
        field: &'static str,
        /// The enforced maximum.
        max: usize,
    },
    /// The map is full ([`MAX_ENTRIES`]) and the heartbeat targets a not-yet-known service.
    #[error("registry capacity exceeded (max {0} entries)")]
    CapacityExceeded(usize),
}

/// Process-lifetime registry of living fleet members.
///
/// Interior mutability via `std::sync::RwLock` (not Tokio): every critical section is
/// synchronous and short (map insert / retain / clone-out) — no lock is ever held across an
/// `.await` (`anti-lock-across-await` / ADN 2). Shared as `Arc<RegistryStore>` between the
/// (future) ingress handler and the read endpoint.
pub struct RegistryStore {
    entries: RwLock<HashMap<String, Registration>>,
    ttl: Duration,
    /// Read by `register` (write-boundary, wired to the P6b ingress).
    max_entries: usize,
}

impl RegistryStore {
    /// Creates an empty registry with the given heartbeat TTL and the default entry cap.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            max_entries: MAX_ENTRIES,
        }
    }

    /// The configured heartbeat TTL.
    #[must_use]
    #[allow(dead_code)] // Consommé par les tests + le futur ingress P6b (pas d'appelant bin).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Write guard, resilient to poison (a poisoned lock never blocks the read path).
    ///
    /// No panic is reachable under the guard (no `.await`, no panicking op), so poison is in
    /// practice inaccessible; the recovery is defense in depth (mirrors `state::VaultRegistry`).
    fn write_entries(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Registration>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Validates a heartbeat against the allowlist and field caps — fail-closed.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::UnknownService`] if `service` is not in [`ALLOWED_SERVICES`].
    /// - [`RegistryError::FieldTooLong`] if `version` or `build_sha` exceeds [`MAX_FIELD_LEN`].
    fn validate(
        service: &str,
        version: &str,
        build_sha: Option<&str>,
    ) -> Result<(), RegistryError> {
        if !ALLOWED_SERVICES.contains(&service) {
            return Err(RegistryError::UnknownService(service.to_owned()));
        }
        if version.len() > MAX_FIELD_LEN {
            return Err(RegistryError::FieldTooLong {
                field: "version",
                max: MAX_FIELD_LEN,
            });
        }
        if let Some(sha) = build_sha
            && sha.len() > MAX_FIELD_LEN
        {
            return Err(RegistryError::FieldTooLong {
                field: "build_sha",
                max: MAX_FIELD_LEN,
            });
        }
        Ok(())
    }

    /// Registers (or refreshes) a member — the single write boundary, reused by the P6b ingress.
    ///
    /// Validates against the allowlist and field caps, enforces the entry cap, then inserts or
    /// overwrites the entry keyed by `service`. A repeat heartbeat from a known member only
    /// refreshes its clock/metadata; it never counts against the cap.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::UnknownService`] / [`RegistryError::FieldTooLong`] from validation.
    /// - [`RegistryError::CapacityExceeded`] if the map is full and `service` is not yet known.
    #[must_use = "a rejected heartbeat must be observed (counted), never dropped silently"]
    pub fn register(&self, reg: Registration) -> Result<(), RegistryError> {
        Self::validate(&reg.service, &reg.version, reg.build_sha.as_deref())?;
        let mut map = self.write_entries();
        // The cap only rejects *new* keys: refreshing an existing member is always allowed.
        if !map.contains_key(&reg.service) && map.len() >= self.max_entries {
            return Err(RegistryError::CapacityExceeded(self.max_entries));
        }
        map.insert(reg.service.clone(), reg);
        Ok(())
    }

    /// Snapshots the living members, dropping expired ones in place (lazy expiration).
    ///
    /// Entries whose last heartbeat is older than the TTL are removed under the write guard,
    /// then the survivors are projected to [`RegistryEntry`] and sorted by service name for a
    /// deterministic body (ADN 2). Uses `saturating_duration_since` so a `last_seen` in the
    /// future (should never happen) is treated as age zero rather than panicking.
    fn live_entries(&self) -> Vec<RegistryEntry> {
        let now = Instant::now();
        let ttl = self.ttl;
        let mut map = self.write_entries();
        map.retain(|_, reg| now.saturating_duration_since(reg.last_seen) <= ttl);
        let mut out: Vec<RegistryEntry> = map
            .values()
            .map(|reg| {
                RegistryEntry::new(
                    reg.service.clone(),
                    reg.version.clone(),
                    reg.build_sha.clone(),
                    reg.pid,
                    reg.last_seen_ms,
                )
            })
            .collect();
        out.sort_by(|a, b| a.service.cmp(&b.service));
        out
    }

    /// Builds the read projection (living members only).
    #[must_use]
    pub fn report(&self) -> RegistryReport {
        RegistryReport::new(self.live_entries())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_at(service: &str, last_seen: Instant, last_seen_ms: i64) -> Registration {
        Registration {
            service: service.to_owned(),
            version: "2.3.0".to_owned(),
            build_sha: Some("deadbeef".to_owned()),
            pid: 1234,
            last_seen,
            last_seen_ms,
        }
    }

    #[test]
    fn register_then_list_returns_living_entry() {
        let store = RegistryStore::new(Duration::from_secs(45));
        store
            .register(reg_at("worker", Instant::now(), 1_000))
            .expect("register worker");
        let report = store.report();
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].service, "worker");
        assert_eq!(report.entries[0].pid, 1234);
    }

    #[test]
    fn expired_entry_is_omitted_on_read() {
        let store = RegistryStore::new(Duration::from_secs(45));
        // last_seen 100 s in the past ⇒ older than the 45 s TTL ⇒ dropped on read.
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(100))
            .expect("instant in test range");
        store
            .register(reg_at("worker", stale, 1_000))
            .expect("register stale worker");
        // A fresh, living member coexists and must survive the sweep.
        store
            .register(reg_at("server", Instant::now(), 2_000))
            .expect("register fresh server");
        let report = store.report();
        assert_eq!(report.entries.len(), 1, "stale entry must be omitted");
        assert_eq!(report.entries[0].service, "server");
    }

    #[test]
    fn unknown_service_is_rejected() {
        let store = RegistryStore::new(Duration::from_secs(45));
        let err = store
            .register(reg_at("rogue-service", Instant::now(), 1_000))
            .expect_err("unknown service must be rejected");
        assert_eq!(
            err,
            RegistryError::UnknownService("rogue-service".to_owned())
        );
        assert!(store.report().entries.is_empty(), "no entry must be stored");
    }

    #[test]
    fn oversized_field_is_rejected() {
        let store = RegistryStore::new(Duration::from_secs(45));
        let mut reg = reg_at("server", Instant::now(), 1_000);
        reg.version = "v".repeat(MAX_FIELD_LEN + 1);
        let err = store
            .register(reg)
            .expect_err("oversized version must be rejected");
        assert_eq!(
            err,
            RegistryError::FieldTooLong {
                field: "version",
                max: MAX_FIELD_LEN,
            }
        );
    }

    #[test]
    fn entry_cap_rejects_new_service_but_allows_refresh() {
        // Small cap so the test does not need to exhaust the real allowlist.
        let store = RegistryStore {
            entries: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(45),
            max_entries: 1,
        };
        store
            .register(reg_at("server", Instant::now(), 1_000))
            .expect("first register fits");
        // Refreshing the SAME known service is always allowed, even at capacity.
        store
            .register(reg_at("server", Instant::now(), 2_000))
            .expect("refresh of known service allowed at cap");
        // A DIFFERENT, allowlisted service is rejected once the cap is reached.
        let err = store
            .register(reg_at("worker", Instant::now(), 3_000))
            .expect_err("new service beyond cap must be rejected");
        assert_eq!(err, RegistryError::CapacityExceeded(1));
        assert_eq!(store.report().entries.len(), 1);
    }

    #[test]
    fn report_is_sorted_by_service_name() {
        let store = RegistryStore::new(Duration::from_secs(45));
        store
            .register(reg_at("worker", Instant::now(), 1_000))
            .expect("register worker");
        store
            .register(reg_at("gateway", Instant::now(), 2_000))
            .expect("register gateway");
        store
            .register(reg_at("engine-embed", Instant::now(), 3_000))
            .expect("register engine-embed");
        let names: Vec<_> = store
            .report()
            .entries
            .into_iter()
            .map(|e| e.service)
            .collect();
        assert_eq!(names, vec!["engine-embed", "gateway", "worker"]);
    }
}
