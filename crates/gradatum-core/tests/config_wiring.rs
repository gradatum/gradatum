//! F-157 — configuration wiring guard for `gradatum-core` (volet 6).
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. The volets 1–3
//! (`gradatum-engine`, `gradatum-server`, `gradatum-gateway`) locked that class down for
//! the service crates; this volet extends it to [`gradatum_core::config::VaultConfig`],
//! the per-vault `config.toml` schema.
//!
//! ## Why core is different — and why the guard still fits
//!
//! `VaultConfig` is a **schema aggregator**: it is loaded in core
//! (`VaultConfig::load_from_root`) but its fields are consumed almost entirely by *other*
//! crates. Exactly one field is read inside core (`EmbedConfig::embeddable_status`, in
//! [`gradatum_core::status`]); the rest are either
//!   - **cross-crate mirrors** — the section is threaded whole to another crate:
//!     `StorageBackendConfig` → `gradatum-storage` (S3/fs factory), `HistoryConfig` and
//!     `VaultSection::default_tenant_id` → `gradatum-vault`; classified [`Wiring::Mirror`],
//!     asserted **absent** from core so an accidental core-side read forces reclassification,
//!   - or **deliberately inert** — a documented dead field kept for schema completeness or
//!     backward-loadability; classified [`Wiring::NotWired`], asserting both that its
//!     documented marker persists AND that it stays unread in core.
//!
//! The `.<field>` reads that would collide with core's domain vocabulary
//! (`embedder_id` is an ANN partition column, `root`/`endpoint`/`service` are storage
//! terms) do so only as **bare identifiers**; a dot-qualified `.embedder_id` /
//! `.root` never appears in core production, so the absent-tokens are clean.
//!
//! ## Findings of this volet
//!
//! - `VaultSection::schema_version` (`[vault] schema_version`) is parsed but read by **no
//!   crate** — the "strict schema version check" its doc alludes to is not implemented.
//!   Classified `NotWired` (marker: the existing "no strict version check" phrasing).
//! - The `[embed]`, `[curator]`, `[index]`, `[drift]`, `[audit]` sections are largely
//!   inert duplicates of the schema; the guard locks the ~24 documented dead fields so a
//!   future edit that wires one (or drops its marker) turns red.
//!
//! ## Mechanism (identical to the earlier volets, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) per struct.
//! 2. **Classification registry** — `Wired`/`NotWired`/`Mirror`/`Delegated` (see [`Wiring`]).
//! 3. **Production scan** — every core source except `config.rs` (where the structs and
//!    their markers live), `#[cfg(test)]` stripped, comments neutralised.
//!
//! Run with: `cargo test -p gradatum-core --test config_wiring`

use std::fs;
use std::path::{Path, PathBuf};

use gradatum_core::config::{
    AuditConfig, CuratorConfig, DriftConfig, EmbedConfig, HistoryConfig, IndexConfig,
    StorageBackendConfig, VaultConfig, VaultSection,
};

