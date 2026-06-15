# RELEASE-POLICY.md

> How Gradatum versions, releases, and tracks supported branches.
> Hybrid policy E — single-track up to `1.0`, LTS + main from `1.0` to `2.0`, conditional codenames from `2.0+`.

---

## Versioning policy (D1, hybrid E)

```
0.x  ────►  1.0  ────►  1.x (LTS)  ────►  2.0  ────►  2.x  ────►  3.0 (codename CCCC)
       single-track       LTS + main          codenames triggered by objective signals
```

| Track | Window | Cadence | Stability promise |
|---|---|---|---|
| `0.x` | Until `1.0` | Frequent (weekly possible) | **No stability promise on public APIs.** Breaking changes require an RFC and a `CHANGELOG.md` entry. Minimum 6 months of `0.x` before `1.0`. |
| `1.x` LTS | After `1.0` | Quarterly minor; security patches monthly | SemVer strict. Backward-compatible additions only. Trait-stability tiers apply (see AM1). |
| `1.x` main | After `1.0` | Continuous | New features land here first. Breaking changes accumulate for the next major. |
| `2.x+` codenames | When triggered (see below) | Codename per major | Codenames are mnemonic, not marketing. They are issued only when an objective release-signal fires. |

**SemVer alignment:** every published crate follows SemVer 2.0.0. The umbrella `gradatum` crate aligns with the highest-precedence breaking change among its re-exports.

### Codename triggers (`2.0+`)

A codename is assigned only when at least one of the following objective signals fires:

- A schema migration that requires `gradatum-admin migrate --from=N --to=N+1`.
- A breaking change in a `gradatum-core` *stable* trait (see AM1).
- A removal of a public crate from the umbrella SDK.
- A change in the default LLM contract (chat or embed).

Codenames are taken from a published list in `CODENAMES.md` (created once `1.0` ships).

---

## Anti-fragility measures (AM1–AM4)

Four invariants protect the project from over-coupling and from "AI cannot replicate this work" risk.

### AM1 — Trait stability tiers

Every public trait in `gradatum-core` is tagged with one of three tiers in its rustdoc:

| Tier | Promise |
|---|---|
| `#[stability::stable]` | SemVer-strict. Cannot change in a minor. Breakage requires a major + RFC + 1-cycle deprecation. |
| `#[stability::unstable]` | May change between minors. Documented in `CHANGELOG.md`. |
| `#[stability::experimental]` | May change between patches. Used only behind a `unstable-` feature flag. |

The CI enforces tier consistency via `cargo public-api` + `cargo semver-checks`.

**Detailed rules, decision matrix, deprecation cycles, and examples:** see [`docs/RFC/RFC-0001-versioning-gradatum-core.md`](docs/RFC/RFC-0001-versioning-gradatum-core.md).

### AM2 — Contractual testkit

`gradatum-core` ships a `testkit` feature exposing trait-conformance tests. Any downstream impl must run them in CI:

```rust
#[cfg(test)]
mod conformance {
    use gradatum_core::testkit::*;
    chat_conformance!(MyChatImpl);
}
```

Failing the testkit blocks publishing the impl.

**Testkit macro signatures, conformance scope, and CI integration:** see [`docs/RFC/RFC-0001-versioning-gradatum-core.md`](docs/RFC/RFC-0001-versioning-gradatum-core.md) §8.

### AM3 — Crates.io name squatting

Reserved crate names on crates.io (`cargo publish` with empty stub):

- `gradatum`, `gradatum-core`, `gradatum-markdown`, `gradatum-vault`,
  `gradatum-storage`, `gradatum-cache`, `gradatum-index`, `gradatum-search`,
  `gradatum-queue`, `gradatum-acl-policy`, `gradatum-acl-auth`, `gradatum-auth`,
  `gradatum-chat`, `gradatum-curator`, `gradatum-embed`, `gradatum-engine`,
  `gradatum-server`, `gradatum-worker`, `gradatum-admin`, `gradatum-cli`,
  `gradatum-mcp-stub`, `gradatum-sdk-rs`.

Reservation happens at the same time the public release is announced.

### AM4 — "AI bus factor = 0"

Every contribution must be reproducible by a human reviewer **without** any AI assistant. This is enforced by:

- PR template asks "Have you read every line of the diff?" — required checkbox.
- Maintainers reject PRs that contain output the contributor cannot explain in plain English.
- All RFCs include a `Drawbacks` section authored without AI assistance.

The point is not to ban AI use; it is to ensure the project survives the AI assistant being unavailable.

---

## Public-release criterion (D5)

The stable release milestone (`v1.0.0`) requires both conditions to be met:

1. **Functional parity** with the predecessor v1.6.2 — every supported call (read, write, search, curate, ACL, multi-vault) returns equivalent or better results.
2. **≥30 days of real-world daily use** as the active memory backend on the maintainer's primary workstation.

These criteria are binary. There is no calendar deadline. Once both are met, the maintainers open a `release-prep` issue, run a final security audit, and tag `v1.0.0`.

### Early OSS availability

The repository may be made public **before `v1.0.0`** at the maintainer's discretion, to enable community feedback during the alpha/beta phase. The D5 criteria above govern the *stable release milestone*, not repository visibility. When the repository is public prior to `v1.0.0`, the status taxonomy below applies: APIs are not stable and no stability promise is made until Gold (`v1.0.0`).

---

## Status taxonomy

| Status | Meaning |
|---|---|
| **Alpha** | Active development; no compatibility guarantee; APIs change without notice. **Current state.** |
| **Beta** | Functional parity with the predecessor v1.6.2 reached; APIs stabilising; breaking changes still possible per `0.x` policy. |
| **Stable v1.0** | Public release. SemVer strict. LTS branch cut. |

---

## Modifying this document

Changes to `RELEASE-POLICY.md` follow the RFC + 14-day lazy-consensus process described in [`GOVERNANCE.md`](GOVERNANCE.md).
