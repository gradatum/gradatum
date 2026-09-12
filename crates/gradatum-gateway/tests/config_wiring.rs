//! F-157 — configuration wiring guard for `gradatum-gateway` (volet 3).
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. The volet 1
//! (`gradatum-engine`) and volet 2 (`gradatum-server`) guards locked that class down for
//! their config structs; this volet extends the same mechanism to the gateway's
//! [`gradatum_gateway::config::Config`] tree (server, router, providers, aliases, …).
//!
//! ## Mechanism (identical to the earlier volets, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) per config
//!    struct. Adding/removing a field breaks this test's compilation until the field is
//!    named in the destructuring AND passed to [`entry`] (hence classified).
//! 2. **Classification registry** — each field is `Wired { proof }` or `NotWired`. `proof`
//!    is a discriminating token that MUST appear in the crate's **production** sources.
//!    Removing the wiring removes the token → RED.
//! 3. **Production scan** — every crate source except `config.rs` (where the structs and
//!    their defaults live), with `#[cfg(test)]` modules stripped and comments neutralised
//!    (a token merely mentioned in a comment is not a real consumption).
//!
//! ## Token discrimination (gateway-specific)
//!
//! The gateway threads config as `config.<section>.<field>` / `state.config.<section>`, so
//! a `<section>.<field>` or a binding-qualified token disambiguates the many homonym leaf
//! fields (`endpoint`, `model`, `enabled`, `timeout_*`). Recurring indirections force
//! non-nominal proofs:
//!   - **shared field names across structs** — `endpoint` exists on `ProviderConfig`,
//!     `RouterConfig` and `VaultAwareConfig`; `model` on `RouterConfig` and `AliasTarget`.
//!     Each proof carries the qualifying binding (`provider_cfg.endpoint`,
//!     `router.endpoint`, `alias.model`, `cfg.model` at the router builder) so a bare
//!     `.endpoint` cannot mask another struct's read.
//!   - **chained multi-line reads** — `state.config.messages.model_map.get(..)` spreads the
//!     access over several lines, so `messages.model_map` never appears as one token; the
//!     proof is the last discriminating segment on its own line (`.model_map`).
//!   - **`Config` has no `Default`** — the sample tree is built from explicit literals plus
//!     the sub-structs' own `Default` / `AliasTarget::simple`.
//!
//! All gateway config is consumed within this crate (a service binary), so — unlike the
//! server volet — no cross-crate `Mirror` variant is needed.
//!
//! Run with: `cargo test -p gradatum-gateway --test config_wiring`

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use gradatum_gateway::config::{
    AgentAwareParams, AliasTarget, Config, LoggingConfig, MessagesConfig, ProviderConfig,
    RouterConfig, ServerConfig, VaultAwareConfig,
};

/// Classification of a config field with respect to its runtime consumption.
enum Wiring {
    /// The field is wired: its value reaches a consumption site. `proof` is a discriminating
    /// token that must appear in the crate's production sources (excluding `config.rs`,
    /// excluding test modules). Removing the wiring removes the token.
    Wired { proof: &'static str },
    /// The field is deliberately parsed-but-not-consumed. It MUST carry a `// not yet wired`
    /// marker near its declaration in `config.rs`, and MUST NOT be read in production.
    ///
    /// No gateway config field is currently `NotWired` (all are wired). The variant — and
    /// its detection branch — is kept as the classification vocabulary: a future
    /// parsed-but-unconsumed field must be classified here.
    #[expect(
        dead_code,
        reason = "F-157 classification vocabulary kept for a future NotWired field"
    )]
    NotWired,
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

/// Top-level [`Config`]. Each field is a section (struct or map); its proof reuses a
/// discriminating token established for that section's consumption.
fn config_registry(c: &Config) -> Vec<Entry> {
    let Config {
        server,
        logging,
        providers,
        aliases,
        gateway,
        vault_aware,
        messages,
        router,
    } = c;
    vec![
        entry(
            "server",
            server,
            Wiring::Wired {
                proof: "config.server",
            },
        ),
        entry(
            "logging",
            logging,
            Wiring::Wired {
                proof: "config.logging",
            },
        ),
        entry(
            "providers",
            providers,
            Wiring::Wired {
                proof: "config.providers",
            },
        ),
        entry(
            "aliases",
            aliases,
            Wiring::Wired {
                proof: "config.aliases",
            },
        ),
        entry(
            "gateway",
            gateway,
            Wiring::Wired {
                proof: "config.gateway",
            },
        ),
        entry(
            "vault_aware",
            vault_aware,
            Wiring::Wired {
                proof: "config.vault_aware",
            },
        ),
        entry(
            "messages",
            messages,
            Wiring::Wired {
                proof: "config.messages",
            },
        ),
        entry(
            "router",
            router,
            Wiring::Wired {
                proof: "config.router",
            },
        ),
    ]
}

