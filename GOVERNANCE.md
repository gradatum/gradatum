# GOVERNANCE.md

> How decisions are made on Gradatum.
> Lightweight while the project is in `0.x`; tightens as the project approaches `1.0`.

---

## Roles

| Role | Responsibility |
|---|---|
| **Maintainer** | Has commit and merge rights to `main`. Listed in [`MAINTAINERS.md`](MAINTAINERS.md). |
| **Reviewer** | Approves PRs in their area of expertise. Promoted from sustained contributor activity. |
| **Contributor** | Anyone who opens an issue or PR. Must have signed the [CLA](CLA.md). |
| **Lead maintainer** | Single tie-breaker for stalled votes. Rotates yearly once `bus_factor ≥ 3`. |

The role of `Lead maintainer` is intentionally singular and explicit. It exists only to break ties — not to override consensus.

---

## Decision-making

| Change scope | Process |
|---|---|
| Bug fix, doc typo, dependency bump within SemVer compatible range | Single maintainer review + merge. |
| New feature inside an existing crate, no public-API change | One maintainer + one reviewer review. |
| Structural change (new crate, removed crate, public-API change in `gradatum-core`, schema migration, port change, new dependency) | **RFC required** — see below. |
| Versioning policy, governance, license, code of conduct | RFC + lazy-consensus 14 days + lead-maintainer ratification. |

**Lazy consensus:** an RFC is accepted if no maintainer raises a sustained objection within the lazy-consensus window. Silence = acceptance. An objection must propose a concrete alternative or a measurable concern — "I disagree" alone is not sufficient.

---

## RFC process

Structural changes follow the RFC workflow:

1. **Draft.** Open a PR adding `rfcs/NNNN-short-title.md` based on [`RFC-TEMPLATE.md`](RFC-TEMPLATE.md).
2. **Discussion.** Minimum 7 days open for comment. Maintainers and reviewers discuss inline.
3. **Resolution.** One of: `accepted` (merged with status updated to `accepted`), `postponed` (filed for later), `rejected` (PR closed with rationale).
4. **Implementation.** A new tracking issue is opened referencing the RFC. The implementation may span multiple PRs; each cross-references the RFC number.

RFC numbering is monotonic. Numbers are not reused.

### RFC process for `gradatum-core` changes

Changes to public traits in `gradatum-core` (trait addition, method addition, signature change, stability-tier promotion) **always require an RFC**, even if logically simple. Reason: trait changes break downstream implementations across the ecosystem. An RFC ensures:

- **Explicit decision-making:** trait changes are discussed openly, not merged silently.
- **Forward-looking documentation:** future maintainers (or AI agents with limited context) can trace why a trait looks the way it does.
- **Coordination:** external implementers (users, third-party crates) see the decision window and can raise concerns.

Scope:
- **Requires RFC:** add public trait, remove public trait, change method signature, change method return type, add supertrait bound, promote `unstable` → `stable`, deprecate/remove method from `stable` trait, split or merge traits.
- **Does not require RFC:** add method with default impl to sealed trait (minor bump), adjust documentation, adjust internal types, internal refactoring that preserves public surface.

See [`docs/RFC/RFC-0001-versioning-gradatum-core.md`](docs/RFC/RFC-0001-versioning-gradatum-core.md) for the full stability-tier policy, decision matrix, and examples.

---

## Versioning & release

See [`RELEASE-POLICY.md`](RELEASE-POLICY.md). The release-train cadence, version policy, and public-release criterion live there to keep this file focused on people and process.

---

## Code of conduct

This project follows [Contributor Covenant 2.1](CODE_OF_CONDUCT.md). Maintainers enforce it; the lead maintainer is the escalation path.

---

## Modifying this document

Changes to `GOVERNANCE.md` require an RFC and 14-day lazy-consensus, regardless of how minor they appear.
