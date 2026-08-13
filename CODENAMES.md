# CODENAMES.md

> Published list of codenames for `2.0+` majors, per [`RELEASE-POLICY.md`](RELEASE-POLICY.md#codename-triggers-20).
> Codenames are mnemonic, not marketing: each one is picked to say something true about the
> release it names, not to sound impressive.

---

## Convention

Codenames are drawn from **geological strata** — the layers a sedimentary rock is built from —
assigned in **alphabetical order as majors increase**: `2.0` gets a name starting with `A`,
`3.0` a name starting with `B`, and so on. The point of tying the letter to the major number is
that the order is self-evident: nobody needs to open this file to know that a codename starting
with `C` names a later major than one starting with `B`.

The theme fits the project on its own terms: a sedimentary stratum is matter deposited layer by
layer over time, each layer a record of the conditions that laid it down. A Gradatum vault
accumulates the same way — note by note, revision by revision — and the name of the project
itself, *gradatum*, means "by degrees." The codename theme is not decoration; it names the
project's own model of memory.

---

## Assigned

### `2.0` — **Alluvium**

Alluvium is sediment deposited by moving water — a riverbed, a floodplain — built up in
successive layers as the water that carries it recedes. It is chosen for the major that made
identity strictly credential-derived and closed the `1.x` line: a release that, like the
sediment it's named for, is one deposit in an ongoing accumulation, not a break from what came
before it.

---

## Candidates for future majors

Unassigned. Listed to make the convention usable without re-litigating it at each major; the
actual name for a given major is still picked at the time that major's codename trigger fires,
and may differ from what's listed here.

| Major | Letter | Candidates | Why it fits |
|---|---|---|---|
| `3.0` | B | `Bedding`, `Breccia`, `Basin` | `Bedding` is the term for the layered structure of sedimentary rock itself — each bed a distinct depositional event. `Breccia` is fragments of older rock cemented into one. `Basin` is the depression where sediment collects over time. |
| `4.0` | C | `Colluvium`, `Cratonic`, `Clastic` | `Colluvium` is sediment moved and deposited by gravity rather than water — a different transport mechanism from `Alluvium`. `Cratonic` describes old, stable continental basement rock. `Clastic` describes rock made of cemented fragments of older rock. |
| `5.0` | D | `Delta`, `Diagenesis`, `Drift` | `Delta` is the classic depositional landform where a river meets standing water. `Diagenesis` is the process by which loose sediment consolidates into rock over time. `Drift` is material transported and deposited by glacial ice. |

---

## Trigger reminder

A codename is assigned only when one of the objective release-signals in
[`RELEASE-POLICY.md`](RELEASE-POLICY.md#codename-triggers-20) fires — an incompatible index
schema migration, a breaking change to a `gradatum-core` *stable* trait, removal of a public
crate from the umbrella SDK, or a change to the default LLM contract. A major version bump on
its own does not require a codename if none of those signals fired for it.
