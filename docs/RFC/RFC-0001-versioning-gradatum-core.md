# RFC-0001 — Trait stability tiers and versioning for `gradatum-core`

| Field | Value |
|---|---|
| **RFC number** | 0001 |
| **Status** | `accepted` |
| **Started** | 2026-05-03 |
| **Resolved** | 2026-05-03 |
| **Tracking issue** | — (Phase 0bis — no issue tracking yet) |
| **Affected crates** | `gradatum-core` (primary); `gradatum-chat`, `gradatum-embed`, `gradatum-acl-policy`, `gradatum-auth`, all downstream consumers |
| **Authors** | Gradatum maintainers |

---

## 1. Definitions

Three operational tiers govern every public trait in `gradatum-core`. Commit to one tier at trait definition; stability is a contract.

| Tier | Definition | Permission | Example |
|---|---|---|---|
| **`#[stability::stable]`** | SemVer-strict. Trait signatures and semantics are frozen. Cannot change in a minor release. Breaking changes require a major version + RFC + 1-cycle deprecation (1 minor with deprecated marker, then removal in following major). Enforced by `cargo semver-checks`. | Add method with default impl **only if** trait is `#[sealed]` or explicitly documented to permit impl-free defaults. Calling code **must never** check for method presence at runtime. | `Chat`, `Embedder` (v1.0+) |
| **`#[stability::unstable]`** | May evolve within a minor. Signatures may change; behavior may refactor. **All changes documented in `CHANGELOG.md`** — one entry per unstable trait touched. **No SemVer guarantee.** | Add, remove, refactor any method. Rename arguments. Add traits to supertrait list. Failing conformance tests is acceptable if justified in `CHANGELOG.md`. | `Reranker` (v0.1, off by feature flag) |
| **`#[stability::experimental]`** | May change **between patches**. Used **only behind a `unstable-<trait-name>` Cargo feature**. No documentation in `CHANGELOG.md` required, but allowed. Conformance tests not required. | Add, remove, refactor everything. Used for prototyping. Removes responsibility for deprecation cycles. | Future: tool-use canonicalization bridge (v0.1, feature `llm-bridge-tool-use` OFF by default) |

---

## 2. Motivation

`gradatum-core` is the contract boundary for downstream implementations (external `Chat` providers, custom `Embedder` backends, third-party `AclPolicy` rules engines). Once v1.0 ships and the project enters public stewardship, trait breakage becomes non-negotiable unless the team is willing to break every downstream implementation.

Current state (Phase 0bis): All traits are scaffolding stubs. No consumer code exists. This RFC chooses one of three stability regimes per trait before Phase 1 implementation fills them. The choice gates whether future versions must follow SemVer strict, allow unstable changes, or prototype behind feature flags.

**Pain points this solves:**
- Downstream consumers (humans and AI agents) need clear expectations about what breakage means.
- The project must survive "AI assistant unavailable" — explicit rules prevent implicit assumptions that sink after tool rotation.
- Public release (D5 criterion: ≥30 days real-world use) requires confidence that trait signatures will not thrash in the following patch.

**Cross-references:**
- `RELEASE-POLICY.md` §AM1 — trait-stability tiers; §Versioning — SemVer 0.x pre-release window.
- `PROJECT-CONTEXT.md` §4 — trait-mapped crates and their dependencies.

---

## 3. Design

### 3.1 Trait stability tagging

Every public trait in `gradatum-core` receives one of three attribute marks at definition:

```rust
/// Chat provider backend (LLM or local fallback).
#[stability::stable]
pub trait Chat: Send + Sync {
    /// Call the language model.
    fn generate(
        &self,
        params: &GenerationParams,
    ) -> impl Future<Output = Result<Generation, ChatError>>;
}

/// Reranker for semantic search results.
#[stability::unstable]
pub trait Reranker: Send + Sync {
    /// Score and reorder results by relevance.
    fn rerank(&self, docs: &[Doc], query: &str) -> Result<Vec<(Doc, f32)>>;
}

/// Tool-use canonicalization bridge (future multi-provider harmony).
#[stability::experimental]
pub trait ToolUseBridge: Send + Sync {
    /// Translate provider-specific tool-call to canonical format.
    fn canonicalize(&self, call: &ProviderToolCall) -> Result<CanonicalToolCall>;
}
```

