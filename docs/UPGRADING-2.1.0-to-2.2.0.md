# Upgrading from 2.1 to 2.2

**Read this before upgrading.** 2.2.0 is a *minor* release. If your `Cargo.toml` allows it
(`gradatum-core = "2.1"`, …), Cargo **adopts 2.2.0 silently** — no action on your part. And if you
**operate a `project-map` registry**, the write contract of that registry changes under you: cards
your automation writes today may be **refused** by the 2.2 validator, with no earlier deprecation
cycle. This guide is written for both moments.

This is the third migration guide of the project, after
[`UPGRADING-1.0.0-to-2.0.0.md`](UPGRADING-1.0.0-to-2.0.0.md) and
[`UPGRADING-2.0.0-to-2.1.0.md`](UPGRADING-2.0.0-to-2.1.0.md). It is a **standalone** guide, one file
per version line, matching the two guides before it — see [Why this guide is
standalone](#why-this-guide-is-standalone-not-cumulative).

---

## Who this concerns

There are **two distinct audiences**, and the breaks that hit them are different:

| You are… | What breaks | Where to look |
|---|---|---|
| **A Rust library consumer** — you depend on `gradatum-core` / other crates | Almost nothing new. 2.2 is **additive** at the library surface; the new enum variants are absorbed by `#[non_exhaustive]` | [§Library consumers](#library-consumers-rust-crates) |
| **A registry operator** — you write cards into a `project-map` vault via the API | The **model** changes and the **write contract** tightens. Cards you write today can be refused | [§The registry model change](#the-registry-model-change-f-184) onward |

**The critical property of a minor release:** because 2.2.0 is adopted automatically, this guide is
the *only* warning you get. Do not skip it because "a minor cannot break me" — for a registry
operator, a minor changes what the server *accepts*, which no `Cargo.toml` pin protects you from once
you point at a 2.2 server.

---

## The break inventory

Two classes of break, and this guide states **which class each one is** — because they are found by
different means, and one class is invisible to the compatibility tooling.

| Break | Class | Detected by | Card |
|---|---|---|---|
| The version becomes a **card**; work cards no longer carry `[[version:]]`/`[[release:]]`; release is **derived** from `[[track:]]` | **Model / data-contract** | **Hand-inventoried** — invisible to public-api diff (it is about accepted *data*, not symbols) | **F-184** |
| Role-coherence guards at the validator: incoherent role combinations and unknown roles are **refused** instead of silently accepted | **Data-contract** | **Hand-inventoried** — invisible to public-api diff (refuses previously-accepted data) | **F-213** |
| `[[visibilite:]]` axis extended to work cards; strict on structure cards (ROADMAP requires exactly 1, BACKLOG forbids it) | **Data-contract** | **Hand-inventoried** | **F-256** |
| New wire kinds `[[kind:ROADMAP]]` / `[[kind:BACKLOG]]` (structure cards); Rust `KindKind::Roadmap` / `::Backlog` variants | **Library surface (additive)** | **Tool-adjacent** — additive under `#[non_exhaustive]`, so **not a compile break** | **F-184** |
| New listing endpoint `GET /api/v1/project-map/cards?version=<v>` | **HTTP surface (additive)** | **Additive** — no removal | **F-211** |

> **How the tool measurement was done, and its result.** The sanctioned measurement is the
> public-api surface reference diffed against the previous line
> (`git diff v2.1.0..HEAD -- public-api/baseline/`). At the time of writing that diff is **empty**:
> no *breaking* symbol change is measured for 2.2.0. This is expected — the 2.2 breaks are on
> *accepted data* and *behaviour*, which the surface tool cannot see, exactly as F-254 predicted.
> ⚠️ **Caveat, and a release-gate item, not a consumer item:** the `gradatum-core` baseline blob is
> **stale** — it has not been regenerated since the 2.1.0 remediation and does **not** yet list the
> additive `KindKind::Roadmap` / `::Backlog` variants present in the source. The baseline must be
> regenerated before the 2.2.0 tag so the additive-only claim is *measured*, not asserted. Until
> then, the "tool detected nothing breaking" line above rests on an out-of-date baseline for the
> additive rows; the **hand-inventoried** rows stand on their own.

Everything below expands the hand-inventoried rows — the ones that actually change what a registry
operator must do.

---

## The registry model change (F-184)

This is the heaviest break of the milestone, and the one an earlier version of the F-254 card
wrongly denied. **The version stops being an attribute of a card and becomes a card of its own.**

### Before (2.1 model)

A work card carried its own version and publication state inline:

```
[[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] [[version:gradatum/2.1.0]] [[release:released]]
[[feature:F-42]]
```

- `[[version:]]` — which version the card belongs to.
- `[[release:]]` — its publication state (`roadmap` / `planned` / `released` / `dropped`), **stored**
  on the card and edited by hand as reality moved. It drifted (that is what F-184 removes).

### After (2.2 model)

The version is now a **structure card** (`[[kind:ROADMAP]]` per version, `[[kind:BACKLOG]]` per
project). Work cards attach to it with a single `[[track:]]` pointer and **no longer store**
`[[version:]]` or `[[release:]]`:

```
# Work card
[[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] [[track:gradatum/2.1.0]] [[feature:F-42]]

# Structure card (ROADMAP) it points at
[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] [[version:gradatum/2.1.0]] [[visibilite:public]]
```

- **`[[track:]]`** = the single attachment. `project/<semver>` points at a ROADMAP;
  `project/backlog` points at the project BACKLOG.
- **Release is derived, never stored.** The server computes it from the card's own status first,
  then the tracked structure — see below. The stored `[[release:]]` axis is *gone* from work cards.
- **Structure cards** are pointed at and point at nothing: they carry a `[[version:]]` (their
  identity), never a `[[track:]]`, never a `[[feature:]]`. A ROADMAP carries exactly one
  `[[visibilite:]]` (the interne/public gate governing whether its cards reach the public site); a
  BACKLOG carries none (a backlog is never published).

### The derivation rule (status-of-card first)

`gradatum_core::project_map::derive_release(card_status, track_target)` is the single source of
truth. Order matters:

1. Card `DONE` ⇒ `released` — **whatever it tracks**.
2. Card `OBSOLETE` ⇒ `dropped`.
3. Otherwise (non-terminal card) derive from the tracked structure:
   - tracks a **BACKLOG** ⇒ `roadmap`;
   - tracks a **ROADMAP** that is `DONE` ⇒ `released`; `OPEN`/`IN_PROGRESS`/`BLOCKED` ⇒ `planned`;
     `OBSOLETE` ⇒ `dropped`; `BRAINSTORMING` ⇒ `roadmap`.
4. Roll-forward: a non-terminal card under an `OBSOLETE` ROADMAP that carries a `[[porteuse:]]`
   derives from the **carrier's** status (one level).

> There is no `Backlog` variant in `ReleaseKind`: its vocabulary mirrors the website
> (`roadmap` / `planned` / `released` / `dropped`). A BACKLOG structure derives to `release:roadmap`.

---

## The write-contract change (F-213 + F-256)

The 2.2 validator (`validate_links`) **refuses** combinations that 2.1 accepted silently, and
refuses unknown roles instead of swallowing them. Every refusal below is a **real** message. Each
one **names what to write instead** where a substitution exists — that is a design requirement of
F-254 (criterion 3), so a refusal on a contract that just changed is self-documenting.

| If you write (valid in 2.1) | 2.2 refuses with | Write instead |
|---|---|---|
| A feature work card with neither `[[version:]]` nor `[[track:]]` | `feature work card must be derivable: needs a [[version:]] or a [[track:]] (found neither)` | Add a `[[track:project/<version|backlog>]]` |
| `[[release:roadmap]]` **and** a concrete `[[version:project/x.y.z]]` | `incoherent roles: [[release:roadmap]] means no committed target version, but a concrete [[version:...]] is present. Use [[release:planned]] ... or drop the version ...` | Use `[[release:planned]]`, or drop the version (or use the `project/backlog` sentinel) |
| Two `[[release:]]` links on a work card | `incorrect number of release: links (0 on a structure card, at most 1 on a work card)` | At most one |
| Two `[[version:]]` links | `at most 1 version: link allowed` | At most one |
| A ROADMAP/BACKLOG carrying a `[[track:]]` | `track: link forbidden on a structure card (ROADMAP/BACKLOG)` | Remove it — structure cards are pointed at, they point at nothing |
| A ROADMAP without a `[[visibilite:]]` | `ROADMAP card: exactly 1 visibilite: link required` | Add exactly one (`interne` or `public`) |
| A BACKLOG carrying `[[visibilite:]]` | `visibilite: link forbidden on a BACKLOG (a backlog is never published)` | Remove it |
| A structure card without exactly one addressable version | `structure card (ROADMAP/BACKLOG): exactly 1 addressable version: link required` | Give it exactly one `[[version:project/x.y.z]]` |
| A structure card carrying a `[[feature:]]` | `feature: link forbidden on a structure card (ROADMAP/BACKLOG are never numbered)` | Remove it |
| An unknown/untyped role | `link without typed prefix nor section:ULID format: ...` | Use a known typed role — unknown roles are no longer swallowed |

**The `[[release:]]` axis on work cards is being retired**, not just constrained: after the model
migration a work card should carry no `[[release:]]` at all (release is derived). The validator still
tolerates *at most one* during the transition, but the end state is zero.

---

## The new listing surface (F-211) — additive, no action required

`GET /api/v1/project-map/cards?version=<v>` lists every card of a milestone — work **and** structure
— in one request, with each axis (id, `F-XX`, status, kind, **derived** release, version,
visibility, title, dependency roles) **named as a column**. It replaces the old "substring oracle"
(searching for a version string) and the export+manual-filter workaround. It is purely additive:
nothing you call today is removed. `release` is the **derived** value (never the stored one, which is
being retired); a work card with an unresolved `[[track:]]` is listed with `release: null` — the null
is the visible signal of the anomaly, never a silent drop.

---

## Make-before-break — the operational rule that governs the whole migration

**This is the single most important thing in this guide.** The migration removes stored data
(`[[version:]]`/`[[release:]]`) that the export path used to read. If you remove the stored axis
**before** the readers that derive it are deployed, **the export breaks** — it will read a value that
is no longer there. This regression was **lived and corrected during the 2.2 development itself**
(internal 2.1.2→2.1.5, "fix export dérivé").

The rule is therefore:

> **Deploy the derived readers first. Remove the stored data last. Never the other way around.**

Concretely, the ordering invariant across the phases is:

1. `derive_release` and the bi-form readers (which accept *both* the stored axis **and** the derived
   `[[track:]]`) must be **live in production** before any card is stripped.
2. Only once the readers no longer *depend* on the stored axis may you strip it.

Between those two points the data is **double-written**: work cards carry both the legacy
`[[version:]]`/`[[release:]]` **and** the new `[[track:]]`. That overlap is deliberate — it is what
lets you flip readers and verify parity *before* the irreversible strip. Skipping the overlap is the
exact mistake that broke the export.

---

## The migration sequence, with gates

The migration ran in phases. Each phase has a **dry-run-by-default** script under
`scripts/internal/` (see [The migration scripts](#the-migration-scripts)). The gates in the right
column are the maintainer's own internal release discipline — a registry operator adapting this to their own deployment
should keep the equivalent checkpoints.

| # | Phase | What it does | Reversible? | Gate before it |
|---|---|---|---|---|
| 3 | **Create structure cards** | Writes one ROADMAP per version, one BACKLOG per project, the `0.0.0` bootstrap ROADMAP, the permanent `system` ROADMAP | Yes (new cards only) | dry-run reviewed |
| 4 | **Double-write `[[track:]]`** | Adds `[[track:]]` to every live work card, keeping `[[version:]]`/`[[release:]]` | Yes (additive) | dry-run reviewed; targets all resolve |
| 5 | **Census** | Read-only. Derives release from `[[track:]]`, compares to the stored `[[release:]]`, sorts each divergence into *derivation bug* vs *pre-existing registry incoherence* | N/A (read-only) | — |
| 6 | **Flip readers + DEPLOY** | Ships `derive_release` + bi-form readers so release is computed server-side. **This is the make-before-break pivot** | Code deploy | regression-safety review + operator GO |
| 7 | **Strip stored axis + DEPLOY** | Removes `[[version:]]`/`[[release:]]` from work cards. **Irreversible.** Forbidden while any derivation blocker remains | **Irreversible** | Phase 6 live & verified; census clean; operator GO |
| 8 | **Governance** | The maintainer's internal governance review of structuring docs (out of scope for a registry operator) | — | maintainer review |

The census (Phase 5) is the gate that makes the irreversible strip safe: it must report **0
non-derivable cards** and **0 derivation bugs** before Phase 7. Pre-existing registry incoherences it
surfaces are *hygiene*, not blockers to the mechanism — but they were masked by the stored axis and
should be resolved on their own track.

---

## The migration scripts

The four phase scripts are the **sanctioned record of how this migration was executed**, not a
generic tool. They are hardcoded to the `gradatum` project, read frozen Phase-0/1 inventory files,
and target the live loopback server — they are **not directly reusable** by an external registry
without adaptation. They are documented here as the procedure, with their safety properties, so an
operator can read exactly what each phase did and port the shape to their own deployment.

| Script | Phase | Key safety properties |
|---|---|---|
| `scripts/internal/phase3-create-structure-cards.py` | 3 | Dry-run by default (`--apply` required + `GRADATUM_URL`/`GRADATUM_APIKEY`). Every value **derived** from frozen inventory, never memory. Async write confirmed by polling `job_status` to a terminal `Done` (hard bound 5 polls); Conflict/DLQ/Cancelled = failure |
| `scripts/internal/phase4-double-write-track-pointer.py` | 4 | Dry-run by default. Reads live state (never assumes). Derives `[[track:]]` target from the card's own `[[version:]]`; every target must resolve to an existing structure card or it is a **blocking error, never a skip**. `downgraded` cards excluded natively by the server |
| `scripts/internal/phase5-census-release-derivation.py` | 5 | **Read-only by construction** — emits no write request. Status-of-card-first derivation, exact mirror of the Rust `derive_release`. Non-derivable count printed explicitly and **must be 0** |
| `scripts/internal/phase7-strip-version-release.py` | 7 | Dry-run by default. RMW compare-and-swap (`note_id` + `expected_sha256`; stale sha ⇒ Conflict, card intact). Author guard (`--expected-author` pre-flight, fail-closed) + **canary** (first card re-read, real author compared, halt *before* card #2 on divergence). Orphan guard: never strips a card without a `[[track:]]`. Idempotent (no token to remove ⇒ no write) |

**Always run each script with no `--apply` first** and review its plan and proofs. `--apply` requires
explicit credentials and refuses to run otherwise.

### Why there is no consolidated orchestrator

A single "run the whole migration" script was considered and **deliberately not built**. It would
chain four one-shot scripts that are hardcoded to this project and its frozen inventory files — an
orchestrator over them would be neither generic nor reusable by an external operator, and building it
for a hypothetical future caller is speculative (YAGNI). The four dry-run-first scripts plus this
guide's sequence are the deliverable. An external operator adapts the *shape* (create structures →
double-write → census → flip readers+deploy → strip+deploy), not our binaries.

---

## Rolling back

- **Library consumers:** 2.2 is a minor; if a break surfaces, pin to your last `2.1.x`. Your data is
  untouched.
- **Registry operators, before Phase 7:** the double-write state (Phase 4) is fully reversible — the
  stored `[[version:]]`/`[[release:]]` are still present, so a rollback of the reader deploy (Phase
  6) leaves the registry readable by the old code path.
- **Registry operators, after Phase 7:** the strip is **irreversible** — the stored axis is gone and
  release is derived. There is no rollback of the data; roll back only if Phase 6 readers are live.
  This is why Phase 7 is gated on a clean census and an explicit operator GO.

---

## Library consumers (Rust crates)

At the surface level, 2.2 is additive:

- `KindKind` gained `Roadmap` and `Backlog` variants. Because `KindKind` is `#[non_exhaustive]`
  (since 2.1), your `match` already needs a `_` arm — the new variants fall into it and **do not
  break compilation**. If you want to handle structure cards explicitly, add arms for the two new
  variants.
- `KindKind::from_wire` now accepts `"ROADMAP"` and `"BACKLOG"`; the retired `"CHORE"`/`"SPIKE"`
  values still return `None` (removed in 2.1).
- No public symbol was **removed** in 2.2 (measured: empty public-api baseline diff, subject to the
  stale-baseline caveat above).

If you *derive* release yourself, prefer the sanctioned `gradatum_core::project_map::derive_release`
over reading a stored `[[release:]]` — the stored axis is being retired.

---

## Why this guide is standalone, not cumulative

F-254 left this open: a standalone guide per version line, or one cumulative guide with an entry per
line (so a consumer jumping 2.0→2.2 reads one document). **Decision: standalone.** Rationale:

- The two existing guides (`UPGRADING-1.0.0-to-2.0.0.md`, `UPGRADING-2.0.0-to-2.1.0.md`) are already
  one-file-per-line; a standalone 2.1→2.2 guide is consistent and discoverable by the established
  naming convention.
- Restructuring two already-shipped, already-linked guides into one cumulative document is a larger,
  riskier change than this milestone warrants, and it touches published surfaces.
- The 2.0→2.2 skip case is served by cross-links: this guide names its predecessor, and a consumer
  reads two short documents rather than one long one.

The cumulative option retains genuine merit and can be revisited when the guide count grows; it is
recorded here as considered and deferred, not overlooked.

---

## Tracking

Every break above carries its gradatum card: **F-184** (model — the version becomes a card, release
derived), **F-213** (role-coherence guards at the validator), **F-256** (visibility axis on work
cards), **F-211** (single-request listing). The inventory distinguishes **tool-detected** breaks
(public-api surface diff — empty for 2.2, one stale-baseline caveat) from **hand-inventoried** breaks
(the data-contract and model changes, invisible to the surface tool because they act on accepted data
and behaviour). The reachability of this guide from the repository homepage and the changelog is
checked on the **published** repository, not on a local tree.