fn server_registry(c: &ServerConfig) -> Vec<Entry> {
    let ServerConfig {
        listen,
        registry_db,
        bearer_token_env,
        rate_limit_per_minute,
        circuit_threshold,
        circuit_window_secs,
        circuit_cooldown_secs,
        max_total_tokens,
        trust_localhost,
        enable_slot_passthrough,
        allowed_origins,
        max_tools_per_request,
    } = c;
    vec![
        entry(
            "listen",
            listen,
            Wiring::Wired {
                proof: "server.listen",
            },
        ),
        // `main.rs`: `config\n.server\n.registry_db` — proof is the field read (leading dot);
        // the `registry_db: None` literals in fixtures carry no dot.
        entry(
            "registry_db",
            registry_db,
            Wiring::Wired {
                proof: ".registry_db",
            },
        ),
        entry(
            "bearer_token_env",
            bearer_token_env,
            Wiring::Wired {
                proof: ".bearer_token_env",
            },
        ),
        entry(
            "rate_limit_per_minute",
            rate_limit_per_minute,
            Wiring::Wired {
                proof: "server.rate_limit_per_minute",
            },
        ),
        entry(
            "circuit_threshold",
            circuit_threshold,
            Wiring::Wired {
                proof: "server.circuit_threshold",
            },
        ),
        entry(
            "circuit_window_secs",
            circuit_window_secs,
            Wiring::Wired {
                proof: "server.circuit_window_secs",
            },
        ),
        entry(
            "circuit_cooldown_secs",
            circuit_cooldown_secs,
            Wiring::Wired {
                proof: "server.circuit_cooldown_secs",
            },
        ),
        entry(
            "max_total_tokens",
            max_total_tokens,
            Wiring::Wired {
                proof: "server.max_total_tokens",
            },
        ),
        entry(
            "trust_localhost",
            trust_localhost,
            Wiring::Wired {
                proof: "server.trust_localhost",
            },
        ),
        entry(
            "enable_slot_passthrough",
            enable_slot_passthrough,
            Wiring::Wired {
                proof: "server.enable_slot_passthrough",
            },
        ),
        entry(
            "allowed_origins",
            allowed_origins,
            Wiring::Wired {
                proof: "server.allowed_origins",
            },
        ),
        entry(
            "max_tools_per_request",
            max_tools_per_request,
            Wiring::Wired {
                proof: "server.max_tools_per_request",
            },
        ),
    ]
}

fn logging_registry(c: &LoggingConfig) -> Vec<Entry> {
    let LoggingConfig { level } = c;
    vec![entry(
        "level",
        level,
        Wiring::Wired {
            proof: "logging.level",
        },
    )]
}

/// [`RouterConfig`]: all fields are read inside `RouterClient::from_config` (router.rs)
/// under a `cfg: &RouterConfig` binding, except `endpoint` which is also logged from
/// `config.router.endpoint`. `cfg.model` / `cfg.timeout_ms` / `cfg.max_concurrent` /
/// `cfg.query_head_chars` disambiguate from `AliasTarget.model` and the `max_concurrent`
/// mention inside a `Display` string.
fn router_registry(c: &RouterConfig) -> Vec<Entry> {
    let RouterConfig {
        enabled,
        endpoint,
        model,
        timeout_ms,
        max_concurrent,
        query_head_chars,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "cfg.enabled",
            },
        ),
        entry(
            "endpoint",
            endpoint,
            Wiring::Wired {
                proof: "router.endpoint",
            },
        ),
        entry("model", model, Wiring::Wired { proof: "cfg.model" }),
        entry(
            "timeout_ms",
            timeout_ms,
            Wiring::Wired {
                proof: "cfg.timeout_ms",
            },
        ),
        entry(
            "max_concurrent",
            max_concurrent,
            Wiring::Wired {
                proof: "cfg.max_concurrent",
            },
        ),
        entry(
            "query_head_chars",
            query_head_chars,
            Wiring::Wired {
                proof: "cfg.query_head_chars",
            },
        ),
    ]
}

