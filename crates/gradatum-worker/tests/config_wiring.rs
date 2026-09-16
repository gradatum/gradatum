//! F-157 — configuration wiring guard for `gradatum-worker` (volet 5).
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. The volet 1
//! (`gradatum-engine`) and volet 2 (`gradatum-server`) guards locked that class down for
//! their config structs; this volet extends the same mechanism to the worker's TOML
//! sections (`[apalis]`, `[distill_cron]`, `[multi_tenant]`, `[apalis.metrics]`,
//! `[apalis.health]`, `[curator]` / `[curator.llm]`).
//!
//! ## The finding of this volet
//!
//! [`WorkerLlmConfig::max_tokens`] (`[curator.llm] max_tokens`) is parsed but **never
//! read**: only the top-level `[curator] llm_review_max_tokens` is propagated into the
//! pipeline (see the `From<&WorkerCuratorConfig>` impl). It is documented as redundant and
//! deliberately not propagated, so it is classified [`Wiring::NotWired`] here (the guard
//! asserts the marker persists and the field stays unread). Wiring it would decide which
//! of two `max_tokens` sources wins — a runtime-behaviour change, out of this test's
//! scope; the finding is reported rather than silently fixed.
//!
//! ## Coverage boundary
//!
//! Ten config structs are exposed through the worker's `lib` surface and covered here.
//! Two more are **not** reachable from an external integration test and are left
//! uncovered by design (reported as a follow-up, not silently dropped):
//!   - `WorkerEmbedConfig` (`[embed]`) — a private DTO defined in `main.rs` (the binary
//!     root), absent from the `lib` surface tests compile against.
//!   - `WorkerStorageConfig` (`[storage] root`) — a module-private struct in `queue_path`;
//!     covering it would require widening its visibility, avoided here (test-additive).
//!
//! ## Mechanism (identical to the earlier volets, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) per struct.
//! 2. **Classification registry** — `Wired { proof }` or `NotWired { decl_file, marker }`.
//! 3. **Production scan** — all crate sources (worker has no `config.rs`), `#[cfg(test)]`
//!    stripped, comments neutralised. Config struct declarations are in scanned files, but
//!    a bare `field:` declaration carries no leading dot, so binding-qualified proofs
//!    (`w.backend`, `l.base_url`, `workers.curate`, `mt.enabled`, …) never match a decl.
//!
//! Run with: `cargo test -p gradatum-worker --test config_wiring`

use std::fs;
use std::path::{Path, PathBuf};

use gradatum_worker::apalis_handlers::MultiTenantCfg;
use gradatum_worker::curator_loader::{WorkerCuratorConfig, WorkerLlmConfig};
use gradatum_worker::health::HealthServerConfig;
use gradatum_worker::metrics::MetricsConfig;
use gradatum_worker::monitor::{ApalisConfig, WorkerConfig, WorkersConfig};
use gradatum_worker::schedules::{DistillCronConfig, ScheduleConfig};

