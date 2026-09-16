//! The deferred privacy sweep on the server side (F-160 L-D.2).
//!
//! Two halves, and neither of them re-judges anything itself:
//!
//! - [`rescan_tick`] is the product's enqueue site. It runs on the
//!   `TASK_PRIVACY_RESCAN` schedule, and its job is to **enqueue** one
//!   [`Job::PrivacyRescan`](gradatum_core::Job::PrivacyRescan) per vault that holds
//!   work — never to do the work. The vault layer owns no queue handle, so the party
//!   that holds both the registry and the `QueueStore` is the only one that can start
//!   a sweep.
//! - [`rescan_selected_notes`] is what the worker's handler reaches, through the
//!   internal loopback route. It selects by tag and hands every note to
//!   [`Registry::rescan_privacy_note`](gradatum_vault::Registry::rescan_privacy_note),
//!   which is the write primitive — the same funnel every other write uses.
//!
//! ## The tick's four clauses
//!
//! 1. **Perimeter.** Every vault of the registry, minus every `vault_id` beginning
//!    with `code-`. Those hold source code and its companion artefacts, where a
//!    personal-data verdict would flag the material itself; the same lexical test is
//!    Guard 0 of the write path.
//! 2. **The guard is re-applied after selection.** The prefix test above is a
//!    *filter*, deliberately not a substitute: a note can move between the moment it
//!    is selected and the moment it is re-judged, so the pass re-runs Guard 0 on the
//!    note's own resolved namespace and section. Nothing here is trusted to be the
//!    authority — see clause 1 of `gradatum_vault::privacy_rescan`.
//! 3. **An unchanged note is not written.** The re-judgement resolves the verdict on
//!    a clone and writes only if the block or the reserved tags differ; that
//!    comparison lives in the primitive, so the sweep cannot accidentally be the one
//!    path that rewrites an already-judged corpus.
//! 4. **Bounded work.** One tick enqueues at most [`MAX_JOBS_PER_TICK`] jobs, and each
//!    job carries a batch bound ([`RESCAN_BATCH_LIMIT`]) that its execution honours.
//!    Nothing here loops over a corpus.
//!
//! ## The tick is always spawned; the work is what is gated
//!
//! `[privacy].rescan_enabled` gates the *work*, never the spawn. With the rescan
//! disabled, with no category configured, or with the pass-through detector mounted,
//! the tick still runs, still records its run, and selects nothing — and it says so
//! in its [`RescanTickStats`]. That is the difference between a task that is idle and
//! a task that never started, and only the first one is observable from the outside.

use chrono::{DateTime, Utc};
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scheduled_health::TaskOutcome;
use gradatum_core::scope::VaultId;
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, PrivacyRescanSpec, TriggerSource,
};
use gradatum_vault::Registry as _;
use ulid::Ulid;

use crate::state::{AppState, DetectorMount};

/// The tag a note carries when a pass did not complete over it.
///
/// Spelled literally rather than read from `gradatum_vault`'s reserved-tag type: this
/// is a *query value* handed to the index, and the note's `tags` column holds the
/// literal. The refusal of that spelling in `Tag`'s constructors is a rule about what
/// a caller may write, not about what a reader may ask for.
pub const UNSCANNED_TAG: &str = "privacy-unscanned";

/// Notes one execution of a sweep may re-judge.
///
/// The index clamps any page to `[1, 200]`, so a larger value would be silently
/// lowered — and a batch bound that is not the one the query honoured is not a bound.
pub const RESCAN_BATCH_LIMIT: usize = 200;

/// Jobs one tick may enqueue.
///
/// A tick that enqueued one job per vault of a large registry would hand the workers a
/// burst nobody bounded. The cap is deliberately not a rotation: a tick that reaches it
/// logs which vaults it left for the next one, and the next tick re-derives the
/// selection from the index, so no state is carried and nothing is lost.
pub const MAX_JOBS_PER_TICK: usize = 64;