fn messages_registry(c: &MessagesConfig) -> Vec<Entry> {
    let MessagesConfig {
        default_alias,
        model_map,
    } = c;
    vec![
        entry(
            "default_alias",
            default_alias,
            Wiring::Wired {
                proof: "messages.default_alias",
            },
        ),
        // chained multi-line read `state.config.messages.model_map.get(..)`.
        entry(
            "model_map",
            model_map,
            Wiring::Wired {
                proof: ".model_map",
            },
        ),
    ]
}

/// [`ProviderConfig`] (values of the `providers` map). Read in `provider_pool.rs`
/// (`cfg.endpoint`/`cfg.timeout_secs`/`cfg.api_key_env`) and in the chat/embeddings
/// handlers (`provider_cfg.endpoint`/`provider_cfg.timeout_secs`).
fn provider_registry(c: &ProviderConfig) -> Vec<Entry> {
    let ProviderConfig {
        endpoint,
        timeout_secs,
        api_key_env,
    } = c;
    vec![
        entry(
            "endpoint",
            endpoint,
            Wiring::Wired {
                proof: "provider_cfg.endpoint",
            },
        ),
        entry(
            "timeout_secs",
            timeout_secs,
            Wiring::Wired {
                proof: ".timeout_secs",
            },
        ),
        entry(
            "api_key_env",
            api_key_env,
            Wiring::Wired {
                proof: ".api_key_env",
            },
        ),
    ]
}

/// [`AliasTarget`] (values of the `aliases` map). Read in the chat/embeddings handlers
/// and in `smart_router.rs` under an `alias: &AliasTarget` binding.
fn alias_registry(c: &AliasTarget) -> Vec<Entry> {
    let AliasTarget {
        provider,
        model,
        fallback_provider,
        fallback_model,
        temperature_default,
        max_tokens_default,
        vision_capable,
    } = c;
    vec![
        entry(
            "provider",
            provider,
            Wiring::Wired {
                proof: "alias.provider",
            },
        ),
        entry(
            "model",
            model,
            Wiring::Wired {
                proof: "alias.model",
            },
        ),
        entry(
            "fallback_provider",
            fallback_provider,
            Wiring::Wired {
                proof: ".fallback_provider",
            },
        ),
        entry(
            "fallback_model",
            fallback_model,
            Wiring::Wired {
                proof: ".fallback_model",
            },
        ),
        entry(
            "temperature_default",
            temperature_default,
            Wiring::Wired {
                proof: ".temperature_default",
            },
        ),
        entry(
            "max_tokens_default",
            max_tokens_default,
            Wiring::Wired {
                proof: ".max_tokens_default",
            },
        ),
        entry(
            "vision_capable",
            vision_capable,
            Wiring::Wired {
                proof: ".vision_capable",
            },
        ),
    ]
}

/// [`AgentAwareParams`] (values of the `gateway` map). Read in `smart_router.rs` under a
/// `p: &AgentAwareParams` closure binding (`p.temperature` / `p.max_tokens` /
/// `p.alias_override`) — bare `.temperature` / `.max_tokens` collide with the request body.
fn agent_aware_registry(c: &AgentAwareParams) -> Vec<Entry> {
    let AgentAwareParams {
        temperature,
        max_tokens,
        alias_override,
    } = c;
    vec![
        entry(
            "temperature",
            temperature,
            Wiring::Wired {
                proof: "p.temperature",
            },
        ),
        entry(
            "max_tokens",
            max_tokens,
            Wiring::Wired {
                proof: "p.max_tokens",
            },
        ),
        entry(
            "alias_override",
            alias_override,
            Wiring::Wired {
                proof: "p.alias_override",
            },
        ),
    ]
}