/// Classification of a config field with respect to its runtime consumption.
enum Wiring {
    /// Wired: its value reaches a consumption site. `proof` is a discriminating token
    /// that must appear in production sources (test modules stripped). Removing the
    /// wiring removes the token → RED.
    Wired { proof: &'static str },
    /// Deliberately parsed-but-not-consumed. `decl_file` is the crate source (relative to
    /// `src/`) where the field is declared; `marker` is the doc phrase that must persist
    /// near the `pub <field>:` declaration. The field must ALSO stay unread in production
    /// (a `.<field>` read → it was wired without reclassification → RED).
    NotWired {
        decl_file: &'static str,
        marker: &'static str,
    },
}

/// One registry entry: a field name plus its classification.
struct Entry {
    name: &'static str,
    wiring: Wiring,
}

/// Builds an entry, **consuming** the binding produced by the destructuring. The unused
/// binding forces the mechanical link between the exhaustive destructuring and the
/// registry (adding a field forces naming it, else E0027; silenced only by an entry).
fn entry<T>(name: &'static str, _binding: &T, wiring: Wiring) -> Entry {
    Entry { name, wiring }
}

// ---------------------------------------------------------------------------
// Registries — one exhaustive destructuring per config struct.
// ---------------------------------------------------------------------------

fn apalis_registry(c: &ApalisConfig) -> Vec<Entry> {
    let ApalisConfig { workers, schedules } = c;
    vec![
        entry("workers", workers, Wiring::Wired { proof: ".workers" }),
        entry(
            "schedules",
            schedules,
            Wiring::Wired {
                proof: ".schedules",
            },
        ),
    ]
}

/// [`WorkersConfig`]: each kind read as `config.workers.<kind>` in `build_monitor`.
fn workers_registry(c: &WorkersConfig) -> Vec<Entry> {
    let WorkersConfig {
        curate,
        embed,
        reindex,
        purge,
        forget,
        distill,
        validate,
        privacy_rescan,
    } = c;
    vec![
        entry(
            "curate",
            curate,
            Wiring::Wired {
                proof: "workers.curate",
            },
        ),
        entry(
            "embed",
            embed,
            Wiring::Wired {
                proof: "workers.embed",
            },
        ),
        entry(
            "reindex",
            reindex,
            Wiring::Wired {
                proof: "workers.reindex",
            },
        ),
        entry(
            "purge",
            purge,
            Wiring::Wired {
                proof: "workers.purge",
            },
        ),
        entry(
            "forget",
            forget,
            Wiring::Wired {
                proof: "workers.forget",
            },
        ),
        entry(
            "distill",
            distill,
            Wiring::Wired {
                proof: "workers.distill",
            },
        ),
        entry(
            "validate",
            validate,
            Wiring::Wired {
                proof: "workers.validate",
            },
        ),
        entry(
            "privacy_rescan",
            privacy_rescan,
            Wiring::Wired {
                proof: "workers.privacy_rescan",
            },
        ),
    ]
}

/// [`WorkerConfig`]: `concurrency` read as `.concurrency`, `timeout_secs`/`max_retries`
/// read from the per-kind `cfg` binding in `build_monitor`.
fn worker_registry(c: &WorkerConfig) -> Vec<Entry> {
    let WorkerConfig {
        concurrency,
        timeout_secs,
        max_retries,
    } = c;
    vec![
        entry(
            "concurrency",
            concurrency,
            Wiring::Wired {
                proof: ".concurrency",
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
            "max_retries",
            max_retries,
            Wiring::Wired {
                proof: ".max_retries",
            },
        ),
    ]
}

/// [`ScheduleConfig`]: read under a `sched_cfg` binding in `build_monitor`; `cron` also
/// via `self.cron` in `schedule()`.
fn schedule_registry(c: &ScheduleConfig) -> Vec<Entry> {
    let ScheduleConfig {
        name,
        cron,
        retention_days,
    } = c;
    vec![
        entry(
            "name",
            name,
            Wiring::Wired {
                proof: "sched_cfg.name",
            },
        ),
        entry(
            "cron",
            cron,
            Wiring::Wired {
                proof: "sched_cfg.cron",
            },
        ),
        entry(
            "retention_days",
            retention_days,
            Wiring::Wired {
                proof: ".retention_days",
            },
        ),
    ]
}

/// [`DistillCronConfig`]: `enabled` read as `distill_cron.enabled` in `build_monitor`;
/// the rest under a `cfg`/`self` binding in the cron handler.
fn distill_cron_registry(c: &DistillCronConfig) -> Vec<Entry> {
    let DistillCronConfig {
        enabled,
        cron,
        sections,
        pressure_min,
        max_jobs_per_tick,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "distill_cron.enabled",
            },
        ),
        entry("cron", cron, Wiring::Wired { proof: "self.cron" }),
        entry("sections", sections, Wiring::Wired { proof: ".sections" }),
        entry(
            "pressure_min",
            pressure_min,
            Wiring::Wired {
                proof: ".pressure_min",
            },
        ),
        entry(
            "max_jobs_per_tick",
            max_jobs_per_tick,
            Wiring::Wired {
                proof: ".max_jobs_per_tick",
            },
        ),
    ]
}

fn multi_tenant_registry(c: &MultiTenantCfg) -> Vec<Entry> {
    let MultiTenantCfg { enabled } = c;
    // read as `mt.enabled` in the handlers' tenant guard.
    vec![entry(
        "enabled",
        enabled,
        Wiring::Wired {
            proof: "mt.enabled",
        },
    )]
}