/// What one tick did.
///
/// `#[non_exhaustive]`: further counters may be added without breaking a caller
/// compiled against this shape, and no crate outside this one can fabricate a tick
/// that never ran.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RescanTickStats {
    /// Vaults the registry holds, `code-*` namespaces included.
    pub vaults_considered: usize,
    /// Vaults the tick selected: sweepable, and holding at least one unscanned note.
    pub vaults_selected: usize,
    /// Unscanned notes seen across the selected vaults — the size of the work the
    /// enqueued jobs cover, not the size of what they will re-judge.
    pub notes_seen: usize,
    /// Jobs enqueued by this tick.
    pub jobs_enqueued: usize,
    /// Whether the tick selected nothing because the work is gated off — the rescan
    /// disabled, no category configured, or the pass-through detector mounted.
    ///
    /// Reported rather than inferred: a tick that selected nothing because there was
    /// nothing to select and a tick that selected nothing because it is switched off
    /// are two different operational facts, and this field is what tells them apart.
    pub disabled: bool,
}

/// What one sweep did with the notes it selected.
///
/// `#[non_exhaustive]` for the same reason as [`RescanTickStats`], and no crate outside
/// this one can fabricate a summary the vault never produced.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrivacyRescanSummary {
    /// Notes re-judged to something other than what they carried, and written.
    pub rewritten: usize,
    /// Notes re-judged to exactly what they carried: nothing was written.
    pub unchanged: usize,
    /// Notes no pass ran for — excluded by Guard 0, or no longer there.
    pub skipped: usize,
}

/// Runs one tick of the deferred sweep: select, then enqueue.
///
/// `now_ms` is the tick's wall clock, passed in rather than read here so the run it
/// records and the instant it reports are the same one.
///
/// Enqueues nothing when the work is gated off, and records the run either way:
/// `ok` for a tick that did what it could, `error` for one that could not read a
/// vault's index. A gated-off tick is a **successful** tick — the gate is the
/// configuration, not a failure — which is what makes the task's `run_count` an
/// honest liveness signal.
pub async fn rescan_tick(state: &AppState, now_ms: i64) -> RescanTickStats {
    let start = std::time::Instant::now();
    let mut stats = RescanTickStats::default();
    let vault_ids = state.vaults.vault_ids();
    stats.vaults_considered = vault_ids.len();

    // Clause 1 is a *filter*, and the gate is checked before it: a vault the tick may
    // not sweep is not a vault the tick considered and passed over.
    let mut error: Option<String> = None;
    if !work_is_armed(state) {
        stats.disabled = true;
    } else {
        for vault_id in vault_ids {
            if vault_id
                .as_str()
                .starts_with(crate::internal::CODE_VAULT_PREFIX)
            {
                continue;
            }
            let Some(vault) = state.vaults.get(&vault_id) else {
                error = Some(format!("vault `{}` is not mounted", vault_id.as_str()));
                continue;
            };
            let selected = match vault
                .index()
                .list_notes_by_tag(
                    vault_id.as_str(),
                    None,
                    UNSCANNED_TAG,
                    RESCAN_BATCH_LIMIT,
                    None,
                )
                .await
            {
                Ok((records, _page)) => records,
                Err(e) => {
                    // One unreadable vault does not abort the sweep: the others hold work
                    // too, and a tick that gave up on the first error would leave every
                    // vault behind it permanently unscanned.
                    tracing::warn!(
                        vault = %vault_id.as_str(),
                        error = %e,
                        "privacy-rescan: index read failed for this vault (non fatal)"
                    );
                    error = Some(format!("list_notes_by_tag({}): {e}", vault_id.as_str()));
                    continue;
                }
            };
            if selected.is_empty() {
                continue;
            }
            stats.vaults_selected += 1;
            stats.notes_seen += selected.len();

            if stats.jobs_enqueued >= MAX_JOBS_PER_TICK {
                // The cap is not a rotation: the vaults left over are named, and the next
                // tick re-derives the selection from the index — no cursor is carried.
                tracing::warn!(
                    vault = %vault_id.as_str(),
                    cap = MAX_JOBS_PER_TICK,
                    "privacy-rescan: per-tick job cap reached — this vault is left for the next tick"
                );
                continue;
            }

            let record = build_rescan_job_record(vault_id.as_str(), now_ms);
            match state.job_store.enqueue(record).await {
                Ok(_) => stats.jobs_enqueued += 1,
                Err(e) => {
                    tracing::warn!(
                        vault = %vault_id.as_str(),
                        error = %e,
                        "privacy-rescan: enqueue failed (non fatal)"
                    );
                    error = Some(format!("enqueue({}): {e}", vault_id.as_str()));
                }
            }
        }
    }

    // The run is recorded last, so the row it writes carries the tick's own instant and
    // the duration of the work it actually did.
    let (outcome, err_msg) = match &error {
        None => (TaskOutcome::Ok, None),
        Some(msg) => (TaskOutcome::Error, Some(msg.as_str())),
    };
    if let Err(e) = state
        .search
        .record_task_run(
            crate::scheduled_tasks::TASK_PRIVACY_RESCAN,
            outcome,
            start.elapsed().as_millis() as i64,
            err_msg,
            now_ms,
        )
        .await
    {
        tracing::warn!(error = %e, "record_task_run privacy-rescan failed (non fatal)");
    }

    stats
}

