//! F-157 — configuration wiring guard for `gradatum-worker` (volet 5).
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. The volet 1
//! (`gradatum-engine`) and volet 2 (`gradatum-server`) guards locked that class down for
//! their config structs; this volet extends the same mechanism to the worker's TOML
//! sections (`[apalis]`, `[distill_cron]`, `[multi_tenant]`, `[queue]`, `[apalis.metrics]`,
//! `[apalis.health]`, `[curator]` / `[curator.llm]`).
//!
//! ## The finding of this volet, and its resolution
//!
//! `[curator.llm] max_tokens` was parsed and **never read**: only the top-level
//! `[curator] llm_review_max_tokens` is propagated into the pipeline (see the
//! `From<&WorkerCuratorConfig>` impl). Wiring it would have made a second source live for
//! a value already arbitrated elsewhere, so the field has been **removed** instead of
//! wired. `[curator] llm_review_max_tokens` is the only key carrying the effect; a
//! `server.toml` still carrying the removed key keeps loading — the struct declares no
//! `deny_unknown_fields`, so the retired key is ignored rather than rejected.
//! `a_config_still_carrying_the_removed_llm_max_tokens_key_loads` below pins exactly that.
//!
//! With that field gone, no worker config field is deliberately left unread, so this
//! volet's guard carries **no exemption vocabulary**: every field must reach a consumer.
//!
//! ## Coverage boundary
//!
//! Eleven config structs are exposed through the worker's `lib` surface and covered here.
//! Two more are **not** reachable from an external integration test and are left
//! uncovered **on purpose** — widening a production visibility, or moving a DTO out of the
//! binary root, is a production change made for a test's convenience, and neither is
//! worth it:
//!   - `WorkerEmbedConfig` (`[embed]`) — a private DTO defined in `main.rs` (the binary
//!     root), absent from the `lib` surface tests compile against.
//!   - `WorkerStorageConfig` (`[storage] root`) — a module-private struct in `queue_path`
//!     whose `root` field is private too; covering it would take two widenings.
//!
//! The risk that matters is not these two but a **third** struct appearing unnoticed, so
//! `every_figment_extraction_site_is_whitelisted` below pins the exhaustive set of
//! extraction sites against a bounded whitelist and fails, naming the newcomer, if one is
//! added outside it. Assumed partial coverage, made visible — not silently dropped.
//!
//! ## Mechanism (identical to the earlier volets, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) per struct.
//! 2. **Consumption registry** — one witness token per field, required to occur in
//!    production sources.
//! 3. **Production scan** — all crate sources (worker has no `config.rs`), `#[cfg(test)]`
//!    stripped, comments neutralised. Config struct declarations are in scanned files, but
//!    a bare `field:` declaration carries no leading dot, so binding-qualified proofs
//!    (`w.backend`, `l.base_url`, `workers.curate`, `mt.enabled`, …) never match a decl.
//!
//! Run with: `cargo test -p gradatum-worker --test config_wiring`

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Format, Toml};
use gradatum_worker::apalis_handlers::MultiTenantCfg;
use gradatum_worker::curator_loader::{WorkerCuratorConfig, WorkerLlmConfig};
use gradatum_worker::health::HealthServerConfig;
use gradatum_worker::metrics::MetricsConfig;
use gradatum_worker::monitor::{ApalisConfig, WorkerConfig, WorkersConfig};
use gradatum_worker::schedules::{DistillCronConfig, QueueConfig, ScheduleConfig};

/// One registry entry: a config field name plus the production token that witnesses its
/// consumption. `proof` is a discriminating token that must appear in production sources
/// (test modules stripped, comments neutralised). Removing the wiring removes the token,
/// and the guard turns RED naming the field.
///
/// There is **no exemption variant**. The earlier volets of this guard carried a
/// `NotWired { decl_file, marker }` classification for a field parsed and deliberately
/// left unread; the worker's single such field (`[curator.llm] max_tokens`) has been
/// removed rather than wired, so every field of every covered struct now has to reach a
/// consumer. A field with no consumption site cannot be silenced here — only wired or
/// deleted, which is the property the guard exists to enforce.
struct Entry {
    name: &'static str,
    proof: &'static str,
}

/// Builds an entry, **consuming** the binding produced by the destructuring. The unused
/// binding forces the mechanical link between the exhaustive destructuring and the
/// registry (adding a field forces naming it, else E0027; silenced only by an entry).
fn entry<T>(name: &'static str, _binding: &T, proof: &'static str) -> Entry {
    Entry { name, proof }
}

// ---------------------------------------------------------------------------
// Registries — one exhaustive destructuring per config struct.
// ---------------------------------------------------------------------------

