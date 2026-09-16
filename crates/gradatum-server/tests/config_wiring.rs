//! F-157 — configuration wiring guard for `gradatum-server` (the « class test »).
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. That was exactly the
//! incident of the two [`gradatum_engine`] fields `warm_up` / `max_tokens` (parsed,
//! validated, then silently ignored) — detected only by hand at a tag pre-flight. The
//! volet 1 guard (`gradatum-engine/tests/config_wiring.rs`) locked that class down for
//! `EngineConfig`. This is the volet 2 extension to the server, whose `ServerConfig`
//! aggregates ~28 deserialised sub-structs and 100+ leaf fields — the surface the F-157
//! card names first.
//!
//! ## Mechanism (identical to volet 1, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) per config
//!    struct. Adding/removing a field to any struct breaks this test's compilation until
//!    the field is named in the destructuring AND passed to [`entry`] (hence classified).
//!    This is the strongest branch: a new field can never appear unclassified.
//! 2. **Classification registry** — each field is `Wired { proof }`, `NotWired`, or
//!    `Mirror { .. }` (see [`Wiring`]). `proof` is a hand-picked discriminating token that
//!    MUST appear in the crate's **production** sources. Removing the wiring removes the
//!    token → RED.
//! 3. **Production scan** — every crate source except `config.rs` (where the structs and
//!    their accessors are defined), with `#[cfg(test)]` modules stripped and comments
//!    neutralised (a token merely mentioned in a comment is not a real consumption).
//!
//! ## Token discrimination (server-specific)
//!
//! The server threads config as `state.server_config.<section>.<field>` /
//! `cfg.<section>.<field>`, so a `<section>.<field>` token disambiguates the many
//! homonym leaf fields (`enabled`, `retention_days`, `timeout_ms`, …). Three indirections
//! recur and force a non-nominal proof token, exactly as volet 1 documented for
//! `bind_addr` / `timeout_secs`:
//!   - **accessor-only reads** — fields read only inside a `config.rs` accessor
//!     (`SalienceConfig::resolve`, `HealthConfig::effective_interval_secs`,
//!     `RegistryConfig::effective_ttl_secs`, `DowngradeConfig::thresholds`,
//!     `StorageConfig::legacy_alias_used`, …). `config.rs` is excluded from the corpus, so
//!     the proof is the **accessor name**, present at the call site in production.
//!   - **shared generic bindings** — `main.rs` reuses one `retention_cfg` binding for the
//!     event-log / session-trace retention structs, so `.retention_days` / `.max_rows`
//!     are read under an anonymous binding. The proof is the bare token and the LIMIT is
//!     assumed (see the per-field comments): unwiring one of the shared consumers may not
//!     turn this test red while the other still reads its own field.
//!   - **cross-crate mirror** — the server deserialises `[curator.llm]` for schema
//!     symmetry but acts on it only through the `/health` LLM probe (which reads
//!     `base_url`). The remaining [`LlmConfig`] fields are consumed by `gradatum-worker`,
//!     which mirrors the same TOML section. They are classified [`Wiring::Mirror`]:
//!     asserted **absent** from this crate's production (so a future accidental server-side
//!     read forces a reclassification), and their cross-crate consumer is recorded.
//!
//! Run with: `cargo test -p gradatum-server --test config_wiring`

use gradatum_server::config::{
    AclConfig, ArchiveConfig, AuditConfig, AuthConfig, ContextConfig, CuratorConfig,
    DowngradeConfig, EmbedConfig, EventLogConfig, HealthConfig, HttpConfig, InternalApiConfig,
    LlmConfig, LogConfig, MultiTenantConfig, PerVaultOverride, PrivacyConfig, PrivacyThresholds,
    RateLimitConfig, RegistryConfig, RegistryIngressConfig, ReviewPromoteConfig, SalienceConfig,
    ScoringConfig, SearchConfig, ServerConfig, SessionTraceConfig, StorageConfig, StudioConfig,
    TlsConfig,
};
use gradatum_server::proactive_recall::ProactiveRecallConfig;
use std::fs;
use std::path::{Path, PathBuf};

