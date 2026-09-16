# Upgrading from 2.3.1 to 2.4.0

**Read this before upgrading.** 2.4.0 is a *minor* release. If your `Cargo.toml` allows it
(`gradatum-core = "2.3"`, …), Cargo **adopts 2.4.0 silently** — no action on your part. But two
things change under you that no `Cargo.toml` pin protects you from once you point at a 2.4 server:
a request body field is **removed** (an unknown field is now rejected instead of ignored), and if
you **operate an ACL preset**, one configuration key is **removed** and a preset that still carries
it no longer loads. This guide is written for both moments.

It is a **standalone** guide, one file per version line, matching the guides before it — see [Why
this guide is standalone](#why-this-guide-is-standalone-not-cumulative).

---

## Who this concerns

There are **three distinct audiences**, and the breaks that hit them are different:

| You are… | What breaks | Where to look |
|---|---|---|
| **A Rust library consumer** — you depend on `gradatum-dto` / other crates | The `pub` field `VaultListRequest::pattern` is **removed**. Code that constructs or reads it no longer compiles | [§The `vault_list` pattern field is removed](#the-vault_list-pattern-field-is-removed) |
| **An API client** — you call `POST /api/v1/vault_list` or the four read tools | A body carrying `pattern` is now **rejected** (was silently ignored); a non-canonical `section` is now a **400** (was accepted or denied) | [§The `vault_list` pattern field](#the-vault_list-pattern-field-is-removed), [§Unknown sections are now request errors](#unknown-sections-are-now-request-errors) |
| **An ACL preset operator** — you maintain a `bearer.toml` / preset with `[[consumer]]` blocks | The `sees_personal_classified` key is **removed**. A preset that still carries it **fails to load**, and the server then serves **DENY-ALL** | [§The ACL preset field removal](#the-acl-preset-field-removal) |

**The critical property of a minor release:** because 2.4.0 is adopted automatically, this guide is
the *only* warning you get. Do not skip it because "a minor cannot break me" — a minor changes what
the server *accepts* and how it *loads its ACL preset*, neither of which a `Cargo.toml` pin covers.

---

## The break inventory

| Break | Class | Before 2.4.0 | After 2.4.0 |
|---|---|---|---|
| `VaultListRequest::pattern` removed (`vault_list` body field) | **Library surface** + **HTTP wire** | Field accepted; the server read it and **discarded it silently** (it never filtered anything) | Field gone from the struct; a body carrying `pattern` is **rejected at deserialization** — HTTP **422 Unprocessable Entity** |
| A non-canonical `section` on the four reads (`vault_list`, `vault_search`, `vault_context`, `vault_links`) | **HTTP wire (behaviour)** | An invented section slipped past as a **200** or was denied **403** | Rejected as a **request error — HTTP 400** (empty body on REST; the message is carried over MCP) |
| `ConsumerEntry::sees_personal_classified` removed (ACL preset key) | **Library surface** + **config-contract** | Boolean key on each `[[consumer]]`; `false` denied the `personal-classified` section by a compiled short-circuit | Key **removed**; a preset carrying it **fails to parse**; the denial is now expressed as `!` negation patterns |

> **How the tool measurement was done.** The sanctioned measurement is the `public-api` surface
> reference: removing `VaultListRequest::pattern` and `ConsumerEntry::sees_personal_classified` each
> shows up as a `struct_pub_field_missing` break, appraised under the minor rank and matched to a
> named `semver_deviations` entry in `RELEASE-MANIFEST.yaml`. The HTTP-wire and config-contract
> consequences (422, 400, DENY-ALL-on-bad-preset) are **behavioural** — invisible to the surface
> tool, which is why they are stated here in full.

Everything below expands the rows.

---

## The `vault_list` pattern field is removed

`VaultListRequest` used to carry a `pattern: Option<String>` field. It was **never implemented**:
the server read it and threw it away (`let _ = req.pattern;`). Setting it filtered nothing — the
response was exactly the one you got without it. It is now removed.

**If you are a Rust library consumer:** the field is gone from the struct. Any code that sets or
reads `VaultListRequest::pattern` no longer compiles. Delete the field from your struct literal;
there is no replacement, because there was never any behaviour behind it. To narrow a listing,
filter on `section`, or filter the response client-side.

**If you are an API client:** `VaultListRequest` carries `#[serde(deny_unknown_fields)]`. A body
that still sends `pattern` is now **rejected at deserialization**, before the handler runs:

```
POST /api/v1/vault_list
{"pattern": "decisions/*"}
→ 422 Unprocessable Entity
   body: "Failed to deserialize the JSON body into the target type:
          unknown field `pattern`, expected one of ..."
```

> **The status code is 422, not 400.** An unknown field on syntactically valid JSON is a serde
> *data* error, which the HTTP layer maps to **422 Unprocessable Entity**. 400 Bad Request is
> reserved for JSON *syntax* errors. The rejection body names the offending field.

**What to do:** remove `pattern` from the request body. If you were relying on it to narrow a
listing (you were not — it did nothing), filter on `section` instead, or filter the returned entries
client-side.

**One residual sharp edge, named.** An MCP client that connected **before** you upgraded holds a
frozen copy of the tool catalogue that still advertises `pattern` in the `vault_list` input schema.
It will keep offering the field until it reconnects and re-reads the catalogue. Reconnect your MCP
clients after the upgrade so their advertised schema matches the server.

---

## Unknown sections are now request errors

The four read surfaces (`vault_list`, `vault_search`, `vault_context`, `vault_links`) validate the
`section` argument against the canonical set **before** the ACL gate. A section name outside that set
is now a **request error — HTTP 400** (with an empty body on the REST surface; the explanatory
message is carried on the MCP surface).

**Before,** an invented section name slipped through: depending on the caller's ACL it surfaced as a
**200** (empty listing) or as a **403** (access denied). Neither told the caller the real problem —
that the *section itself* does not exist.

**What to do:** if any automation sends a section name it invented, correct it to a canonical
section. A previously "empty but successful" (200) or "forbidden" (403) response for such a name is
now a clear **400**.

---

## The ACL preset field removal

`[[consumer]]` blocks in an ACL preset used to carry a boolean `sees_personal_classified`. When
`false` (the default), it denied the `personal-classified` section through a compiled short-circuit.
That key is **removed in 2.4.0**. The denial is now expressed the same way as every other rule in
the preset: as `!` **negation patterns** at the head of the consumer's `read_patterns` **and**
`write_patterns`.

### The consequence if you do nothing — stated without euphemism

A preset that still carries `sees_personal_classified` **does not parse**. And a preset that does not
parse **does not stop the server from starting**: the loader (`with_acl_preset_path`) logs a single
`warn` and **falls back to the empty engine**, which is **DENY-ALL**. Every authenticated request is
then refused on every locus — indistinguishable from an outage, except that the server reports
itself healthy.

There is therefore **no automatic safety net at boot**. The only real safeguard is the **order of
operations**: re-align the preset *first*, confirm it loads, *then* deploy 2.4.0.

### What was measured — the failure is silent, and it is not a hypothesis

The failure mode above was not read off the code. It was **measured** by starting 2.4.0 against a
real state whose preset still carried the removed key:

| Observation | Result |
|---|---|
| Off-server control on the inherited preset | `gradatum-admin acl validate --preset …` exits **1**, naming the removed field |
| Server startup | **succeeds** — the process does not refuse to start; readiness ≈ 1 s |
| `GET /health` | answers **HTTP 200**, carrying `version` and `build_sha` — it is a liveness surface, not a preset check |
| An authenticated read | **403**, empty body; a request with no token gets **401** |
| Boot journal | `WARN ACL preset unreadable (parse error) — DENY-ALL fallback` followed by the parse error naming the removed field, then one `ERROR` per active key whose owner no consumer declares |

**No start ever fails.** That is precisely what makes this dangerous: the upgrade looks successful
and the service is mute.

**The re-alignment is reversible, and that too was measured.** A re-aligned preset is loaded and
served correctly by the **older** 2.3.1 binary as well — the negations are ordinary patterns its
engine already understands, and the removed key's absence was already its default. A rollback is
therefore **binary-only**: there is no configuration to revert alongside the binary.

### Re-alignment procedure (do this before switching to 2.4.0)

1. **Remove** every `sees_personal_classified = true|false` line from each `[[consumer]]` block.

2. For every consumer that had `sees_personal_classified = false` (explicitly **or** by default —
   the default was `false`), **prepend** these four negation patterns to **both** `read_patterns`
   and `write_patterns`:

   ```toml
   read_patterns  = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "**"]
   write_patterns = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "..."]
   ```

   The negations are evaluated before the allow rules, so they must come first. The two `**/…`
   forms already subsume the two unprefixed ones; the redundancy is kept for readability, not
   because each covers a distinct case.

3. Consumers that had `sees_personal_classified = true` (typically the `admin` and `studio`
   consumers): **no negations** — leave their patterns unchanged.

> **What the negations now match — and what they no longer do.** The denial is by locus **segment**,
> not by substring. `!**/personal-classified` and `!**/personal-classified/**` deny the
> `personal-classified` section at any depth, including a vault literally named `personal-classified`
> (the server always produces a `{vault}/{section}` locus). Affix variants such as
> `personal-classified-2026` or `my-personal-classified` are **no longer denied** — this is the
> intended behaviour.

> **Read patterns are deny-by-default: a consumer with only negations reads nothing — add an
> explicit positive pattern.** Negations subtract from an allow set; they do not create one. If a
> consumer's `read_patterns` end up holding nothing but `!`-prefixed entries — because the field
> was empty before this migration, or already held only negations — that consumer now matches no
> locus at all, and every read it attempts becomes a `403`. This holds for **any** consumer, not
> only ones migrated away from `sees_personal_classified`. When re-aligning the preset, add at
> least one positive pattern for whatever that consumer must actually read:
>
> ```toml
> # Before — negations only, no positive: every read is now denied
> read_patterns = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**"]
>
> # After — an explicit positive added for what this consumer must read
> read_patterns = ["!personal-classified", "!personal-classified/**", "!**/personal-classified", "!**/personal-classified/**", "main/project-map"]
> ```

### The control command — confirm the preset loads before you deploy

Validate the edited preset **off-server**, read-only, through the same parse the server runs at
boot:

```
gradatum-admin acl validate --preset <path-to-bearer.toml>
```

This command opens no store, mints nothing and writes nothing. It reads the file and runs it through
the server's own preset loader.

- **A valid preset** exits `0` and prints a one-line summary on stdout:

  ```
  ACL preset <path> is valid — 13 consumer(s), 88 negation pattern(s).
  ```

- **A preset that still carries `sees_personal_classified`** (or is otherwise malformed) exits
  non-zero and names the offending key on stderr:

  ```
  Error: the ACL preset <path> does not parse: removed preset field `sees_personal_classified`: express the denial with a `!` negation pattern instead
  ```

A clean (`0`) run against your edited preset is your go/no-go signal. Only deploy 2.4.0 once it
passes.

> **Do not use `gradatum-admin api-key list` as this check.** It opens only the key store and never
> reads the preset, so a preset that still carries the removed key would pass it cleanly — a false
> green. And `api-key create` does reach the parse, but it *mints a key* as a side effect, so it is
> not a validation command. `acl validate` is the read-only check.

### The post-startup control — confirm the running server actually loaded the preset

Validating the preset off-server proves the file is good; it does not prove the running server
loaded *that* file. After deploying, confirm the server's own log shows it loaded a preset:

```
journalctl -u gradatum-server | grep 'AclEngine loaded from preset'
```

**The presence of that line is the positive signal.** Its *absence* — for any reason: an empty or
unset preset path, a file the process cannot read, or a parse error — means the loader fell back to
the empty engine and the server is serving **DENY-ALL**. Do not infer health from the process being
up; infer it from this line being present.

Then run a **positive functional check before any negative one**: as an ordinary consumer that is
allowed to read (not the admin), read a benign section — e.g. `section=decisions` — and confirm it
returns **200**. A DENY-ALL server fails this check, so it can no longer be mistaken for a healthy
one. Only after this positive check passes should you probe that `personal-classified` is refused
(the negative check).

---

## Mandatory pre-flight — make before break

**This is a pre-flight, not advice.** Everything in it is done *before* a 2.4.0 binary runs, and
the one check that can fail silently is the preset. The failure it prevents was measured, not
inferred — the measurement is stated in full under
[§The ACL preset field removal](#the-acl-preset-field-removal) above.

The ordering is not optional, because the failure mode is silent-at-boot:

1. **Edit the preset first** — remove the key, add the negations.
2. **Verify it loads** with `gradatum-admin acl validate --preset <path>` (exit `0`).
3. **Then deploy 2.4.0.** A server started against an un-migrated preset serves DENY-ALL and reports
   itself healthy; only step 2 catches that before it reaches production.
4. **After deploy, confirm the running server loaded the preset** — `AclEngine loaded from preset`
   present in `journalctl -u gradatum-server`, then a positive read (`section=decisions` under an
   ordinary consumer returns 200) before any negative probe.

Update any API client and Rust consumer (drop `pattern`, correct invented section names) in the same
window.

---

## The deployment sequence, in order

A version change is not only a binary swap. The order below is the one that keeps every step
reversible; the repository's local deploy script
([`scripts/deploy-gradatum-local.sh`](../scripts/deploy-gradatum-local.sh)) implements it, and you
can follow it by hand.

1. **Back up.** Two backups matter, and they are not the same one.

   - The **cold pre-migration backup** is taken by the units themselves:
     `ExecStartPre=/usr/local/bin/gradatum-pre-migration-backup` runs at **every** start of the
     units that migrate, so it covers the deploy, a manual `systemctl restart`, a restart after a
     crash and a reboot alike. It copies **cold** (service stopped) after
     `wal_checkpoint(TRUNCATE)`, and it is **fail-closed**: a backup that fails prevents the
     service from starting, so a failed backup migrates nothing. Archives land under
     `/var/lib/gradatum/backups/pre-migration/<timestamp>/` as `dbs.tar.gz` plus `SHA256SUMS`.
     Restoring is a deliberate manual operation, with the services stopped:

     ```bash
     cd /var/lib/gradatum/backups/pre-migration/<timestamp>
     sha256sum -c SHA256SUMS && sudo tar -xzf dbs.tar.gz -C /
     ```

   - The **deploy-time backup** of the binaries and databases you are about to replace, taken
     before anything is stopped.

   Note that the local script does **not** copy `index.db` in its own backup, and that is
   deliberate: the `ExecStartPre` guard above already covers every path that migrates the index,
   cold and fail-closed, for one extra snapshot rather than a second retention policy.

2. **Re-align and control the preset** — [§The ACL preset field removal](#the-acl-preset-field-removal),
   go/no-go on `gradatum-admin acl validate --preset <path>` exiting `0`. This happens **before**
   any new binary is installed.

3. **Install the new binaries.** Before substituting anything, validate each binary off the live
   path (a *transit zone*): prove it executes, and — for the multi-role engine binary — prove it
   accepts **every** configuration it will be asked to serve. One refusal means **no substitution
   anywhere**, leaving the live binaries byte-identical. This is the step that turns "the binary
   starts" into "the binary can serve *this* host's configuration", before the swap rather than
   after it.

4. **Restart the units, in order.** Stop the **worker first, then the server** — so no worker is
   left leasing work against a server that is going down. Start the **server first, then the
   worker**. The **gateway starts independently**: its unit carries no ordering on the server, so
   it is restarted on its own and is not a step of the server's sequence. The **engines are
   restarted one at a time**, each behind a health gate, so a single bad instance is detected
   before the next is touched; an anomaly at that stage restores the fleet **all-or-nothing**.

5. **Check `/health` — and read the boot window correctly.** `GET /health` on the main listener
   returns **HTTP 200 unconditionally**: the *body* carries the real state, including `starting`
   and `unhealthy`. Two consequences, and they point in opposite directions.

   - A gate that checks **only the HTTP code** is satisfied as soon as the listener answers;
   - A gate that reads the **`.status` field** must tolerate the boot window. A freshly started
     server can report `unhealthy` for roughly **40 seconds** (measured) before it settles, so
     apply a floor of at least **60 seconds** — below that, the honest reading is "still starting",
     not "the deploy failed".

   Read `.version` **and** `.build_sha` **from the same response** (two successive reads can
   straddle a restart and describe two different processes), and require both to match the target.
   That is what proves the process actually running is the one you shipped, rather than a binary
   left in place by a silent install failure.

6. **Close on the fleet — the same `version` and the same `build_sha`, everywhere.** Every gradatum
   surface that publishes `/health` (server, worker, gateway, engine) publishes both values
   (`build_sha` stays public for exactly this reason). Walk the fleet and require **both** to match
   on **every** instance. One instance behind on either is a partially upgraded fleet: an engine
   still on the old build is still answering through the gateway, and nothing in its responses
   tells the caller so. This is the closing gate, not a formality.

---

## Migration scripts

**This line ships no migration script, and none is owed.** Two independent properties make one
unnecessary:

- **No schema migration runs between 2.3.1 and 2.4.0.** The harness compares the two migration
  ledgers — `_sqlx_migrations` in the queue database and `_schema_migrations` in the index — line
  by line, `installed_on` and `checksum` included, and finds them **identical** before and after
  the upgrade. Nothing is applied that a script would have to apply first, and nothing has to be
  un-applied on the way back ([§Rolling back](#rolling-back)).
- **No new job variant is introduced by this line.** The queue's variant set is unchanged, so no
  pending job has to be drained, converted or re-enqueued before an older worker can execute it.

The breaks of this line are a configuration-key removal and a wire-contract change
([§The break inventory](#the-break-inventory)) — not the import renames and type paths that the
2.0 → 2.1 script exists for.

**There is deliberately no preset-realignment script either.** The edit is mechanical in
appearance — delete one line per consumer, prepend four negation patterns — but those patterns
belong at the head of each consumer's own `read_patterns` and `write_patterns`, and a rewrite
applied uniformly across the `[[consumer]]` blocks would get the consumers that carried the key
set to `true` wrong. The failure it produced would be exactly the silent, fleet-wide denial
described above, and `acl validate` is already the mechanical go/no-go on the result. The edit is
written out, once, under **Re-alignment procedure** in
[§The ACL preset field removal](#the-acl-preset-field-removal).

**What would change this conclusion.** Two things, and both arrive with a *later* line rather than
this one: a migration the new binary applies at startup, or a job variant a worker can enqueue but
an older worker cannot execute. Either one makes a script — or at least a documented order of
operations — due.

**Where you will see it.** A migration added by a later line is caught mechanically: the ledger
comparison is one of the harness's blocking keys, so the run goes **red**, naming that key and
printing the divergences. A new job variant is caught one step later, at the rollback — the older
worker has no executor for it, which is why the queue discipline under
[§Rolling back](#rolling-back) keeps its force whatever the line. Running the harness is described
in [§Verifying the upgrade](#verifying-the-upgrade).

---

## Rolling back

**The rollback of this line is a binary-only rollback, and it was exercised — not assumed.** The
harness restarts the 2.3.1 server against the already-migrated state and takes one more set of
measurements there; they are identical to the "before" set. See
[§Verifying the upgrade](#verifying-the-upgrade).

What that rests on, and what you must still get right:

- **No schema migration runs in this line.** The state is not carried forward irreversibly: there
  is no data-side rollback step to run, because there is nothing to un-apply. (This is a property
  of 2.4.0, verified against the previous line — check it for any future version rather than
  assuming it.)
- **No new job variant is introduced by this line.** A job enqueued by 2.4.0 is therefore
  executable by the 2.3.1 worker. As a general discipline, still roll the worker back with the
  queues quiet: a pending job whose variant the older worker does not know has no executor there.
- **No configuration to revert.** A preset already re-aligned to negation patterns **remains valid
  on 2.3.1** — the negations are ordinary patterns the older engine understands, and the removed
  key's absence was already its default. So rolling the binary back needs no preset change: **one
  path back, not two.** Conversely, a preset you never touched — still carrying the key — **parses
  again** on 2.3.1, so if you upgraded before re-aligning, rolling the binary back restores service
  on its own. The break is one-directional: the field is removed forward, not backward. The
  `pattern` field, by contrast, is accepted and ignored again by the old server, and non-canonical
  sections stop being 400s.
- **Roll the fleet back, or not at all.** The unit order is the mirror of the deployment: stop the
  worker before the server, start the server before the worker; the gateway on its own; the
  engines one at a time behind a health gate. Mixed versions across a fleet are the failure this
  ordering exists to prevent — the same-`version`-and-same-`build_sha` rule of step 6 applies to a
  rollback exactly as it applies to a deploy.

---

## Verifying the upgrade

The upgrade is covered by a harness in the repository, `tests/upgrade/`. It **generates** a real
2.3.1 state from a versioned text fixture, **mounts** it with the 2.4.0 binaries, takes a set of
measurements before and after, and then **restarts the 2.3.1 binary on the migrated state** for a
third set. Nothing in it is simulated: the versions and build identifiers are read from `/health`,
never deduced from a path.

**What it compares, at zero tolerance.** Migration tables line by line (including the column that
a *re-application* would change), the listing in the order rendered, a fixed set of literal
searches, the feature-card export **byte for byte**, per-table counts, and an ordered digest of the
job queue. Each measurement is written to the output directory, not merely printed.

**Exit codes — and a `2` is never a green.**

| Code | Meaning |
|---|---|
| `0` | Parity established |
| `1` | Divergence — **every gap is named** |
| `2` | **Not executed** — a required binary, tool, state or free port was missing, so *no measurement was taken* |

A `2` is not a divergence either: `1` is reserved for a measured difference between the state
before and the state after, never for the tooling being unable to measure.

### Replaying the upgrade harness by hand

The harness reads no absolute machine path; everything comes from
`GRADATUM_UPGRADE_*` environment variables. Export them, then run the command below — it is the
exact line carried in `RELEASE-MANIFEST.yaml` under `validate:`, byte for byte, so a local replay
exercises what the release gate exercises:

```bash
export GRADATUM_UPGRADE_STATE=…      # the 2.3.1 state to mount (produced by the generator)
export GRADATUM_UPGRADE_BIN_231=…    # directory holding the 2.3.1 binaries
export GRADATUM_UPGRADE_BIN_240=…    # directory holding the 2.4.0 binaries
export GRADATUM_UPGRADE_OUT=…        # output directory — every measurement is written here
export GRADATUM_UPGRADE_PHASES=…     # per-phase durations, one line per phase

bash tests/upgrade/run-upgrade-test.sh --state "$GRADATUM_UPGRADE_STATE" --admin-bin-231 "$GRADATUM_UPGRADE_BIN_231/gradatum-admin" --server-bin-231 "$GRADATUM_UPGRADE_BIN_231/gradatum-server" --admin-bin-240 "$GRADATUM_UPGRADE_BIN_240/gradatum-admin" --server-bin-240 "$GRADATUM_UPGRADE_BIN_240/gradatum-server" --out "$GRADATUM_UPGRADE_OUT" --port 19290
```

The port is the only thing you may need to change: the release gate uses `19290`, and the harness
probes it by a **real bind** before doing anything else. If something already listens there, it
exits `2` — with **nothing started and no measurement directory created** — rather than mounting
the state on a port it does not control. Pass another free port and nothing else changes.

**What it proves, and what it does not.** Read this before treating a green run as coverage it does
not have:

- It proves the **data** migrates without divergence, and that the old binary still serves the
  migrated state.
- It does **not** exercise the binary rollback *as a deployment* — the traffic switch, the unit
  ordering, the clean stop. It proves the old binary starts and measures the same, not that your
  fleet is back on 2.3.1.
- It does **not** rebuild derived artifacts (embeddings, ANN index, summaries).
- It **never reads production**: it works on a state produced by its own generator. Copying your
  real state, mounting it aside and comparing is a **manual operator pre-flight**, not a CI step.
- It polls `/health` under a 60-second floor and records the real time to the first `healthy`. On a
  synthetic state with no embedder, curator or worker reachable, that time is `never` — recorded
  as a measurement, never treated as a failure. The harness concludes on the measurements, not on
  the health state.

The full mechanics — the exact list of comparison keys, the generator, the diagnostic options, and
what each key does and does not cover — are in
[`tests/upgrade/README.md`](../tests/upgrade/README.md).

---

## Docker

This line ships container packaging alongside the binaries — a `Dockerfile` and a
`docker-compose.yml` at the repository root, with the image built from source. See
[Guide A — Docker quickstart](guides/A-docker-quickstart.md) for the build, the services, the
volumes and the health wiring.

The upgrade sections above apply to a container deployment without adaptation: the preset to
re-align is the same file (mounted into the server), and `/health` on the main listener behaves the
same way — **HTTP 200 with the real state in the body**. Two container-specific points:

- A container healthcheck written as `curl -fsS http://127.0.0.1:19090/health` only fails on an
  HTTP error status, so it is **not** tripped by the boot window described in step 5 above; its
  `start_period` still has to cover a first start on a cold, empty state.
- Because `status` is in the body rather than the status line, a supervisor that reads the *body*
  must apply the same floor, or it will restart a server that is merely still starting.

---

## Library consumers (Rust crates)

The only source break at the library surface is the two removed `pub` fields:

- `gradatum_dto::VaultListRequest::pattern` — delete it from any struct literal; no replacement.
- `gradatum_acl_policy::ConsumerEntry::sees_personal_classified` — delete it; express the denial as
  negation patterns as described above.

Both are matched by named entries in `RELEASE-MANIFEST.yaml` under `semver_deviations`. Everything
else at the library surface is additive and absorbed by `#[non_exhaustive]`.

---

## Why this guide is standalone, not cumulative

Each version line gets one migration guide, describing the breaks introduced **by that line only**.
If you are several versions behind, read the guides in order — this one does not repeat the breaks of
the guides before it. Applying them out of order, or skipping one, can leave you with a preset or a
client that a later version rejects.

The guides for the earlier lines, in order:

| Guide | Line |
|---|---|
| [`docs/UPGRADING-1.0.0-to-2.0.0.md`](UPGRADING-1.0.0-to-2.0.0.md) | how the server determines *who is calling* |
| [`docs/UPGRADING-2.0.0-to-2.1.0.md`](UPGRADING-2.0.0-to-2.1.0.md) | 2.0 → 2.1 |
| [`docs/UPGRADING-2.1.0-to-2.2.0.md`](UPGRADING-2.1.0-to-2.2.0.md) | the `project-map` registry model and the write contract |
| **this guide** | 2.3.1 → 2.4.0 |