fn vault_aware_registry(c: &VaultAwareConfig) -> Vec<Entry> {
    let VaultAwareConfig {
        event_log_endpoint,
        batch_size,
        flush_interval_secs,
    } = c;
    vec![
        entry(
            "event_log_endpoint",
            event_log_endpoint,
            Wiring::Wired {
                proof: ".event_log_endpoint",
            },
        ),
        entry(
            "batch_size",
            batch_size,
            Wiring::Wired {
                proof: ".batch_size",
            },
        ),
        entry(
            "flush_interval_secs",
            flush_interval_secs,
            Wiring::Wired {
                proof: ".flush_interval_secs",
            },
        ),
    ]
}

/// Aggregates every struct's registry. `Config` has no `Default`, so the sample tree is
/// built with explicit literals plus the sub-structs' own `Default` / `AliasTarget::simple`
/// (all fields `pub`, none `#[non_exhaustive]`).
fn full_registry() -> Vec<(&'static str, Vec<Entry>)> {
    let server = ServerConfig {
        listen: String::new(),
        registry_db: None,
        bearer_token_env: None,
        rate_limit_per_minute: 0,
        circuit_threshold: 0,
        circuit_window_secs: 0,
        circuit_cooldown_secs: 0,
        max_total_tokens: 0,
        trust_localhost: false,
        enable_slot_passthrough: false,
        allowed_origins: vec![],
        max_tools_per_request: 0,
    };
    let provider = ProviderConfig {
        endpoint: String::new(),
        timeout_secs: 0,
        api_key_env: None,
    };
    let alias = AliasTarget::simple("", "");
    let agent = AgentAwareParams::default();
    let config = Config {
        server: server.clone(),
        logging: LoggingConfig::default(),
        providers: BTreeMap::new(),
        aliases: HashMap::new(),
        gateway: HashMap::new(),
        vault_aware: VaultAwareConfig::default(),
        messages: MessagesConfig::default(),
        router: RouterConfig::default(),
    };

    vec![
        ("Config", config_registry(&config)),
        ("ServerConfig", server_registry(&config.server)),
        ("LoggingConfig", logging_registry(&config.logging)),
        ("RouterConfig", router_registry(&config.router)),
        ("MessagesConfig", messages_registry(&config.messages)),
        (
            "VaultAwareConfig",
            vault_aware_registry(&config.vault_aware),
        ),
        ("ProviderConfig", provider_registry(&provider)),
        ("AliasTarget", alias_registry(&alias)),
        ("AgentAwareParams", agent_aware_registry(&agent)),
    ]
}

// ---------------------------------------------------------------------------
// Production source scan (strips #[cfg(test)] modules) — verbatim from earlier volets.
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
/// literal prefix (`.`, `<section>.`, `<binding>.`).
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

// ---------------------------------------------------------------------------
// Scanner self-check.
// ---------------------------------------------------------------------------

/// The stripper must neither over-remove (keep production code) nor under-remove (drop
/// test modules). Without this guard a scanner regression would make the test pass blind.
#[test]
fn scanner_is_sound() {
    let corpus = production_corpus();
    assert!(
        corpus.contains("from_config"),
        "corpus must keep production code (fn from_config) — over-stripping?"
    );
    assert!(
        !corpus.contains("#[test]"),
        "corpus must contain no #[test] — a test module survived stripping"
    );
    assert!(
        !corpus.contains("mod tests"),
        "corpus must contain no `mod tests` — test module not removed"
    );
    // Name-agnostic structural witness: any `#[cfg(test)]` region must be gone after
    // stripping, regardless of the module name.
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

/// Every field of every deserialised `gradatum-gateway` config struct reaches a
/// consumption site.
///
/// Verified by scanning the sources — not by re-reading. Removing a `Wired` field's
/// consumption makes this red by naming the field. Adding/removing a field breaks
/// compilation (exhaustive destructuring).
#[test]
fn every_gateway_config_field_reaches_a_consumer() {
    let corpus = production_corpus();
    let mut orphans: Vec<String> = Vec::new();
    let mut wired = 0usize;

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
                Wiring::NotWired => {
                    orphans.push(format!(
                        "`{}::{}` classified NotWired — unexpected on the gateway; if a \
                         parsed-but-unconsumed field was added, wire the `config.rs` marker \
                         check as in the engine volet",
                        struct_name, e.name
                    ));
                }
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} gateway config field(s) in wiring default (wired={}):\n  - {}",
        orphans.len(),
        wired,
        orphans.join("\n  - ")
    );
}