/// Classification of a config field with respect to its runtime consumption.
enum Wiring {
    /// The field is wired: its value reaches a consumption site. `proof` is a discriminating
    /// token that must appear in the crate's production sources (excluding `config.rs`,
    /// excluding test modules). Removing the wiring removes the token.
    Wired { proof: &'static str },
    /// The field reaches config-load **validation only**, never a runtime consumer (G34).
    /// `validation_marker` is a discriminating token that MUST appear in `config.rs`,
    /// proving the load-time check still exists. Removing the check removes the token → RED.
    ///
    /// The `[privacy] mode` field is the delivered case: it is read only to be accepted or
    /// refused at load (`mask` is refused in 2.4.0), and no masking path consumes it.
    Validated { validation_marker: &'static str },
    /// The field is deliberately **parsed-but-not-consumed** ("parsed-only", G34). It MUST
    /// carry a `// not yet wired` marker near its declaration in `config.rs`, and its
    /// `consumption_token` MUST NOT appear in this crate's production — if it does, the
    /// field was wired without being reclassified `Wired`.
    NotWired { consumption_token: &'static str },
    /// The field is parsed here for schema symmetry but consumed by **another crate** (a
    /// TOML section mirrored across services). `consumer` names that crate; `absent_token`
    /// is a discriminating token that must NOT appear in *this* crate's production — if it
    /// does, the field was wired server-side and must be reclassified `Wired`.
    Mirror {
        consumer: &'static str,
        absent_token: &'static str,
    },
}

/// One registry entry: a field name plus its classification.
struct Entry {
    name: &'static str,
    wiring: Wiring,
}

/// Builds an entry, **consuming** the binding produced by the destructuring.
///
/// `_binding` is not used for its value: it forces the mechanical link between the
/// exhaustive destructuring and the registry. Adding a field to a struct forces naming it
/// in the destructuring (else E0027); the unused binding is silenced only by passing it
/// here — i.e. by adding a registry entry.
fn entry<T>(name: &'static str, _binding: &T, wiring: Wiring) -> Entry {
    Entry { name, wiring }
}

// ---------------------------------------------------------------------------
// Registries — one exhaustive destructuring per config struct.
// ---------------------------------------------------------------------------

/// Top-level [`ServerConfig`]. Each field is a sub-struct/map; its proof reuses a
/// discriminating token already established for that section's consumption (a section
/// cannot be read without being threaded). Bare section tokens like `.audit` / `.search`
/// / `.context` are deliberately avoided — they collide with `AppState` fields and
/// `anyhow::Context`.
fn server_registry(c: &ServerConfig) -> Vec<Entry> {
    let ServerConfig {
        server,
        storage,
        auth,
        acl,
        log,
        curator,
        ratelimit,
        embed,
        event_log,
        session_trace,
        archive,
        scoring,
        salience,
        studio,
        health,
        registry,
        registry_ingress,
        internal_api,
        search,
        review_promote,
        audit,
        downgrade,
        context,
        proactive_recall,
        multi_tenant,
        privacy,
        per_vault,
    } = c;
    vec![
        entry(
            "server",
            server,
            Wiring::Wired {
                proof: "server.bind",
            },
        ),
        entry(
            "storage",
            storage,
            Wiring::Wired {
                proof: "storage.root",
            },
        ),
        entry(
            "auth",
            auth,
            Wiring::Wired {
                proof: "revocation_store",
            },
        ),
        entry(
            "acl",
            acl,
            Wiring::Wired {
                proof: ".preset_path",
            },
        ),
        entry(
            "log",
            log,
            Wiring::Wired {
                proof: "log.format",
            },
        ),
        entry(
            "curator",
            curator,
            Wiring::Wired {
                proof: "curator.backend",
            },
        ),
        entry(
            "ratelimit",
            ratelimit,
            Wiring::Wired {
                proof: "ratelimit.enabled",
            },
        ),
        entry(
            "embed",
            embed,
            Wiring::Wired {
                proof: "embed.endpoint",
            },
        ),
        entry(
            "event_log",
            event_log,
            Wiring::Wired {
                proof: "event_log.purge_interval_secs",
            },
        ),
        entry(
            "session_trace",
            session_trace,
            Wiring::Wired {
                proof: "session_trace.purge_interval_secs",
            },
        ),
        entry(
            "archive",
            archive,
            Wiring::Wired {
                proof: "archive.retention_days",
            },
        ),
        entry(
            "scoring",
            scoring,
            Wiring::Wired {
                proof: ".trust_decay_enabled",
            },
        ),
        entry(
            "salience",
            salience,
            Wiring::Wired {
                proof: "salience.resolve",
            },
        ),
        entry("studio", studio, Wiring::Wired { proof: ".ui_dir" }),
        entry(
            "health",
            health,
            Wiring::Wired {
                proof: "effective_interval_secs",
            },
        ),
        entry(
            "registry",
            registry,
            Wiring::Wired {
                proof: "effective_ttl_secs",
            },
        ),
        entry(
            "registry_ingress",
            registry_ingress,
            Wiring::Wired {
                proof: "registry_ingress.enabled",
            },
        ),
        entry(
            "internal_api",
            internal_api,
            Wiring::Wired {
                proof: "internal_api.bind",
            },
        ),
        entry(
            "search",
            search,
            Wiring::Wired {
                proof: "search.ann_backend",
            },
        ),
        entry(
            "review_promote",
            review_promote,
            Wiring::Wired {
                proof: "review_promote.enabled",
            },
        ),
        entry(
            "audit",
            audit,
            Wiring::Wired {
                proof: "audit.interval_secs",
            },
        ),
        entry(
            "downgrade",
            downgrade,
            Wiring::Wired {
                proof: "downgrade.validate",
            },
        ),
        entry(
            "context",
            context,
            Wiring::Wired {
                proof: "context.embed_timeout_ms",
            },
        ),
        entry(
            "proactive_recall",
            proactive_recall,
            Wiring::Wired {
                proof: "proactive_recall.refresh_interval_secs",
            },
        ),
        entry(
            "multi_tenant",
            multi_tenant,
            Wiring::Wired {
                proof: "multi_tenant.enabled",
            },
        ),
        // F-160 privacy frontier: the section is threaded from `main.rs` to the detector
        // mount through the dedicated builder, which is the consumption site. The section
        // being threaded does NOT mean every sub-field is consumed — the field-level
        // classification lives in `privacy_registry` (G34), which is what makes the
        // parsed-but-unconsumed sub-fields visible instead of masked by this one entry.
        entry(
            "privacy",
            privacy,
            Wiring::Wired {
                proof: "with_privacy",
            },
        ),
        // per_vault map: read only through the `config.rs` resolvers.
        entry(
            "per_vault",
            per_vault,
            Wiring::Wired {
                proof: "resolve_salience_per_vault",
            },
        ),
    ]
}

fn http_registry(c: &HttpConfig) -> Vec<Entry> {
    let HttpConfig {
        bind,
        metrics_bind,
        tls,
    } = c;
    vec![
        entry(
            "bind",
            bind,
            Wiring::Wired {
                proof: "server.bind",
            },
        ),
        entry(
            "metrics_bind",
            metrics_bind,
            Wiring::Wired {
                proof: "server.metrics_bind",
            },
        ),
        entry(
            "tls",
            tls,
            Wiring::Wired {
                proof: "server.tls",
            },
        ),
    ]
}

fn tls_registry(c: &TlsConfig) -> Vec<Entry> {
    let TlsConfig {
        cert_path,
        key_path,
    } = c;
    vec![
        entry(
            "cert_path",
            cert_path,
            Wiring::Wired {
                proof: ".cert_path",
            },
        ),
        entry("key_path", key_path, Wiring::Wired { proof: ".key_path" }),
    ]
}

fn storage_registry(c: &StorageConfig) -> Vec<Entry> {
    let StorageConfig {
        root,
        vault_index_path,
        legacy_alias_used,
        vault_index_path_override_diverges,
    } = c;
    vec![
        entry(
            "root",
            root,
            Wiring::Wired {
                proof: "storage.root",
            },
        ),
        entry(
            "vault_index_path",
            vault_index_path,
            Wiring::Wired {
                proof: "storage.vault_index_path",
            },
        ),
        // read only through the accessors of the same name (defined in `config.rs`).
        entry(
            "legacy_alias_used",
            legacy_alias_used,
            Wiring::Wired {
                proof: "legacy_alias_used(",
            },
        ),
        entry(
            "vault_index_path_override_diverges",
            vault_index_path_override_diverges,
            Wiring::Wired {
                proof: "vault_index_path_override_diverges(",
            },
        ),
    ]
}

fn auth_registry(c: &AuthConfig) -> Vec<Entry> {
    let AuthConfig {
        jwt_ttl_human_secs,
        jwt_ttl_service_secs,
        revocation_store,
        revocation_db_path,
        api_keys_db_path,
    } = c;
    vec![
        entry(
            "jwt_ttl_human_secs",
            jwt_ttl_human_secs,
            Wiring::Wired {
                proof: "jwt_ttl_human_secs",
            },
        ),
        entry(
            "jwt_ttl_service_secs",
            jwt_ttl_service_secs,
            Wiring::Wired {
                proof: "jwt_ttl_service_secs",
            },
        ),
        entry(
            "revocation_store",
            revocation_store,
            Wiring::Wired {
                proof: "revocation_store",
            },
        ),
        entry(
            "revocation_db_path",
            revocation_db_path,
            Wiring::Wired {
                proof: ".revocation_db_path",
            },
        ),
        entry(
            "api_keys_db_path",
            api_keys_db_path,
            Wiring::Wired {
                proof: ".api_keys_db_path",
            },
        ),
    ]
}

fn acl_registry(c: &AclConfig) -> Vec<Entry> {
    let AclConfig { preset_path } = c;
    vec![entry(
        "preset_path",
        preset_path,
        Wiring::Wired {
            proof: ".preset_path",
        },
    )]
}

fn log_registry(c: &LogConfig) -> Vec<Entry> {
    let LogConfig { format } = c;
    vec![entry(
        "format",
        format,
        Wiring::Wired {
            proof: "log.format",
        },
    )]
}

fn curator_registry(c: &CuratorConfig) -> Vec<Entry> {
    let CuratorConfig { backend, llm } = c;
    vec![
        entry(
            "backend",
            backend,
            Wiring::Wired {
                proof: "curator.backend",
            },
        ),
        entry(
            "llm",
            llm,
            Wiring::Wired {
                proof: "curator.llm",
            },
        ),
    ]
}

/// [`LlmConfig`]: only `base_url` is read server-side (the `/health` LLM probe). The rest
/// is a schema mirror consumed by `gradatum-worker` — the F-157 finding of this volet.
fn llm_registry(c: &LlmConfig) -> Vec<Entry> {
    let LlmConfig {
        backend,
        base_url,
        model,
        api_key_env,
        timeout_ms,
        fallback,
    } = c;
    vec![
        entry(
            "backend",
            backend,
            Wiring::Mirror {
                consumer: "gradatum-worker",
                absent_token: "llm.backend",
            },
        ),
        entry(
            "base_url",
            base_url,
            Wiring::Wired {
                proof: "llm.base_url",
            },
        ),
        entry(
            "model",
            model,
            Wiring::Mirror {
                consumer: "gradatum-worker",
                absent_token: "llm.model",
            },
        ),
        entry(
            "api_key_env",
            api_key_env,
            Wiring::Mirror {
                consumer: "gradatum-worker",
                absent_token: "llm.api_key_env",
            },
        ),
        entry(
            "timeout_ms",
            timeout_ms,
            Wiring::Mirror {
                consumer: "gradatum-worker",
                absent_token: "llm.timeout_ms",
            },
        ),
        entry(
            "fallback",
            fallback,
            Wiring::Mirror {
                consumer: "gradatum-worker",
                absent_token: "llm.fallback",
            },
        ),
    ]
}

fn ratelimit_registry(c: &RateLimitConfig) -> Vec<Entry> {
    let RateLimitConfig {
        enabled,
        per_minute,
        burst,
        exempt_localhost,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "ratelimit.enabled",
            },
        ),
        entry(
            "per_minute",
            per_minute,
            Wiring::Wired {
                proof: ".per_minute",
            },
        ),
        entry("burst", burst, Wiring::Wired { proof: ".burst" }),
        entry(
            "exempt_localhost",
            exempt_localhost,
            Wiring::Wired {
                proof: ".exempt_localhost",
            },
        ),
    ]
}

fn embed_registry(c: &EmbedConfig) -> Vec<Entry> {
    let EmbedConfig {
        enabled,
        endpoint,
        model,
        dim,
        timeout_ms,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "embed.enabled",
            },
        ),
        entry(
            "endpoint",
            endpoint,
            Wiring::Wired {
                proof: "embed.endpoint",
            },
        ),
        entry(
            "model",
            model,
            Wiring::Wired {
                proof: "embed.model",
            },
        ),
        entry("dim", dim, Wiring::Wired { proof: "embed.dim" }),
        entry(
            "timeout_ms",
            timeout_ms,
            Wiring::Wired {
                proof: "embed.timeout_ms",
            },
        ),
    ]
}

