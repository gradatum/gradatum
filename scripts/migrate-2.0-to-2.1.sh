#!/usr/bin/env bash
# migrate-2.0-to-2.1.sh — mechanical source migration from the 2.0 public API to 2.1.
#
# Part of F-249 (gradatum 2.1.0). Covers the *mechanical* substitutions of the
# 2.0 → 2.1 migration guide (docs/UPGRADING-2.0.0-to-2.1.0.md). It renames import
# paths and type paths only. Every case it does NOT rewrite is reported, file by
# file, so that what remains is exactly what needs a human decision.
#
#   bash scripts/migrate-2.0-to-2.1.sh [PATH] [--dry-run]
#
#   PATH       project to migrate (default: current directory)
#   --dry-run  print the rewrites without touching files
#
# Exit codes (unchanged — this script only decides what belongs to each class):
#   0  all mechanical substitutions applied and no BLOCKING leftover remains
#   1  substitutions applied, but some usages could not be rewritten
#      mechanically (printed) — those are the manual cases of the guide
#   2  usage / unusable (no .rs files found, unknown argument)
#
# Only class A below (symbols REMOVED in 2.1) can produce exit code 1. Class B
# (symbols that SURVIVE with a changed signature) is printed as a review list and
# NEVER influences the exit code: a consumer who has finished migrating still has
# those calls in their code, so treating them as failures would pin the script to
# exit 1 for ever — a permanent false red, on correct code. `0` must stay reachable
# for a fully migrated project that uses the queue; that is the contract.
#
# What it rewrites (import & type paths only):
#   F-248  gradatum_core::provenance::compute_distill_trust
#          → gradatum_distill::compute_distill_trust
#   F-177  gradatum_queue::queue::JobStatus / gradatum_queue::JobStatus
#          → gradatum_core::job::JobStatus
#   F-177  gradatum_queue::queue::QueueError / gradatum_queue::QueueError
#          → gradatum_core::job::QueueError
#   F-177  brace imports `gradatum_queue::queue::{JobStatus, QueueError}`
#          → `gradatum_core::job::{JobStatus, QueueError}` (when every member maps)
#
# CLASS A — REMOVED in 2.1, reported and NOT rewritten → exit code 1.
# These symbols do not exist in the 2.1 surface. Their presence is, by construction,
# work that remains; once it is done they are gone and the exit code drops back to 0.
# Each is flagged file:line, never silently swallowed:
#   - remaining `gradatum_queue::queue` usages (SqliteQueue, Queue trait,
#     NewJob, JobInfo, LeasedJob, JobId) — the legacy queue was removed, there
#     is no type-for-type replacement (F-177);
#   - `KindKind::Chore` / `KindKind::Spike` removal (F-220) — the 2.1 enum is
#     `KindKind::{Feature, Enhancement, Fix, Task}`; categorise former chore/spike
#     work as `Task`;
#   - enum variants that exist in the 2.0 `gradatum_queue::queue` types but have
#     NO counterpart in the 2.1 `gradatum_core::job` types (F-177). Renaming the
#     type path does not rename its variants, and the two variant sets are
#     disjoint on these names, so a `match` arm left behind will not compile:
#       QueueError::Sqlx / ::Time / ::CorruptedStatus  (2.0 only)
#       JobStatus::Leased / ::Dead                     (2.0 only)
#     The 2.1 sets are QueueError::{Storage, NotFound, Serialization,
#     InvalidTransition, Cancelled, NotLeased, NotImplemented} and
#     JobStatus::{Pending, Running, Waiting, Done, Failed, DLQ, Cancelled,
#     Conflict} — pick the closest one by MEANING, not by position.
#
# CLASS B — SURVIVING in 2.1 with a changed signature: printed as a REVIEW list,
# with NO effect on the exit code. Every one of these still exists in 2.1 under the
# same name, so correctly migrated code still contains them. They are call sites to
# re-read once, not work that remains — which is exactly why they must not be able
# to hold the exit code at 1:
#   - `SqliteQueueStore::new(pool)` / `run_migrations(&pool)` / `apply_sqlite_pragmas`
#     / `idempotency_*` — they now take a `QueueDb` instead of a `SqlitePool`
#     (F-145), a construction change, not a rename;
#   - `ApiKeyError::Sql` / `RevocationError::Sqlite` — the variant survives, only its
#     payload changed (sqlx::Error → rusqlite::Error, F-145);
#   - `QueueError::NotLeased` — the variant survives, only its payload changed
#     (`JobId`, an `i64` → `Ulid`, F-177). Check what you pass to / bind from it.
#
# What it does NOT detect — you must find these yourself, following the guide.
# This list is meant to be COMPLETE, not illustrative: a partial list of blind spots
# is worse than none, because it reads as a promise of exhaustiveness. Each entry
# below was established by diffing the v2.0.0 and 2.1 public API surfaces
# (public-api/baseline/), not by recollection.
#
#   - literals and exhaustive matches over a type that became `#[non_exhaustive]`
#     in 2.1 (F-245, F-246). A struct literal without a trailing `..`, or a `match`
#     without a wildcard arm, stops compiling at the CONSUMER. Which expressions are
#     affected depends on the TYPE of the value — information a text scan does not
#     have; matching lexically (every line carrying `match` and the type name) would
#     drown the real cases under false positives, so this script does not claim to.
#     EIGHT types are concerned, measured, not the two named in the release notes:
#       gradatum_core::section::Section          gradatum_core::project_map::ProjectMapRoles
#       gradatum_dto::VaultTagsRequest           gradatum_engine::health::HealthSnapshot
#       gradatum_engine::health::TelemetryStatus gradatum_engine::sink::ExchangeError
#       gradatum_index::drift::DriftScanResult   gradatum_index::BackfillReport
#     Add a `_ => …` arm, or a `..` rest pattern, where the compiler flags one.
#
#   - call sites naming a REMOVED associated function by its SHORT name (F-177).
#     This is a limit of the approach, not an oversight: the script rewrites PATHS,
#     and these call sites carry none. A consumer who imported the type writes
#     `use …::JobStatus;` then `JobStatus::as_str(s)` — no path at the call site;
#     and a fully-qualified call is rewritten to its 2.1 path BEFORE the scan runs,
#     so it no longer carries a `gradatum_queue::` prefix either. Scanning before
#     rewriting does not fix this — it would flag every import line the rewrite has
#     just correctly fixed, trading a blind spot for a flood. Removed in 2.1:
#       JobStatus::as_str / JobStatus::from_str — no equivalent on the 2.1
#         `gradatum_core::job::JobStatus`; serialise the variant yourself;
#       SqliteQueue::new / SqliteQueue::in_memory — the whole type is gone (see
#         CLASS A); construct a `GradatumQueue` over `gradatum_jobs` instead.
#     `JobStatus::as_str` is deliberately NOT a token: `gradatum_queue::LegacyJobStatus`
#     SURVIVES in 2.1 and still exposes `as_str`, and the short string is a substring
#     of the long one — the token would fire on correct code. The compiler names these
#     for you; this scan cannot do so without lying.
#
# After running this script you must still:
#   1. add `gradatum-distill = "2.1"` to Cargo.toml if compute_distill_trust was used;
#   2. handle every CLASS A leftover manually, following the guide;
#   2bis. re-read each CLASS B site once — they compile only after the `QueueDb` /
#      `rusqlite::Error` / `Ulid` change; the script cannot tell a done one from a
#      pending one, so it reports them without judging them;
#   3. audit your literals and `match` statements over the eight `#[non_exhaustive]`
#      types listed above, and your call sites of the removed short-name functions —
#      the compiler is the instrument for both, this script is not.
#
# NOTE: this script is a helper, not a substitute for reading the guide. A script
# that claimed to cover everything would be worse than one that is honest.