/// Whether the work is switched on: enabled, with a category, under a real detector.
///
/// The three conditions mirror the mount rule exactly — a pass needs a detector to run,
/// and a sweep needs a pass. `detector_mount` is read rather than inferred from the
/// category list so that the gate cannot drift from what is actually mounted on the
/// vaults.
fn work_is_armed(state: &AppState) -> bool {
    state.privacy.rescan_enabled
        && !state.privacy.categories.is_empty()
        && !matches!(state.detector_mount(), DetectorMount::Noop)
}

/// Builds the job one tick enqueues for one vault.
///
/// The row carries a vault name and a batch bound, and nothing else: the selection is
/// derived from the index at execution time, so a replayed job needs no carried state.
fn build_rescan_job_record(vault_id: &str, now_ms: i64) -> JobRecord {
    let at = DateTime::<Utc>::from_timestamp_millis(now_ms).unwrap_or_else(Utc::now);
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::PrivacyRescan(PrivacyRescanSpec::new(vault_id, RESCAN_BATCH_LIMIT)),
            // System: no human or agent asked for this sweep, and `Cron` is the trigger
            // the class matches. `Low`: a sweep must never compete with a live writer.
            class: JobClass::System,
            mode: JobMode::Batch,
            scope: JobScope::Vault(vault_id.to_owned()),
            priority: JobPriority::Low,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Cron,
            scheduled_at: at,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: at,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Re-judges one batch of a vault's unscanned notes through the write primitive.
///
/// `limit` is the batch bound the caller's job carries. The selection is derived from
/// the index at call time, so a replayed job re-selects and the notes already
/// re-judged are simply no longer unscanned.
///
/// # Errors
///
/// - [`GradatumError::Storage`] when the index cannot be read, or when a re-judgement
///   fails. A note the sweep cannot re-judge is reported as such rather than dropped:
///   a batch that silently skipped the failures would let a permanently unscannable
///   corpus look swept.
pub async fn rescan_selected_notes(
    state: &AppState,
    vault_id: &str,
    limit: usize,
) -> Result<PrivacyRescanSummary, GradatumError> {
    // Fail-closed on an unmounted vault: a sweep that fell back to the singleton would
    // re-judge one tenant's notes under another tenant's name.
    let vault = state.vaults.get(&VaultId::new(vault_id)).ok_or_else(|| {
        GradatumError::Storage(format!(
            "privacy rescan: vault `{vault_id}` is not registered"
        ))
    })?;

    // Selection by tag, bounded by the batch bound the caller's job carries. The index
    // clamps the page to `[1, 200]`; `RESCAN_BATCH_LIMIT` is that ceiling, so the bound
    // the query honours is the bound the job states.
    let (records, _page) = vault
        .index()
        .list_notes_by_tag(vault_id, None, UNSCANNED_TAG, limit, None)
        .await?;

    let mut summary = PrivacyRescanSummary::default();
    for record in records {
        // An id the index cannot render as a ULID is a record this sweep has no way to
        // address. It is counted as skipped rather than raised: one unreadable row must
        // not strand the rest of the batch, and the summary still says a note was passed
        // over.
        let Ok(ulid) = Ulid::from_string(&record.id) else {
            summary.skipped += 1;
            continue;
        };
        // The verdict is the primitive's, never this function's: the guard, the pass and
        // the write all live behind `rescan_privacy_note`, which is the same funnel every
        // other write uses.
        match vault.rescan_privacy_note(vault_id, NoteId(ulid)).await? {
            gradatum_vault::PrivacyRescanOutcome::Rewritten => summary.rewritten += 1,
            gradatum_vault::PrivacyRescanOutcome::Unchanged => summary.unchanged += 1,
            // `Skipped`, and any state a later version adds: the sweep's own tally is not
            // the place to fail closed on a vocabulary it does not own — an unknown
            // verdict is one no write was made for.
            _ => summary.skipped += 1,
        }
    }

    Ok(summary)
}
