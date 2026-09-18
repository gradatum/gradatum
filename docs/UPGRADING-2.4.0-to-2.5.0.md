# Upgrading from 2.4.0 to 2.5.0

This guide covers the 2.4.0 → 2.5.0 step only; it is not cumulative with the guides before it —
one file per version line, same as `UPGRADING-2.3.1-to-2.4.0.md`.

**Read this before upgrading.** 2.5.0 is a *minor* release. If your `Cargo.toml` allows it
(`gradatum-core = "2.4"`, …), Cargo **adopts 2.5.0 silently** — no action on your part. Five
things change under you, and a `Cargo.toml` pin covers none of them:

- a configuration key is **removed** from the schema of `server.toml`. It never had an effect, and
  a file that still carries it **keeps loading**, so there is no rename-or-fail moment and nothing
  to do;
- a preset that is **present but unusable** now **refuses to start the server** instead of being
  discarded in silence. This is the one item here that can take a service down, and it carries a
  mandatory pre-flight;
- **purged job payloads are physically scrubbed from this line onward** — and the payloads 2.4.0
  already purged are **not** retroactively scrubbed. A one-time queue rewrite is recommended at
  the upgrade;
- `[privacy] mode = "mask"` becomes reachable, and it is **destructive**: it removes the version
  history of every note it rewrites, and the snapshots taken before the switch are beyond what any
  write reaches. Switching to it comes with a one-shot purge you run yourself.
- the `project-map export-features` report **gains a `visibility` key** on every entry. The report
  stays **additive** — the keys it carried before are unchanged — so nothing is required of you,
  and a byte comparison across this line differs by that key alone.

That first point is why it is worth two minutes of your attention rather than none. What follows
tells you which key went away and which key actually carries the setting, then the changes that
do require an operator decision.

