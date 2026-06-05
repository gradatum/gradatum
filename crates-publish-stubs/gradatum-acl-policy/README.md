# gradatum-acl-policy

> ACL policy engine with globset pattern matching, deny-wins semantics, and personal-classified circuit breaker.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Enums

```rust
/// ACL operation being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclOp {
    Read,
    Write,
}

/// Result of an ACL policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclDecision {
    /// Access granted.
    Allow,
    /// Explicit deny — negation pattern matched or personal-classified circuit breaker (B3).
    DenyExplicit,
    /// Implicit deny — no allow pattern matched (default-deny B2).
    DenyImplicit,
}
```

### Structs

```rust
/// Compiled ACL policy loaded from a TOML preset.
pub struct AclPolicy { ... }

impl AclPolicy {
    /// Load and compile a policy from TOML bytes.
    pub fn from_toml(toml: &str) -> Result<Self, AclError>

    /// Evaluate the policy for a given consumer, locus, and operation.
    pub fn evaluate(
        &self,
        consumer_id: &str,
        trust: &TrustContext,
        locus: &str,
        op: AclOp,
    ) -> AclDecision
}
```

### Evaluation order (priority descending)

1. `TrustContext::Unauthenticated` → `DenyImplicit` (immediate)
2. Unknown identity (no consumer match) → `DenyImplicit`
3. B3: `personal-classified` bypass → `DenyExplicit`
4. Negation pattern (`!glob`) match → `DenyExplicit` (deny-wins)
5. Allow pattern match → `Allow`
6. Default → `DenyImplicit`

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AclError {
    Toml(toml::de::Error),
    Glob(globset::Error),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0