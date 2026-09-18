# gradatum-privacy

> Regex-based floor detector and span geometry for the gradatum personal-data detection frontier.

**Status**: v2.5.0 — public, Apache-2.0. Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-privacy` supplies the always-available detector that sits beneath any heavier
model detector at the personal-data frontier. `RegexDetector` implements
`gradatum_core::pii::PiiDetector` with a small, deterministic set of patterns (e-mail, IBAN,
French phone numbers, French social-security numbers, addresses) and depends on no external
process.

An *address* here is an IPv4 address with an optional port or prefix length, an IPv6 address,
or a `/mnt` mount path. Each pattern is checked against its surrounding bytes after matching,
because a regex has no lookaround: a ten-digit run is a phone number only when nothing around
it says timestamp or identifier, and a four-group dotted run is an address only when its
boundary says so. The floor is deliberately precise rather than eager — it does not report the
loopback range, the unspecified address, or IPv6 spellings shorter than the compressed form it
accepts, all of which cost it recall rather than precision.

The crate also carries the geometry a detector needs around it:

- `WindowPlan` cuts a body larger than a detector's per-call ceiling into overlapping windows.
- `rebase` moves the offsets a detector returned from window coordinates back onto the body.
- `validate_and_merge` rejects spans that cannot be applied and fuses overlapping spans of
  the same category, across windows as well as within one.

## Default posture: report-only, no categories

The detection frontier runs in **`report_only`** mode by default and with an **empty category
set**: a pass records what it observed without acting on the note, and marks nothing until an
operator activates categories. A pass that runs but does not complete leaves a note findable
for a later rescan; a completed pass over an empty category set is a no-op. Personal data is
never emitted — a span carries an offset and a category, never a matched value.

## Security

Detector payloads and spans never carry a matched value or note body off the frontier. See
[SECURITY.md](https://github.com/gradatum/gradatum/blob/main/SECURITY.md) for the disclosure
policy.

## License

Apache-2.0