fn event_log_registry(c: &EventLogConfig) -> Vec<Entry> {
    let EventLogConfig {
        retention_days,
        purge_interval_secs,
        max_rows,
    } = c;
    vec![
        // LIMIT ASSUMED: `.retention_days` / `.max_rows` are read under a generic
        // `retention_cfg` binding shared with SessionTraceConfig/ArchiveConfig in main.rs.
        entry(
            "retention_days",
            retention_days,
            Wiring::Wired {
                proof: ".retention_days",
            },
        ),
        entry(
            "purge_interval_secs",
            purge_interval_secs,
            Wiring::Wired {
                proof: "event_log.purge_interval_secs",
            },
        ),
        entry("max_rows", max_rows, Wiring::Wired { proof: ".max_rows" }),
    ]
}

fn session_trace_registry(c: &SessionTraceConfig) -> Vec<Entry> {
    let SessionTraceConfig {
        retention_days,
        purge_interval_secs,
        max_rows,
    } = c;
    vec![
        // LIMIT ASSUMED: shared `.retention_days` / `.max_rows` tokens (see EventLogConfig).
        entry(
            "retention_days",
            retention_days,
            Wiring::Wired {
                proof: ".retention_days",
            },
        ),
        entry(
            "purge_interval_secs",
            purge_interval_secs,
            Wiring::Wired {
                proof: "session_trace.purge_interval_secs",
            },
        ),
        entry("max_rows", max_rows, Wiring::Wired { proof: ".max_rows" }),
    ]
}

