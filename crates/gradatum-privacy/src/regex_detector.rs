use std::sync::LazyLock;

use gradatum_core::pii::{
    DetectInput, Detection, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
};
use regex::Regex;

/// Score carried by every regex-produced span. Regex matches are exact by
/// construction and are not subject to a probability threshold.
const MAX_SCORE: f32 = 1.0;

// Patterns are compiled once, at first use, and never inside `detect`.
// `expect` is sound here: each pattern is built from compile-time fragments, so
// a failure to compile is an authoring bug caught on the first call, never a
// function of runtime input.

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
        .expect("email pattern is a valid compile-time literal")
});

static IBAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{2}[0-9]{2}[A-Z0-9]{11,30}\b")
        .expect("iban pattern is a valid compile-time literal")
});

/// French telephone numbers: ten digits opening on a non-zero digit, or the
/// country code `+33` followed by nine. Space, dot and hyphen are accepted
/// between digit pairs, and are optional.
///
/// The pattern carries no anchor of its own — see [`phone_context`]. A bare
/// ten-digit run is a fragment of an ISO-8601 timestamp about as often as it is
/// a telephone number, so the surrounding bytes decide, not the pattern alone.
static PHONE_FR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\+33[\s.-]?[1-9]|0[1-9])(?:[\s.-]?[0-9]{2}){4}")
        .expect("french phone pattern is a valid compile-time literal")
});

static NIR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[12]\s?\d{2}\s?(?:0[1-9]|1[0-2])\s?\d{2}\s?\d{3}\s?\d{3}\s?\d{2}\b")
        .expect("french nir pattern is a valid compile-time literal")
});

/// One dotted-quad octet, `0` to `255` inclusive.
///
/// A spelled-out leading zero is not accepted as an octet, which keeps
/// version-shaped runs such as `06.12.34.56` out of the quads.
const IPV4_OCTET: &str = r"(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])";

/// Four dotted-quad octets.
static IPV4_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"{IPV4_OCTET}(?:\.{IPV4_OCTET}){{3}}"))
        .expect("ipv4 pattern compiles from compile-time fragments")
});

/// An IPv4 address carrying the port it answers on, `192.0.2.10:8443`.
///
/// The span covers the pair: a host and its port are one observed location, and
/// splitting them would report the port as an address of its own.
static IPV4_PORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"{IPV4_OCTET}(?:\.{IPV4_OCTET}){{3}}:[0-9]{{1,5}}"
    ))
    .expect("ipv4:port pattern compiles from compile-time fragments")
});

/// An IPv4 address carrying a prefix length, `198.51.100.0/24`.
///
/// Prefix lengths below `/8` are not read: they name a share of the address
/// space too coarse to be an observation about any particular host.
static IPV4_CIDR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"{IPV4_OCTET}(?:\.{IPV4_OCTET}){{3}}/(?:[89]|1[0-9]|2[0-9]|3[0-2])"
    ))
    .expect("ipv4 cidr pattern compiles from compile-time fragments")
});

/// An IPv6 address, in its full eight-group form or in a compressed one.
///
/// The compressed form is read as "at least two groups, then `::`, then at
/// least one group". Short spellings (`::1`, `fe80::1`) and the IPv4-mapped
/// tail are therefore not read — a deliberate false negative that keeps Rust
/// paths (`std::io`) and MAC-address-shaped runs out of the address count.
static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:[0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}|(?:[0-9A-Fa-f]{1,4}:){2,7}:[0-9A-Fa-f]{1,4}(?::[0-9A-Fa-f]{1,4}){0,5}",
    )
    .expect("ipv6 pattern is a valid compile-time literal")
});

/// A mount path: `/mnt` followed by at least one segment.
///
/// The segment charset ends the path at the first byte that cannot belong to
/// one, so a path stops on a space, a quote, a comma or a closing parenthesis
/// without the pattern having to name every terminator.
static MOUNT_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/mnt(?:/[A-Za-z0-9._~%+-]+)+")
        .expect("mount path pattern is a valid compile-time literal")
});

/// A compiled floor pattern: the expression, the category it reports, and the
/// context predicate a candidate match must satisfy to be kept.
///
/// The predicate exists because the regex engine offers no lookaround: a left
/// boundary cannot be written inside the pattern, and one that *consumed* its
/// right boundary would swallow the separator between two adjacent entities and
/// hide the second from the scan.
struct Pattern {
    re: &'static LazyLock<Regex>,
    category: PiiCategory,
    accept: fn(&str, usize, usize) -> bool,
}

/// Accepts every match: the pattern is already self-delimiting.
fn accept_any(_text: &str, _start: usize, _end: usize) -> bool {
    true
}

/// Returns the byte at `index`, or `None` past the end of the body.
fn byte_at(text: &str, index: usize) -> Option<u8> {
    text.as_bytes().get(index).copied()
}

/// Rejects a match that continues a numeric run on either side.
///
/// A match is a whole token only when it is neither glued to a digit, nor glued
/// to a `.` that itself abuts a digit — the shape of a dotted run such as a
/// version number, an IP fragment or an ISO-8601 fractional second. A `.` at a
/// sentence end is not followed by a digit and is therefore accepted.
fn bounded_by_number(text: &str, start: usize, end: usize) -> bool {
    if start > 0 {
        if byte_at(text, start - 1).is_some_and(|b| b.is_ascii_digit()) {
            return false;
        }
        if byte_at(text, start - 1) == Some(b'.')
            && byte_at(text, start - 2).is_some_and(|b| b.is_ascii_digit())
        {
            return false;
        }
    }
    if byte_at(text, end).is_some_and(|b| b.is_ascii_digit()) {
        return false;
    }
    if byte_at(text, end) == Some(b'.')
        && byte_at(text, end + 1).is_some_and(|b| b.is_ascii_digit())
    {
        return false;
    }
    true
}