### 3.2 Enforcement and CI

**`cargo public-api`**: Extract and version-pin the public API at each commit. Diffing against the previous tag detects unintended signature drift. Runs on every PR.

**`cargo semver-checks`**: Enforces SemVer 2.0.0 rules. Traits marked `#[stability::stable]` **must** pass `cargo semver-checks` — adding a method without default impl is a major bump. `unstable` and `experimental` traits are exempt (marked `--allow-unstable` in `Cargo.toml`).

**Conformance testkit** (AM2): `gradatum-core` ships a `testkit` Cargo feature with parameterized trait tests. Example:

```rust
#[cfg(test)]
mod conformance {
    use gradatum_core::testkit::*;

    struct MockChat;
    
    #[async_trait::async_trait]
    impl Chat for MockChat {
        async fn generate(&self, params: &GenerationParams) 
            -> Result<Generation, ChatError> 
        {
            Ok(Generation {
                text: "mock response".into(),
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    #[tokio::test]
    async fn mock_passes_conformance() {
        chat_conformance!(MockChat);
    }
}
```

Any downstream impl must run these tests (enabled in their `Cargo.toml` dev-dependency):

```toml
[dev-dependencies]
gradatum-core = { version = "1.0", features = ["testkit"] }
```

Failing conformance blocks `cargo publish`.

### 3.3 Deprecation cycles for stable traits

**Rule R1:** If a method must be removed from a `stable` trait in major version `N+1`, the trait author:

1. Marks the method `#[deprecated(since = "N.M")]` in minor `N.M`.
2. Updates `CHANGELOG.md`: "Method `Foo::bar` deprecated; remove in `N+1`."
3. In major `N+1`, removes the method.

**Example:**

```rust
pub trait Storage {
    /// Deprecated since 1.2. Use `read_batch` for better performance.
    #[deprecated(
        since = "1.2",
        note = "Use `read_batch(keys: &[String])` instead. Will be removed in 2.0."
    )]
    fn read(&self, key: &str) -> Result<Vec<u8>>;

    /// New method, returns multiple at once.
    fn read_batch(&self, keys: &[String]) -> Result<Vec<(String, Vec<u8>)>>;
}
```

Compiler emits deprecation warnings on downstream use. Downstream has one full minor release (e.g., 1.2 → 1.3 → 2.0) to migrate.

### 3.4 Feature flags for experimental traits

Experimental traits are **gated behind Cargo features named `unstable-<trait-name>`**:

```toml
[features]
# In gradatum-core/Cargo.toml
unstable-tool-use = []
```

In code:

```rust
#[cfg(feature = "unstable-tool-use")]
#[stability::experimental]
pub trait ToolUseBridge: Send + Sync { ... }
```

A downstream consumer enabling the feature:

```toml
[dependencies]
gradatum-core = { version = "0.1", features = ["unstable-tool-use"] }
```

Acknowledges: "I understand this trait may break between patches. I will check `CHANGELOG.md` on every update."

---

## 4. Decision matrix per change type

**If [change type] → [verdict for tier]**

| Change | Stable | Unstable | Experimental |
|---|---|---|---|
| **Add method with default impl** (trait is sealed or docs permit) | **Minor OK** (no caller can fail to implement it) | **Any version OK** | **Any version OK** |
| **Add method without default impl** | **Major + RFC required** (forces all impls to add code) | **Minor OK** (CHANGELOG entry) | **Patch OK** |
| **Rename method** | **Major + RFC + 1-cycle deprecation** | **Minor (CHANGELOG)** | **Patch** |
| **Change argument types** (e.g., `&str` → `&[u8]`) | **Major + RFC + 1-cycle deprecation** | **Minor (CHANGELOG)** | **Patch** |
| **Change return type** | **Major + RFC + 1-cycle deprecation** | **Minor (CHANGELOG)** | **Patch** |
| **Remove method entirely** | **Major + RFC (no deprecation if new major introduces it)** | **Minor (CHANGELOG)** | **Patch** |
| **Add supertrait bound** (e.g., `pub trait Foo: NewBound`) | **Major + RFC** (forces impls to satisfy `NewBound`) | **Minor (CHANGELOG)** | **Patch** |
| **Remove supertrait bound** | **Minor (relaxation)** | **Any** | **Any** |
| **Split trait** (e.g., `Storage` → `StorageRead` + `StorageWrite`) | **Major + RFC + migration tool** | **Minor + RFC** | **Patch** |
| **Merge trait** (inverse: two traits become one) | **Major + RFC** | **Minor + RFC** | **Patch** |
| **Add Generic Associated Type (GAT)** | **Major** (existing `impl` may not compile with new bound) | **Minor (CHANGELOG)** | **Patch** |
| **Change default impl behavior** | **Major if visible (e.g., `fn foo() { new_expensive_op() })` ↔ `fn foo() { cheap_no_op() }`)** | **Minor (CHANGELOG)** | **Patch** |