fn archive_registry(c: &ArchiveConfig) -> Vec<Entry> {
    let ArchiveConfig {
        retention_days,
        gc_interval_secs,
        gc_batch_limit,
    } = c;
    vec![
        entry(
            "retention_days",
            retention_days,
            Wiring::Wired {
                proof: "archive.retention_days",
            },
        ),
        entry(
            "gc_interval_secs",
            gc_interval_secs,
            Wiring::Wired {
                proof: "archive.gc_interval_secs",
            },
        ),
        entry(
            "gc_batch_limit",
            gc_batch_limit,
            Wiring::Wired {
                proof: "archive.gc_batch_limit",
            },
        ),
    ]
}

fn scoring_registry(c: &ScoringConfig) -> Vec<Entry> {
    let ScoringConfig {
        trust_decay_enabled,
        half_life_days,
    } = c;
    vec![
        entry(
            "trust_decay_enabled",
            trust_decay_enabled,
            Wiring::Wired {
                proof: ".trust_decay_enabled",
            },
        ),
        entry(
            "half_life_days",
            half_life_days,
            Wiring::Wired {
                proof: "half_life_days",
            },
        ),
    ]
}

/// [`SalienceConfig`]: all four fields are read inside `resolve()` / `validate()` (in
/// `config.rs`, excluded). Proof = the accessor call site. LIMIT ASSUMED: the four fields
/// share the `salience.resolve` / `salience.validate` anchors.
fn salience_registry(c: &SalienceConfig) -> Vec<Entry> {
    let SalienceConfig {
        enabled,
        gamma,
        k_norm,
        kind_weights,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "salience.resolve",
            },
        ),
        entry(
            "gamma",
            gamma,
            Wiring::Wired {
                proof: "salience.resolve",
            },
        ),
        entry(
            "k_norm",
            k_norm,
            Wiring::Wired {
                proof: "salience.validate",
            },
        ),
        entry(
            "kind_weights",
            kind_weights,
            Wiring::Wired {
                proof: "salience.resolve",
            },
        ),
    ]
}

fn studio_registry(c: &StudioConfig) -> Vec<Entry> {
    let StudioConfig { ui_dir } = c;
    vec![entry("ui_dir", ui_dir, Wiring::Wired { proof: ".ui_dir" })]
}

fn health_registry(c: &HealthConfig) -> Vec<Entry> {
    let HealthConfig {
        probe_interval_secs,
        probe_timeout_ms,
    } = c;
    vec![
        entry(
            "probe_interval_secs",
            probe_interval_secs,
            Wiring::Wired {
                proof: "effective_interval_secs",
            },
        ),
        entry(
            "probe_timeout_ms",
            probe_timeout_ms,
            Wiring::Wired {
                proof: "effective_probe_timeout_ms",
            },
        ),
    ]
}

fn registry_registry(c: &RegistryConfig) -> Vec<Entry> {
    let RegistryConfig { ttl_secs } = c;
    vec![entry(
        "ttl_secs",
        ttl_secs,
        Wiring::Wired {
            proof: "effective_ttl_secs",
        },
    )]
}

fn registry_ingress_registry(c: &RegistryIngressConfig) -> Vec<Entry> {
    let RegistryIngressConfig { enabled, bind } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "registry_ingress.enabled",
            },
        ),
        entry(
            "bind",
            bind,
            Wiring::Wired {
                proof: "registry_ingress.bind",
            },
        ),
    ]
}

fn internal_api_registry(c: &InternalApiConfig) -> Vec<Entry> {
    let InternalApiConfig {
        bind,
        token,
        admin_token,
    } = c;
    vec![
        entry(
            "bind",
            bind,
            Wiring::Wired {
                proof: "internal_api.bind",
            },
        ),
        entry(
            "token",
            token,
            Wiring::Wired {
                proof: "internal_api.token",
            },
        ),
        entry(
            "admin_token",
            admin_token,
            Wiring::Wired {
                proof: "internal_api.admin_token",
            },
        ),
    ]
}