It is a **standalone** guide, one file per version line, matching the guides before it — see [Why
this guide is standalone](#why-this-guide-is-standalone-not-cumulative).

---

## Before you upgrade — two ACL preset checks, before any binary swap

Both checks below are about the ACL preset (`bearer.toml`). Neither is new behaviour introduced by
this line on its own — each is a pre-existing state of a preset meeting the [§ACL preset
gate](#a-configured-but-unusable-acl-preset-now-refuses-to-start-the-server), which *is* new. Run
both checks against the file the service actually reads, before the 2.5.0 binary is installed.

### A leftover `sees_personal_classified` key now stops the server, not just a lint

`sees_personal_classified` was removed from `[[consumer]]` in **2.4.0** — see
[UPGRADING-2.3.1-to-2.4.0.md § The ACL preset field
removal](UPGRADING-2.3.1-to-2.4.0.md#the-acl-preset-field-removal). A preset that still carries it
has never *parsed* since that line. Under 2.4.0, a preset that does not parse does not stop the
server: the loader discards it **in silence** and falls back to the empty **DENY-ALL** engine, which
reports itself healthy on `GET /health`. A deployment that skipped that migration has been running
DENY-ALL, unnoticed, for as long as it has run 2.4.0.

2.5.0 changes the consequence, not the check: the same unparseable preset now hits the [§ACL preset
gate](#a-configured-but-unusable-acl-preset-now-refuses-to-start-the-server) and **refuses to start
the server** instead of falling back. A silent failure becomes a loud one at the next restart —
which is exactly what an upgrade forces.

**Check before you swap the binary:**

```
gradatum-admin acl validate --preset <path-to-bearer.toml>
```

A non-zero exit naming the field means the fix is mandatory before this line, not optional:

```
Error: the ACL preset <path> does not parse: removed preset field `sees_personal_classified`:
express the denial with a `!` negation pattern instead
```

**Fix:** remove every `sees_personal_classified = true|false` line from each `[[consumer]]` block,
and where the value was `false`, add the segment-scoped negations at the head of `read_patterns`
and `write_patterns` instead:

```toml
read_patterns  = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "…"]
write_patterns = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "…"]
```

The full re-alignment procedure — including which consumers had `true` and need no negation — is in
[UPGRADING-2.3.1-to-2.4.0.md § Re-alignment
procedure](UPGRADING-2.3.1-to-2.4.0.md#re-alignment-procedure-do-this-before-switching-to-240).

### `init --preset=hierarchical --projects=…` is refused: the preset has nothing to scope

The shipped `hierarchical` preset used to carry `[[consumer-template]]` blocks holding a `${PROJECT}`
placeholder — blocks the engine has **never read** (`AclPreset` parses `[[consumer]]` only, so those
blocks were always inert in a deployed file). As of this line the shipped preset drops the template
blocks entirely and declares concrete consumers only (13 in the shipped preset: `admin`, `studio`,
`main-agent`, the domain experts, the engine and the wrappers).
`--projects` still substitutes `${PROJECTS}` / `${PROJECT}` wherever a *value* carries one, but the
shipped preset's values carry none any more — so `init` now **refuses** `--projects` on this preset
(non-zero exit, nothing written, no backup taken) instead of silently producing a file with no
per-project identity. Omit the flag: the materialised file is the same as before.

**If your deployment relied on `--projects` to produce per-project consumers**, that scoping must
now be written by hand: one concrete `[[consumer]]` block per project and per role. The preset's own
header comment carries a worked, copy-pasteable example for two projects — copy it, rename the
projects, and mint a key per identity with `gradatum-admin api-key create`. Validate the result the
same way as above (`gradatum-admin acl validate --preset <path>`) before restarting the server.

---

## Who this concerns

| You are… | What changes | Where to look |
|---|---|---|
| **An operator** maintaining a `server.toml` | The key `[curator.llm] max_tokens` is **removed** from the schema. Nothing stops working; the setting has always been `[curator] llm_review_max_tokens` | [§The `[curator.llm] max_tokens` key is removed](#the-curatorllm-max_tokens-key-is-removed) |
| **An ACL preset operator** — you configure a preset, or you rely on the default path | A preset that is **present but unusable** now **refuses the boot**. An absent file and an empty key keep the DENY-ALL fallback. When the key is absent, its default path is now derived from the deployment's own root | [§The ACL preset gate](#a-configured-but-unusable-acl-preset-now-refuses-to-start-the-server) |
| **An operator** running `gradatum-server` + `gradatum-worker` from one `server.toml` | The queue now purges terminal job payloads on a schedule. It did not before, so bodies written while you ran 2.4.0 are still in `db/queue.sqlite` and will be erased within an hour of the worker coming up. | [§The periodic purge is now really scheduled](#the-periodic-purge-is-now-really-scheduled) |
| **An operator** running the job queue | A purge now zeroes the payload on disk. Payloads purged **before** this line stay in the file's free pages until they are recycled — nothing does that for you | [§The job queue](#the-job-queue-purged-payloads-are-now-physically-scrubbed) |
| **A Rust library consumer** — you depend on `gradatum-worker` | The `pub` field `WorkerLlmConfig::max_tokens` is **removed**. Code that constructs or reads it no longer compiles | [§Library consumers](#library-consumers-rust-crates) |
| **A Rust consumer** matching a `gradatum-server` config error | `ConfigError::PrivacyMaskUnavailable` is **renamed** to `PrivacyMaskWithoutActivation`. Code matching the old variant no longer compiles | [§Library consumers](#library-consumers-rust-crates) |
| **An operator switching `[privacy] mode` to `mask`** | Nothing breaks, but the mode has become destructive: its history purge is a step you run, once, after the switch | [§Switching to `mask`](#switching-to-mask-destroys-version-history--plan-the-one-shot-purge) |
| **An operator** running the out-of-process detector | Nothing is required: `[privacy.sidecar]` is new, and absent from a 2.4.0 file. If you add it, a sidecar that does not answer the handshake **refuses the start** rather than falling back | [§The out-of-process detector is mounted from `[privacy.sidecar]`](#the-out-of-process-detector-is-mounted-from-privacysidecar) |
| **An operator reading an exported report** — `export-features` | Each entry gains a `visibility` key. The report is **additive**: every key it carried before is unchanged, and a byte comparison across this line is expected to differ by that key alone | [§The `export-features` report gains a `visibility` axis](#the-export-features-report-gains-a-visibility-axis) |
| **An operator** re-running `init` (or the installer) on an existing root | `init --force` no longer rewrites `config/bearer.toml`: the ACL preset on disk is kept byte-identical. Pass `--overwrite-preset` to replace it — the previous file is saved as `.bak.<timestamp>` first | [§Re-running `init` on an existing installation](DEPLOYMENT.md#re-running-init-on-an-existing-installation) |
| **An API client** — you call the REST API or the MCP tools | Nothing. No request body, no response field, no endpoint, no status code changes in this line | — |

---

## The break inventory

| Break | Class | Before 2.5.0 | After 2.5.0 |
|---|---|---|---|
| `WorkerLlmConfig::max_tokens` removed (`[curator.llm] max_tokens`) | **Library surface** + **config contract** | Key accepted and **parsed**; the value reached no consumer — the review budget is read from `[curator] llm_review_max_tokens` | Field gone from the struct; a `server.toml` still carrying the key **still loads**, the key being ignored as unknown |
| `ConfigError::PrivacyMaskUnavailable` renamed `PrivacyMaskWithoutActivation` | **Library surface** (invisible to the surface gate) | The variant said the mode was *unavailable*: 2.4.0 refused `mode = "mask"` outright, masking not being delivered | The mode is delivered and the refusal is narrower — `mask` over an activation that can rewrite nothing. Same configurations load, same one is refused |
| A configured ACL preset that is present but unusable | **Config contract** + **boot behaviour** | One `warn`, then the boot proceeded with the empty engine — every request refused (**403**) while `/health` answered **200** | **Boot refused**, with an error naming the gate `acl.preset_path` and the file. Absent file and empty key are unchanged (DENY-ALL + warning) |
| The `acl.preset_path` default when the key is absent | **Config contract** | An absolute path outside `storage.root`, shared by every deployment on the host | Derived from the deployment's own root: `<storage.root>/config/bearer.toml` |
| Purged job payloads | **Data at rest** | A purge was logical only: the rows went away, **the bytes stayed** in the file's free pages | `PRAGMA secure_delete=ON` on the queue, and purged payloads zeroed (overflow pages included) after checkpoint — **from this line onward only** |
| The positive-control boot log line | **Operations** | `AclEngine loaded from preset` | `ACL preset gate: engine loaded from preset`, carrying `gate` and `path` fields |

> **How the break was measured, and what the measurement cannot see.** The sanctioned `public-api`
> surface gate reads the rustdoc JSON, which does not emit `#[doc(hidden)]` items. `gradatum-worker`
> is consequently measured at **one item** (`pub mod gradatum_worker`), so the field removal is
> **invisible to that gate**. The break is therefore matched by a **named entry** in
> `RELEASE-MANIFEST.yaml` under `semver_deviations`, at the same granularity as the `pub` field
> removals of the previous line. A break a tool cannot see is still a break; it is not left to the
> tool's silence.
>
> The ACL boot gate, the derived default path and the queue scrubbing are **behavioural and
> data-at-rest** changes: the surface tool has no opinion on any of them, which is why they are
> stated here in full rather than left to a green gate.

---

## The `export-features` report gains a `visibility` axis

The `project-map export-features` report renders one entry per feature card. In this line each entry
gains a **`visibility`** key, carrying the card's visibility axis — and `null` when the card declares
none.

The report is **additive**: every key it carried before this line is still there, spelled the same,
with the same value, and the relative order of those former keys is preserved — the new key is
inserted alongside them. Nothing was renamed and nothing was dropped, so a consumer that reads the
entries by key keeps working, and one that machine-checks the report's former key set sees exactly
one addition.

**Nothing is required of you.** Two consequences are worth knowing rather than acting on:

- if you compare two reports taken on either side of this upgrade **byte for byte**, the comparison
  is expected to differ — by this key, and by nothing else. A diff that shows more than that is a
  finding worth reporting;
- `null` is a **value**, not an absence: a card that declares no visibility is exported with the key
  present and empty, so a reader keying on the field's presence cannot mistake it for a card that
  predates the axis.

---

## The `[curator.llm] max_tokens` key is removed

`WorkerLlmConfig` used to carry a `max_tokens: Option<u32>` field, read from `[curator.llm]`. The
value was **never used**. The token budget of the review call has always been read from the
top-level `[curator] llm_review_max_tokens`, which is the value propagated into the curator
pipeline and the value the shipped configuration template sets. Two keys for one arbitrated setting
is a divergence the product is removing rather than institutionalising, so the field is **deleted
rather than wired** — wiring it would have made a second source live for a setting that already has
one.

### If you operate a `server.toml`

**Nothing is required of you.** The key is gone from the schema, but its removal cannot break a
running configuration: the worker's LLM section declares **no `deny_unknown_fields`**, so a retired
key is ignored exactly as any unknown key always was.

```toml
# before — loads on both versions, and the value does nothing on either
[curator.llm]
backend    = "openai_compat"
base_url   = "http://embedding.example.invalid:8080"
model      = "example-embedding-model"
timeout_ms = 5000
max_tokens = 2048             # ← removed from the schema; parsed and read by nothing

[curator]
llm_review_max_tokens = 128   # ← the key that carries the effect, and always did
```

```toml
# after — the same configuration with the dead key dropped
[curator.llm]
backend    = "openai_compat"
base_url   = "http://embedding.example.invalid:8080"
model      = "example-embedding-model"
timeout_ms = 5000

[curator]
llm_review_max_tokens = 128
```

**The only action worth taking** is deleting the `max_tokens` line from `[curator.llm]` — not
because the server needs it gone, but so the next person to read the file does not spend an
afternoon on a key that looks live. If you were relying on that line to set the review budget, move
the value to `[curator] llm_review_max_tokens`; if you were not, delete it.

### If you are a Rust library consumer

`gradatum_worker::curator_loader::WorkerLlmConfig::max_tokens` is gone. Delete it from any struct
literal, and read the effective budget from `WorkerCuratorConfig::llm_review_max_tokens` instead —
that field is unchanged and is the one the pipeline consumes.

### Why this break needs no pre-flight

The previous guide required a mandatory pre-flight because a preset that failed to parse left the
server serving **DENY-ALL**: switching out of order was an outage, and the sequence had to be
pinned. This break has no such moment.

It removes a key that never had an effect, and it introduces no parse failure a pre-existing
configuration can reach. A configuration that loaded before the upgrade loads after it, unchanged
and unedited. The one new load-time refusal in this line — `mask` over an activation that can
rewrite nothing — is reachable only by a configuration written for 2.5.0, since 2.4.0 refused every
`mask` outright; it is described in [§Switching to `mask`](#switching-to-mask-destroys-version-history--plan-the-one-shot-purge).
There is no window to sequence, so this break carries no migration script, no pre-flight, and no
rollback procedure specific to it — inventing one would suggest a risk that is not there.

The general deployment guidance still applies as it always does: build the new binaries, stop the
old ones, start the new ones, and read `/health` to confirm the state that is actually being
served. See [`docs/DEPLOYMENT.md`](DEPLOYMENT.md).

---

## The out-of-process detector is mounted from `[privacy.sidecar]`

A new **optional** block, and the only key this line adds to the schema. Nothing is required of a
deployment upgrading from 2.4.0: the block is absent from a 2.4.0 file, an absent block is not a
refusal, and a non-empty `categories` keeps mounting the in-process detector exactly as before.

An operator who does want the out-of-process engine adds it by hand, under the `[privacy]` block
it extends:

```toml
[privacy]
categories = ["email"]

[privacy.sidecar]
socket_path = "/run/gradatum-privacy/privacy-sidecar.sock"
binary_digest = "…"    # 64 lowercase hex characters
model_digest = "…"     # 64 lowercase hex characters
```

`max_input_bytes` and `window_overlap_bytes` are **not** repeated here: they stay in `[privacy]`
and are handed to the sidecar client by the server, so that one ceiling governs how a body is cut.
`score_mode`, `timeout_ms_sync_per_kib`, `breaker_n` and `breaker_cooldown_s` are optional and
default to the values the client defines.

Three refusals come with the block, and the third is the one that can take a service down:

- a digest that is not 64 lowercase hexadecimal characters, or an empty `socket_path`, is refused
  **when the file is read**, naming the field and never the value;
- the block over an **empty** `categories` list is admitted and mounts nothing — it names a
  detector no write would ask for;
- a sidecar that cannot be reached, that times out, or that announces a binary, a model or a score
  mode other than the pinned one makes the server **fail to start**. There is no fallback to the
  in-process detector by design, so this is worth a check that the sidecar is up **before** the
  server is restarted. The repair is to start the sidecar, or to remove the block.

The detection frontier itself does not change with the engine: the vault still applies the regex
floor under the model, the thresholds on the fused score, and the exceptions last. See
[`docs/PRIVACY-SIDECAR.md`](PRIVACY-SIDECAR.md#server-configuration) for the deployment, and
[`docs/guides/E-ports-and-config.md`](guides/E-ports-and-config.md) for the full key list.

---

## Switching to `mask` destroys version history — plan the one-shot purge

This line is where the `[privacy] mode = "mask"` the schema has been carrying becomes reachable.
The mode is destructive, and one step of the switch is an operator action rather than a build step.
Nothing here is required of a deployment that stays in `report_only`, which remains the default.

**What the mode does to history.** A write that masks anything removes **every** snapshot under
`<vault>/.history/<note-id>/` — not only the snapshot of the version it replaced. The versions
before it hold the clear text of exactly what the mode exists to remove, in the one directory the
rewrite does not touch, and a note whose history still describes what was removed is not masked.
The write records the destruction: one `WARN` line naming the note identifier and the number of
snapshots removed — the count, never their text.

**What no write reaches, and the command that does.** Snapshots taken while the deployment ran
`report_only` are older than the first masking write, so no write will ever remove them, and they
hold the clear text of notes you have since decided to rewrite. Run this **once, after the switch**:

```bash
gradatum-admin privacy history-purge --root /var/lib/gradatum --dry-run   # read the count first
gradatum-admin privacy history-purge --root /var/lib/gradatum             # then destroy
```

The command selects the notes whose privacy block records `mode = "mask"` — durable on-disk
evidence, readable without the index or the database — and removes their `.history/` snapshots,
printing `notes=… snapshots=… bytes=…`. It never touches the history of a note whose block does not
say `mask`, so a deployment that has not switched destroys nothing; `--dry-run` reports exactly what
a real run would remove and removes nothing. A note whose `.md` cannot be read is reported on a
second line (`notes_unreadable=…`) and left alone, rather than counted as clean.

**What the command is not.** It is not an erasure, and not a substitute for the delete path. It
removes markdown snapshots, and the switch leaves copies it never sees: a pre-switch copy of the
same body can also sit in `.archive/`, in the HTTP audit journal, and in `queue.sqlite` as the
payload of the write job that carried it. The `.md` file and `index.db` are not on that list —
`masked_body()` runs before the note is constructed, so both are written from the already masked
body — but the embedding index is refreshed asynchronously and catches up only when the
`Curate → Embed` cascade reaches the note. See the job-queue bullet in
[`SECURITY.md`](../SECURITY.md) for what each of those holds.

**The queue has its own purge; run it to close the window.** A completed write keeps its body in
`queue.sqlite` until the queue's purge reaches it — a scheduled tick every `[queue]
purge_interval_secs`, for rows older than `[queue] purge_after_secs` (24 h by default). The
masking write rewrites the note and never the job that carried it, so for a complete guarantee run
the queue purge after the switch and after the last masking write:

```bash
gradatum-admin queue purge --dry-run          # the count first
gradatum-admin queue purge                    # then the payloads
```

The keep window (`TERMINAL_PAYLOAD_KEEP`, 200 rows) is deliberately not a setting: the most recent
terminal jobs stay replayable, and a body older than that is not. The purge is physical where it
reaches but does not run `VACUUM` — the upgrade to this line schedules that on its own.

**The purge takes no lock, so run it with the server stopped if you can.** `history-purge` walks
`<vault>/.history/` and unlinks what it finds; it coordinates with nothing. A server writing during
the scan is not an error and corrupts nothing, but a snapshot taken *while* the walk is past its
note can be created after that directory was read and survive the run — the report counts what it
removed and cannot count what it never saw. Rerunning the command destroys it. Stopping the server
first removes the window rather than narrowing it.

**It unlinks regular files, and follows nothing.** A symlink planted under `.history/<id>/` is
neither followed nor removed: the walk classifies each entry on its own type, so the file a link
points at — wherever it lives — keeps its bytes, and the count does not include it. A run over a
history directory holding a link therefore destroys the snapshots and reports exactly those.

**The activation is what is refused at load, not the mode.** `mode = "mask"` loads only over an
activation with at least one category whose threshold is strictly above zero; a `mask` whose
activated categories all sit at the observation point would read as a running redaction while
rewriting nothing, and the load is refused (`PrivacyMaskWithoutActivation`) instead of leaving that
state for an operator to notice. In 2.4.0 any `mode = "mask"` was refused outright, so no
configuration that loaded on 2.4.0 can be refused here: this refusal can only be reached by a
configuration written for this line.

---

## A configured-but-unusable ACL preset now refuses to start the server

Before 2.5.0 the loader answered one question — *did the read fail?* — and treated every failure the
same way: log a warning, keep the empty engine, boot. The empty engine is **DENY-ALL**: every
authenticated request is refused on every locus, and a request with no token gets **401**. A server
in that state is indistinguishable from one enforcing a deliberately restrictive policy, and it
reports itself healthy — `GET /health` answers **HTTP 200** carrying `version` and `build_sha`,
because it is a liveness surface, not a preset check.

2.5.0 answers a second question — *why did the read fail?* — and separates the doors:

| Door | Condition | Behaviour in 2.5.0 |
|---|---|---|
| **Not configured** | `acl.preset_path` set to the empty string | DENY-ALL fallback, `warn` naming the `not-configured` door |
| **Fresh install** | The resolved path does not exist | DENY-ALL fallback, `warn` naming the `absent` door |
| **Unreadable** | The file exists and cannot be read — permissions, a directory where a file is expected, a broken mount | **Boot refused** — `error`, then `acl.preset_path: ACL preset file present but unreadable at <path> — <cause>` |
| **Does not compile** | The file reads, but is not a valid preset — a TOML error, an uncompilable pattern, a removed field | **Boot refused** — `error`, then `acl.preset_path: ACL preset file present but does not compile at <path> — <parse error>` |

The refusal is fail-closed by design: the operator expressed a policy, a file is present, and
discarding it converts a *configuration defect* into a *running server that refuses everything*. A
service that does not start is loud; a service that starts and denies everything reads as correct.
The trade is deliberate, and it has a cost to plan for: a supervisor that restarts on failure will
now **loop** on a bad preset instead of settling into a mute but "healthy" state.

> **A zero-byte preset is not an error.** An empty *file* compiles into a valid preset that declares
> no consumer, so the server **starts** and logs the engine as loaded. It then denies everything,
> because no consumer matches anything — but it is a *loaded* preset, not a fallback, and the boot
> is not refused. This is the opposite of the empty **key** in the table above: an empty value means
> *not configured*, an empty file is a preset that says nothing on purpose.

### The default preset path when the key is absent

When `[acl] preset_path` is **absent**, the effective path is now derived from the deployment's own
root:

```
<storage.root>/config/bearer.toml
```

That is the location `gradatum-admin init` materialises the preset at, and the one `gradatum-admin`
reads back. Before this line, the key defaulted to an **absolute host-global path** outside
`storage.root`: an instance whose root was elsewhere still read that shared file. Once the gate
above classifies a present-but-unreadable file as *configured*, that default becomes an active
hazard — on a host where the shared path exists but is not readable by the service user, startup
would be refused for a deployment that had configured no preset at all. Absence is not a path: it is
a deployment that has not answered the question, and the answer belongs to its own root, never to
the host it happens to run on.

**What to do:** if you relied on the old absolute default, set `preset_path` explicitly — an
explicit value is honoured **verbatim**, the empty string included. Otherwise confirm that the
preset your deployment actually uses is the one at `<storage.root>/config/bearer.toml`.

### Before you upgrade: validate the preset, and check the read

```
gradatum-admin acl validate --preset <path-to-preset>
```

The command is read-only — it opens no store, mints nothing, writes nothing — and it runs the file
through the server's own preset loader. Exit `0` is your go/no-go signal; a non-zero exit names the
offending key or line.

**What that check does not cover — read this before treating a clean run as proof.** The command
runs as **you**, against the file as **you** see it. The server reads it **at boot**, as the
**service user**. They are not the same reader, and the gap between them is exactly where a 2.5.0
boot now fails:

- a parent directory the service user cannot **traverse**, even with the file itself readable;
- a file you can read through a group or an ACL the service user is not in;
- a mount that is not yet present when the service starts — ordering, an automount, a remote
  filesystem;
- a path that is a **directory** rather than a file.

So do both: validate the preset, **and** confirm the service user can open it. Check the file mode
and the traverse bit of every parent directory for that user, and prefer a restart you can watch
over a reboot you cannot. If your deployment runs under a hardening directive that restricts
readable paths, add the preset's directory to it.

### After the upgrade: the positive-control log line moved

The line that proves a preset was loaded was renamed in this line, and a grep that finds nothing is
not evidence of a fallback:

```
# 2.4.0 — the name to grep today
journalctl -u gradatum-server | grep 'AclEngine loaded from preset'

# 2.5.0 — the same positive signal, renamed, carrying structured fields
journalctl -u gradatum-server | grep 'ACL preset gate: engine loaded from preset'
```

On 2.5.0 a server started against an unusable preset never reaches this line — it does not start at
all. So the line's **presence** confirms a preset was loaded, and its absence on a server that *did*
start means one of the two remaining DENY-ALL doors opened: the key was empty, or the file was
absent. Both are logged with a `warn` naming the door, so the journal tells you which — do not infer
health from the process being up. Then run a **positive functional check before any negative one**:
as an ordinary consumer allowed to read, read a benign section and confirm it returns **200**.

### Rollback is binary-only, and it does not make a bad preset safe

The refusal lives in the binary, not in a configuration schema. A preset 2.5.0 accepts is also
accepted by 2.4.0, and there is nothing to revert alongside the binary. But rolling back to 2.4.0
restores the 2.4.0 behaviour — an unusable preset is discarded **in silence** again — so a rollback
is not a way to make a bad preset safe, only a way to make it quiet. Fix the preset.

---

## The job queue: purged payloads are now physically scrubbed

2.5.0 sets `PRAGMA secure_delete=ON` on the queue database and **zeroes a purged payload** — its
overflow pages included — once the purge has been checkpointed. From this line onward, a payload
that has been purged is no longer recoverable from the file's bytes.

**This is not retroactive.** A payload purged by 2.4.0 was deleted **logically**: the rows went
away, the pages went to the free list, and the bytes stayed. Those bytes remain in the free pages of
the queue database until the pages are recycled by later writes — which, on a small or quiet queue,
may be never. If a job payload can carry anything you would not want readable on disk (a note body,
a path, a credential in a job spec), that residue is the reason for this step.

### Recommended one-time procedure, at the upgrade

1. **Measure.** `ls -l <storage.root>/db/queue.sqlite*` — the database plus any `-wal`/`-shm`
   sidecar. This is the size the rewrite has to rebuild, and the size to compare afterwards.

2. **Stop the writers.** Stop **both** the server and the worker. The rewrite needs an **exclusive
   lock** on the database for its whole duration; a writer holding the file makes it wait or fail,
   never wrong.

3. **Back up.** Copy the database aside while the writers are stopped. The rewrite is a safe
   operation, but it is not a free one: it rebuilds the whole file.

4. **Rewrite.**

   ```
   sqlite3 <storage.root>/db/queue.sqlite 'VACUUM;'
   ```

   | Queue size | Expected duration | Before you start |
   |---|---|---|
   | **under 100 MB** | seconds | — |
   | **100 MB – 1 GB** | tens of seconds | at least **1.1 ×** the current size free on the same filesystem: `VACUUM` writes a new file beside the old one and swaps |
   | **over 1 GB** | — | **purge first.** Let the queue's own lifecycle jobs retire terminal records, then measure again |

5. **Restart** the server, then the worker. The ordering rule is unchanged — see
   [§12 of the deployment guide](DEPLOYMENT.md#12-app-host-upgrade-order-gradatum-server-before-gradatum-worker).

6. **Control — read the bytes, do not assume them gone.** Search the raw file for a fragment you
   know was in a purged payload:

   ```
   grep -a -c '<fragment>' <storage.root>/db/queue.sqlite*
   # expected: 0 for every file listed
   ```

   The two mechanisms are not the same one. `VACUUM` is what removes the **residue** — it rebuilds
   the file without the free pages that still hold it. `secure_delete` is what stops the **next**
   purge from leaving any. Run the check after the rewrite, and once more after the services are
   back up and have processed jobs, so both halves are exercised.

> **Do not put the `VACUUM` in the startup path.** It takes an exclusive lock on a database both the
> server and the worker use, and its duration grows with the file. At every boot it is a slow boot,
> and on a large queue it is a start that looks hung. Run it once, by hand, at the upgrade — then
> let `secure_delete` keep the file clean from there. A `VACUUM` is a maintenance operation, not a
> configuration.

---

## The periodic purge is now really scheduled

The purge of terminal job payloads — the note body a `Curate` job carried — was implemented,
batched and measured, and **no scheduled task and no admin command called it**: 2.5.0 adds both,
so an upgrade starts erasing within `queue.purge_interval_secs` (default 1 h) the payloads of
terminal jobs older than `queue.purge_after_secs` (default 24 h), leaving the N most recent
terminal rows untouched; the backlog accumulated under 2.4.0 is erased at the worker's first tick,
and the two new keys are written into the `server.toml` emitted by `gradatum-admin init` (they are
also the values in force when the section is absent). Nothing to do on your side beyond knowing
it, unless you want a different window — in which case set the keys, and note that a retention of
`0` is refused at load rather than clamped, and that `gradatum-admin queue purge --dry-run` prints
what an execution would erase without erasing it.

---

## Mandatory pre-flight — make before break

**This is a pre-flight, not advice.** Both items are done *before* a 2.5.0 binary runs.

1. **Validate the ACL preset, and check that the service user can read it** — the full procedure is
   under [§The ACL preset gate](#a-configured-but-unusable-acl-preset-now-refuses-to-start-the-server).
   This is the step that can stop the server from starting: `gradatum-admin acl validate --preset
   <path>` must exit `0`, **and** the service user must be able to traverse every parent directory
   and open the file. A clean validation under your own account is not that check.

2. **Rewrite the job queue if 2.4.0 has purged payloads you would not want readable on disk** — the
   procedure is under [§The job queue](#the-job-queue-purged-payloads-are-now-physically-scrubbed).
   The server does not require it to start; nothing else will do it for you. Stop the server and the
   worker before the rewrite.

Neither step is reversible in the sense the previous guide's preset re-alignment was: a `VACUUM`
rewrites a file (hence the backup), and the ACL refusal is a property of the binary. Both are
one-time, and both are done with the services stopped.

---

## The deployment sequence, in order

**This line adds no step to the sequence.** Backup, preset control, installing the binaries off the
live path, worker-then-server on stop and server-then-worker on start, reading `/health` through the
boot floor, and the same-`version`-and-same-`build_sha` fleet rule — all of it is set out in full in
the previous guide and applies here unchanged: [§The deployment sequence, in
order](UPGRADING-2.3.1-to-2.4.0.md#the-deployment-sequence-in-order).

What changes is the **content** of the preset-control step. On the previous line a preset the
loader could not read was discarded and the server came up mute; the control step existed to catch
that before production did. On this line the server **refuses to start**, so the same control step
now prevents a failed boot rather than a silent one — and the boot log line it confirms was renamed,
as described [above](#after-the-upgrade-the-positive-control-log-line-moved).

---

## Library consumers (Rust crates)

Two source breaks reach a library consumer in this line — one removed `pub` field, one renamed enum
variant:

- `gradatum_worker::curator_loader::WorkerLlmConfig::max_tokens` — delete it from any struct
  literal; the effective value is `WorkerCuratorConfig::llm_review_max_tokens`.
- `gradatum_server::config::ConfigError::PrivacyMaskUnavailable` — now
  `PrivacyMaskWithoutActivation`; update any `match` naming the old variant. Nothing else about the
  variant changes: the same configurations load and the same one is refused.

Both breaks are matched by named entries in `RELEASE-MANIFEST.yaml` under `semver_deviations`, at
the same granularity: crate, symbol, lint, rendered form and kind, one entry each. Note that a
consumer of the *published* crate is affected even though the surface gate does not report it: the
field is `pub`, it sits in a `pub` module of a publishable crate, and `#[doc(hidden)]` items remain
callable. The rename is affected for the very same reason, and it is matched by the same kind of
entry — an entry is recorded, not inferred. What the gate cannot do is *emit* either one: it reads
the rustdoc JSON, which carries no `#[doc(hidden)]` item, so a rename inside `gradatum_server::config`
produces no measured deviation for anything to pair with. That is why the two are declared in the
manifest by name rather than left to the gate's silence: the entry states the break the measurement
cannot see, and a gate that stays green over it is not evidence that it is absent. Everything else
at the library surface is unchanged in this line.

---

## Why this guide is standalone, not cumulative

Each version line gets one migration guide, describing the breaks introduced **by that line only**.
If you are several versions behind, read the guides in order — this one does not repeat the breaks of
the guides before it. Applying them out of order, or skipping one, can leave you with a preset a
later version refuses to start with.

The guides for the earlier lines, in order:

| Guide | Line |
|---|---|
| [`docs/UPGRADING-1.0.0-to-2.0.0.md`](UPGRADING-1.0.0-to-2.0.0.md) | how the server determines *who is calling* |
| [`docs/UPGRADING-2.0.0-to-2.1.0.md`](UPGRADING-2.0.0-to-2.1.0.md) | 2.0 → 2.1 |
| [`docs/UPGRADING-2.1.0-to-2.2.0.md`](UPGRADING-2.1.0-to-2.2.0.md) | the `project-map` registry model and the write contract |
| [`docs/UPGRADING-2.3.1-to-2.4.0.md`](UPGRADING-2.3.1-to-2.4.0.md) | the `vault_list` body field, unknown sections, and the ACL preset key |
| **this guide** | 2.4.0 → 2.5.0 |