---

## 5. Impact downstream

### 5.1 External Rust implementations

A third-party crate implementing `gradatum-core::Chat`:

```rust
pub struct OpenAIChat { client: OpenAI }
#[async_trait::async_trait]
impl Chat for OpenAIChat { ... }
```

**Before v1.0 (0.x unstable):** Trait may break. Maintainer updates crate, runs `cargo update`, checks `CHANGELOG.md`, adapts impl, publishes a new version. No SemVer obligation — consumer accepts risk.

**After v1.0 (stable):** Trait is frozen. Maintainer can publish for 12+ months without worrying about impl breakage (unless they opt into `unstable-` features). If gradatum-core v2.0 breaks the trait, the external crate can either:
- Maintain two branches (`1.x` for v1.0, `2.x` for v2.0).
- Use a semver crate like `gradatum-core-compat` to bridge signatures.

### 5.2 AI agents and downstream services

`gradatum-server` and `gradatum-worker` (binaries, not libraries) consume `gradatum-core` traits internally. Breaking changes in `stable` traits require:

1. Code changes in the binary (adapting to new signature).
2. Re-publish as a new version (e.g., `gradatum-server v2.0`).
3. Operator manual action: `systemctl stop gradatum-server && /opt/gradatum-server v2.0 && gradatum-admin migrate-data`.

This is acceptable **once per major version** (low churn).

### 5.3 Conformance testkit (AM2)

Any downstream impl that runs the testkit in CI is guaranteed: "If my tests pass, my impl is compatible with the current trait." No silent breakage, no surprise incompatibilities after an update.

---

## 6. Migration path

### 6.1 From 0.x (unstable) to 1.0 (stable)

**Criterion:** Trait promotion from `unstable` → `stable` requires an RFC. The RFC must:

1. Justify why the trait is "ready" (e.g., "3 independent external impls shipping in production; API used for 6+ months without change requests").
2. List all breaking changes made in the 0.x lineage that **will not happen again** (this documents what 1.0 freezes).
3. Acknowledge the 1-cycle deprecation obligation for any future removals.

**Timeline:** v0.1 ships (unstable). v0.2, v0.3, etc. evolve the trait. When confident, issue RFC-XXXX proposing promotion. After acceptance, v1.0 ships with the trait marked `stable`.

### 6.2 From v1.0 to v2.0 (breaking change in stable trait)

If v1.0 is in-use for 6+ months and a breaking change is necessary (e.g., performance-critical refactor):

1. **RFC required:** Document the change, impact assessment, migration steps.
2. **Deprecation marker:** v1.M tags the old method as `#[deprecated]`.
3. **Announcement:** `CHANGELOG.md` entry describing migration.
4. **Upgrade deadline:** v2.0 ships; old method is removed.

### 6.3 Criteria for trait promotion (unstable → stable)

**Measurable criteria (C3):**

- **Zero breaking changes in the last 2 minor releases** (e.g., v0.4 and v0.5 did not modify the trait signature).
- **≥30 days of real-world daily use** — same as public-release criterion D5 in RELEASE-POLICY.md.
- **At least one external impl in production** (not just internal gradatum-server).

If these are met, an RFC proposing promotion is high-confidence and can be fast-tracked.

---

## 7. Examples