fn apalis_registry(c: &ApalisConfig) -> Vec<Entry> {
    let ApalisConfig { workers, schedules } = c;
    vec![
        entry("workers", workers, ".workers"),
        entry("schedules", schedules, ".schedules"),
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
        entry("curate", curate, "workers.curate"),
        entry("embed", embed, "workers.embed"),
        entry("reindex", reindex, "workers.reindex"),
        entry("purge", purge, "workers.purge"),
        entry("forget", forget, "workers.forget"),
        entry("distill", distill, "workers.distill"),
        entry("validate", validate, "workers.validate"),
        entry("privacy_rescan", privacy_rescan, "workers.privacy_rescan"),
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
        entry("concurrency", concurrency, ".concurrency"),
        entry("timeout_secs", timeout_secs, ".timeout_secs"),
        entry("max_retries", max_retries, ".max_retries"),
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
        entry("name", name, "sched_cfg.name"),
        entry("cron", cron, "sched_cfg.cron"),
        entry("retention_days", retention_days, ".retention_days"),
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
        entry("enabled", enabled, "distill_cron.enabled"),
        entry("cron", cron, "self.cron"),
        entry("sections", sections, ".sections"),
        entry("pressure_min", pressure_min, ".pressure_min"),
        entry("max_jobs_per_tick", max_jobs_per_tick, ".max_jobs_per_tick"),
    ]
}

/// [`QueueConfig`] (`[queue]`): both fields are read on the tick path — the cutoff in
/// `run_queue_purge_once` (`cfg.purge_after_secs`, journalised with the count) and in
/// `QueueConfig::purge_cutoff` (`self.purge_after_secs`); the cadence in
/// `QueueConfig::purge_interval` (`self.purge_interval_secs.max(60)`), which is what the
/// `tokio::time::interval` in `main.rs` is built from.
///
/// The keep window is deliberately **not** a field: it is `TERMINAL_PAYLOAD_KEEP` in
/// `gradatum-core`, a product constant documented as having no key, no environment
/// variable and no flag.
fn queue_registry(c: &QueueConfig) -> Vec<Entry> {
    let QueueConfig {
        purge_after_secs,
        purge_interval_secs,
    } = c;
    vec![
        entry("purge_after_secs", purge_after_secs, ".purge_after_secs"),
        entry(
            "purge_interval_secs",
            purge_interval_secs,
            ".purge_interval_secs",
        ),
    ]
}

fn multi_tenant_registry(c: &MultiTenantCfg) -> Vec<Entry> {
    let MultiTenantCfg { enabled } = c;
    // read as `mt.enabled` in the handlers' tenant guard.
    vec![entry("enabled", enabled, "mt.enabled")]
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
        entry("enabled", enabled, "metrics_cfg.enabled"),
        entry("bind", bind, "config.bind"),
        entry("port", port, "config.port"),
    ]
}

fn health_registry(c: &HealthServerConfig) -> Vec<Entry> {
    let HealthServerConfig { bind, port } = c;
    vec![
        entry("bind", bind, "config.bind"),
        entry("port", port, "config.port"),
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
        entry("backend", backend, "w.backend"),
        entry("llm", llm, "w.llm"),
        entry(
            "heuristic_admit_threshold",
            heuristic_admit_threshold,
            ".heuristic_admit_threshold",
        ),
        entry(
            "heuristic_default_status",
            heuristic_default_status,
            ".heuristic_default_status",
        ),
        entry(
            "llm_review_enabled",
            llm_review_enabled,
            ".llm_review_enabled",
        ),
        entry(
            "confidence_threshold",
            confidence_threshold,
            ".confidence_threshold",
        ),
        entry(
            "llm_review_endpoint",
            llm_review_endpoint,
            ".llm_review_endpoint",
        ),
        entry("llm_review_model", llm_review_model, ".llm_review_model"),
        entry(
            "llm_review_timeout_ms",
            llm_review_timeout_ms,
            ".llm_review_timeout_ms",
        ),
        entry(
            "llm_review_max_tokens",
            llm_review_max_tokens,
            ".llm_review_max_tokens",
        ),
        entry(
            "llm_review_fallback",
            llm_review_fallback,
            ".llm_review_fallback",
        ),
    ]
}

