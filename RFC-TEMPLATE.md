# RFC NNNN — Short title

| Field | Value |
|---|---|
| **RFC number** | NNNN (assigned at draft PR open) |
| **Author(s)** | `@handle` |
| **Status** | `draft` / `accepted` / `postponed` / `rejected` |
| **Started** | YYYY-MM-DD |
| **Resolved** | YYYY-MM-DD (or `—`) |
| **Tracking issue** | `#NNN` (filled in once accepted) |
| **Affected crates** | `gradatum-core`, `gradatum-vault`, ... |

---

## 1. Motivation

Why is this change worth making? What concrete problem does it solve, and for whom? Quantify the pain (latency, RAM, lines of code, developer-hours, support tickets) where possible. Avoid speculative motivation.

---

## 2. Design

The proposed solution. Code snippets, types, function signatures, schema diffs, sequence diagrams. Be concrete: a reviewer should be able to predict every public API change after reading this section.

```rust
// Example signature changes
pub trait Foo {
-   fn bar(&self) -> i32;
+   fn bar(&self, ctx: &Context) -> Result<i32>;
}
```

---

## 3. Impact downstream

Which crates, binaries, or external consumers are affected?

| Component | Impact | Effort |
|---|---|---|
| `gradatum-server` | API call signature changes | ~30 LOC |
| `gradatum-cli` | New flag `--ctx` | ~10 LOC |
| `gradatum-sdk-rs` | Re-export change | None |
| External SDK users | Breaking change in trait `Foo` | Migration step required |

Note any change to:

- Public API (rust types, HTTP, MCP, CLI flags).
- Schema (SQLite tables, ULID format, file layout).
- Default ports, default config values, default LLM model.
- CI requirements (new `cargo` plugin, new system dependency).

---

## 4. Migration path

If this is a breaking change, how do existing users migrate?

- **Pre-condition:** what version is required before applying this change.
- **Migration steps:** ordered list of commands or code edits.
- **Rollback:** can the change be reverted? Under what conditions?
- **Tooling:** is a new `gradatum-admin migrate-*` subcommand needed?

For non-breaking changes, write "N/A — additive only".

---

## 5. Alternatives considered

What other designs were considered, and why were they rejected? Include at least one alternative even if obviously inferior — this forces the reader to evaluate the proposal in context.

| Alternative | Pros | Cons | Reason rejected |
|---|---|---|---|
| Do nothing | No effort | Problem persists | Section 1 quantified the cost |
| ... | ... | ... | ... |

---

## 6. Drawbacks

What is the cost of accepting this RFC? Be honest. Examples:

- Increases compile time by N seconds.
- Adds dependency on crate `X` (license, supply-chain weight).
- Locks the project into pattern Y for the next major version.
- Requires operator action at upgrade time.

> **AM4 reminder:** this section must be authored **without** AI assistance. The author should be able to defend it in a synchronous review without referring to generated text.

---

## 7. Unresolved questions

Open items that the RFC does not yet answer. These do not block acceptance if reviewers agree they are resolvable during implementation.

- [ ] ...
- [ ] ...