/// [`MetricsConfig`] and [`HealthServerConfig`] both spawn from a `config` binding and read
/// `config.bind` / `config.port`. LIMIT ASSUMED: `.bind` / `.port` are shared tokens across
/// the two spawn fns — unwiring one alone may not turn this red while the other reads its
/// own. `metrics.enabled` is qualified (`metrics_cfg.enabled`) to avoid the many `.enabled`.
fn metrics_registry(c: &MetricsConfig) -> Vec<Entry> {
    let MetricsConfig {
        enabled,
        bind,
        port,
    } = c;
    vec![
        entry(
            "enabled",
            enabled,
            Wiring::Wired {
                proof: "metrics_cfg.enabled",
            },
        ),
        entry(
            "bind",
            bind,
            Wiring::Wired {
                proof: "config.bind",
            },
        ),
        entry(
            "port",
            port,
            Wiring::Wired {
                proof: "config.port",
            },
        ),
    ]
}

fn health_registry(c: &HealthServerConfig) -> Vec<Entry> {
    let HealthServerConfig { bind, port } = c;
    vec![
        entry(
            "bind",
            bind,
            Wiring::Wired {
                proof: "config.bind",
            },
        ),
        entry(
            "port",
            port,
            Wiring::Wired {
                proof: "config.port",
            },
        ),
    ]
}

/// [`WorkerCuratorConfig`]: gating fields propagated into `CuratorPipelineConfig` via the
/// `From` impl (`w.<field>`); the three deprecated `llm_review_{endpoint,model,timeout_ms}`
/// keys are read by `deprecated_review_override_keys` (`cfg.<field>.is_some()`) to warn the
/// operator — a consumption, hence Wired.
fn curator_registry(c: &WorkerCuratorConfig) -> Vec<Entry> {
    let WorkerCuratorConfig {
        backend,
        llm,
        heuristic_admit_threshold,
        heuristic_default_status,
        llm_review_enabled,
        confidence_threshold,
        llm_review_endpoint,
        llm_review_model,
        llm_review_timeout_ms,
        llm_review_max_tokens,
        llm_review_fallback,
    } = c;
    vec![
        entry("backend", backend, Wiring::Wired { proof: "w.backend" }),
        entry("llm", llm, Wiring::Wired { proof: "w.llm" }),
        entry(
            "heuristic_admit_threshold",
            heuristic_admit_threshold,
            Wiring::Wired {
                proof: ".heuristic_admit_threshold",
            },
        ),
        entry(
            "heuristic_default_status",
            heuristic_default_status,
            Wiring::Wired {
                proof: ".heuristic_default_status",
            },
        ),
        entry(
            "llm_review_enabled",
            llm_review_enabled,
            Wiring::Wired {
                proof: ".llm_review_enabled",
            },
        ),
        entry(
            "confidence_threshold",
            confidence_threshold,
            Wiring::Wired {
                proof: ".confidence_threshold",
            },
        ),
        entry(
            "llm_review_endpoint",
            llm_review_endpoint,
            Wiring::Wired {
                proof: ".llm_review_endpoint",
            },
        ),
        entry(
            "llm_review_model",
            llm_review_model,
            Wiring::Wired {
                proof: ".llm_review_model",
            },
        ),
        entry(
            "llm_review_timeout_ms",
            llm_review_timeout_ms,
            Wiring::Wired {
                proof: ".llm_review_timeout_ms",
            },
        ),
        entry(
            "llm_review_max_tokens",
            llm_review_max_tokens,
            Wiring::Wired {
                proof: ".llm_review_max_tokens",
            },
        ),
        entry(
            "llm_review_fallback",
            llm_review_fallback,
            Wiring::Wired {
                proof: ".llm_review_fallback",
            },
        ),
    ]
}

/// [`WorkerLlmConfig`]: five fields propagated via the `From` impl (`l.<field>`).
/// `max_tokens` is the F-157 finding of this volet — parsed but never read (only
/// `[curator] llm_review_max_tokens` is propagated), documented redundant → `NotWired`.
fn worker_llm_registry(c: &WorkerLlmConfig) -> Vec<Entry> {
    let WorkerLlmConfig {
        backend,
        base_url,
        model,
        api_key_env,
        timeout_ms,
        max_tokens,
    } = c;
    vec![
        entry("backend", backend, Wiring::Wired { proof: "l.backend" }),
        entry(
            "base_url",
            base_url,
            Wiring::Wired {
                proof: "l.base_url",
            },
        ),
        entry("model", model, Wiring::Wired { proof: "l.model" }),
        entry(
            "api_key_env",
            api_key_env,
            Wiring::Wired {
                proof: "l.api_key_env",
            },
        ),
        entry(
            "timeout_ms",
            timeout_ms,
            Wiring::Wired {
                proof: "l.timeout_ms",
            },
        ),
        entry(
            "max_tokens",
            max_tokens,
            Wiring::NotWired {
                decl_file: "curator_loader.rs",
                marker: "not propagated",
            },
        ),
    ]
}