/// Classification of a config field with respect to its runtime consumption.
enum Wiring {
    /// Wired **inside core**: its value reaches a core consumption site. `proof` is a
    /// discriminating token that must appear in core production (excluding `config.rs`,
    /// excluding test modules).
    Wired { proof: &'static str },
    /// Deliberately parsed-but-not-consumed. `marker` is the doc phrase that must persist
    /// near the field's `pub <field>:` declaration in `config.rs`, AND the field must stay
    /// unread in core production (`.<field>` absent). A read → it was wired → RED.
    NotWired { marker: &'static str },
    /// Parsed here for schema symmetry but consumed by **another crate** (a section
    /// threaded whole across the workspace). `consumer` names that crate; `absent_token`
    /// must NOT appear in core production — if it does, the field was wired core-side and
    /// must be reclassified `Wired`.
    Mirror {
        consumer: &'static str,
        absent_token: &'static str,
    },
    /// A top-level section (a sub-struct); its leaves are checked by its own registry.
    /// No token check here — this entry exists so the top-level destructuring stays
    /// exhaustive (a new section breaks compilation until it is classified and given a
    /// registry in [`full_registry`]).
    Delegated,
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

/// Top-level [`VaultConfig`]. No section is threaded inside core (they are read whole by
/// other crates), so every section is `Delegated` to its own registry; this destructuring
/// is the section-level completeness guard.
fn vault_config_registry(c: &VaultConfig) -> Vec<Entry> {
    let VaultConfig {
        vault,
        embed,
        curator,
        index,
        drift,
        audit,
        history,
        storage,
    } = c;
    vec![
        entry("vault", vault, Wiring::Delegated),
        entry("embed", embed, Wiring::Delegated),
        entry("curator", curator, Wiring::Delegated),
        entry("index", index, Wiring::Delegated),
        entry("drift", drift, Wiring::Delegated),
        entry("audit", audit, Wiring::Delegated),
        entry("history", history, Wiring::Delegated),
        entry("storage", storage, Wiring::Delegated),
    ]
}

fn vault_section_registry(c: &VaultSection) -> Vec<Entry> {
    let VaultSection {
        default_tenant_id,
        schema_version,
    } = c;
    vec![
        // read in gradatum-vault (`registry.rs`) to seed the default tenant.
        entry(
            "default_tenant_id",
            default_tenant_id,
            Wiring::Mirror {
                consumer: "gradatum-vault",
                absent_token: ".default_tenant_id",
            },
        ),
        // parsed but read by no crate — the strict schema-version check is not implemented.
        entry(
            "schema_version",
            schema_version,
            Wiring::NotWired {
                marker: "no strict version check",
            },
        ),
    ]
}

/// [`EmbedConfig`]: only `embeddable_status` is read inside core
/// (`NoteStatus::is_embeddable`). Every other field is a documented dead duplicate — the
/// server reads its own `gradatum_server::config::EmbedConfig`.
fn embed_registry(c: &EmbedConfig) -> Vec<Entry> {
    let EmbedConfig {
        embeddable_status,
        embedder_id,
        dim,
        backend,
        fallback_backend,
        http_url,
        http_timeout_ms,
        http_model,
    } = c;
    vec![
        entry(
            "embeddable_status",
            embeddable_status,
            Wiring::Wired {
                proof: ".embeddable_status",
            },
        ),
        entry(
            "embedder_id",
            embedder_id,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "dim",
            dim,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "backend",
            backend,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "fallback_backend",
            fallback_backend,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "http_url",
            http_url,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "http_timeout_ms",
            http_timeout_ms,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "http_model",
            http_model,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
    ]
}

/// [`CuratorConfig`]: the whole section is a schema mirror — the curator pipeline is driven
/// by `gradatum_worker::curator_loader::WorkerCuratorConfig` (from the server TOML), not by
/// this struct. Every field is documented inert here (`Not wired here` / `Deprecated` /
/// `Dead knob`), and none is read in core.
fn curator_registry(c: &CuratorConfig) -> Vec<Entry> {
    let CuratorConfig {
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
        entry(
            "heuristic_admit_threshold",
            heuristic_admit_threshold,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "heuristic_default_status",
            heuristic_default_status,
            Wiring::NotWired {
                marker: "Dead knob",
            },
        ),
        entry(
            "llm_review_enabled",
            llm_review_enabled,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "confidence_threshold",
            confidence_threshold,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "llm_review_endpoint",
            llm_review_endpoint,
            Wiring::NotWired {
                marker: "Deprecated",
            },
        ),
        entry(
            "llm_review_model",
            llm_review_model,
            Wiring::NotWired {
                marker: "Deprecated",
            },
        ),
        entry(
            "llm_review_timeout_ms",
            llm_review_timeout_ms,
            Wiring::NotWired {
                marker: "Deprecated",
            },
        ),
        entry(
            "llm_review_max_tokens",
            llm_review_max_tokens,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "llm_review_fallback",
            llm_review_fallback,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
    ]
}

fn index_registry(c: &IndexConfig) -> Vec<Entry> {
    let IndexConfig {
        backend,
        fts_tokenizer,
    } = c;
    vec![
        // LIMIT ASSUMED: `pub backend:` occurs on both `EmbedConfig` and `IndexConfig` in
        // config.rs; the marker lookup resolves to the first (EmbedConfig). Both are
        // `Not wired`, so the marker assertion holds; the `.backend` absent-check (the
        // strong branch) is per-field-name and covers both regardless.
        entry(
            "backend",
            backend,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "fts_tokenizer",
            fts_tokenizer,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
    ]
}

fn drift_registry(c: &DriftConfig) -> Vec<Entry> {
    let DriftConfig {
        scan_interval_seconds,
    } = c;
    vec![entry(
        "scan_interval_seconds",
        scan_interval_seconds,
        Wiring::NotWired {
            marker: "Not wired",
        },
    )]
}

fn audit_registry(c: &AuditConfig) -> Vec<Entry> {
    let AuditConfig {
        rotation,
        retention_days,
        strict_mode,
    } = c;
    vec![
        entry(
            "rotation",
            rotation,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "retention_days",
            retention_days,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
        entry(
            "strict_mode",
            strict_mode,
            Wiring::NotWired {
                marker: "Not wired",
            },
        ),
    ]
}

/// [`HistoryConfig`]: the CoW snapshot retention policy is applied by
/// `gradatum-vault` (`lifecycle.rs`, `cfg.max_versions` / `cfg.ttl_days`), not core.
fn history_registry(c: &HistoryConfig) -> Vec<Entry> {
    let HistoryConfig {
        max_versions,
        ttl_days,
    } = c;
    vec![
        entry(
            "max_versions",
            max_versions,
            Wiring::Mirror {
                consumer: "gradatum-vault",
                absent_token: ".max_versions",
            },
        ),
        entry(
            "ttl_days",
            ttl_days,
            Wiring::Mirror {
                consumer: "gradatum-vault",
                absent_token: ".ttl_days",
            },
        ),
    ]
}

/// [`StorageBackendConfig`]: the object/fs backend is built by `gradatum-storage`
/// (`factory.rs`), which reads every field; core only carries the section.
fn storage_registry(c: &StorageBackendConfig) -> Vec<Entry> {
    let StorageBackendConfig {
        service,
        endpoint,
        bucket,
        region,
        root,
    } = c;
    vec![
        entry(
            "service",
            service,
            Wiring::Mirror {
                consumer: "gradatum-storage",
                absent_token: ".service",
            },
        ),
        entry(
            "endpoint",
            endpoint,
            Wiring::Mirror {
                consumer: "gradatum-storage",
                absent_token: ".endpoint",
            },
        ),
        entry(
            "bucket",
            bucket,
            Wiring::Mirror {
                consumer: "gradatum-storage",
                absent_token: ".bucket",
            },
        ),
        entry(
            "region",
            region,
            Wiring::Mirror {
                consumer: "gradatum-storage",
                absent_token: ".region",
            },
        ),
        entry(
            "root",
            root,
            Wiring::Mirror {
                consumer: "gradatum-storage",
                absent_token: ".root",
            },
        ),
    ]
}

/// Aggregates every struct's registry. `VaultConfig` and all sub-structs derive `Default`.
fn full_registry() -> Vec<(&'static str, Vec<Entry>)> {
    let cfg = VaultConfig::default();
    vec![
        ("VaultConfig", vault_config_registry(&cfg)),
        ("VaultSection", vault_section_registry(&cfg.vault)),
        ("EmbedConfig", embed_registry(&cfg.embed)),
        ("CuratorConfig", curator_registry(&cfg.curator)),
        ("IndexConfig", index_registry(&cfg.index)),
        ("DriftConfig", drift_registry(&cfg.drift)),
        ("AuditConfig", audit_registry(&cfg.audit)),
        ("HistoryConfig", history_registry(&cfg.history)),
        ("StorageBackendConfig", storage_registry(&cfg.storage)),
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
/// literal prefix (a leading `.`).
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

/// Production corpus: concatenation of all core sources except `config.rs`, test modules
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

/// Raw source of `config.rs` (unmodified — for the NotWired marker check, which must see
/// doc comments the corpus neutralises).
fn config_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("config.rs");
    fs::read_to_string(path).expect("reading config.rs")
}

/// `true` if `field` carries `marker` within the ~500 bytes preceding its `pub <field>:`
/// declaration in `config.rs` (its doc block). Byte distance is computed without slicing on
/// a non-char boundary (config.rs contains multi-byte em dashes).
fn has_marker(config_src: &str, field: &str, marker: &str) -> bool {
    let decl = format!("pub {field}:");
    let Some(idx) = config_src.find(&decl) else {
        return false; // declaration not found → force investigation (RED)
    };
    match config_src[..idx].rfind(marker) {
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
        corpus.contains("is_embeddable"),
        "corpus must keep production code (fn is_embeddable) — over-stripping?"
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

/// Every field of every `VaultConfig` struct is Wired in core, a documented cross-crate
/// Mirror (asserted absent here), or a documented NotWired dead field (marker present +
/// still unread). Top-level sections are Delegated to their own registries.
///
/// Verified by scanning the sources — not by re-reading. Removing a `Wired` consumption,
/// wiring a `Mirror`/`NotWired` field core-side, or dropping a `NotWired` marker makes this
/// red by naming the field. Adding/removing a field breaks compilation.
#[test]
fn every_vault_config_field_reaches_a_consumer_or_is_annotated() {
    let corpus = production_corpus();
    let config_src = config_source();
    let mut orphans: Vec<String> = Vec::new();
    let (mut wired, mut not_wired, mut mirrored, mut delegated) = (0usize, 0usize, 0usize, 0usize);

    for (struct_name, entries) in full_registry() {
        for e in &entries {
            match &e.wiring {
                Wiring::Wired { proof } => {
                    wired += 1;
                    if !reads(&corpus, proof) {
                        orphans.push(format!(
                            "`{}::{}` classified Wired but its consumption token `{}` is \
                             absent from core production — wiring removed?",
                            struct_name, e.name, proof
                        ));
                    }
                }
                Wiring::NotWired { marker } => {
                    not_wired += 1;
                    if !has_marker(&config_src, e.name, marker) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but the marker `{}` disappeared near \
                             its declaration in config.rs — reclassify if it was wired",
                            struct_name, e.name, marker
                        ));
                    }
                    let read_token = format!(".{}", e.name);
                    if reads(&corpus, &read_token) {
                        orphans.push(format!(
                            "`{}::{}` classified NotWired but it is READ in core (`{}`) — it was \
                             wired: reclassify it Wired",
                            struct_name, e.name, read_token
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
                            "`{}::{}` classified Mirror (consumed by `{}`) but its token `{}` now \
                             appears in core production — it was wired core-side: reclassify Wired",
                            struct_name, e.name, consumer, absent_token
                        ));
                    }
                }
                Wiring::Delegated => {
                    delegated += 1;
                }
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} vault config field(s) in wiring default \
         (wired={}, not_wired={}, mirrored={}, delegated={}):\n  - {}",
        orphans.len(),
        wired,
        not_wired,
        mirrored,
        delegated,
        orphans.join("\n  - ")
    );
}