### Example 1: Stable trait, adding method with default impl

```rust
// Before: v1.0
#[stability::stable]
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

// After: v1.1 (minor, no breaking change)
#[stability::stable]
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    
    /// New method with default: upstream can opt-in, no impl required.
    fn dimensions(&self) -> usize {
        384 // sensible default for bge-small-en-v1.5
    }
}
```

**Verdict:** Minor bump (v1.0 → v1.1). External impls do not need to change.

### Example 2: Stable trait, adding method without default impl

```rust
// Before: v1.0
#[stability::stable]
pub trait Storage: Send + Sync {
    fn read(&self, key: &str) -> Result<Vec<u8>>;
}

// After: attempted v1.1 (breaks SemVer!)
#[stability::stable]
pub trait Storage: Send + Sync {
    fn read(&self, key: &str) -> Result<Vec<u8>>;
    fn read_batch(&self, keys: &[String]) -> Result<Vec<(String, Vec<u8>)>>;
}
```

**Verdict:** **Major breaking change. Requires major version bump (v1.0 → v2.0) + RFC + 1-cycle deprecation.** All external impls must add `read_batch`.

**Correct workflow:**
1. v1.M (minor): Mark old method deprecated.
2. v1.M+1 or v1.M+2 (next minor): Announce "read will be removed in v2.0."
3. v2.0: Remove old method.

### Example 3: Unstable trait, changing signature

```rust
// v0.1
#[stability::unstable]
pub trait Reranker: Send + Sync {
    fn rerank(&self, docs: &[Doc], query: &str) -> Result<Vec<(Doc, f32)>>;
}

// v0.2 (breaking change, allowed for unstable)
#[stability::unstable]
pub trait Reranker: Send + Sync {
    /// Changed signature: now accepts metadata context.
    fn rerank(&self, docs: &[Doc], query: &str, context: &ReRankContext) 
        -> Result<Vec<(Doc, f32)>>;
}
```

**Verdict:** Allowed (unstable). Requires `CHANGELOG.md` entry: "Reranker::rerank now accepts context parameter."

External impls must update. No RFC required (not stable).

### Example 4: Experimental trait, Cargo feature gate

```toml
[features]
unstable-tool-use = []
```

```rust
#[cfg(feature = "unstable-tool-use")]
#[stability::experimental]
pub trait ToolUseBridge: Send + Sync {
    fn canonicalize(&self, call: &ProviderToolCall) 
        -> Result<CanonicalToolCall>;
}
```

Downstream that wants this:

```toml
[dependencies]
gradatum-core = { version = "0.1", features = ["unstable-tool-use"] }
```

**Verdict:** This trait may change **between patches** (v0.1.0 → v0.1.1 → v0.1.2). No stability guarantee. Consumer must opt-in explicitly.

### Example 5: Conformance testkit in downstream

**Downstream crate implementing Chat for OpenAI:**

```rust
// openai-gradatum/src/lib.rs
pub struct OpenAIChat { client: reqwest::Client }

#[async_trait::async_trait]
impl gradatum_core::Chat for OpenAIChat {
    async fn generate(
        &self,
        params: &GenerationParams,
    ) -> Result<Generation, ChatError> {
        // ... implementation
        Ok(Generation { /* ... */ })
    }
}

#[cfg(test)]
mod conformance {
    use gradatum_core::testkit::*;
    use super::*;

    #[tokio::test]
    async fn openai_chat_conforms() {
        chat_conformance!(OpenAIChat); // Runs all contract tests
    }
}
```

Run before publish:

```bash
cargo test --features gradatum-core/testkit
# Output: passed 8/8 conformance tests
cargo publish
```

### Example 6: Deprecation cycle (Stable trait)

```rust
// v1.0
pub trait Vault: Send + Sync {
    fn list_notes(&self) -> Result<Vec<Note>>;
}

// v1.2 — mark old method deprecated, add new one
pub trait Vault: Send + Sync {
    #[deprecated(
        since = "1.2",
        note = "Use list_notes_paginated(limit, offset) for large vaults. Will be removed in 2.0."
    )]
    fn list_notes(&self) -> Result<Vec<Note>>;

    fn list_notes_paginated(&self, limit: usize, offset: usize) 
        -> Result<(Vec<Note>, usize)>;
}

// v2.0 — old method removed
pub trait Vault: Send + Sync {
    fn list_notes_paginated(&self, limit: usize, offset: usize) 
        -> Result<(Vec<Note>, usize)>;
}
```

