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
| `0.x` | Until `1.0` | Frequent (weekly possible) | **No stability promise on public APIs.** Breaking changes are tracked as a project-map feature card and require a `CHANGELOG.md` entry. **1.0 ships when the D5 maturity criteria are met** (see *Public-release criterion* below), not on a fixed calendar. |
| `1.x` LTS | After `1.0` | Quarterly minor; security patches monthly | SemVer strict. Backward-compatible additions only. Trait-stability tiers apply (see AM1). |
| `1.x` main | After `1.0` | Continuous | New features land here first. Breaking changes accumulate for the next major. |
| `2.x+` codenames | When triggered (see below) | Codename per major | Codenames are mnemonic, not marketing. They are issued only when an objective release-signal fires. |

**SemVer alignment:** every published crate follows SemVer 2.0.0. The umbrella `gradatum` crate aligns with the highest-precedence breaking change among its re-exports.

**In this concrete instance, the `1.x` LTS branch was closed early, without a final security
fix.** Rather than following the quarterly-minor / monthly-patch cadence in the table above,
`1.x` was closed directly after `1.0.0` — see [`SECURITY.md`](SECURITY.md) § Supported
versions. The cadence above is the general policy for an LTS branch that is kept open; it was
never exercised for `1.x`.

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


### Tag namespaces

Two disjoint tag namespaces exist in this repository.

- **Public release tags** — `v{version}` (e.g. `v2.0.0`). Pushing one is an *event*, not a
  label: it starts both release pipelines, including the one that emits the CycloneDX SBOM and
  the SLSA build provenance attestation. Never create a `v*` tag for anything that is not going
  to the public repository and crates.io.
- **Internal milestones** — `internal/{version}` (e.g. `internal/2.0.6`). These name an
  increment deployed on the maintainer's own fleet without publishing it. They match no `v*`
  trigger, so they start no pipeline, and they are never pushed to the public repository.

**Why the two must stay disjoint.** A CI consistency check rejects any `vX.Y.Z` cited anywhere
in the documentation that has neither a `## [X.Y.Z]` CHANGELOG section nor a matching git tag —
either one suffices. Its comparison is a whole-line match, so an `internal/` tag never satisfies
it on behalf of a `v` version: an internal milestone cannot make an unpublished version look
shipped. That is the property this check exists to hold, and tagging an unpublished version
`vX.Y.Z` would silently destroy it.
---

## Anti-fragility measures (AM1–AM4)

Four invariants protect the project from over-coupling and from "AI cannot replicate this work" risk.

### AM1 — Trait stability tiers

Three tiers are defined for public traits in `gradatum-core`:

| Tier | Promise |
|---|---|
| `#[stability::stable]` | SemVer-strict. Cannot change in a minor. Breakage requires a major + a tracked feature card + 1-cycle deprecation. |
| `#[stability::unstable]` | May change between minors. Documented in `CHANGELOG.md`. |
| `#[stability::experimental]` | May change between patches. Used only behind a `unstable-` feature flag. |

**No tier is actually applied.** None of the 14 public traits in `gradatum-core` carries a
`stability::` attribute — the five occurrences in `src/` are rustdoc prose, two of which state
the attribute is deferred pending an `unstable-storage-traits` feature. Since no tier is
posted, there is nothing for CI to cross-check: `cargo public-api` and `cargo semver-checks`
run against the whole surface uniformly, and every public trait is treated as SemVer-strict by
default. Tagging the traits is planned for a future minor.

**The major + feature-card + 1-cycle deprecation requirement above applies once tiers are
posted, not before.** The promise is written for a `#[stability::stable]` trait, and no trait
carries that attribute yet: with no tier posted, there is no stable surface for a deprecation
cycle to protect, and every public trait is versioned as part of the ordinary major/minor/patch
surface — a breaking change to it requires a major and a `CHANGELOG.md` entry, nothing more.
This is the state the `gradatum-core` storage traits are in as of this release: their
signatures changed in a breaking way, directly, without a tracked feature card or a deprecation
cycle, because none of them carries a posted tier. Once a trait is tagged
`#[stability::stable]`, breaking it is held to the full promise — major, feature card, and one
full cycle of deprecation before removal.

