# Upgrading from 2.0 to 2.1

**Read this before upgrading.** 2.1.0 is a *minor* release, so if your `Cargo.toml` allows it
(`gradatum-core = "2.0"`, `gradatum-queue = "2.0"`, …), Cargo **adopts 2.1.0 silently** — no action
on your part, and the next `cargo build` breaks with no warning. This guide is written for that
moment: you have a compile error in hand and you want to know what happened and what to write
instead.

This is the first migration guide of the project. The order below is also the recommended order of
operations. Every breaking change cites the gradatum card that carries it (F-XXX). A companion
script automates the mechanical substitutions — see [The migration script](#the-migration-script).

---

## Your build just broke — start here

Find your error in this table, then jump to the section. Every error below is a **real** message,
measured against 2.1.0 (rustc stable).

| Error you see | Break | Go to |
|---|---|---|
| `error[E0432]: unresolved import gradatum_queue::queue` / `could not find queue in gradatum_queue` | The legacy queue module was removed | [§1 The legacy queue is gone](#1-the-legacy-queue-is-gone-f-177) |
| `error[E0432]: unresolved import gradatum_core::provenance::compute_distill_trust` | `compute_distill_trust` moved crate | [§2 compute_distill_trust moved](#2-compute_distill_trust-moved-to-gradatum-distill-f-248) |
| `error[E0004]: non-exhaustive patterns: _ not covered` on a `Section` match | `Section` became `#[non_exhaustive]` | [§3 Section](#3-section-became-non-exhaustive-and-gained-a-snapshot-variant-f-246) |
| `error[E0599]: no variant or associated item named Chore/Spike found for enum KindKind` | `KindKind::Chore`/`::Spike` removed | [§4 KindKind vocabulary](#4-kindkind-chore-and-spike-are-removed-f-220) |
| `no method named as_database_error found for struct rusqlite::Error` (inside a match on `ApiKeyError::Sql` / `RevocationError::Sqlite`) | Error payload type changed from `sqlx::Error` | [§5 Error payloads](#5-error-payloads-now-carry-rusqlite-error-f-145) |
| `error[E0308]: mismatched types: expected QueueDb, found SqlitePool` | Queue DB handle type changed | [§6 Queue DB handles](#6-queue-database-handles-now-take-queuedb-f-145) |
| `error[E0639]: cannot create non-exhaustive struct using struct expression` on a `HealthSnapshot` / `DriftScanResult` struct literal | Struct became `#[non_exhaustive]` | [§7 Accumulated breaks](#7-accumulated-breaks-f-245) |
| `GET /api/v1/jobs/:id` now answers `404` | Legacy jobs route removed | [§1 HTTP route](#the-http-route) |

If your error is not in the table, keep reading — every known break is inventoried below.

> **If you have no error at all, do not stop here.** One change in this release alters behaviour
> without breaking the build: search now treats `OR`, `NOT` and `NEAR` as literal words. See
> [§8 Search treats OR, NOT, NEAR and AND as literal words](#8-search-treats-or-not-near-and-and-as-literal-words-f-162).

---

## What happened, in one paragraph

Between 2.0.0 and 2.1.0, gradatum finished retiring its `sqlx`-based storage layer in favour of a
synchronous `rusqlite` engine (F-145), and removed the legacy `jobs_v2` queue (F-177) that was
the last consumer of it. It also shipped several smaller breaks: one function moved crate
(F-248), two public types changed shape (F-246, F-220), and the field additions accumulated
over the 2.0.x line are absorbed by `#[non_exhaustive]` annotations on the two affected structs
(F-245). The break inventory below is the union of two sources: the public-api surface
reference `git diff internal/2.0.9..HEAD -- public-api/baseline/`, and the 22 entries of
`RELEASE-MANIFEST.yaml`, which span the whole `2.0.0 → 2.1.0` range. The `2.0.1` … `2.0.9`
milestones were never published independently, so a consumer upgrading from `2.0.0` receives all
of them at once; the manifest entries are what cover that wider range, and each break below is
attributed to the card that introduced it.

**The critical property of a minor release:** because 2.1.0 is adopted automatically, this guide is
the *only* warning you get. There is no earlier deprecation cycle for the breaks below beyond the
`#[deprecated]` notices already shipped in 2.0.8 — see [§4](#4-kindkind-chore-and-spike-are-removed-f-220).

---

## The migration script

A companion script automates the mechanical substitutions (import renames and type paths):

```bash
bash scripts/migrate-2.0-to-2.1.sh /path/to/your/crate
```

- `--dry-run` prints what would change without touching files.
- The script only touches `*.rs` files under the target (excluding `target/`).

After the rewrites, the script prints **two separate lists**, and only the first one affects the
exit code:

| List | What it contains | Exit code |
|---|---|---|
| **Leftovers** — *"could NOT be rewritten mechanically"* | Symbols **removed** in 2.1. They cannot appear in migrated code, so their presence is work that remains | Sets exit code **`1`** |
| **Re-read** — *"call site(s) to RE-READ"* | Symbols that **survive** in 2.1 under the same name with a changed signature | **No effect** — exit code stays as-is |

This split matters: the re-read list is *expected* to be non-empty in correctly migrated code, because
those symbols still exist. If it counted as failure, any consumer using the queue would be pinned at
exit `1` for ever, on code that is right. **Exit `0` is reachable once the migration is finished**, and
it means "nothing removed is left" — not "nothing was found".

- Exit code `0` = no removed symbol remains (a re-read list may still be printed, and that is fine).
- Exit code `1` = at least one removed symbol remains; each is printed `file:line  (symbol)`.
- Exit code `2` = usage error (no `.rs` files found, unknown argument).

Run it, handle the leftovers, skim the re-read list once, then compile.

---

## 1. The legacy queue is gone (F-177)

**Card: F-177.** The `jobs_v2` table (which held full copies of deleted notes outside any vault
lifecycle — persistence, not history) and every public type that read it were removed. The live
queue is now `gradatum_jobs`, driven by `gradatum_core::QueueStore` (implemented by
`gradatum_db_sqlite::SqliteQueueStore`).

### What breaks

Every public item of the removed `gradatum_queue::queue` module, plus its root re-exports:

| Removed | Replaced by |
|---|---|
| `gradatum_queue::queue` (module) | `gradatum_queue::GradatumQueue` (a `QueueStore` facade) |
| `gradatum_queue::queue::SqliteQueue` / `gradatum_queue::SqliteQueue` | `GradatumQueue` on `SqliteQueueStore` — see the constructor below |
| `gradatum_queue::queue::Queue` (trait) / `gradatum_queue::Queue` | `gradatum_core::QueueStore` |
| `gradatum_queue::queue::NewJob` / `gradatum_queue::NewJob` | `gradatum_core::job::JobRecord` |
| `gradatum_queue::queue::JobInfo` / `gradatum_queue::JobInfo` | `gradatum_core::job::JobRecord` |
| `gradatum_queue::queue::LeasedJob` / `gradatum_queue::LeasedJob` | `gradatum_core::job::JobRecord` (via `QueueStore::dequeue`) |
| `gradatum_queue::queue::JobId` (= `i64`) / `gradatum_queue::JobId` | `ulid::Ulid` |
| `gradatum_queue::queue::JobStatus` / `gradatum_queue::JobStatus` | `gradatum_core::job::JobStatus` |
| `gradatum_queue::queue::QueueError` / `gradatum_queue::QueueError` | `gradatum_core::job::QueueError` |

**Probable error:**

```
error[E0432]: unresolved import `gradatum_queue::queue`: could not find `queue` in `gradatum_queue`
```

### What to write instead

The legacy `SqliteQueue` was a self-contained, sqlx-backed queue. The replacement is a
`QueueStore` implementation. Construction changes completely:

```rust
// 2.0
let pool = sqlx::SqlitePool::connect("sqlite:///var/lib/gradatum/queue.db").await?;
gradatum_db_sqlite::run_migrations(&pool).await?;
let store = gradatum_db_sqlite::SqliteQueueStore::new(pool);
let queue = gradatum_queue::GradatumQueue::new(store);
```

```rust
// 2.1 — QueueDb is the connection handle; open it, migrate, wrap it.
let db = gradatum_db_sqlite::open_queue_db(Path::new("/var/lib/gradatum/queue.db")).await?;
gradatum_db_sqlite::run_migrations(&db).await?;
let store = gradatum_db_sqlite::SqliteQueueStore::new(db);
let queue = gradatum_queue::GradatumQueue::new(store);
```

The data model differs, so the operation mapping is **not** type-for-type:

- `SqliteQueue::enqueue(NewJob { kind, payload, tenant_id, max_attempts })` →
  `QueueStore::enqueue(JobRecord)` — build a `gradatum_core::job::JobRecord` instead of a
  `NewJob`. The job identifier changes from `JobId = i64` to `ulid::Ulid`.
- `Queue::get(JobId) -> Option<JobInfo>` →
  `QueueStore::get(id: Ulid, tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError>`.
- `Queue::lease(&[&str], Duration) -> Option<LeasedJob>` → `QueueStore::dequeue(tenant_filter)` /
  `dequeue_by_kind(kind, tenant_filter)`.
- `JobStatus::as_str()` / `JobStatus::from_str()` (states `Pending`/`Leased`/`Done`/`Dead`) →
  `gradatum_core::job::JobStatus` (states `Pending`/`Running`/`Waiting`/`Done`/`Failed`/`DLQ`/
  `Cancelled`/…). The mapping is a decision, not a rename — see the non-automatable list below.
- `QueueError::Sqlx(sqlx::Error)` → `QueueError::Storage(String)` (or `gradatum_core::job::QueueError`'s other variants).

### The HTTP route

**`GET /api/v1/jobs/:id`** (the backward-compat route that read `jobs_v2`) is removed and answers
`404`. The replacement, **`GET /api/v1/jobs/{ulid}/v2`**, has been the route that
`vault_write`/`vault_forget` point their `poll_url` at since 2.0 — update any client that calls the
old path directly.

---

## 2. `compute_distill_trust` moved to `gradatum-distill` (F-248)

**Card: F-248.** The distillation logic was gathered into a new crate, `gradatum-distill`.
`gradatum_core::provenance::compute_distill_trust` is removed; the function is unchanged and now
lives at `gradatum_distill::compute_distill_trust`.

**Probable error:**

```
error[E0432]: unresolved import `gradatum_core::provenance::compute_distill_trust`: no `compute_distill_trust` in `provenance`
```

**What to write instead:**

```rust
// 2.0
use gradatum_core::provenance::compute_distill_trust;
let t = compute_distill_trust(&ids, &lookup, 0.8);

// 2.1 — same signature, same behaviour (mean of source trusts × confidence, clamped to [0,1])
use gradatum_distill::compute_distill_trust;
let t = compute_distill_trust(&ids, &lookup, 0.8);
```

Add the crate to `Cargo.toml`:

```toml
gradatum-distill = "2.1"
```

`TrustLookup` stays in `gradatum_core::provenance`; the job vocabulary (`DistillMode`,
`DistillSource`, `Job::Distill`) is unchanged. This is the **only break the migration script
rewrites fully** — the path substitution is type-preserving.

---

## 3. `Section` became non-exhaustive and gained a `Snapshot` variant (F-246)

**Card: F-246.** A 14th canonical section, `snapshot`, was added, and the `Section` enum was
marked `#[non_exhaustive]` in the same change so that future section additions stay additive. The
re-exported array `Section::ALL` grew from 13 to 14 entries, and a new constant
`Section::DEFAULT_SEARCH_EXCLUDED` lists the sections excluded from search by default.

**Probable error** — any `match` on `Section` that lists the variants and has no wildcard arm:

```
error[E0004]: non-exhaustive patterns: `_` not covered: pattern `_` not covered
```

**What to write instead** — add the wildcard arm to every exhaustive `match` on `Section`:

```rust
// 2.0
match section {
    Section::Decisions => "decision",
    Section::Architecture => "architecture",
    // …
}

// 2.1 — `#[non_exhaustive]` requires a wildcard; treat unknown sections as generic.
match section {
    Section::Decisions => "decision",
    Section::Architecture => "architecture",
    // …
    _ => "other",   // ← required from 2.1.0
}
```

Notes:

- You cannot match `Section::Snapshot` from outside the crate and still be exhaustive — the whole
  point of `#[non_exhaustive]` is that the variant list is open. If you need to special-case the
  `snapshot` section, call the canonical-string helpers (`Section::from_canonical_str("snapshot")`)
  instead of matching the variant.
- If you sized an array to `Section::ALL.len()` as a const of 13, it now needs to follow `ALL` at
  runtime, not a hardcoded count.

---

## 4. KindKind Chore and Spike are removed (F-220)

**Card: F-220.** The `CHORE` and `SPIKE` kind vocabulary was removed by F-220. The two variants
were kept in 2.0.8 as `#[deprecated]` shims so source written against the published 2.0.0 API
would keep compiling; **2.1.0 removes them for good**. Both the Rust variants and the `CHORE`/
`SPIKE` wire values are gone — `KindKind::from_wire` now returns `None` for them.

**Probable error:**

```
error[E0599]: no variant or associated item named `Chore` found for enum `KindKind`
```

**What to write instead** — use the deliberate catch-all, `KindKind::Task`:

```rust
// 2.0
KindKind::Chore    // or KindKind::Spike

// 2.1
KindKind::Task     // maintenance, tooling, bounded exploration all fold into Task
```

If your code sends or parses the wire vocabulary, replace `"CHORE"` / `"SPIKE"` with `"TASK"`, and
handle the now-possible `None` from `KindKind::from_wire`. **Not automatable** — the right variant
is a semantic choice.

---

## 5. Error payloads now carry rusqlite Error (F-145)

**Card: F-145.** `gradatum-auth` and `gradatum-acl-auth` abandoned `sqlx` for a synchronous
`rusqlite` connection. The error variants that carried the SQL driver error changed payload type:

- `RevocationError::Sqlite(sqlx_core::error::Error)` →
  `RevocationError::Sqlite(rusqlite::error::Error)` — plus a new `RevocationError::Blocking` variant.
- `ApiKeyError::Sql(sqlx_core::error::Error)` →
  `ApiKeyError::Sql(rusqlite::error::Error)` — plus two new variants,
  `ApiKeyError::Blocking` and `ApiKeyError::Migration`.

Both enums are `#[non_exhaustive]`, so this is invisible to `cargo-semver-checks` (a false green)
but visible in the public-api surface reference — it is one of the breaks this guide exists for.

**Probable error** — only if you call an `sqlx`-specific method on the inner error:

```
error[E0599]: no method named `as_database_error` found for struct `rusqlite::Error`
```

**What to write instead.** If you only **format** the inner error, or match on it to build your
own message, nothing changes — `rusqlite::Error` implements `Display` and `std::error::Error` like
`sqlx::Error` did:

```rust
match err {
    ApiKeyError::Sql(e) => format!("sqlite: {e}"),        // unchanged — both Display
    _ => String::new(),
}
```

**Forwarding with `?` is not in that "nothing changes" set.** Both variants are declared
`#[from]`, so 2.0 generated `impl From<sqlx::Error> for ApiKeyError` and
`impl From<sqlx::Error> for RevocationError`. Those impls are **gone** in 2.1 — the generated
impls now take `rusqlite::Error`. Any `?` that relied on the conversion no longer compiles:

```rust
// 2.0 — compiled: `?` used From<sqlx::Error> for ApiKeyError
fn load(pool: &sqlx::SqlitePool) -> Result<Row, ApiKeyError> {
    let row = some_sqlx_call(pool)?;   // 2.1: no longer converts
    Ok(row)
}
```

If the surrounding code still produces an `sqlx::Error` (because *your* crate kept `sqlx`), you
must convert it yourself — there is no longer any implicit bridge between the two error types.

If you called `sqlx`-specific accessors, adapt to the `rusqlite::Error` API:

```rust
// 2.0
ApiKeyError::Sql(e) => e.as_database_error().map(|d| d.message().to_string()),

// 2.1
ApiKeyError::Sql(e) => Some(e.message()),   // rusqlite::Error::message() -> String
```

**Not automatable, but not blocking either.** The variant names are unchanged, so a path rewrite
cannot fix this; you must inspect each `Sql(..)` / `Sqlite(..)` arm yourself. The script lists these
call sites in its **re-read** list, which does **not** affect the exit code — the variants survive in
2.1, so their presence is expected in migrated code. If you `match` these enums, remember they are
`#[non_exhaustive]` — a wildcard arm is already required.

---

## 6. Queue database handles now take `QueueDb` (F-145)

**Card: F-145.** `gradatum-db-sqlite` and `gradatum-queue` abandoned `sqlx` for a `rusqlite`
connection wrapped in a `QueueDb` handle (a single connection under `Arc<Mutex<…>>`, operated on a
blocking thread). Every public signature that took a `sqlx::SqlitePool` now takes a `QueueDb`:

| 2.0 signature | 2.1 signature |
|---|---|
| `SqliteQueueStore::new(sqlx::SqlitePool) -> Self` | `SqliteQueueStore::new(QueueDb) -> Self` |
| `run_migrations(&SqlitePool) -> Result<(), sqlx::MigrateError>` | `run_migrations(&QueueDb) -> Result<usize, QueueError>` |
| `apply_sqlite_pragmas(&SqlitePool) -> Result<(), sqlx::Error>` | `apply_sqlite_pragmas(&QueueDb) -> Result<(), QueueError>` |
| `idempotency_cleanup(&SqlitePool, i64)` | `idempotency_cleanup(&QueueDb, i64)` |
| `idempotency_insert(&SqlitePool, &str, &str)` | `idempotency_insert(&QueueDb, &str, &str)` |
| `idempotency_lookup(&SqlitePool, &str)` | `idempotency_lookup(&QueueDb, &str)` |

**Probable error:**

```
error[E0308]: mismatched types: expected `QueueDb`, found `sqlx::SqlitePool`
```

**What to write instead.** Open the `QueueDb` with one of the three constructors — all return
`Result<QueueDb, QueueError>`:

```rust
use gradatum_db_sqlite::{open_queue_db, open_queue_db_existing, open_queue_db_in_memory};
use std::path::Path;

// creates the file if absent, WAL + busy_timeout 5s (same settings as the old sqlx options)
let db = open_queue_db(Path::new("/var/lib/gradatum/queue.db")).await?;
// fail-fast if absent (parity with sqlx create_if_missing(false))
let db = open_queue_db_existing(Path::new("/var/lib/gradatum/queue.db")).await?;
// tests
let db = open_queue_db_in_memory().await?;
```

Then pass `&db` where the old code passed `&pool`, and note the `run_migrations` return type:

```rust
// 2.0
run_migrations(&pool).await?;          // -> Result<(), _>

// 2.1
let applied = run_migrations(&db).await?;   // -> Result<usize, QueueError> — count of migrations applied
```

**Not automatable, but not blocking either.** These are construction changes, not renames — the
connection object itself changes type. The script lists these call sites in its **re-read** list,
which does **not** affect the exit code: the functions still exist in 2.1 under the same names, so a
fully migrated project still contains them.

---

## 7. Accumulated breaks (F-245)

**Card: F-245.** The field additions accumulated across the 2.0.x line are **absorbed** by a
`#[non_exhaustive]` annotation on each affected struct. Measured against the **published** 2.0.0
baseline with cargo-semver-checks 0.50, the annotation suppresses the
`constructible_struct_adds_field` lint: only `struct_marked_non_exhaustive` is emitted. What you
actually meet in 2.1.0 is the annotation itself — and it makes **every future field addition
additive**.

1. **`IndexStore::last_indexed_at` is *not* a break in this minor.** The method added to the
   trait in 2.0.1 gained a **default implementation `Ok(None)`**. If you implement
   `gradatum_core::index_store::IndexStore` (or its root re-export), you no longer need to write
   it: the default reports the indexation freshness as **unknown**. Override it only if your store
   tracks the last-indexed timestamp — then return `Ok(None)` when the live corpus is empty,
   never as a fallback on failure.

2. **`HealthSnapshot` and `DriftScanResult` are now `#[non_exhaustive]`.**
   `gradatum_engine::health::HealthSnapshot` (reachable with the `serve` feature) and
   `gradatum_index::drift::DriftScanResult` are marked `#[non_exhaustive]`. The fields they
   gained across the 2.0.x line (`event_log`, `untracked`, `embeddable_notes_without_vector`)
   are **no longer breaks**. Constructing these structs with a **struct literal now fails**:

   ```
   error[E0639]: cannot create non-exhaustive struct using struct expression
   ```

   The fields remain public and writable — only the literal is forbidden outside the defining
   crate. **What to do:**

   - `HealthSnapshot`: you never needed to build one — obtain it from
     `HealthState::snapshot()`, or deserialize the `/health` JSON.
   - `DriftScanResult`: obtain it from `scan_phase_a()`; to start from an empty one use
     `DriftScanResult::default()` (still derived) and set the fields you need.

   **Not automatable** — the migration script rewrites paths, not struct expressions. For a
   `DriftScanResult` literal, `..Default::default()` restores construction.

---

## 8. Search treats `OR`, `NOT`, `NEAR` and `AND` as literal words (F-162)

**Card: F-162.** This section is the one break in this guide that produces **no compile error at
all**. Your build stays green, your tests may stay green, and the behaviour of `vault_search`
changes underneath you. That is precisely why it is documented here: a silent break is more
dangerous than a loud one, not less — there is no error message to lead you back to this page.

### What changes

Every whitespace-separated token of a search query is now quoted before it reaches the full-text
engine. The reserved words `AND`, `OR`, `NOT` and `NEAR` — matched case-insensitively — are
quoted along with the rest, so they are searched as **ordinary words** instead of being
interpreted as query operators. Tokens are then joined by the engine's implicit AND.

| Query | 2.0 | 2.1 |
|---|---|---|
| `gradatum OR notes` | union of the two terms | notes containing `gradatum` **and** the literal word `or` **and** `notes` |
| `notes NOT debug` | `notes` excluding `debug` | notes containing `notes` **and** the literal word `not` **and** `debug` |
| `alpha NEAR beta` | proximity search | the three words, each literal, all required |

### What to write instead

There is no substitute at the API level: gradatum never documented or guaranteed an operator
query language for its consumers, and none is exposed in 2.1. Rewrite the affected call sites:

- **Union** (`A OR B`) — issue two separate queries and merge the result sets yourself.
- **Exclusion** (`A NOT B`) — query for `A`, then filter `B` out on your side.
- **Proximity** (`A NEAR B`) — query for both terms and rank or filter on your side.

### How to find your affected call sites

The migration script cannot help here — there is no symbol to rewrite. Grep your own code for
query strings containing these words as operators:

```bash
grep -rnE '"[^"]*\b(AND|OR|NOT|NEAR)\b[^"]*"' --include='*.rs' .
```

Review each hit: a query that merely *contains* the word (`"not found"`) was already searched
literally in 2.0 for the non-reserved part, and is unaffected in intent; a query that *relied* on
the word as an operator now returns a different result set.

---

## Non-automatable cases — the honest list

The migration script rewrites import/type paths only. Everything below must be done by hand. The
three groups differ by **what the script does about them**, which is what tells you whether your
build is still incomplete or merely worth a second look.

### Group 1 — reported as leftovers, and they hold the exit code at `1`

These symbols were **removed** in 2.1. Migrated code does not contain them, so each occurrence is
work that remains. Once they are gone, the exit code drops back to `0`.

| Case | Card | Why it is not mechanical |
|---|---|---|
| Legacy queue *operations* and types (`SqliteQueue`, `Queue`, `NewJob`→`JobRecord`, `JobInfo`, `LeasedJob`, `JobId`→`Ulid`) | F-177 | The data model changed, not just the path |
| `QueueError` / `JobStatus` **variant** mapping (`QueueError::Sqlx`/`Time`/`CorruptedStatus`, `JobStatus::Leased`/`Dead`) | F-177 | Renaming the type path does not rename its variants, and the two variant sets are **disjoint** — none of these five names exists in the 2.1 surface. Picking the closest 2.1 variant is a decision about meaning |
| `KindKind::Chore` / `KindKind::Spike` replacements | F-220 | The 2.1 enum is `KindKind::{Feature, Enhancement, Fix, Task}`; the right variant is a semantic choice |

### Group 2 — reported as a re-read list, with no effect on the exit code

These symbols **survive** in 2.1 under the same name; only their signature or payload changed.
Correctly migrated code still contains them, so finding them says nothing about whether you are
done. **They are not failures.** If your call sites already read the 2.1 way, there is nothing to do.

| Case | Card | What changed |
|---|---|---|
| `SqliteQueueStore::new` / `run_migrations` / `apply_sqlite_pragmas` / `idempotency_*` | F-145 | They take a `QueueDb` instead of a `SqlitePool` — a construction change, not a rename |
| `ApiKeyError::Sql` / `RevocationError::Sqlite` | F-145 | The variant is kept; only the payload changed (`sqlx::Error` → `rusqlite::Error`) |
| `QueueError::NotLeased` | F-177 | The variant is kept; only the payload changed (`JobId`, an `i64` → `Ulid`) |

### Group 3 — not detected at all; you must find these yourself

The script makes no claim about these. Nothing will be printed, and the exit code will not move.

| Case | Card | Why a text scan cannot find it |
|---|---|---|
| `Section` `match` arms | F-246 | Whether a `match` is affected depends on the **type** of the matched value — information a text scan does not have. Matching every line containing `match` and `Section` would drown the real cases in false positives |
| `HealthSnapshot` / `DriftScanResult` struct literals | F-245 | Use the provided API instead (`HealthState::snapshot()`, `scan_phase_a()`, `DriftScanResult::default()`) |
| `JobStatus::as_str()` / `from_str()` call sites | F-177 | `gradatum_core::job::JobStatus` has no such methods. After the path rewrite the call no longer matches any reported symbol, so it surfaces only at compile time |

---

## Order of operations

1. **Run the script**: `bash scripts/migrate-2.0-to-2.1.sh /path/to/your/crate` (or `--dry-run`
   first). It rewrites the mechanical import/type paths, then prints the **leftovers** (removed
   symbols — these set exit `1`) and the **re-read** list (surviving symbols — no effect on the
   exit code).
2. **Add the missing dependency** if the script reported a `compute_distill_trust` rewrite:
   `gradatum-distill = "2.1"` in `Cargo.toml`.
3. **Handle the leftovers** the script printed (Group 1), then skim its re-read list (Group 2), in
   this order:
   1. legacy queue operations (§1),
   2. `Section` match arms (§3),
   3. `KindKind` replacements (§4),
   4. error-payload arms (§5),
   5. queue DB construction (§6),
   6. struct literals (§7).
4. **Compile.** The break list covers the full `2.0.0 → 2.1.0` range: every removal in the
   public-api surface reference is attributed, and the manifest entries span the intermediate
   `2.0.x` milestones. If a symbol is still unresolved after the script, it is one of the named
   non-automatable cases above. Note that exit `0` means no *removed* symbol remains; a re-read
   list may still be printed, and that is not a failure. Group 3 is invisible to the script — it
   surfaces only at compile time.
5. If you call the removed HTTP route, update it (§1 — HTTP route).
6. **Review your search queries** (§8). Nothing will fail to compile; this step is manual by
   nature and is the one most easily skipped.

---

## Rolling back

Because 2.1.0 is a minor, rolling back is pinning your dependency to the last 2.0.x you were on —
the break is in the crate you adopted, not in your data. Nothing in 2.1.0 rewrites your storage:
the queue migration and the driver change run against the same databases. Note that `jobs_v2` is
dropped by the 2.1.0 migration — data retained there (deleted-note payloads) is not migrated, by
design; it was persistence, not history. There is no 2.0.x deprecation release to fall back to for
the removed symbols; pin to your previous 2.0.x if you are not ready.

---

## Tracking

Every break above carries its gradatum card: **F-177** (legacy queue), **F-248**
(`compute_distill_trust`), **F-246** (`Section`), **F-220** (`KindKind`), **F-145** (`rusqlite`
migration), **F-245** (accumulated breaks absorbed by `#[non_exhaustive]`). The inventory is derived from the public-surface
reference (`public-api/baseline/`) between `internal/2.0.9` and this revision, crossed with
`RELEASE-MANIFEST.yaml`. The F-145 signature changes (§5 and §6) are present in the surface diff
but absent from the manifest — the class of break the compatibility gate cannot see. The
reachability of this guide from the repository homepage and the changelog is checked on the
published repository, not on a local tree.