Downstream consumer compiling against v1.2:

```
warning: use of deprecated function `Vault::list_notes`
  |> Use list_notes_paginated(limit, offset) instead.
```

They have until v2.0 to migrate.

---

## 8. Testkit macro (AM2)

### 8.1 Signature and scope

The `gradatum_core::testkit` module exports parameterized conformance tests. Example for `Chat`:

```rust
#[macro_export]
macro_rules! chat_conformance {
    ($impl_type:ty) => {
        // Expands to:
        #[tokio::test]
        async fn chat_impl_responds() { /* ... */ }

        #[tokio::test]
        async fn chat_impl_respects_max_tokens() { /* ... */ }

        #[tokio::test]
        async fn chat_impl_handles_invalid_params() { /* ... */ }

        #[tokio::test]
        async fn chat_impl_timeout_respected() { /* ... */ }
        
        // ... more tests
    };
}
```

### 8.2 Behaviors verified

For `Chat`:
- Generates text output under normal conditions.
- Respects `max_tokens` parameter (output does not exceed it).
- Returns appropriate errors for invalid inputs (e.g., negative temperature).
- Completes within timeout.
- Handles concurrent calls without deadlock.

For `Embedder`:
- Returns vectors of correct `dimensions()`.
- Same input produces same output (deterministic, no randomness).
- Rejects `None` / empty inputs gracefully.
- Handles batch calls efficiently.

For `AclPolicy`:
- Matches rules correctly against bearer tokens.
- Rejects invalid tokens.
- Respects precedence (earlier rules win).

### 8.3 CI integration

In the `gradatum` workspace `Cargo.toml`:

```toml
[workspace]
members = ["crates/*"]

[[test]]
name = "conformance"
path = "tests/conformance.rs"
```

File `tests/conformance.rs`:

```rust
use gradatum_core::testkit::*;
use gradatum_chat::OpenAIChat;
use gradatum_embed::LocalEmbedder;

#[test]
fn all_core_impls_conform() {
    chat_conformance!(OpenAIChat);
    embedder_conformance!(LocalEmbedder);
}
```

CI runs this on every PR:

```yaml
# .forgejo/workflows/conformance.yaml
on: [push, pull_request]
jobs:
  conformance:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --test conformance --features gradatum-core/testkit
```

Failing conformance blocks merge.

---

## 9. Cross-references

- **`RELEASE-POLICY.md`** §AM1 (trait-stability tiers) — this RFC expands AM1 with decision matrix and examples.
- **`RELEASE-POLICY.md`** §AM2 (testkit) — this RFC details the macro signature and CI integration.
- **`RELEASE-POLICY.md`** §D5 (public-release criterion) — ≥30 days real-world use + functional parity = gate to move trait from `unstable` to `stable`.
- **`PROJECT-CONTEXT.md`** §4 (DAG of dependencies) — shows which crates depend on which traits in `gradatum-core`.
- **`GOVERNANCE.md`** → *see amendment below* — new section "RFC process for gradatum-core changes" links this RFC.

---

## 10. Alternatives considered

| Alternative | Pros | Cons | Reason rejected |
|---|---|---|---|
| **No stability tiers; SemVer strict from v0.1** | Simplicity; external impls trust SemVer 2.0.0 | Locks in design decisions too early; Phase 0bis stubs will thrash with real impl | Gradient (unstable → stable) allows learning before committing |
| **Single-tier: all traits unstable until v2.0** | Maximum flexibility during early development | External impls have zero confidence; upgrade churn; conflicts with "AI bus factor = 0" (AM4) | Hybrid E (0.x unstable, 1.0+ mostly stable with experimental gate) is sweet spot |
| **Calendar-based freeze (6 months = automatic stable)** | Predictable timeline | Arbitrary; a trait might need 3 months or 12 months of feedback | Use measurable criteria: 0 breaking changes in last 2 minors + 30 days real use + ≥1 external impl |
| **No deprecation cycle; major version bumps freely** | Implementation simplicity | Downstream pain; upgrading a dependency kills 10 things at once | 1-cycle deprecation is a modest investment for courtesy |

