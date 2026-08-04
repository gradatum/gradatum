//! C3 caveat: bind/TLS validation must fail-closed when binding non-loopback w/o TLS.

use gradatum_server::config::{
    AclConfig, AuthConfig, ConfigError, ContextConfig, CuratorConfig, EmbedConfig, EventLogConfig,
    HttpConfig, LogConfig, RateLimitConfig, ReviewPromoteConfig, RevocationStoreKind,
    SalienceConfig, ScoringConfig, SearchConfig, ServerConfig, SessionTraceConfig, StorageConfig,
    StudioConfig, TlsConfig,
};
use gradatum_server::proactive_recall::ProactiveRecallConfig;
use std::path::PathBuf;

fn cfg(bind: &str, tls: Option<TlsConfig>) -> ServerConfig {
    ServerConfig {
        server: HttpConfig {
            bind: bind.parse().unwrap(),
            metrics_bind: "127.0.0.1:19091".parse().unwrap(),
            tls,
        },
        storage: StorageConfig {
            root: PathBuf::from("/tmp"),
            vault_index_path: PathBuf::from("/tmp/db"),
            legacy_alias_used: false,
            vault_index_path_override_diverges: false,
        },
        auth: AuthConfig {
            jwt_ttl_human_secs: 3600,
            jwt_ttl_service_secs: 86400,
            revocation_store: RevocationStoreKind::Memory,
            revocation_db_path: None,
            api_keys_db_path: None,
        },
        acl: AclConfig {
            preset_path: PathBuf::from("/tmp/acl"),
        },
        log: LogConfig {
            format: "json".into(),
        },
        curator: CuratorConfig::default(),
        ratelimit: RateLimitConfig::default(),
        embed: EmbedConfig::default(),
        event_log: EventLogConfig::default(),
        session_trace: SessionTraceConfig::default(),
        archive: gradatum_server::config::ArchiveConfig::default(),
        scoring: ScoringConfig::default(),
        salience: SalienceConfig::default(),
        studio: StudioConfig::default(),
        internal_api: gradatum_server::config::InternalApiConfig::default(),
        search: SearchConfig::default(),
        review_promote: ReviewPromoteConfig::default(),
        audit: gradatum_server::config::AuditConfig::default(),
        downgrade: gradatum_server::config::DowngradeConfig::default(),
        context: ContextConfig::default(),
        proactive_recall: ProactiveRecallConfig::default(),
        multi_tenant: gradatum_server::config::MultiTenantConfig::default(),
        per_vault: std::collections::HashMap::new(),
    }
}

fn dummy_tls() -> TlsConfig {
    TlsConfig {
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
    }
}

#[test]
fn loopback_v4_no_tls_ok() {
    cfg("127.0.0.1:19090", None).validate_bind_tls().unwrap();
}

#[test]
fn loopback_v6_no_tls_ok() {
    cfg("[::1]:19090", None).validate_bind_tls().unwrap();
}

#[test]
fn wildcard_v4_no_tls_refused() {
    let err = cfg("0.0.0.0:19090", None).validate_bind_tls().unwrap_err();
    assert!(matches!(err, ConfigError::BindTlsRefused { .. }));
}

#[test]
fn lan_ip_no_tls_refused() {
    // 192.0.2.1 = RFC 5737 TEST-NET-1 (documentation range, non-loopback, non-TLS-exempt)
    let err = cfg("192.0.2.1:19090", None)
        .validate_bind_tls()
        .unwrap_err();
    assert!(matches!(err, ConfigError::BindTlsRefused { .. }));
}

#[test]
fn wildcard_v4_with_tls_ok() {
    cfg("0.0.0.0:19090", Some(dummy_tls()))
        .validate_bind_tls()
        .unwrap();
}