fn search_registry(c: &SearchConfig) -> Vec<Entry> {
    let SearchConfig {
        ann_backend,
        ann_ef_search,
    } = c;
    vec![
        entry(
            "ann_backend",
            ann_backend,
            Wiring::Wired {
                proof: "search.ann_backend",
            },
        ),
        entry(
            "ann_ef_search",
            ann_ef_search,
            Wiring::Wired {
                proof: ".ann_ef_search",
            },
        ),
    ]
}

fn review_promote_registry(c: &ReviewPromoteConfig) -> Vec<Entry> {
    let ReviewPromoteConfig {
        enabled,
        age_days,
        interval_secs,
        max_per_tick,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "review_promote.enabled",
            },
        ),
        entry("age_days", age_days, Wiring::Wired { proof: ".age_days" }),
        entry(
            "interval_secs",
            interval_secs,
            Wiring::Wired {
                proof: "review_promote.interval_secs",
            },
        ),
        entry(
            "max_per_tick",
            max_per_tick,
            Wiring::Wired {
                proof: ".max_per_tick",
            },
        ),
    ]
}

fn audit_registry(c: &AuditConfig) -> Vec<Entry> {
    let AuditConfig {
        enabled,
        interval_secs,
        max_scan,
    } = c;
    vec![
        // LIMIT ASSUMED: `enabled` is read as `!cfg.enabled` inside `audit_job::audit_once`
        // (param named `cfg`); no section-qualified token exists, so the shared `.enabled`
        // token is used — unwiring the audit gate alone may not turn this red.
        entry("enabled", enabled, Wiring::Wired { proof: ".enabled" }),
        entry(
            "interval_secs",
            interval_secs,
            Wiring::Wired {
                proof: "audit.interval_secs",
            },
        ),
        entry("max_scan", max_scan, Wiring::Wired { proof: ".max_scan" }),
    ]
}

fn downgrade_registry(c: &DowngradeConfig) -> Vec<Entry> {
    let DowngradeConfig {
        enabled,
        age_min_days,
        trust_max,
        usage_window_days,
        max_per_run,
        protected_extra,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "downgrade_cfg.enabled",
            },
        ),
        // age_min_days / trust_max are read only inside `thresholds()` (config.rs).
        entry(
            "age_min_days",
            age_min_days,
            Wiring::Wired {
                proof: ".thresholds(",
            },
        ),
        entry(
            "trust_max",
            trust_max,
            Wiring::Wired {
                proof: ".thresholds(",
            },
        ),
        entry(
            "usage_window_days",
            usage_window_days,
            Wiring::Wired {
                proof: ".usage_window_days",
            },
        ),
        entry(
            "max_per_run",
            max_per_run,
            Wiring::Wired {
                proof: ".max_per_run",
            },
        ),
        entry(
            "protected_extra",
            protected_extra,
            Wiring::Wired {
                proof: ".protected_sections",
            },
        ),
    ]
}

fn context_registry(c: &ContextConfig) -> Vec<Entry> {
    let ContextConfig {
        default_budget_tokens,
        top_n_candidates,
        max_skills,
        skills_budget_fraction,
        embed_timeout_ms,
        stub_budget_tokens,
        cache_breakpoint_threshold_tokens,
    } = c;
    vec![
        entry(
            "default_budget_tokens",
            default_budget_tokens,
            Wiring::Wired {
                proof: "context.default_budget_tokens",
            },
        ),
        entry(
            "top_n_candidates",
            top_n_candidates,
            Wiring::Wired {
                proof: "context.top_n_candidates",
            },
        ),
        entry(
            "max_skills",
            max_skills,
            Wiring::Wired {
                proof: "context.max_skills",
            },
        ),
        entry(
            "skills_budget_fraction",
            skills_budget_fraction,
            Wiring::Wired {
                proof: "context.skills_budget_fraction",
            },
        ),
        entry(
            "embed_timeout_ms",
            embed_timeout_ms,
            Wiring::Wired {
                proof: "context.embed_timeout_ms",
            },
        ),
        entry(
            "stub_budget_tokens",
            stub_budget_tokens,
            Wiring::Wired {
                proof: "context.stub_budget_tokens",
            },
        ),
        entry(
            "cache_breakpoint_threshold_tokens",
            cache_breakpoint_threshold_tokens,
            Wiring::Wired {
                proof: "context.cache_breakpoint_threshold_tokens",
            },
        ),
    ]
}

fn proactive_recall_registry(c: &ProactiveRecallConfig) -> Vec<Entry> {
    let ProactiveRecallConfig {
        refresh_interval_secs,
        recent_k,
        surface_size,
    } = c;
    vec![
        entry(
            "refresh_interval_secs",
            refresh_interval_secs,
            Wiring::Wired {
                proof: "proactive_recall.refresh_interval_secs",
            },
        ),
        entry("recent_k", recent_k, Wiring::Wired { proof: ".recent_k" }),
        entry(
            "surface_size",
            surface_size,
            Wiring::Wired {
                proof: ".surface_size",
            },
        ),
    ]
}

fn multi_tenant_registry(c: &MultiTenantConfig) -> Vec<Entry> {
    let MultiTenantConfig { enabled } = c;
    vec![entry(
        "enabled",
        enabled,
        Wiring::Wired {
            proof: "multi_tenant.enabled",
        },
    )]
}

