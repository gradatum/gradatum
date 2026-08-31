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
| Structural change (new crate, removed crate, public-API change in `gradatum-core`, schema migration, port change, new dependency) | PR + maintainer review, same as above — tracked to completion as a **project-map feature card** (see below), not a written design document. |
| Versioning policy, governance, license, code of conduct | PR + maintainer review; the lead maintainer breaks a stalled tie. |

---

## Structural change tracking (project-map)

Gradatum does not run a written-RFC-and-vote process. Structural changes are proposed and
discussed the same way as any other change — **open an issue first** to align scope (see
[`CONTRIBUTING.md`](CONTRIBUTING.md)) — and, once scope is agreed, a maintainer tracks the work
to completion as a **project-map feature card**: a note in the vault's `project-map` section,
identified by a server-assigned, immutable `[[feature:F-XX]]` link and carrying typed wikilinks
for the work-lifecycle axis (`[[status:BRAINSTORMING|OPEN|IN_PROGRESS|BLOCKED|DONE|OBSOLETE]]`),
the kind of change (`[[kind:FEATURE|ENHANCEMENT|FIX|TASK]]`), the delivery axis
(`[[release:roadmap|planned|released|dropped]]`), and, once a version is targeted, that version
(`[[version:gradatum/x.y.z]]`).

A maintainer creates the card via the `create_feature_card` MCP tool (equivalently, `POST
/api/v1/project-map/create-feature`) — the server allocates the `F-XX` number, the caller never
picks it. This is a maintainer-side tracking mechanism, not a public collaboration surface:
external contributors track and discuss scope through the issue tracker, the same as before;
they do not write project-map cards directly. Only `kind:FEATURE` cards are mirrored to the
public roadmap (`README.md` § Roadmap, gradatum.org); `FIX`/`TASK`/`ENHANCEMENT`
cards stay in the maintainers' tracking vault.

The durable, publicly readable record of what shipped, and why, is **`CHANGELOG.md`** — one
entry per version. That record does not change with this section; only the mechanism a
maintainer uses to track work-in-progress between issue and changelog entry does.

Changes to public traits in `gradatum-core` (trait addition, method addition, signature change,
stability-tier promotion) go through the same PR-plus-feature-card path as any other structural
change — there is no separate heavier process for this crate specifically. The trait-stability
tier definitions themselves (what a `stable`/`unstable`/`experimental` tag promises) are policy,
not process, and live in [`RELEASE-POLICY.md`](RELEASE-POLICY.md) § AM1.

> **Historical note.** This repository formerly ran a written-RFC process
> (`docs/RFC/RFC-NNNN-*.md`, a `RFC-TEMPLATE.md` skeleton, a 7-day discussion window). It is
> retired: no change has gone through that process since project-map feature cards were
> introduced, and this document had drifted into describing a workflow nobody was following.
> The RFC content that still described current behavior (the single-port, path-prefix HTTP/MCP
> routing decision) was folded into [`ARCHITECTURE.md`](ARCHITECTURE.md); the rest — including
> the never-implemented `gradatum-core` trait-stability tagging design and the Windows/macOS
> portability rules for a platform tier no longer supported — was historical record only and was
> not carried forward.

---

## Versioning & release

See [`RELEASE-POLICY.md`](RELEASE-POLICY.md). The release-train cadence, version policy, and public-release criterion live there to keep this file focused on people and process.

---

## Code of conduct

This project follows [Contributor Covenant 2.1](CODE_OF_CONDUCT.md). Maintainers enforce it; the lead maintainer is the escalation path.

---

## Modifying this document

Changes to `GOVERNANCE.md` follow the same PR + maintainer review as any structural change (see
above); there is no separate written-proposal process.
