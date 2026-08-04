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
| `0.x` | Until `1.0` | Frequent (weekly possible) | **No stability promise on public APIs.** Breaking changes require an RFC and a `CHANGELOG.md` entry. **1.0 ships when the D5 maturity criteria are met** (see *Public-release criterion* below), not on a fixed calendar. |
| `1.x` LTS | After `1.0` | Quarterly minor; security patches monthly | SemVer strict. Backward-compatible additions only. Trait-stability tiers apply (see AM1). |
| `1.x` main | After `1.0` | Continuous | New features land here first. Breaking changes accumulate for the next major. |
| `2.x+` codenames | When triggered (see below) | Codename per major | Codenames are mnemonic, not marketing. They are issued only when an objective release-signal fires. |

**SemVer alignment:** every published crate follows SemVer 2.0.0. The umbrella `gradatum` crate aligns with the highest-precedence breaking change among its re-exports.

### Codename triggers (`2.0+`)

A codename is assigned only when at least one of the following objective signals fires:

- A schema migration that is not backward-compatible with the previous on-disk index.
  (Migrations are applied automatically at startup by the embedded runner, which tracks
  applied revisions in the `_schema_migrations` table — there is no operator-run
  `migrate` subcommand, so the signal is the incompatible migration itself.)
- A breaking change in a `gradatum-core` *stable* trait (see AM1).
- A removal of a public crate from the umbrella SDK.
- A change in the default LLM contract (chat or embed).

Codenames are taken from a published list in `CODENAMES.md` (created once `1.0` ships).

---

## Anti-fragility measures (AM1–AM4)

Four invariants protect the project from over-coupling and from "AI cannot replicate this work" risk.

### AM1 — Trait stability tiers

Three tiers are defined for public traits in `gradatum-core`:

| Tier | Promise |
|---|---|
| `#[stability::stable]` | SemVer-strict. Cannot change in a minor. Breakage requires a major + RFC + 1-cycle deprecation. |
| `#[stability::unstable]` | May change between minors. Documented in `CHANGELOG.md`. |
| `#[stability::experimental]` | May change between patches. Used only behind a `unstable-` feature flag. |

**No tier is actually applied in `1.0.0`.** None of the 14 public traits in `gradatum-core`
carries a `stability::` attribute — the five occurrences in `src/` are rustdoc prose, two of
which state the attribute is deferred pending an `unstable-storage-traits` feature. Since no
tier is posted, there is nothing for CI to cross-check: `cargo public-api` and
`cargo semver-checks` run against the whole surface uniformly, and every public trait is
treated as SemVer-strict by default. Tagging the traits is planned for a `1.x` minor.

**Detailed rules, decision matrix, deprecation cycles, and examples:** see [`docs/RFC/RFC-0001-versioning-gradatum-core.md`](docs/RFC/RFC-0001-versioning-gradatum-core.md).

### AM2 — Contractual testkit *(planned, not shipped in 1.0.0)*

`gradatum-core` does not yet ship a `testkit` feature. Trait-conformance tests are planned for a
future minor; until then, downstream impls are validated against the trait signatures only.
The crate's only feature is `test-utils`, which exposes `InMemorySink` for consumer tests — it
carries no conformance macro.

**Planned testkit scope and CI integration:** see [`docs/RFC/RFC-0001-versioning-gradatum-core.md`](docs/RFC/RFC-0001-versioning-gradatum-core.md) §8.

### AM3 — Crates.io namespace

The names in scope for this namespace are the crates this workspace publishes, plus
`gradatum-cli` (see the caveats below):

- `gradatum`, `gradatum-acl-auth`, `gradatum-acl-policy`, `gradatum-admin`,
  `gradatum-auth`, `gradatum-cache`, `gradatum-chat`, `gradatum-cli`,
  `gradatum-core`, `gradatum-curator`, `gradatum-db-sqlite`, `gradatum-dto`,
  `gradatum-embed`, `gradatum-engine`, `gradatum-gateway`, `gradatum-index`,
  `gradatum-ingest`, `gradatum-markdown`, `gradatum-mcp-stub`, `gradatum-queue`,
  `gradatum-sdk-rs`, `gradatum-search`, `gradatum-server`, `gradatum-storage`,
  `gradatum-studio`, `gradatum-vault`, `gradatum-warden`, `gradatum-worker`.

Two caveats apply to that list:

- `gradatum-cli` is **no longer published from this workspace** — it carries
  `publish = false`. The name stays reserved and its last published version remains on
  crates.io; the crate is not part of the release train.
- `gradatum-studio` is published only as a `0.0.2` placeholder. The real crate is built and
  served from this workspace but has not yet been released to crates.io.

Publishing an entirely new crate name is a structural change and requires an RFC
(see [`GOVERNANCE.md`](GOVERNANCE.md)).

### AM4 — "AI bus factor = 0"

Every contribution must be reproducible by a human reviewer **without** any AI assistant. This is enforced by:

- Maintainers reject PRs that contain output the contributor cannot explain in plain English.
- All RFCs include a `Drawbacks` section authored without AI assistance
  (see [`RFC-TEMPLATE.md`](RFC-TEMPLATE.md) §6).

A repository PR template carrying a "Have you read every line of the diff?" checkbox is
**planned, not shipped**: `.github/` currently holds workflows only. Until it lands, AM4 is
enforced by maintainer review, not by tooling.

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
| **Alpha** | Active development; no compatibility guarantee; APIs change without notice. |
| **Beta** | Functional parity with the predecessor v1.6.2 reached; APIs stabilising; breaking changes still possible per `0.x` policy. |
| **Stable v1.0** | Public release. SemVer strict. |

---

## Modifying this document

Changes to `RELEASE-POLICY.md` follow the RFC + 14-day lazy-consensus process described in [`GOVERNANCE.md`](GOVERNANCE.md).