/// Aggregates every struct's registry. Samples come from `Default` where available, else
/// explicit literals (all fields `pub`, none `#[non_exhaustive]`).
fn full_registry() -> Vec<(&'static str, Vec<Entry>)> {
    let apalis = ApalisConfig::default();
    let worker = WorkerConfig::default();
    let schedule = ScheduleConfig {
        name: String::new(),
        cron: String::new(),
        retention_days: 1,
    };
    let distill = DistillCronConfig {
        enabled: false,
        cron: String::new(),
        sections: vec![],
        pressure_min: 1,
        max_jobs_per_tick: 1,
    };
    let metrics = MetricsConfig::default();
    let health = HealthServerConfig::default();
    let mt = MultiTenantCfg::default();
    let curator = WorkerCuratorConfig::default();
    let llm = WorkerLlmConfig {
        backend: String::new(),
        base_url: String::new(),
        model: String::new(),
        api_key_env: None,
        timeout_ms: 0,
        max_tokens: None,
    };

    vec![
        ("ApalisConfig", apalis_registry(&apalis)),
        ("WorkersConfig", workers_registry(&apalis.workers)),
        ("WorkerConfig", worker_registry(&worker)),
        ("ScheduleConfig", schedule_registry(&schedule)),
        ("DistillCronConfig", distill_cron_registry(&distill)),
        ("MultiTenantCfg", multi_tenant_registry(&mt)),
        ("MetricsConfig", metrics_registry(&metrics)),
        ("HealthServerConfig", health_registry(&health)),
        ("WorkerCuratorConfig", curator_registry(&curator)),
        ("WorkerLlmConfig", worker_llm_registry(&llm)),
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
/// literal prefix (`.`, `<binding>.`).
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

/// Production corpus: concatenation of all crate sources (the worker has no `config.rs` —
/// its config structs live across module files), test modules removed, comments
/// neutralised.
fn production_corpus() -> String {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    files.sort();

    let mut corpus = String::new();
    for f in &files {
        let raw = fs::read_to_string(f).expect("reading a production source");
        corpus.push_str(&strip_tests_and_comments(&raw));
        corpus.push('\n');
    }
    corpus
}

/// Raw source of `src/<file>` (unmodified — for the NotWired marker check, which must see
/// doc comments the corpus neutralises).
fn raw_source(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading src/{file}: {e}"))
}

/// `true` if `field` carries `marker` within the ~500 bytes preceding its `pub <field>:`
/// declaration in `raw` (its doc block).
fn has_marker(raw: &str, field: &str, marker: &str) -> bool {
    let decl = format!("pub {field}:");
    let Some(idx) = raw.find(&decl) else {
        return false; // declaration not found → force investigation (RED)
    };
    match raw[..idx].rfind(marker) {
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
        corpus.contains("build_monitor"),
        "corpus must keep production code (fn build_monitor) — over-stripping?"
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

/// Every field of every deserialised `gradatum-worker` config struct (lib surface) reaches
/// a consumption site, OR is a documented `NotWired` field (marker present + still unread).
///
/// Verified by scanning the sources — not by re-reading. Removing a `Wired` field's
/// consumption makes this red by naming the field; wiring a `NotWired` field (or dropping
/// its marker) makes it red too. Adding/removing a field breaks compilation.
#[test]
fn every_worker_config_field_reaches_a_consumer_or_is_annotated() {
    let corpus = production_corpus();
    let mut orphans: Vec<String> = Vec::new();
    let mut wired = 0usize;
    let mut not_wired = 0usize;

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
                Wiring::NotWired { decl_file, marker } => {
                    not_wired += 1;
                    let raw = raw_source(decl_file);
                    if !has_marker(&raw, e.name, marker) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but the marker `{}` disappeared near \
                             its declaration in {} — reclassify if it was wired",
                            struct_name, e.name, marker, decl_file
                        ));
                    }
                    let read_token = format!(".{}", e.name);
                    if reads(&corpus, &read_token) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but it is READ in production (`{}`) — \
                             it was wired: reclassify it Wired",
                            struct_name, e.name, read_token
                        ));
                    }
                }
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} worker config field(s) in wiring default (wired={}, not_wired={}):\n  - {}",
        orphans.len(),
        wired,
        not_wired,
        orphans.join("\n  - ")
    );
}
