# gradatum-acl-policy

> ACL policy engine with globset pattern matching, deny-wins semantics, and personal-classified circuit breaker.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-acl-policy` evaluates access control decisions for vault operations. It loads
a compiled policy from a TOML preset and applies rules in priority order:

1. `Unauthenticated` trust context → `DenyImplicit` immediately.
2. Unknown consumer identity → `DenyImplicit`.
3. Personal-classified circuit breaker → `DenyExplicit` if consumer lacks the flag.
4. Negation pattern (`!glob`) match → `DenyExplicit` (**deny-wins**).
5. Allow pattern match → `Allow`.
6. No match → `DenyImplicit` (default deny).

Patterns use `globset` for efficient compiled matching. Policies are loaded once at startup.

## Usage

```toml
[dependencies]
gradatum-acl-policy = "2.1.0"
```

```rust
use gradatum_acl_policy::{AclEngine, AclOp, AclDecision};

let engine = AclEngine::from_preset_str(toml_str)?;
let decision = engine.evaluate(&trust_context, AclOp::Write, "decisions/my-note.md");
assert_eq!(decision, AclDecision::Allow);
```

## License

Apache-2.0