/// Context of a telephone-number candidate: the numeric boundary rule, plus the
/// timestamp context a ten-digit run otherwise hides in.
///
/// A match touched by `:` or by the ISO-8601 date/time separator `T` is a clock
/// reading, a port or a timestamp field, never a telephone number.
fn phone_context(text: &str, start: usize, end: usize) -> bool {
    if !bounded_by_number(text, start, end) {
        return false;
    }
    if start > 0 && byte_at(text, start - 1).is_some_and(|b| matches!(b, b':' | b'T')) {
        return false;
    }
    !byte_at(text, end).is_some_and(|b| matches!(b, b':' | b'T'))
}

/// Returns `true` when `matched` is an address the floor deliberately leaves
/// alone: the loopback range and the unspecified address.
///
/// Neither is personal data. They name the host itself or no host at all, and
/// reporting them would mark every note that mentions a local service.
///
/// `matched` is an IPv4-family match, so `/` introduces a prefix length and `:`
/// a port; neither byte can appear inside the address itself.
fn is_unreportable_ipv4(matched: &str) -> bool {
    let address = matched.split(['/', ':']).next().unwrap_or(matched);
    address.starts_with("127.") || address == "0.0.0.0"
}

/// Context of an IPv4-family candidate, port and prefix length included: the
/// numeric boundary rule, plus the blocks the floor leaves alone.
///
/// A four-group dotted run has no anchor of its own. A version such as
/// `1.2.3.4` is not distinguishable from an address by shape, and is reported
/// as one; a run with a fifth group is, and is not.
fn ipv4_context(text: &str, start: usize, end: usize) -> bool {
    bounded_by_number(text, start, end) && !is_unreportable_ipv4(&text[start..end])
}

/// Context of an IPv6 candidate: the match must not be a fragment of a longer
/// address, so neither neighbour may be a hex digit or a colon.
fn ipv6_context(text: &str, start: usize, end: usize) -> bool {
    let continues = |b: u8| b.is_ascii_hexdigit() || b == b':';
    if start > 0 && byte_at(text, start - 1).is_some_and(continues) {
        return false;
    }
    !byte_at(text, end).is_some_and(continues)
}

/// Context of a mount-path candidate: the path must not be a suffix of a longer
/// token, so its first `/` must not be glued to a path-ish byte.
fn mount_path_context(text: &str, start: usize, _end: usize) -> bool {
    if start == 0 {
        return true;
    }
    let previous = text.as_bytes()[start - 1];
    !(previous.is_ascii_alphanumeric() || matches!(previous, b'_' | b'-' | b'.' | b'/' | b'~'))
}

/// Ordered list of the patterns the detector applies.
///
/// IBAN precedes the shorter numeric patterns so that, after overlap pruning, a
/// long account number absorbs any phone- or NIR-shaped substring nested inside
/// it rather than emitting a spurious second span. The address patterns close
/// the list, widest first: pruning keeps the longest span at a given start, so a
/// prefix length or a port absorbs the bare address nested inside it.
fn patterns() -> [Pattern; 9] {
    [
        Pattern {
            re: &EMAIL_RE,
            category: PiiCategory::Email,
            accept: accept_any,
        },
        Pattern {
            re: &IBAN_RE,
            category: PiiCategory::AccountNumber,
            accept: accept_any,
        },
        Pattern {
            re: &PHONE_FR_RE,
            category: PiiCategory::Phone,
            accept: phone_context,
        },
        Pattern {
            re: &NIR_RE,
            category: PiiCategory::AccountNumber,
            accept: accept_any,
        },
        Pattern {
            re: &IPV4_CIDR_RE,
            category: PiiCategory::Address,
            accept: ipv4_context,
        },
        Pattern {
            re: &IPV4_PORT_RE,
            category: PiiCategory::Address,
            accept: ipv4_context,
        },
        Pattern {
            re: &IPV4_RE,
            category: PiiCategory::Address,
            accept: ipv4_context,
        },
        Pattern {
            re: &IPV6_RE,
            category: PiiCategory::Address,
            accept: ipv6_context,
        },
        Pattern {
            re: &MOUNT_PATH_RE,
            category: PiiCategory::Address,
            accept: mount_path_context,
        },
    ]
}

/// Deterministic floor detector backed by a fixed set of compiled patterns.
///
/// Covers e-mail addresses, IBANs, French phone numbers, French social-security
/// numbers (NIR), and addresses — IPv4 with an optional port or prefix length,
/// IPv6, and mount paths. Every span carries the maximal score.
pub struct RegexDetector;

impl RegexDetector {
    /// Builds a [`RegexDetector`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for RegexDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiDetector for RegexDetector {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        let text = input.text;

        let mut spans: Vec<PiiSpan> = Vec::new();
        for pattern in patterns() {
            for m in pattern.re.find_iter(text) {
                if !(pattern.accept)(text, m.start(), m.end()) {
                    continue;
                }
                spans.push(PiiSpan::new(
                    m.start(),
                    m.len(),
                    pattern.category,
                    MAX_SCORE,
                ));
            }
        }

        // Deterministic order, then greedy overlap pruning: earlier start first,
        // longer match first on ties. A span nested in an already-kept, longer
        // match is dropped — full regex ∪ sidecar fusion is a later concern; here
        // we only stop a sub-pattern from double-counting inside another entity.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.len.cmp(&a.len)));

        let mut kept: Vec<PiiSpan> = Vec::with_capacity(spans.len());
        let mut last_end = 0usize;
        for span in spans {
            if kept.is_empty() || span.start >= last_end {
                last_end = span.start + span.len;
                kept.push(span);
            }
        }

        Ok(Detection::new(kept, 0))
    }
}