/// [`PrivacyConfig`] — F-160 personal-data frontier, classified at the FIELD level (G34).
///
/// The top-level `privacy` entry only proves the section is threaded; this registry is
/// what makes the sub-field bypass visible. In 2.4.0 only the byte budgets, the rescan
/// gate/interval and the *presence* of `categories` (its emptiness decides the mount)
/// reach a consumer. `mode` is validated at load and consumed by no masking path;
/// `thresholds` and `exceptions` are parsed but not yet wired to the detector (2.5.0).
fn privacy_registry(c: &PrivacyConfig) -> Vec<Entry> {
    let PrivacyConfig {
        mode,
        categories,
        max_input_bytes,
        window_overlap_bytes,
        thresholds,
        exceptions,
        rescan_enabled,
        rescan_interval_secs,
    } = c;
    vec![
        entry(
            "mode",
            mode,
            Wiring::Validated {
                validation_marker: "PrivacyMaskUnavailable",
            },
        ),
        entry(
            "categories",
            categories,
            Wiring::Wired {
                proof: "privacy.categories",
            },
        ),
        // Both byte budgets reach the detector through `self.privacy.budgets()`.
        entry(
            "max_input_bytes",
            max_input_bytes,
            Wiring::Wired {
                proof: "privacy.budgets",
            },
        ),
        entry(
            "window_overlap_bytes",
            window_overlap_bytes,
            Wiring::Wired {
                proof: "privacy.budgets",
            },
        ),
        entry(
            "thresholds",
            thresholds,
            Wiring::NotWired {
                consumption_token: "privacy.thresholds",
            },
        ),
        entry(
            "exceptions",
            exceptions,
            Wiring::NotWired {
                consumption_token: "privacy.exceptions",
            },
        ),
        entry(
            "rescan_enabled",
            rescan_enabled,
            Wiring::Wired {
                proof: "privacy.rescan_enabled",
            },
        ),
        entry(
            "rescan_interval_secs",
            rescan_interval_secs,
            Wiring::Wired {
                proof: "privacy.rescan_interval_secs",
            },
        ),
    ]
}

/// [`PrivacyThresholds`] — parsed for schema completeness, consumed by nothing in 2.4.0
/// (2.5.0). Both fields are parsed-only; a future field added here cannot appear
/// unclassified (exhaustive destructuring).
fn privacy_thresholds_registry(c: &PrivacyThresholds) -> Vec<Entry> {
    let PrivacyThresholds {
        default_m,
        per_category,
    } = c;
    vec![
        entry(
            "default_m",
            default_m,
            Wiring::NotWired {
                consumption_token: ".default_m",
            },
        ),
        entry(
            "per_category",
            per_category,
            Wiring::NotWired {
                consumption_token: ".per_category",
            },
        ),
    ]
}

fn per_vault_override_registry(c: &PerVaultOverride) -> Vec<Entry> {
    let PerVaultOverride {
        salience,
        review_promote,
    } = c;
    vec![
        entry(
            "salience",
            salience,
            Wiring::Wired {
                proof: "resolve_salience_per_vault",
            },
        ),
        entry(
            "review_promote",
            review_promote,
            Wiring::Wired {
                proof: "review_promote_for",
            },
        ),
    ]
}

/// Aggregates every struct's registry, prefixing each entry name with its struct for
/// legible diagnostics. Sample instances for the `Option`/map-nested structs are built
/// with explicit literals (all fields `pub`, none `#[non_exhaustive]`).
fn full_registry() -> Vec<(&'static str, Vec<Entry>)> {
    let cfg = ServerConfig::default();
    let tls = TlsConfig {
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
    };
    let llm = LlmConfig {
        backend: String::new(),
        base_url: String::new(),
        model: String::new(),
        api_key_env: None,
        timeout_ms: 0,
        fallback: None,
    };
    let over = PerVaultOverride {
        salience: None,
        review_promote: None,
    };

    vec![
        ("ServerConfig", server_registry(&cfg)),
        ("HttpConfig", http_registry(&cfg.server)),
        ("TlsConfig", tls_registry(&tls)),
        ("StorageConfig", storage_registry(&cfg.storage)),
        ("AuthConfig", auth_registry(&cfg.auth)),
        ("AclConfig", acl_registry(&cfg.acl)),
        ("LogConfig", log_registry(&cfg.log)),
        ("CuratorConfig", curator_registry(&cfg.curator)),
        ("LlmConfig", llm_registry(&llm)),
        ("RateLimitConfig", ratelimit_registry(&cfg.ratelimit)),
        ("EmbedConfig", embed_registry(&cfg.embed)),
        ("EventLogConfig", event_log_registry(&cfg.event_log)),
        (
            "SessionTraceConfig",
            session_trace_registry(&cfg.session_trace),
        ),
        ("ArchiveConfig", archive_registry(&cfg.archive)),
        ("ScoringConfig", scoring_registry(&cfg.scoring)),
        ("SalienceConfig", salience_registry(&cfg.salience)),
        ("StudioConfig", studio_registry(&cfg.studio)),
        ("HealthConfig", health_registry(&cfg.health)),
        ("RegistryConfig", registry_registry(&cfg.registry)),
        (
            "RegistryIngressConfig",
            registry_ingress_registry(&cfg.registry_ingress),
        ),
        (
            "InternalApiConfig",
            internal_api_registry(&cfg.internal_api),
        ),
        ("SearchConfig", search_registry(&cfg.search)),
        (
            "ReviewPromoteConfig",
            review_promote_registry(&cfg.review_promote),
        ),
        ("AuditConfig", audit_registry(&cfg.audit)),
        ("DowngradeConfig", downgrade_registry(&cfg.downgrade)),
        ("ContextConfig", context_registry(&cfg.context)),
        (
            "ProactiveRecallConfig",
            proactive_recall_registry(&cfg.proactive_recall),
        ),
        (
            "MultiTenantConfig",
            multi_tenant_registry(&cfg.multi_tenant),
        ),
        ("PrivacyConfig", privacy_registry(&cfg.privacy)),
        (
            "PrivacyThresholds",
            privacy_thresholds_registry(&cfg.privacy.thresholds),
        ),
        ("PerVaultOverride", per_vault_override_registry(&over)),
    ]
}

