# Contributing to Gradatum

Thank you for considering contributing to Gradatum! This is an alpha-stage
project being built openly. Issues, ideas, and PRs are welcome.

> **Note**: This is a stub. A fuller `CONTRIBUTING.md` (style guide, dev setup,
> commit conventions, release process) will land in **Phase 4**. For now, the
> essentials are below.

---

## Quick links

- 🐛 **Bug reports / feature requests** → [Issues](https://github.com/gradatum/gradatum/issues)
- 🔀 **Pull requests** → [PRs](https://github.com/gradatum/gradatum/pulls)
- 📜 **License** → [Apache-2.0](LICENSE)
- ✍️ **Contributor License Agreement** → [CLA.md](CLA.md)
- 🤖 **AI assistants** → [AGENTS.md](AGENTS.md)
- 🏗️ **Architecture** → [ARCHITECTURE.md](ARCHITECTURE.md)
- 🗺️ **Roadmap** → [CHANGELOG.md](CHANGELOG.md)

---

## Contributor License Agreement (CLA)

Before your Pull Request can be merged, you must **sign the
[Gradatum CLA](CLA.md)**.

Signing is automated through the **CLA Assistant bot**: on your first PR, the
bot will post a comment with a one-click sign link (GitHub OAuth). After
signing once, all future PRs from your account are covered.

The CLA is based on the standard Apache Software Foundation ICLA, with one
adaptation: it grants Gradatum maintainers the right to relicense the project
in the future if needed (for example, to defend against unfair commercial
exploitation). You retain copyright on your contributions.

See [CLA.md](CLA.md) for the full text.

## Code of conduct

We follow the [Contributor Covenant](https://www.contributor-covenant.org/).
A formal `CODE_OF_CONDUCT.md` will land in Phase 4. In the meantime: be
respectful, be patient, assume good faith. Disrespectful behavior toward any
contributor — newcomer or veteran — is not tolerated.

## Submitting a Pull Request

1. **Open an issue first** for substantive changes (architecture, new
   features) so we can align scope before you invest time.
2. **Fork** the repo, create a topic branch (`fix/X`, `feat/Y`, `docs/Z`).
3. **Follow the existing code style** — `cargo fmt` + `cargo clippy
   --workspace -- -D warnings` must pass.
4. **Add tests** for behavior changes.
5. **Sign the CLA** when prompted by the bot.
6. **Open a PR** with a clear description of intent and impact.
7. A maintainer will review. Be patient — this is a side project for many of
   us.

## Reporting security issues

**Do not open a public issue** for security vulnerabilities.
See [`SECURITY.md`](SECURITY.md) for the full disclosure policy and contact details.

## Development workflow

For feature work and bug fixes, the standard flow is:

1. **Open an issue** to discuss scope and design before writing code.
2. **Fork** the repository and create a topic branch (`fix/X`, `feat/Y`, `docs/Z`).
3. Implement your change. Keep commits focused; `cargo fmt` + `cargo clippy --workspace -- -D warnings` must pass locally.
4. **Open a PR** against `main`. Reference the issue in the PR description.
5. A maintainer will review. Substantial changes (new crates, breaking API changes, schema migrations) require an RFC first — see [`RFC-TEMPLATE.md`](RFC-TEMPLATE.md).
6. Once approved, a maintainer merges.

For version history and current roadmap, see [CHANGELOG.md](CHANGELOG.md).

---

## `deny.toml` ignore policy (security advisories)

Gradatum uses [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) in CI
to enforce supply-chain security: advisories, licenses, banned crates, and
sources. The `[advisories.ignore]` list in [`deny.toml`](deny.toml) records
RustSec advisories explicitly accepted as known risks. To preserve the integrity
of this gate, additions follow a strict policy.

**Glossary** (terms used below):
- **Concern**: a tracked item with an explicit verdict (`Accepted` / `Mitigated`)
  documented in a decision file committed in this repository under `docs/decisions/`.
- **Maintainer review**: a recorded review by
  multiple maintainers before merge, captured in the PR thread or in a
  decision file under `docs/decisions/`.

### Allowed pattern (all conditions required)

An advisory may be added to `[advisories.ignore]` **only** when **all** of the
following hold:

1. The advisory is **explicitly resolved with a documented verdict** (Accepted
   or Mitigated) in a decision file that is **committed in this repository**
   under `docs/decisions/<decision>.md` §Concerns.
2. The exception entry **includes a `reason` field** referencing the verdict
   (e.g., `reason = "Concern T08 — sqlite-only usage, rsa via sqlx-mysql, no upstream fix"`).
3. A **comment header above the entry** documents:
   - (a) **Why** the dependency is reachable in the build graph.
   - (b) **Why** the fix is not currently available (no upstream patch, archived
     repo, etc.).
   - (c) **`REVISIT <Phase X.Y or YYYY-MM-DD>`**: target version, phase, or
     calendar deadline for the next mandatory reassessment.
4. The PR introducing the ignore is **reviewed by maintainers before merge**
   if it is not already explicitly resolved in the relevant phase plan.

### Emergency advisory fast-track (CVSS ≥ 9.0 or 0-day public)

When a CRITICAL advisory is published mid-sprint and would block a release:

1. Maintainer-on-call approval within **4 hours** (recorded in PR comment).
2. Ignore entry includes `reason = "EMERGENCY — <advisory-id> — approved <maintainer> <date>"`
   plus the standard 3-line comment header (with `REVISIT` set to the next
   sprint at the latest).
3. **Post-commit maintainer review within 48 hours** to confirm the analysis
   (code path unreachable, mitigation in place, etc.).
4. If the post-commit review revokes the emergency, the ignore is removed and
   the build of the impacted release is re-cut.

The fast-track is reserved for genuine emergencies (CVSS ≥ 9.0 or
exploitation-in-the-wild). Convenience use of the fast-track is forbidden.

### Forbidden patterns (PRs including these will not be merged as-is)

- **Blanket ignore** — no `reason` field, no comment header, no `REVISIT`
  deadline.
- **Convenience ignore** to make CI green without arbitration in a phase plan
  or maintainer review.
- **Yanked-handling downgrade** (deny → warn) as a quick fix when a transitive
  dependency is yanked. Pin the indirect dep or switch crate instead.
- **License exception** for non-OSI-approved or copyleft-incompatible licenses
  without explicit legal review.
- **Fake-Concern bypass** — invoking a Concern that lives only in a personal
  note, an external doc, or an uncommitted file. The decision must be a committed
  file under `docs/decisions/`.
- **Indefinite ignore** without `REVISIT` deadline. Every ignore is reviewed
  at the deadline or at the next phase plan, whichever comes first.

### Scope beyond `[advisories.ignore]`

The same discipline applies, with adapted templates, to:

- **`[licenses].exceptions`** — any dual-license or non-standard SPDX
  identifier requires the same 3-line comment header (reason, why unavoidable,
  REVISIT deadline) plus a legal-review note when the identifier is not
  Apache-2.0 compatible.
- **`[bans].skip`** — exemptions for `*-sys` crates or known duplicates require
  a `reviewed-on <YYYY-MM-DD> by <maintainer>` comment and a `REVISIT`
  deadline.
- **`[sources]`** — unknown git registries or untrusted sources require an
  explicit Concern in a committed decision file + maintainer review before being
  added to `allow-git` / `allow-registry`. No silent exception.

### Current `[advisories.ignore]` entries (state of the tree)

As of this writing, `deny.toml` contains two active ignore entries:

- **`RUSTSEC-2025-0141`** — `bincode` v2 unmaintained (team ceased
  development). No security vulnerability, classification is "unmaintained"
  only. Used transitively by `gradatum-queue` and `gradatum-server`. Migration
  to an alternative (`postcard`, `bitcode`, or `rkyv`) is deferred to
  Phase 2.1+, when the queue data format is stabilised.
- **`RUSTSEC-2025-0068`** — `serde_yml` 0.0.12 unsound (emitter segfault).
  Only the `Deserializer` is used (config reading), so the unsafe emitter
  code path is never executed. Upstream repo is archived; migration to
  `serde-yaml-ng` is planned in a future supply-chain hardening PR.

Both entries follow the policy above (reason field present; multi-line
comment header documenting (a)/(b)/(c) and the migration target).

A previous entry, `RUSTSEC-2024-0436` (`paste` crate, transitive via
`fastembed`), was removed from the active list after the HTTP-stack bump made
`paste` no longer transitively reachable. The entry is kept as a comment in
`deny.toml` for historical traceability.

### What if the advisory is not yet documented?

Open a **draft PR** with the proposed ignore + `reason` + 3-line comment
header (including `REVISIT`), then request a **maintainer review** before
merge. The review outcome is recorded either in the PR thread or in a
decision note referenced from the PR description.

**Voice availability**: if a maintainer review is requested and a reviewer is
unavailable for more than **48 hours**, the PR is escalated to the lead
maintainer for unilateral decision (recorded in the PR description), to avoid
indefinite blocking.

## License

By contributing to Gradatum, you agree that your contributions will be
licensed under [Apache-2.0](LICENSE), subject to the terms of the
[CLA](CLA.md).

## Linux-only platform note

gradatum targets **Linux exclusively** as of 2026-06-05. Windows/cross-platform support is
deferred indefinitely. See [RFC-0002](docs/RFC/RFC-0002-cross-platform-support.md) (superseded)
for historical context on the prior tiered-support model.

PRs are validated on Linux x86_64 and Linux aarch64 only. No Windows cross-compile job
runs in CI. The portability rules from RFC-0002 §5 (R1–R13) are archived; they are no
longer required in the PR checklist.