---

## 11. Drawbacks

1. **Complexity:** Three tiers instead of one. CI must enforce them. Maintainers must choose wisely. Mitigated: templates and examples reduce cognitive load.

2. **False confidence:** Marking a trait `stable` is a commitment. If the design is wrong, the project is locked in until a major version. Mitigation: require RFC + real-world use (30 days) before promotion.

3. **Feature flag overhead:** `unstable-*` flags clutter `Cargo.toml` and documentation. Mitigated: features are off by default; consumers opt-in explicitly.

4. **Testkit brittleness:** Conformance tests assume behavior semantics (e.g., "Chat respects max_tokens"). If the LLM backend is different (offline vs. online), tests might fail. Mitigated: testkit focuses on invariants (determinism, error handling) not fine-grained behavior.

5. **Maintenance burden:** As traits evolve, deprecation markers accumulate. Cleanup happens every major version. Mitigated: this is acceptable (happens ~once per 12-24 months for stable).

---

## 12. Unresolved questions

- **Q1:** Adopt SemVer 2.0.0 vocabulary strictly ("breaking change" vs. "breakage")? **Decision:** Yes, use SemVer terminology throughout. Enforced by `cargo semver-checks` output format.

- **Q2:** Is "1-cycle deprecation" (1 minor release between `#[deprecated]` and removal) too generous or too strict? **Decision:** Accepted. Downstream typically updates dependencies on quarterly cadence; 1 minor = ~3 months = reasonable. Revisit after first major release (v2.0).

- **Q3:** Conformance testkit tests behavior semantics (e.g., "Chat output respects max_tokens") or only compilation + type signatures? **Decision:** Both. Testkit verifies (1) impl compiles and (2) impl behaves per contract (determinism, error codes, timing). Semantic tests assume local/offline behavior; external online backends may skip tests labeled `[integration]`.

- **Q4:** Should external `AclPolicy` impls be allowed (opening the ACL system to plugins), or should `AclPolicy` remain internal only? **Deferred to RFC-0002** (Phase 1 design). Current assumption: `AclPolicy` is `unstable`, blocking third-party impls until RFC-0002 is accepted.

- **Q5:** Who is responsible for maintaining testkit conformance tests as traits evolve? **Deferred to `GOVERNANCE.md` RFC process.** Current assumption: maintainer updates testkit in the same PR as breaking changes.

---

## 13. Implementation checklist (Phase 0bis → Phase 1)

- [ ] Tag all traits in `gradatum-core/src/lib.rs` with `#[stability::stable]` or `#[stability::unstable]` or `#[stability::experimental]`.
- [ ] Create `gradatum-core/testkit.rs` module with `chat_conformance!`, `embedder_conformance!`, `acl_policy_conformance!` macros.
- [ ] Add `[features]` section to `gradatum-core/Cargo.toml`: `testkit = []`, `unstable-reranker = []`, `unstable-tool-use = []` (placeholder).
- [ ] Configure `Cargo.toml` for `cargo semver-checks`: add allowed-unstable rules for `unstable` and `experimental` traits.
- [ ] Create `.forgejo/workflows/conformance.yaml` CI job (Phase 0bis or Phase 1).
- [ ] Update `GOVERNANCE.md` section: "RFC process for gradatum-core changes" — link this RFC.
- [ ] Write section in `CONTRIBUTING.md`: "Trait stability policy" — short summary + link to RFC-0001.
- [ ] Update `RELEASE-POLICY.md` §AM1 and §AM2 with forward-reference: "See RFC-0001 for detailed rules and examples."

---

## References

- Rust RFC Process: https://rust-lang.github.io/rfcs/
- SemVer 2.0.0: https://semver.org/
- `cargo-semver-checks`: https://github.com/obi1kenobi/cargo-semver-checks
- `cargo-public-api`: https://github.com/Enselic/cargo-public-api
- Design review: Phase 0bis governance (2026-05-02)