> **Dérogation, actée le 2026-08-22.** La ligne 2.x tolère une rupture de surface publique en
> version **mineure** aux trois conditions cumulatives suivantes : (1) le symbole rompu figure
> nommément dans `semver_deviations` du manifeste de release, avec sa carte d'origine ; (2) le
> guide de migration de la version le documente ; (3) le journal des changements l'énumère. Une
> rupture qui ne remplit pas les trois est refusée par la chaîne, quel que soit le rang. Sur un
> rang de **correctif**, aucune dérogation n'est admise. Le rang est dérivé et croisé par
> `scripts/internal/resolve-release-rank.sh` ; l'appariement rupture <-> inventaire par
> `scripts/internal/check-deviation-match.py` (couple `lint`+`rendered`).

> **Dérogation, actée le 2026-08-23 : refus de préavis machine `#[deprecated]`.** Les symboles
> retirés en `2.1.0` ne reçoivent **aucun préavis machine** `#[deprecated]` dans une version
> antérieure — en particulier, aucune version `2.0.10` de dépréciations n'est publiée pour ce
> jalon. Raison retenue : publier un préavis exigerait un cycle de release public complet (les
> 10 portes de `safety-release-guard`, sur les 26 crates publiées) pour un avertissement
> compilateur qui ne bénéficie qu'au consommateur exécutant `cargo update` dans l'intervalle
> entre les deux versions — le coût opérationnel excède le bénéfice pour cette rupture précise.
> Compensation actée en échange, et qui reste **obligatoire** pour `2.1.0` : le guide et le
> script de migration publics (carte F-249). Cette dérogation s'ajoute aux trois conditions
> cumulatives ci-dessus, elle ne les remplace pas — un symbole retiré sans préavis machine doit
> toujours figurer nommément dans `semver_deviations`, dans le guide de migration, et dans
> `CHANGELOG.md`.

**Detailed decision-matrix rules, deprecation-cycle examples, and Cargo-feature mechanics for
this tiering scheme** were drafted in a design RFC that predates project-map feature-card
tracking and has since been retired; no standalone document currently expands on the tier
table above. The tier definitions in this section are the current complete public record until
a tier is posted on a trait.

### AM2 — Contractual testkit *(planned, not shipped in 1.0.0)*

`gradatum-core` does not yet ship a `testkit` feature. Trait-conformance tests are planned for a
future minor; until then, downstream impls are validated against the trait signatures only.
The crate's only feature is `test-utils`, which exposes `InMemorySink` for consumer tests — it
carries no conformance macro.

**Planned testkit scope and CI integration:** not yet drafted publicly; tracked as future work
once trait-stability tags are introduced (see AM1 above).

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
- `gradatum-studio` is published on crates.io. The early `0.0.x` versions were placeholders;
  the crate now ships the real, built UI bundle, with its published version tracking the
  workspace release.
- `gradatum-mcp-stub` is **retired from the distribution as of `2.0.0`** — `publish = false`,
  no longer built or shipped. Its last published crates.io version remains `1.0.0`. The name
  stays reserved; source is kept in-tree (see [`ARCHITECTURE.md`](ARCHITECTURE.md) § API
  surface topology for why, and what replaces it).

Publishing an entirely new crate name is a structural change, tracked the same way as any
other (see [`GOVERNANCE.md`](GOVERNANCE.md) § Structural change tracking).

### AM4 — "AI bus factor = 0"

Every contribution must be reproducible by a human reviewer **without** any AI assistant. This is enforced by:

- Maintainers reject PRs that contain output the contributor cannot explain in plain English.

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

Changes to `RELEASE-POLICY.md` follow the same PR + maintainer review process described in [`GOVERNANCE.md`](GOVERNANCE.md).