// ---------------------------------------------------------------------------
// Production source scan (strips #[cfg(test)] modules) — verbatim from volet 1.
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `//` … end of line. `b[i]` is the first `/`.
fn skip_line_comment(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j < b.len() && b[j] != b'\n' {
        j += 1;
    }
    j
}

/// `/* … */`. `b[i]` is the first `/`.
fn skip_block_comment(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
        j += 1;
    }
    (j + 2).min(b.len())
}

/// String `"…"` with escapes. `b[i] == '"'`.
fn skip_string(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => {
                j += 1;
                break;
            }
            _ => j += 1,
        }
    }
    j
}

/// Raw string `r"…"` / `r#"…"#`. `b[i] == 'r'`. `None` if it is not one.
fn skip_raw_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut hashes = 0usize;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"' {
            let mut k = j + 1;
            let mut cnt = 0;
            while k < b.len() && cnt < hashes && b[k] == b'#' {
                cnt += 1;
                k += 1;
            }
            if cnt == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(b.len())
}

/// Char literal `'x'` / `'\n'` or lifetime label `'a`. `b[i] == '\''`.
fn skip_char_or_lifetime(b: &[u8], i: usize) -> usize {
    let n = b.len();
    if i + 1 >= n {
        return i + 1;
    }
    if b[i + 1] == b'\\' {
        let mut j = i + 2;
        while j < n && b[j] != b'\'' {
            j += 1;
        }
        return (j + 1).min(n);
    }
    if i + 2 < n && b[i + 2] == b'\'' {
        return i + 3; // 'x'
    }
    i + 1 // lifetime / label: consume only the apostrophe
}

/// If `src[i..]` starts with a `#[cfg(...)]` attribute whose predicate activates `test`,
/// returns the index right after the closing `]`. Otherwise `None`. Conservative: a
/// predicate containing `not` is never treated as test (`#[cfg(not(test))]` is production).
fn test_cfg_attr_end(src: &str, i: usize) -> Option<usize> {
    if !src[i..].starts_with("#[cfg(") {
        return None;
    }
    let b = src.as_bytes();
    let n = b.len();
    let mut j = i + 6;
    let mut depth = 1i32;
    while j < n && depth > 0 {
        match b[j] {
            b'"' => j = skip_string(b, j),
            b'\'' => j = skip_char_or_lifetime(b, j),
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
            }
            _ => j += 1,
        }
    }
    if j >= n || b[j] != b']' {
        return None;
    }
    let pred = &src[i + 6..j - 1];
    if cfg_pred_is_test(pred) {
        Some(j + 1)
    } else {
        None
    }
}

