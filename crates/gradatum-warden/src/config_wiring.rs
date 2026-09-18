//! Configuration wiring guard for `gradatum-warden`.
//!
//! ## The defect class this catches
//!
//! A configuration field deserialised at runtime, **then read by no consumption
//! site**, while its documentation describes an active behaviour. The volet 1 guard
//! (`gradatum-engine`) and volet 2 guard (`gradatum-server`) locked that class down for
//! their config structs; this volet extends the same mechanism to
//! [`crate::WardenConfig`] (the L0 IP filter + rate limiter).
//!
//! ## Why this lives in `src/` (not `tests/`)
//!
//! Unlike the engine/server volets, [`crate::WardenConfig`] is `#[non_exhaustive]`. An
//! exhaustive destructuring — the guard's strongest branch (compile-time completeness) —
//! is only permitted **within the defining crate**. An integration test in `tests/` is a
//! separate crate and would be forced to write `..`, silently dropping any newly added
//! field. So the guard is an in-crate `#[cfg(test)]` module, and this file is excluded
//! from the scanned corpus by name (like `config.rs`) so its own `proof` string literals
//! never masquerade as production consumption.
//!
//! ## Mechanism (identical to the earlier volets, no macro, no dependency)
//!
//! 1. **Compile-time completeness** — one *exhaustive destructuring* (no `..`) of
//!    [`crate::WardenConfig`]. Adding/removing a field breaks this test's compilation
//!    until the field is named in the destructuring AND passed to [`entry`] (classified).
//! 2. **Classification registry** — each field is `Wired { proof }` or `NotWired`.
//!    `proof` is a discriminating token that MUST appear in the crate's **production**
//!    sources. Removing the wiring removes the token → RED.
//! 3. **Production scan** — every crate source except `config.rs` (where the struct is
//!    defined) and this file, with `#[cfg(test)]` modules stripped and comments
//!    neutralised (a token merely mentioned in a comment is not a real consumption).
//!
//! `WardenConfig` is a single self-consumed struct (all fields read inside `service.rs`),
//! so — like the engine volet — no cross-crate `Mirror` variant is needed here.

use crate::WardenConfig;
use std::fs;
use std::path::{Path, PathBuf};

/// Classification of a config field with respect to its runtime consumption.
enum Wiring {
    /// The field is wired: its value reaches a consumption site. `proof` is a discriminating
    /// token that must appear in the crate's production sources (excluding `config.rs` and
    /// this file, excluding test modules). Removing the wiring removes the token.
    Wired { proof: &'static str },
    /// The field is deliberately parsed-but-not-consumed. It MUST carry a `// not yet wired`
    /// marker near its declaration in `config.rs`, and MUST NOT be read in production.
    ///
    /// No `WardenConfig` field is currently `NotWired` (all are wired). The variant — and
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
/// exhaustive destructuring and the registry. Adding a field to the struct forces naming
/// it in the destructuring (else E0027); the unused binding is silenced only by passing it
/// here — i.e. by adding a registry entry.
fn entry<T>(name: &'static str, _binding: &T, wiring: Wiring) -> Entry {
    Entry { name, wiring }
}

/// Wiring registry of [`WardenConfig`].
///
/// The exhaustive destructuring below is the compile-time completeness guard: any
/// field add/remove breaks compilation until the registry is updated.
fn warden_registry(c: &WardenConfig) -> Vec<Entry> {
    // EXHAUSTIVE destructuring — no `..`. Intentional: this is the guard. Permitted here
    // because this module is compiled as part of the defining crate.
    let WardenConfig {
        enabled,
        rate_limit_per_minute,
        rate_limit_burst,
        bypass_loopback,
        ip_allow,
        ip_deny,
    } = c;
    vec![
        // `service.rs`: `if config.enabled { .. }` / `if !state.config.enabled { .. }`.
        entry("enabled", enabled, Wiring::Wired { proof: ".enabled" }),
        entry(
            "rate_limit_per_minute",
            rate_limit_per_minute,
            Wiring::Wired {
                proof: ".rate_limit_per_minute",
            },
        ),
        entry(
            "rate_limit_burst",
            rate_limit_burst,
            Wiring::Wired {
                proof: ".rate_limit_burst",
            },
        ),
        entry(
            "bypass_loopback",
            bypass_loopback,
            Wiring::Wired {
                proof: ".bypass_loopback",
            },
        ),
        // `service.rs`: `ip_allowed(ip, &state.config.ip_allow, &state.config.ip_deny)`.
        // The proof `.ip_allow` matches `.ip_allow,` (the field read) but not the free
        // function `ip_allowed` (no leading dot).
        entry("ip_allow", ip_allow, Wiring::Wired { proof: ".ip_allow" }),
        entry("ip_deny", ip_deny, Wiring::Wired { proof: ".ip_deny" }),
    ]
}

// ---------------------------------------------------------------------------
// Production source scan (strips #[cfg(test)] modules).
//
// Edition 2021 crate: no `let` chains (`&& let Some(..)`) — nested `if let` instead.
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
        if b[i] == b'r' {
            if let Some(e) = skip_raw_string(b, i) {
                i = e;
                continue;
            }
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
        if b[i] == b'r' {
            if let Some(e) = skip_raw_string(b, i) {
                out.push_str(&src[i..e]);
                i = e;
                continue;
            }
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
        if b[i] == b'#' {
            if let Some(attr_end) = test_cfg_attr_end(src, i) {
                i = skip_item_after_attr(src, attr_end);
                continue;
            }
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

/// Production corpus: concatenation of all crate sources except `config.rs` (where the
/// struct is defined) and this guard file, test modules removed, comments neutralised.
fn production_corpus() -> String {
    let src_dir = manifest_dir().join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    files.sort();

    let mut corpus = String::new();
    for f in &files {
        if f.file_name()
            .is_some_and(|n| n == "config.rs" || n == "config_wiring.rs")
        {
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
        corpus.contains("ip_allowed"),
        "corpus must keep production code (fn ip_allowed) — over-stripping?"
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

/// Every field of [`WardenConfig`] reaches a consumption site.
///
/// Verified by scanning the sources — not by re-reading. Removing a `Wired` field's
/// consumption makes this red by naming the field. Adding/removing a field breaks
/// compilation (exhaustive destructuring).
#[test]
fn every_warden_config_field_reaches_a_consumer() {
    let corpus = production_corpus();
    let mut orphans: Vec<String> = Vec::new();
    let mut wired = 0usize;

    for e in &warden_registry(&WardenConfig::default()) {
        match &e.wiring {
            Wiring::Wired { proof } => {
                wired += 1;
                if !reads(&corpus, proof) {
                    orphans.push(format!(
                        "`WardenConfig::{}` classified Wired but its consumption token `{}` \
                         is absent from production sources — wiring removed?",
                        e.name, proof
                    ));
                }
            }
            Wiring::NotWired => {
                // No NotWired field on the warden today; branch kept for completeness.
                orphans.push(format!(
                    "`WardenConfig::{}` classified NotWired — unexpected; if a \
                     parsed-but-unconsumed field was added, wire the `config.rs` marker \
                     check as in the engine volet",
                    e.name
                ));
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} warden config field(s) in wiring default (wired={}):\n  - {}",
        orphans.len(),
        wired,
        orphans.join("\n  - ")
    );
}