set -uo pipefail

ORIG="$(pwd -P)"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="."
DRY_RUN=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) grep -v '^#!' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "migrate-2.0-to-2.1.sh: unknown argument '$a' (try --help)" >&2; exit 2 ;;
    *) TARGET="$a" ;;
  esac
done

# A relative target is resolved against the caller's directory, not the repo root.
case "$TARGET" in
  /*) : ;;
  *) TARGET="$ORIG/$TARGET" ;;
esac

[ -d "$TARGET" ] || { echo "migrate-2.0-to-2.1.sh: '$TARGET' is not a directory" >&2; exit 2; }

# ── Collect the source files to scan ─────────────────────────────────────────
# Excludes only the project's own build output (`$TARGET/target/`), never an
# unrelated directory that happens to be named `target`.
mapfile -t FILES < <(find "$TARGET" -type f -name '*.rs' -not -path "$TARGET/target/*" 2>/dev/null | sort)
if [ "${#FILES[@]}" -eq 0 ]; then
  echo "migrate-2.0-to-2.1.sh: no .rs files found under '$TARGET' (excluding $TARGET/target/)" >&2
  exit 2
fi

export TARGET DRY_RUN
python3 - "${FILES[@]}" <<'PY'
import os, re, sys

SUBST = [
    # F-248 — compute_distill_trust moved to the gradatum-distill crate.
    (re.compile(r'\bgradatum_core::provenance::compute_distill_trust\b'),
     'gradatum_distill::compute_distill_trust'),
    # F-177 — legacy sqlx queue types, now in gradatum_core::job.
    (re.compile(r'\bgradatum_queue::queue::JobStatus\b'),
     'gradatum_core::job::JobStatus'),
    (re.compile(r'\bgradatum_queue::queue::QueueError\b'),
     'gradatum_core::job::QueueError'),
    (re.compile(r'\bgradatum_queue::JobStatus\b'),
     'gradatum_core::job::JobStatus'),
    (re.compile(r'\bgradatum_queue::QueueError\b'),
     'gradatum_core::job::QueueError'),
]

# Members of the removed `gradatum_queue::queue` module that have a new home.
MEMBER = {
    'JobStatus': 'gradatum_core::job::JobStatus',
    'QueueError': 'gradatum_core::job::QueueError',
}

# Brace-group import: gradatum_queue::queue::{A, B, C}
BRACE = re.compile(r'gradatum_queue::queue::\{([^}]*)\}')


def rewrite(text):
    # Full-path substitutions first (specific before generic).
    for rx, repl in SUBST:
        text = rx.sub(repl, text)

    # Brace-group imports: rewrite only when every member maps.
    def brace_repl(m):
        members = [x.strip() for x in m.group(1).split(',')]
        members = [x for x in members if x]
        if not members:
            return m.group(0)
        mapped = [MEMBER.get(x) for x in members]
        if any(x is None for x in mapped):
            return m.group(0)  # mixed / unmappable — leave for manual review
        target = 'gradatum_core::job'
        tails = []
        for mem, new in zip(members, mapped):
            prefix = target + '::'
            if new.startswith(prefix):
                tails.append(new[len(prefix):])
            else:
                return m.group(0)
        return '{}::{{{}}}'.format(target, ', '.join(tails))

    return BRACE.sub(brace_repl, text)


def scan_and_rewrite(path, dry_run):
    with open(path, 'r', encoding='utf-8') as fh:
        original = fh.read()
    rewritten = rewrite(original)
    changed = rewritten != original
    if changed and not dry_run:
        with open(path, 'w', encoding='utf-8') as fh:
            fh.write(rewritten)
    # Report what needs a human decision, in TWO classes — every manual case NAMED in
    # the header above, so that `exit 0` provably means "nothing left to do", never
    # "not looked for". The split exists because the two classes answer different
    # questions: class A asks "is there work left?" (it drives the exit code), class B
    # asks "which sites changed shape?" (it does not). Merging them would make `exit 0`
    # unreachable for any consumer that uses the queue, since class B survives
    # migration. The `Section` non-exhaustive match arms (F-246) are in NEITHER: they
    # cannot be told apart from unrelated `match`/`Section` text without type
    # information (see header). Matching is by substring, one class per line, A first.
    # CLASS A — symbols REMOVED in 2.1. Absent from migrated code, so their presence
    # IS work that remains → these, and only these, drive exit code 1.
    REMOVED_TOKENS = (
        # F-177 — legacy queue types with no type-for-type replacement.
        'gradatum_queue::queue', 'gradatum_queue::SqliteQueue',
        'gradatum_queue::NewJob', 'gradatum_queue::JobInfo',
        'gradatum_queue::LeasedJob', 'gradatum_queue::JobId',
        'gradatum_queue::Queue',
        # F-177 — 2.0 variants with no counterpart in the 2.1 enums. The type path
        # is rewritten above, but its variants are NOT: these arms would fail to
        # compile. Qualified `Type::Variant` on purpose — the bare variant names
        # (`Time`, `Leased`, `Sqlx`) are far too common to be discriminating.
        'QueueError::Sqlx', 'QueueError::Time', 'QueueError::CorruptedStatus',
        'JobStatus::Leased', 'JobStatus::Dead',
        # F-220 — removed for good in 2.1.0; the enum is now
        # KindKind::{Feature, Enhancement, Fix, Task}.
        'KindKind::Chore', 'KindKind::Spike',
    )

    # CLASS B — symbols that SURVIVE in 2.1 under the same name, with a changed
    # signature. Correctly migrated code STILL contains them, so they are reported
    # as a review list and MUST NOT touch the exit code: otherwise every consumer
    # who uses the queue is pinned at exit 1 for ever, on code that is right.
    CHANGED_SIGNATURE_TOKENS = (
        # F-145 — now take a `QueueDb` instead of a `SqlitePool`.
        'SqliteQueueStore::new', 'run_migrations', 'apply_sqlite_pragmas',
        'idempotency_',
        # F-145 — variant kept, payload changed (sqlx::Error -> rusqlite::Error).
        'ApiKeyError::Sql', 'RevocationError::Sqlite',
        # F-177 — variant kept, payload changed (JobId/i64 -> Ulid).
        'QueueError::NotLeased',
    )

    # One class per line, class A first: a line that carries removed code is work
    # that remains, whatever else it also carries.
    removed = []
    changed_sig = []
    for i, line in enumerate(rewritten.splitlines(), 1):
        hit = next((t for t in REMOVED_TOKENS if t in line), None)
        if hit is not None:
            removed.append((i, hit))
            continue
        hit = next((t for t in CHANGED_SIGNATURE_TOKENS if t in line), None)
        if hit is not None:
            changed_sig.append((i, hit))
    return changed, removed, changed_sig


dry_run = os.environ.get('DRY_RUN') == '1'
status = 0
total_changed = 0
leftovers = []
reviews = []

for f in sys.argv[1:]:
    changed, removed, changed_sig = scan_and_rewrite(f, dry_run)
    rel = os.path.relpath(f, os.environ['TARGET'])
    if changed:
        total_changed += 1
        if dry_run:
            print('would rewrite: {}'.format(rel))
        else:
            print('rewrote: {}'.format(rel))
    for lineno, token in removed:
        leftovers.append((rel, lineno, token))
    for lineno, token in changed_sig:
        reviews.append((rel, lineno, token))

print('---')
if total_changed == 0 and not dry_run:
    print('no mechanical substitution was needed.')
elif dry_run:
    print('{} file(s) would be rewritten.'.format(total_changed))
else:
    print('{} file(s) rewritten.'.format(total_changed))

if leftovers:
    status = 1
    print('---')
    print('{} occurrence(s) could NOT be rewritten mechanically — manual migration required:'.format(len(leftovers)))
    for path, lineno, token in leftovers:
        print('  {}:{}  ({})'.format(path, lineno, token))
    print('See docs/UPGRADING-2.0.0-to-2.1.0.md — each is a named non-automatable case.')
else:
    print('no leftover usages — every mechanical case is covered.')

# Review list — printed AFTER the verdict, and deliberately outside it. These symbols
# still exist in 2.1; finding them says nothing about whether you are done. Reporting
# them as failures would hold the exit code at 1 for every consumer that uses the
# queue, on correct code — so `status` is not touched here.
if reviews:
    print('---')
    print('{} call site(s) to RE-READ — these symbols survive in 2.1 with a changed'.format(len(reviews)))
    print('signature. This is NOT an error and does NOT affect the exit code ({}):'.format(status))
    for path, lineno, token in reviews:
        print('  {}:{}  ({})'.format(path, lineno, token))
    print('Expect `QueueDb` instead of `SqlitePool`, `rusqlite::Error` instead of')
    print('`sqlx::Error`, and `Ulid` instead of `JobId`. If yours already read that')
    print('way, there is nothing to do — see docs/UPGRADING-2.0.0-to-2.1.0.md.')

sys.exit(status)
PY
PY_RC=$?
exit "$PY_RC"