/// `true` if the `cfg` predicate activates test compilation: contains the atom `test`
/// (outside a string, word-bounded) and no `not` (conservative).
fn cfg_pred_is_test(pred: &str) -> bool {
    if pred.contains("not") {
        return false;
    }
    let pb = pred.as_bytes();
    let n = pb.len();
    let mut i = 0;
    while i < n {
        if pb[i] == b'"' {
            i = skip_string(pb, i);
            continue;
        }
        if pred[i..].starts_with("test") {
            let before_ok = i == 0 || !is_ident_byte(pb[i - 1]);
            let after = i + 4;
            let after_ok = after >= n || !is_ident_byte(pb[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Exclusive end of the attributed item starting right after the cfg-test attribute, at
/// `attr_end`. Stacked attributes are crossed. Handles `mod tests { … }` / `fn f() { … }`
/// (end = matched `}`) and `use …;` (end = `;`). Brace counting ignores comments, strings
/// (raw included) and char literals.
fn skip_item_after_attr(src: &str, attr_end: usize) -> usize {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = attr_end;
    let mut depth: i32 = 0;
    let mut seen_brace = false;
    while i < n {
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i = skip_line_comment(b, i);
            continue;
        }
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i = skip_block_comment(b, i);
            continue;
        }
        if b[i] == b'r'
            && let Some(e) = skip_raw_string(b, i)
        {
            i = e;
            continue;
        }
        if b[i] == b'"' {
            i = skip_string(b, i);
            continue;
        }
        if b[i] == b'\'' {
            i = skip_char_or_lifetime(b, i);
            continue;
        }
        match b[i] {
            b'{' => {
                depth += 1;
                seen_brace = true;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if seen_brace && depth == 0 {
                    return i;
                }
            }
            b';' if !seen_brace && depth == 0 => return i + 1,
            _ => i += 1,
        }
    }
    n
}

/// Removes `#[cfg(test)]` items from a Rust source and replaces comments with a space (a
/// token mentioned in a comment is not a real consumption). Strings/literals are kept.
fn strip_tests_and_comments(src: &str) -> String {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
            out.push(' ');
            i = skip_line_comment(b, i);
            continue;
        }
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            out.push(' ');
            i = skip_block_comment(b, i);
            continue;
        }
        if b[i] == b'r'
            && let Some(e) = skip_raw_string(b, i)
        {
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'"' {
            let e = skip_string(b, i);
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'\'' {
            let e = skip_char_or_lifetime(b, i);
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'#'
            && let Some(attr_end) = test_cfg_attr_end(src, i)
        {
            i = skip_item_after_attr(src, attr_end);
            continue;
        }
        let ch = src[i..].chars().next().expect("index on a utf8 boundary");
        let l = ch.len_utf8();
        out.push_str(&src[i..i + l]);
        i += l;
    }
    out
}

/// `true` if `token` appears in `haystack` followed by a right word boundary (the next
/// character is not an identifier character). The left boundary is carried by the token's
/// literal prefix (`.`, `<section>.`).
fn reads(haystack: &str, token: &str) -> bool {
    let hb = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(token) {
        let idx = from + rel;
        let after = idx + token.len();
        if after >= hb.len() || !is_ident_byte(hb[after]) {
            return true;
        }
        from = idx + 1;
    }
    false
}

/// `.rs` paths under `dir`, recursively.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("read_dir of the source directory") {
        let p = e.expect("directory entry").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Production corpus: concatenation of all crate sources except `config.rs`, test modules
/// removed, comments neutralised.
fn production_corpus() -> String {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    files.sort();

    let mut corpus = String::new();
    for f in &files {
        if f.file_name().is_some_and(|n| n == "config.rs") {
            continue;
        }
        let raw = fs::read_to_string(f).expect("reading a production source");
        corpus.push_str(&strip_tests_and_comments(&raw));
        corpus.push('\n');
    }
    corpus
}

/// Raw `config.rs` source, untouched (for annotation-marker verification).
fn config_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("config.rs");
    fs::read_to_string(path).expect("reading config.rs")
}

/// True if `field` carries a `not yet wired` marker within the ~500 characters preceding
/// its `pub <field>:` declaration in `config.rs` (i.e. within its own doc block), without
/// slicing on a byte that is not a char boundary (`config.rs` holds multibyte em dashes).
fn has_not_wired_marker(config_src: &str, field: &str) -> bool {
    let decl = format!("pub {field}:");
    let Some(idx) = config_src.find(&decl) else {
        return false; // field not found → fail (forces investigation)
    };
    match config_src[..idx].rfind("not yet wired") {
        Some(m) => idx - m < 500,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Scanner self-check.
// ---------------------------------------------------------------------------

/// The stripper must neither over-remove (keep production code) nor under-remove (drop
/// test modules). Without this guard a scanner regression would make the test pass blind.
#[test]
fn scanner_is_sound() {
    let corpus = production_corpus();
    assert!(
        corpus.contains("spawn_internal_listener"),
        "corpus must keep production code (fn spawn_internal_listener) — over-stripping?"
    );
    assert!(
        !corpus.contains("#[test]"),
        "corpus must contain no #[test] — a test module survived stripping"
    );
    assert!(
        !corpus.contains("mod tests"),
        "corpus must contain no `mod tests` — test module not removed"
    );
    // Name-agnostic structural witness: any `#[cfg(test)]` / `#[cfg(all(test…))]` region
    // must be gone after stripping, regardless of the module name.
    assert!(
        !corpus.contains("#[cfg(test)]"),
        "corpus must contain no `#[cfg(test)]` — a test-gated region survived stripping"
    );
    assert!(
        !corpus.contains("cfg(all(test"),
        "corpus must contain no `cfg(all(test…))` — a composite test-gated region survived"
    );
}

// ---------------------------------------------------------------------------
// The F-157 guard.
// ---------------------------------------------------------------------------

/// Every field of every deserialised `gradatum-server` config struct reaches a
/// consumption site, OR is a documented cross-crate mirror (asserted absent here).
///
/// Verified by scanning the sources — not by re-reading. Removing a `Wired` field's
/// consumption makes this red by naming the field; wiring a `Mirror` field server-side
/// makes it red too. Adding/removing a field breaks compilation (exhaustive destructuring).
#[test]
fn every_server_config_field_reaches_a_consumer_or_is_mirrored() {
    let corpus = production_corpus();
    let config_src = config_source();
    let mut orphans: Vec<String> = Vec::new();
    let mut wired = 0usize;
    let mut validated = 0usize;
    let mut parsed_only = 0usize;
    let mut mirrored = 0usize;

    for (struct_name, entries) in full_registry() {
        for e in &entries {
            match &e.wiring {
                Wiring::Wired { proof } => {
                    wired += 1;
                    if !reads(&corpus, proof) {
                        orphans.push(format!(
                            "`{}::{}` classified Wired but its consumption token `{}` is \
                             absent from production sources — wiring removed?",
                            struct_name, e.name, proof
                        ));
                    }
                }
                Wiring::Validated { validation_marker } => {
                    validated += 1;
                    // The field reaches config-load validation only; the marker proves that
                    // load-time check still exists in config.rs.
                    if !config_src.contains(validation_marker) {
                        orphans.push(format!(
                            "`{}::{}` classified Validated but its validation marker `{}` is \
                             absent from config.rs — the load-time check was removed?",
                            struct_name, e.name, validation_marker
                        ));
                    }
                }
                Wiring::NotWired { consumption_token } => {
                    parsed_only += 1;
                    // (a) the `// not yet wired` marker must sit near the declaration.
                    if !has_not_wired_marker(&config_src, e.name) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but the `not yet wired` marker is \
                             absent near its declaration in config.rs",
                            struct_name, e.name
                        ));
                    }
                    // (b) it must NOT be consumed (else it was wired without reclassifying).
                    if reads(&corpus, consumption_token) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but its consumption token `{}` now \
                             appears in production — it was wired: reclassify it Wired",
                            struct_name, e.name, consumption_token
                        ));
                    }
                }
                Wiring::Mirror {
                    consumer,
                    absent_token,
                } => {
                    mirrored += 1;
                    if reads(&corpus, absent_token) {
                        orphans.push(format!(
                            "`{}::{}` classified Mirror (consumed by `{}`) but its token `{}` \
                             now appears in this crate's production — it was wired server-side: \
                             reclassify it Wired",
                            struct_name, e.name, consumer, absent_token
                        ));
                    }
                }
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} server config field(s) in wiring default \
         (wired={}, validated={}, parsed_only={}, mirrored={}):\n  - {}",
        orphans.len(),
        wired,
        validated,
        parsed_only,
        mirrored,
        orphans.join("\n  - ")
    );
}