/// [`WorkerLlmConfig`]: every field propagated via the `From` impl (`l.<field>`). The
/// former `max_tokens` field — this volet's finding — was removed rather than wired; the
/// struct's own set is now exactly the set the pipeline consumes.
fn worker_llm_registry(c: &WorkerLlmConfig) -> Vec<Entry> {
    let WorkerLlmConfig {
        backend,
        base_url,
        model,
        api_key_env,
        timeout_ms,
    } = c;
    vec![
        entry("backend", backend, "l.backend"),
        entry("base_url", base_url, "l.base_url"),
        entry("model", model, "l.model"),
        entry("api_key_env", api_key_env, "l.api_key_env"),
        entry("timeout_ms", timeout_ms, "l.timeout_ms"),
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
    let queue = QueueConfig::default();
    let curator = WorkerCuratorConfig::default();
    let llm = WorkerLlmConfig {
        backend: String::new(),
        base_url: String::new(),
        model: String::new(),
        api_key_env: None,
        timeout_ms: 0,
    };

    vec![
        ("ApalisConfig", apalis_registry(&apalis)),
        ("WorkersConfig", workers_registry(&apalis.workers)),
        ("WorkerConfig", worker_registry(&worker)),
        ("ScheduleConfig", schedule_registry(&schedule)),
        ("DistillCronConfig", distill_cron_registry(&distill)),
        ("MultiTenantCfg", multi_tenant_registry(&mt)),
        ("QueueConfig", queue_registry(&queue)),
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

/// Crate root, resolved at run time.
///
/// The value baked in at compile time goes stale as soon as a test binary is reused
/// from another checkout directory (shared build artefacts): the environment variable
/// wins, and the compile-time value only serves as a fallback outside cargo.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// Production corpus: concatenation of all crate sources (the worker has no `config.rs` —
/// its config structs live across module files), test modules removed, comments
/// neutralised.
fn production_corpus() -> String {
    let src_dir = manifest_dir().join("src");
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
/// a consumption site. There is no exemption: a field with no consumption site fails here,
/// named.
///
/// Verified by scanning the sources — not by re-reading. Removing a field's consumption
/// makes this red by naming the field. Adding or removing a field breaks compilation.
#[test]
fn every_worker_config_field_reaches_a_consumer() {
    let corpus = production_corpus();
    let mut orphans: Vec<String> = Vec::new();
    let mut wired = 0usize;

    for (struct_name, entries) in full_registry() {
        for e in &entries {
            wired += 1;
            if !reads(&corpus, e.proof) {
                orphans.push(format!(
                    "`{}::{}` has no consumption site: its witness token `{}` is absent from \
                     production sources — wiring removed?",
                    struct_name, e.name, e.proof
                ));
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} worker config field(s) reach no consumer (checked={}):\n  - {}",
        orphans.len(),
        wired,
        orphans.join("\n  - ")
    );
}

/// A `server.toml` that still carries the removed `[curator.llm] max_tokens` key loads
/// unchanged.
///
/// The removal changes what the product *documents*, not what an existing file is allowed
/// to say: `WorkerLlmConfig` declares no `deny_unknown_fields`, so the retired key is
/// ignored exactly as any unknown key always was. A configuration that loaded before the
/// upgrade still loads after it — the operator is handed a guide to follow, not an outage
/// to repair.
#[test]
fn a_config_still_carrying_the_removed_llm_max_tokens_key_loads() {
    let toml = r#"
[llm]
backend = "openai_compat"
base_url = "http://example.invalid"
model = "example-model"
timeout_ms = 5000
max_tokens = 2048
"#;

    let cfg: WorkerCuratorConfig = Figment::new()
        .merge(Toml::string(toml))
        .extract()
        .expect("a config carrying the removed [curator.llm] max_tokens key must still load");

    let llm = cfg.llm.expect("the [curator.llm] section must be present");
    assert_eq!(
        llm.model, "example-model",
        "the surviving keys must keep their values around the ignored one"
    );
    assert_eq!(llm.timeout_ms, 5000);
}

// ---------------------------------------------------------------------------
// Coverage boundary, pinned rather than assumed.
// ---------------------------------------------------------------------------

/// Config types extracted from the server TOML at a figment site **and covered** by the
/// registries above — the crate's `lib` surface exposes all seven.
const COVERED_EXTRACTION_SITES: [&str; 7] = [
    "ApalisConfig",
    "DistillCronConfig",
    "HealthServerConfig",
    "MetricsConfig",
    "MultiTenantCfg",
    "QueueConfig",
    "WorkerCuratorConfig",
];

/// Config types extracted at a figment site but **not covered**, on purpose — named here
/// so the gap is a decision on the record rather than an omission.
///
///   - `WorkerEmbedConfig` — a private DTO in `main.rs`, the binary root. Reaching it from
///     `tests/` would mean moving it into the lib: a structural change to production code,
///     made for a test's convenience.
///   - `WorkerStorageConfig` — a module-private struct in `queue_path` whose `root` field
///     is private too. Reaching it would take two visibility widenings.
///
/// Neither is worth a production change; what is worth it is that a **third** struct cannot
/// appear unnoticed, which is what the test below enforces.
const UNCOVERED_EXTRACTION_SITES: [&str; 2] = ["WorkerEmbedConfig", "WorkerStorageConfig"];

/// The generic parameter of the `load_section` helper itself. Its signature is
/// `pub fn load_section<T>(` and its body forwards to `extract_inner::<T>`, so **both**
/// scan shapes meet it — at the helper's definition, not at a config site. It names no
/// config type, and no config struct in this crate is named by a single letter.
const HELPER_TYPE_PARAM: &str = "T";

/// The type named at a `load_section` binding site: `let <b>: <Type> = load_section(`.
/// `at` is the index just past the opening `(`. Returns the last path segment.
///
/// The check that the text between the `=` and the call is exactly `load_section(` is what
/// separates a **call site** from the helper's own **definition**: without it, the scan
/// walks back to an unrelated `=` earlier in the file and reports whatever preceded it.
fn type_bound_before(corpus: &str, at: usize) -> Option<String> {
    let head = &corpus[..at];
    let eq = head.rfind('=')?;
    if corpus[eq + 1..at].trim() != "load_section(" {
        return None;
    }
    let annotated = head[..eq].trim_end();
    let ty = annotated[annotated.rfind(':')? + 1..].trim();
    if ty.is_empty()
        || !ty
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    {
        return None;
    }
    Some(last_segment(ty))
}

/// The type named by a turbofish: `extract_inner::<Type>(`. `at` is the index just past
/// the opening `<`. Returns the last path segment.
fn type_in_turbofish(corpus: &str, at: usize) -> Option<String> {
    let ty = corpus[at..].split('>').next()?.trim();
    if ty.is_empty()
        || !ty
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    {
        return None;
    }
    Some(last_segment(ty))
}

/// `a::b::C` → `C`; a bare `C` is returned unchanged.
fn last_segment(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).trim().to_string()
}

/// Types extracted from the TOML at a figment site in production sources. Two shapes are
/// recognised, and they are the only two the crate uses: the `load_section` helper
/// (annotated binding) and an explicit `extract_inner::<T>` turbofish.
///
/// [`HELPER_TYPE_PARAM`] is dropped: it is the helper's own parameter, met at its
/// definition and inside its body, and never a config type.
fn extraction_sites(corpus: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (pattern, turbofish) in [("load_section(", false), ("extract_inner::<", true)] {
        let mut from = 0usize;
        while let Some(rel) = corpus[from..].find(pattern) {
            let at = from + rel + pattern.len();
            let named = if turbofish {
                type_in_turbofish(corpus, at)
            } else {
                type_bound_before(corpus, at)
            };
            if let Some(ty) = named
                && ty != HELPER_TYPE_PARAM
            {
                found.insert(ty);
            }
            from = at;
        }
    }
    found
}

/// Every figment extraction site in production is on the whitelist.
///
/// The guard above covers eleven structs; two extracted types are knowingly outside it. This
/// test does not widen anything — it makes the boundary **countable**. Each extraction site
/// is enumerated mechanically, and the set must equal the seven covered types plus the two
/// named exceptions, exactly. A struct extracted outside that set fails this test **naming
/// it**, so the coverage gap can grow only on purpose.
#[test]
fn every_figment_extraction_site_is_whitelisted() {
    let corpus = production_corpus();
    let found = extraction_sites(&corpus);

    let expected: BTreeSet<String> = COVERED_EXTRACTION_SITES
        .iter()
        .chain(UNCOVERED_EXTRACTION_SITES.iter())
        .map(|s| (*s).to_string())
        .collect();

    let newcomer: Vec<&String> = found.difference(&expected).collect();
    let vanished: Vec<&String> = expected.difference(&found).collect();
    assert!(
        newcomer.is_empty() && vanished.is_empty(),
        "figment extraction sites drifted from the whitelist.\n  \
         extracted but not whitelisted (a new site needs a decision): {newcomer:?}\n  \
         whitelisted but no longer extracted (stale entry): {vanished:?}\n  \
         found: {found:?}"
    );

    // The whitelist's `covered` half must still name structs the registries actually
    // cover — otherwise an entry could be moved to the uncovered half to silence a
    // regression, and the gap would read as a decision.
    let registered: BTreeSet<&str> = full_registry().into_iter().map(|(n, _)| n).collect();
    for name in COVERED_EXTRACTION_SITES {
        assert!(
            registered.contains(name),
            "`{name}` is listed as covered but no registry covers it — the coverage \
             whitelist and the registries disagree"
        );
    }
}
