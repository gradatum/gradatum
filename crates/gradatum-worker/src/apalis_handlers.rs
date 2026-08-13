//! Apalis handlers for the active [`gradatum_core::Job`] variants.
//!
//! Each handler is an async function `fn(GradatumJob) -> Result<JobOutput, HandlerError>`
//! matching the signature expected by `apalis::WorkerBuilder`.
//!
//! # Implemented handlers
//!
//! The `handle_curate`, `handle_embed`, `handle_forget`, and `handle_purge` handlers are
//! fully operational. They rely on the `gradatum-curator`, `gradatum-vault`, and
//! `gradatum-embed` crates via dependencies injected by [`crate::monitor::build_monitor`].
//!
//! | Handler | Job variant | Status |
//! |---|---|---|
//! | `handle_curate` | `Job::Curate(CurateSpec)` | Operational |
//! | `handle_embed` | `Job::Embed(EmbedSpec)` | Operational |
//! | `handle_forget` | `Job::Forget(ForgetSpec)` | Operational |
//! | `handle_purge` | `Job::Purge(PurgeSpec)` | Operational |
//! | `handle_distill` | `Job::Distill(DistillSource)` | Operational (deterministic MVP synthesis) |
//! | `handle_reindex` | `Job::ReIndex(ReIndexMode)` | Deferred (see below) |
//!
//! # DryRunAware
//!
//! Each handler checks `job.record.is_dry_run()` as its FIRST instruction
//! (`JobMode::DryRun` = single mechanism, no side effects).
//!
//! # Dependency injection
//!
//! `build_monitor` injects via `.data()`:
//! - `Data<Arc<dyn InternalClient>>` — **the only read/write path to the vault and index**
//! - `Data<Arc<dyn CuratorProcess + Send + Sync>>` — curator pipeline
//! - `Data<Arc<dyn Embedder + Send + Sync>>` — embedding backend
//! - `Data<Arc<dyn QueueStore + Send + Sync>>` — job queue (enqueue, `mark_conflict`)
//! - `Data<Arc<dyn DistillSynthesizer + Send + Sync>>` — synthesis producer (distill)
//! - `Data<MultiTenantCfg>` — multi-tenant config, for vault resolution
//!
//! The cron workers additionally receive their own data (DLQ pool, retention,
//! distill-cron config, metrics).
//!
//! Neither `Vault` nor `Index` is injected, and no handler holds one: `gradatum-vault` and
//! `gradatum-index` are **dev-dependencies** of this crate, reachable from `tests/` only.
//! Handler documentation that mentions `vault.*` or `index.*` calls is describing what the
//! *server* does behind an `InternalClient` call, not what the handler does.
//!
//! # ReIndex — deferred
//!
//! `handle_reindex` returns an explicit error for all modes: `SqliteIndex::rebuild_fts()`
//! and `get_notes_without_embedding()` are not yet implemented. `VectorsOnly` and
//! `Full` also depend on a vector backend (planned).
//!
//! | Mode | Status |
//! |---|---|
//! | `FtsOnly` | Deferred |
//! | `MissingOnly` | Deferred |
//! | `VectorsOnly` | Deferred (requires vector backend) |
//! | `Full` | Deferred (requires vector backend) |
//!
//! # References
//!
//! - `docs/decisions/ARCH-D15-apalis-embedded.md`

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use smallvec::SmallVec;

use toml::Value as TomlValue;

use gradatum_core::{
    CurateSpec, DryRunAware, EmbedSpec, ForgetScope, GradatumJob, Job, JobClass, JobLifecycle,
    JobLineage, JobMode, JobOutput, JobPriority, JobRecord, JobRetry, JobScheduling, JobScope,
    JobSpec, JobStatus, QueueStore, TriggerSource, ValidateSpec,
    author::AuthorRef,
    frontmatter::{ExtraFields, Frontmatter},
    identity::{ContentHash, NoteId},
    index::AnchorSrc,
    job_kind_str,
    scope::VaultId,
    section::{Section, section_to_doc_kind},
    status::NoteStatus,
    tag::Tag,
};
use gradatum_curator::{CurateOutcome, CuratorProcess};
use gradatum_dto::{
    PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest, PersistForgetRequest,
    TemporalEntryDto,
};
use gradatum_embed::Embedder;

use crate::internal_client::{InternalClient, InternalClientError, NoteIdDto};
use crate::wikilinks::resolve_wikilinks_via_client;

// ─────────────────────────────────────────────────────────────────────────────
// Handler errors
// ─────────────────────────────────────────────────────────────────────────────

/// Error returned by Apalis handlers.
///
/// Conforms to the signature expected by `apalis::WorkerBuilder`.
/// Mapped to `apalis::Error` via `From`.
///
/// `MissingDependency` and `InvalidPayload` are retained for future payload validation.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// Dependency not injected — `build_monitor` must call `.data()` on the worker.
    #[error("dependency absent: {0}")]
    MissingDependency(&'static str),

    /// Received `Job` variant not handled by this handler.
    #[error("unexpected job variant: {0}")]
    UnexpectedVariant(String),

    /// Job payload missing or invalid (title/body absent for vault_write).
    #[error("invalid job payload: {0}")]
    InvalidPayload(String),

    /// Business error propagated from the vault or the curator.
    #[error("business error: {0}")]
    Business(String),
}

/// The worker's multi-vault configuration — a mirror of the `[multi_tenant]` section of
/// `server.toml`. Worker and server read the same file and the same flag, so the setting
/// has a single source of truth.
///
/// Defaults to `enabled = false`, which keeps the strict single-vault path.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct MultiTenantCfg {
    /// Enables per-vault job iteration and the acceptance of tenants other than `"main"`.
    #[serde(default)]
    pub enabled: bool,
}

/// Generalised tenant guard — supersedes [`ensure_main_tenant`] at the call sites.
///
/// - Flag off: strictly [`ensure_main_tenant`], with the same messages and logs.
/// - Flag on: `VaultId::parse`, because the spec comes off the queue and is therefore an
///   untrusted boundary. Any well-formed tenant passes: AUTHORISATION was already decided
///   at enqueue time by the server-side grants, so the worker does not re-decide it, it
///   only validates the shape.
///
/// # Errors
/// `HandlerError::Business` — terminal **by default** (`max_retries = 0`, the serde default of
/// [`crate::monitor`], overridable in configuration) — on a rejected or malformed tenant.
#[must_use = "the tenant guard result must short-circuit the handler"]
fn ensure_job_tenant(tenant_id: &str, multi_tenant_enabled: bool) -> Result<(), HandlerError> {
    if !multi_tenant_enabled {
        return ensure_main_tenant(tenant_id);
    }
    gradatum_core::scope::VaultId::parse(tenant_id)
        .map(|_| ())
        .map_err(|e| HandlerError::Business(format!("invalid job tenant '{tenant_id}': {e}")))
}

/// Truncates a string copied into a **persisted** error message.
///
/// Handler error messages end up in the database (`apalis_backend.rs`,
/// `AckResult::Failure(format!("{e:#}"), _)`). The strings that feed them come from
/// the queue — an untrusted boundary, of unbounded length. The cut is made on a
/// `char` boundary (never in the middle of a UTF-8 sequence).
fn truncate_for_log(s: &str) -> String {
    /// Maximum length, in `char`s, of a fragment copied into a persisted message.
    const MAX_FRAGMENT: usize = 48;
    match s.char_indices().nth(MAX_FRAGMENT) {
        None => s.to_owned(),
        Some((cut, _)) => format!("{}…", &s[..cut]),
    }
}

/// **Bounded** rendering of a `JobScope` for a persisted error message.
///
/// `{scope:?}` is not usable here: `Notes(Vec<Ulid>)` grows with the batch and
/// `Locus(String)` / `Vault(String)` are unbounded. We render a label of bounded
/// size — cardinality for collections, a truncated prefix for strings.
fn scope_label(scope: &gradatum_core::job::JobScope) -> String {
    match scope {
        JobScope::VaultWide => "VaultWide".to_owned(),
        JobScope::Vault(v) => format!("Vault({})", truncate_for_log(v)),
        JobScope::Locus(l) => format!("Locus({})", truncate_for_log(l)),
        JobScope::Notes(ids) => format!("Notes({} ids)", ids.len()),
        JobScope::Session(id) => format!("Session({id})"),
        // `JobScope` est `#[non_exhaustive]` — bras exigé par le compilateur.
        _ => "<unknown variant>".to_owned(),
    }
}

/// Resolves the vault of a job whose spec carries no tenant, using `JobSpec.scope`.
///
/// A scope determines a vault only if it **carries** one: `JobScope::Vault(v)` does,
/// `VaultWide` / `Locus` / `Notes` / `Session` do not — they describe *what* the work
/// covers, never *where* it lives.
///
/// - Flag off: a single vault exists, so a scope carrying no vault resolves to `"main"`.
///   That is *the* answer, not an arbitrary pick. A `Vault(v)` with `v != "main"` is
///   rejected terminally (fail-closed — such a scope can only come from an enqueue made
///   with the flag on). This OFF path is NOT a "let everything through": the `_` arm
///   (a future `JobScope` variant) returns `Err` **at OFF too** — an unknown variant
///   inherits no vault, whatever the state of the flag.
/// - Flag on: `Vault(v)` is validated by `VaultId::parse`. Every other scope is rejected
///   terminally. Falling back to `"main"` would silently elect one vault out of N while
///   the returned `vault_id` scopes destructive access (`delete_note` in `handle_purge`,
///   `persist_forget` in `handle_forget`). The enqueue site must carry `JobScope::Vault(v)`.
///
/// # Errors
/// `HandlerError::Business` — terminal **by default** (`max_retries = 0`, the serde default of
/// [`crate::monitor`]; the value is overridable in configuration, so the absence of a
/// retry is a property of the config, not of the error type) — on a malformed vault, on a
/// non-`main` vault while the flag is off, or on a scope carrying no vault while the
/// flag is on. Retrying cannot make an absent vault appear, so the job must fail loudly.
#[must_use = "the resolved job vault must scope every index/vault access"]
fn resolve_job_vault(
    scope: &gradatum_core::job::JobScope,
    multi_tenant_enabled: bool,
) -> Result<String, HandlerError> {
    match scope {
        JobScope::Vault(v) => {
            if !multi_tenant_enabled {
                if v == "main" {
                    return Ok("main".to_owned());
                }
                return Err(HandlerError::Business(format!(
                    "unsupported vault scope (mono-vault): '{v}' ≠ 'main'"
                )));
            }
            VaultId::parse(v)
                .map(|vid| vid.as_str().to_owned())
                .map_err(|e| HandlerError::Business(format!("invalid job vault '{v}': {e}")))
        }
        // Scopes portant un périmètre mais AUCUN vault. À OFF ils désignent le vault
        // unique ; à ON ils sont ambigus → refus terminal, jamais un « main » silencieux.
        JobScope::VaultWide | JobScope::Locus(_) | JobScope::Notes(_) | JobScope::Session(_) => {
            if multi_tenant_enabled {
                Err(HandlerError::Business(format!(
                    "ambiguous job vault: scope {} carries no vault while multi-vault is \
                     enabled — the enqueue site must carry JobScope::Vault(v)",
                    scope_label(scope)
                )))
            } else {
                Ok("main".to_owned())
            }
        }
        // `JobScope` est `#[non_exhaustive]` et vit dans `gradatum-core` : depuis cette
        // crate le compilateur EXIGE ce bras, il ne peut pas être supprimé. Fail-closed —
        // une variante future n'hérite d'aucun vault par défaut.
        _ => Err(HandlerError::Business(format!(
            "unhandled job scope {}: no determinable vault",
            scope_label(scope)
        ))),
    }
}

/// The **"one job = exactly one vault"** invariant, applied to `Job::Forget`.
///
/// `handle_forget` used to have TWO sources of vault truth: `ForgetScope.vault*` drove
/// the **listing**, `resolve_job_vault(JobSpec.scope)` drove the **mutation**
/// (`persist_forget`). Nothing tied them together: a job could list in one vault and
/// mutate in another.
///
/// Operator ruling (2026-07-27): **a job carries exactly one vault**, and that vault is
/// given to it by its scope (the job runs under a system profile with access to all the
/// vaults of its tenant; it does not derive a vault from a calling credential). The
/// single source is therefore `vault_id`, taken from `JobSpec.scope` — the **only** one
/// of the two that scopes destructive access.
///
/// `ForgetScope.vault*` is demoted to the rank of **consistency assertion**: it no longer
/// elects anything, it must merely agree. Disagreement is refused terminally rather than
/// arbitrated silently — arbitrating means picking a vault at random between two
/// contradictory intents, on a destructive path.
///
/// A multi-vault `Agent { vaults }` is refused here: the **fan-out** (N vaults ⇒ N jobs,
/// one per vault) is the responsibility of the enqueue site, not of the handler. A handler
/// that "fanned out" at execution time would drop a single job onto N vaults, which the
/// invariant forbids.
///
/// The guard is **unconditional** (it is not gated on `multi_tenant`): at OFF
/// `vault_id` is always `"main"`, so only an already inconsistent `ForgetScope` (listing
/// outside the mutated vault) can trigger it. It does not turn a healthy case red, it
/// makes loud a case that was silently wrong.
///
/// # Errors
/// `HandlerError::Business` — terminal, never retried: a contradictory scope does not
/// become consistent on retry.
#[must_use = "the forget scope consistency verdict must short-circuit the handler"]
fn ensure_forget_scope_vault(scope: &ForgetScope, vault_id: &str) -> Result<(), HandlerError> {
    let divergent = |field: &str, found: &str| {
        HandlerError::Business(format!(
            "forget: scope vault mismatch — ForgetScope::{field} carries '{}' while the job \
             is scoped on vault '{vault_id}'. A job targets exactly one vault; the enqueue site \
             must emit one job per vault (JobScope::Vault(v) + ForgetScope on the same v).",
            truncate_for_log(found)
        ))
    };
    match scope {
        // `None` ne contredit rien : le vault reste celui du job.
        ForgetScope::Topic { vault: None, .. } => Ok(()),
        ForgetScope::Topic { vault: Some(v), .. } => {
            if v == vault_id {
                Ok(())
            } else {
                Err(divergent("Topic.vault", v))
            }
        }
        ForgetScope::Locus { vault, .. } => {
            if vault == vault_id {
                Ok(())
            } else {
                Err(divergent("Locus.vault", vault))
            }
        }
        // Vide = « aucune contrainte », singleton = accord exigé, N > 1 = fan-out non fait.
        ForgetScope::Agent { vaults, .. } => match vaults.as_slice() {
            [] => Ok(()),
            [only] if only == vault_id => Ok(()),
            [only] => Err(divergent("Agent.vaults", only)),
            many => Err(HandlerError::Business(format!(
                "forget: multi-vault ForgetScope::Agent ({} vaults) on a job scoped '{vault_id}' — \
                 a job targets exactly one vault. The enqueue site must fan out one job per vault.",
                many.len()
            ))),
        },
        // `ForgetScope` est `#[non_exhaustive]` : fail-closed, une variante future
        // n'hérite d'aucun accord implicite avec le vault du job.
        _ => Err(HandlerError::Business(
            "forget: unsupported ForgetScope variant for vault reconciliation — a future \
             variant inherits no agreement with the job vault (fail-closed)"
                .to_owned(),
        )),
    }
}

/// Single-vault tenant guard — cross-tenant isolation on the flag-off path.
///
/// The worker is a separate process, NOT covered by the HTTP middleware. While the
/// deployment is mono-tenant (`"main"`), a `JobSpec` carrying a `tenant_id` other than
/// `"main"` must be rejected terminally rather than retried forever
/// (`HandlerError::Business` is not retried on the business side). Restrictive-only.
///
/// # Errors
/// Returns `HandlerError::Business` if `tenant_id != "main"`.
#[must_use = "the tenant guard result must short-circuit the handler"]
fn ensure_main_tenant(tenant_id: &str) -> Result<(), HandlerError> {
    if tenant_id != "main" {
        tracing::warn!(
            tenant_id = %tenant_id,
            "worker: job rejected — tenant ≠ main (mono-vault invariant, P0 cross-tenant)"
        );
        return Err(HandlerError::Business(format!(
            "unsupported tenant (mono-vault): '{tenant_id}' ≠ 'main'"
        )));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Curate
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Curate`] — `inbox/` classification.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Curate(CurateSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "curate")` without any write.
/// - In `Batch` mode: calls `CuratorPipeline::process()` and persists to the vault.
///
/// # Two use cases
///
/// 1. **vault_write (new note)**: `CurateSpec.title` + `.body` are `Some` —
///    the note is created through the internal persist API (`POST /internal/v1/persist/curated`),
///    honoring the pre-allocated ULID (`spec.note_id`). `spec.expected_sha256` is
///    forwarded to that API and **is honoured**; it also discriminates the two modes:
///    - `None` → CREATE (fresh ULID), unconditional write;
///    - `Some` → RMW update under an **optimistic lock** (compare-and-swap on the hash).
///
///    On a mismatch the server answers `InternalClientError::Conflict`; the handler then
///    calls `queue.mark_conflict(...)` — moving the job to the **`Conflict`** status — and
///    returns `Ok(JobOutput)` with a `result_note_md` describing the conflict and the
///    current `sha256`. It does **not** return `Err`, so a conflict is visible on the job
///    status, not through the handler's `Result`.
/// 2. **reclassification**: `title`/`body` are `None` —
///    the note already exists; it is read from the vault via `note_id` and updated via
///    `write_note_with_id` to **preserve the ULID** (spec.note_id == stored ULID).
///    Critical invariant: `write_note` must NOT be used here (it generates `NoteId::new()` →
///    divergent ULID → invalid 202 note_id → dead wikilinks).
///
/// # Title persistence
///
/// The handler does **not** call `index.upsert_note_title()` — it holds no index. The
/// resolved title (`spec.title` for vault_write, `extract_h1_title(body)` for
/// reclassification) is sent in the persist request, and the **server** performs
/// `upsert_note_title` (and `write_temporal_entry`) inside `persist_curated`.
///
/// # Side effects
///
/// - Writes the note to the vault: `Admitted` → `live`, `Pending` → `pending-review`
///   (`outcome_to_status` maps `Pending` to `NoteStatus::PendingReview`; it never yields
///   `Staging`, despite the wording of the log line on that branch).
/// - Persists `[[...]]` wikilinks via `SqliteIndex` (non-fatal).
/// - Enqueues a `Job::Embed` if the note is admitted or pending (non-fatal, best-effort).
///
/// # Timeout
///
/// Per-job timeout is enforced by the Apalis Tower layer (see `monitor.rs`
/// `cfg.timeout_secs`, default 30 s for curate). This handler adds no redundant
/// timeout — the Tower layer is outer and takes effect first.
pub async fn handle_curate(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    curator: Data<Arc<dyn CuratorProcess + Send + Sync>>,
    queue: Data<Arc<dyn QueueStore + Send + Sync>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "curate — simulation"));
    }
    // Extract the CurateSpec
    let spec = match &job.record.spec.kind {
        Job::Curate(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Cross-tenant guard: terminally reject if tenant ≠ main (worker outside HTTP middleware).
    ensure_job_tenant(&spec.tenant_id, mt.enabled)?;

    // Build the CuratorNote from the spec.
    // vault_write path: title + body present in the spec.
    // Reclassification path: title/body None → read from the vault.
    let (note_id_for_vault, curator_note, existing_dto_opt) =
        if spec.title.is_some() && spec.body.is_some() {
            // vault_write path: note to create
            let curator_note = gradatum_curator::Note {
                id: spec.note_id.to_string(),
                title: spec.title.clone().unwrap_or_default(),
                body: spec.body.clone().unwrap_or_default(),
                tags_hint: spec.tags.clone(),
                section_hint: spec.section_hint.clone(),
            };
            (None, curator_note, None) // create path : write_note_with_id(spec.note_id) honore l'ULID préalloué
        } else {
            // Reclassification path: read the existing note via InternalClient
            let note_id = NoteId(spec.note_id);
            let id_str = note_id.to_string();
            // Lecture scopée sur le tenant DÉJÀ validé par `ensure_job_tenant` — le même
            // que celui porté par le `persist_curated` en aval. Une lecture non scopée
            // résoudrait sur `main` et re-classifierait l'homonyme du mauvais vault.
            let existing_dto = client
                .get_note(&spec.tenant_id, &id_str)
                .await
                .map_err(|e| HandlerError::Business(format!("read_note: {e}")))?;

            // Guard P1-A (audit reviewer v0.7.3, sécu A2/C6) : section=identity → no-op.
            //
            // Une âme est gérée exclusivement via vault_write + injection MCP (F-34 v0.7.3).
            // La laisser passer dans le curator risque de :
            //   1. Re-sectionner la note hors `identity` (changement ACL, perte protection).
            //   2. Clobber le title canonique `identity/<agent>` via extract_h1_title.
            //
            // Retourner un JobOutput vide (no-op) préserve section + title intacts.
            if existing_dto.section == "identity" {
                tracing::info!(
                    job_id = %job.record.id,
                    note_id = %spec.note_id,
                    "curate: identity note — reclassification skipped (no-op F-34 v0.7.3)"
                );
                return Ok(JobOutput {
                    notes_created: vec![],
                    notes_modified: vec![],
                    files: vec![],
                    result_note_md: format!(
                        "curate: identity note {} — no-op reclassification \
                         (protected section F-34 v0.7.3)",
                        spec.note_id
                    ),
                });
            }

            let title_for_curator = gradatum_curator::extract_h1_title(&existing_dto.body)
                .unwrap_or_else(|| existing_dto.section.clone());
            let curator_note = gradatum_curator::Note {
                id: spec.note_id.to_string(),
                title: title_for_curator,
                body: existing_dto.body.clone(),
                tags_hint: existing_dto.tags.clone(),
                section_hint: None,
            };
            (Some(note_id), curator_note, Some(existing_dto))
        };

    let tenant_id = spec.tenant_id.clone();
    let body_for_write = spec
        .body
        .clone()
        .unwrap_or_else(|| curator_note.body.clone());
    // Resolved title captured before `curator.process` consumes `curator_note`.
    // Used after the match for `upsert_note_title` — populates the near-empty `notes.title` column.
    let title_resolved = curator_note.title.clone();

    let (curate_outcome, curation_path) = curator.process_traced(curator_note).await;

    // Status resolved via the single canonical mapping (worker SSOT parity).
    // Admitted → Live, Pending → PendingReview, Rejected → None (no write).
    let write_status = gradatum_curator::outcome_to_status(&curate_outcome);

    // F-66 instrumentation: decision path + outcome forwarded to the server, which
    // owns the Prometheus registry (:19091). The server increments
    // `gradatum_curator_decisions{path, outcome}` once per persisted note.
    // Rejected outcomes are not persisted (no round-trip) — a dormant path in LIVE.
    let curator_decision = gradatum_dto::CuratorDecisionDto {
        path: curation_path.as_str().to_string(),
        outcome: gradatum_curator::outcome_label(&curate_outcome).to_string(),
    };

    let written_note_id = match curate_outcome {
        CurateOutcome::Admitted { ref decisions } => {
            let _section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            // Build the PersistCuratedRequest for both paths
            let (
                curate_title,
                curate_body,
                curate_section_str,
                curate_tags,
                curate_author,
                curate_status_str,
                curate_trust,
                curate_expected_sha256,
                curate_provenance,
            ) = if let Some(_existing_note_id) = note_id_for_vault {
                // Reclassification: use existing body/tags from the pre-read DTO
                let dto = existing_dto_opt
                    .as_ref()
                    .expect("reclass path always has existing_dto");
                let reclass_title = gradatum_curator::extract_h1_title(&dto.body)
                    .unwrap_or_else(|| decisions.canonical_section.clone());
                let mut merged_tags = dto.tags.clone();
                for tag_str in &decisions.tags {
                    if !merged_tags.contains(tag_str) {
                        merged_tags.push(tag_str.clone());
                    }
                }
                (
                    reclass_title,
                    dto.body.clone(),
                    decisions.canonical_section.clone(),
                    merged_tags,
                    None::<String>,
                    "live".to_string(),
                    None::<f32>,
                    None::<String>,
                    None::<String>,
                )
            } else {
                // vault_write path
                let status = write_status.expect("Admitted → Some(Live) by outcome_to_status");
                let status_str = status_to_str(status);
                let author = spec.author.clone();
                let provenance = Some(
                    gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref())
                        .to_string(),
                );
                let expected_sha256 = spec.expected_sha256.map(|h| ContentHash(h).hex());
                let mut all_tags = spec.tags.clone();
                for t in &decisions.tags {
                    if !all_tags.contains(t) {
                        all_tags.push(t.clone());
                    }
                }
                (
                    title_resolved.clone(),
                    body_for_write.clone(),
                    decisions.canonical_section.clone(),
                    all_tags,
                    author,
                    status_str,
                    None::<f32>,
                    expected_sha256,
                    provenance,
                )
            };
            // Ancre temporelle — correctif C-1 (P1, 2026-06-29).
            // `doc_kind` est partagé entre les deux branches (factorisé hors if/else).
            let curate_doc_kind = section_to_doc_kind(
                &section_from_str(&curate_section_str).unwrap_or(Section::Reference),
            )
            .to_string();
            // `Option<(anchor_ms, anchor_src)>` :
            //   Some(_) → écriture TemporalEntryDto dans persist_curated.
            //   None    → court-circuit : INSERT OR REPLACE non exécuté → ancre existante préservée.
            let curate_temporal_opt: Option<(i64, AnchorSrc)> =
                if let Some(_existing_note_id) = note_id_for_vault {
                    // Reclassification/RMW (C-1) : honorer occurred_at si fourni (symétrie CREATE).
                    // Sans occurred_at : court-circuit (temporal: None) → zéro clobber de l'ancre
                    // existante dans temporal_index (INSERT OR REPLACE non déclenché).
                    // ECON: lecture préalable temporal_index évitée (+1 DB read inutile ici).
                    // Une note sans entrée préalable et sans occurred_at reste sans entrée
                    // (temporal_index optionnel). Upgrade: lire l'entrée existante pour fallback
                    // Created précis si ce besoin devient réel.
                    spec.occurred_at.as_ref().map(|occ| {
                        let mut extra_for_anchor = ExtraFields::empty();
                        extra_for_anchor
                            .insert("occurred_at".to_string(), toml::Value::String(occ.clone()));
                        let created_ms = Utc::now().timestamp_millis();
                        resolve_temporal_anchor(&extra_for_anchor, created_ms)
                    })
                } else {
                    // vault_write path (CREATE + RMW update in-place) — correctif C-1 (P1, REV.2).
                    //
                    // Discriminateur CREATE vs RMW : `curate_expected_sha256` (hérite de
                    // spec.expected_sha256 déjà consommé ci-dessus par ContentHash).
                    //   None = CREATE (ULID neuf, pas de lock optimiste)
                    //   Some = RMW UPDATE (lock optimiste, API rejette sans sha)
                    //
                    // Contrat (spec design 2026-06-29-c1-temporal-anchor-preservation-design.md) :
                    //   1. occurred_at fourni  → resolve_temporal_anchor (SSOT). Vaut CREATE et RMW.
                    //   2. occurred_at absent + RMW (sha Some) → court-circuit (temporal: None).
                    //      L'ancre existante dans temporal_index est préservée ; INSERT OR REPLACE
                    //      non déclenché. ECON: discriminateur sha évite toute lecture temporal_index.
                    //   3. occurred_at absent + CREATE (sha None) → (now(), Created). Comportement
                    //      historique préservé pour la création neuve.
                    if let Some(occ) = &spec.occurred_at {
                        // Cas 1 : occurred_at fourni — ancre événementielle.
                        // Valeur TOML::String (Datetime INTERDIT — frontmatter.rs:63).
                        let mut extra_for_anchor = ExtraFields::empty();
                        extra_for_anchor
                            .insert("occurred_at".to_string(), toml::Value::String(occ.clone()));
                        let created_ms = Utc::now().timestamp_millis();
                        Some(resolve_temporal_anchor(&extra_for_anchor, created_ms))
                    } else if curate_expected_sha256.is_some() {
                        // Cas 2 : RMW sans occurred_at → court-circuit (C-1 fix).
                        // temporal: None → persist_curated ne déclenche pas write_temporal_entry.
                        None
                    } else {
                        // Cas 3 : CREATE sans occurred_at → (now(), Created).
                        let created_ms = Utc::now().timestamp_millis();
                        Some(resolve_temporal_anchor(&ExtraFields::empty(), created_ms))
                    }
                };
            let note_id_str = spec.note_id.to_string();
            // B5 wikilinks — résolution parallèle AVANT persist_curated (non-fatale).
            // Les liens résolus sont passés dans persist_req.links pour que le serveur
            // exécute upsert_link atomiquement dans persist_curated.
            let resolved =
                resolve_wikilinks_via_client(&client, &tenant_id, &note_id_str, &curate_body).await;
            let mut persist_req = PersistCuratedRequest::new(
                note_id_str.clone(),
                tenant_id.clone().into(),
                curate_title,
                curate_body,
                curate_section_str,
                curate_status_str,
            );
            persist_req.tags = curate_tags;
            persist_req.author = curate_author;
            persist_req.trust = curate_trust;
            persist_req.expected_sha256 = curate_expected_sha256;
            persist_req.temporal =
                curate_temporal_opt.map(|(anchor_ms, anchor_src)| TemporalEntryDto {
                    anchor_ms,
                    anchor_src: anchor_src.as_db_str().to_string(),
                    doc_kind: curate_doc_kind,
                    valid_until_ms: None,
                });
            persist_req.links = resolved.links;
            // F-147 : liens recalculés depuis le corps → autoritatifs SSI la résolution
            // fut complète (aucun lookup transitoire en échec).
            persist_req.links_authoritative = resolved.complete;
            persist_req.provenance = curate_provenance;
            persist_req.curator_decision = Some(curator_decision.clone());
            // target_vault reste None (défaut du constructeur).
            match client.persist_curated(&persist_req).await {
                Ok(_ok) => {}
                Err(InternalClientError::Conflict { current_sha256_hex }) => {
                    // Optimistic-lock conflict — only possible on vault_write path with expected_sha256
                    let job_id = job.record.id;
                    let duration_ms: u32 = job
                        .record
                        .lifecycle
                        .started_at
                        .map(|s| {
                            (Utc::now() - s)
                                .num_milliseconds()
                                .max(0)
                                .min(i64::from(u32::MAX)) as u32
                        })
                        .unwrap_or(0);
                    // Contrat gelé WriteConflictDto (gradatum-dto) : `current_sha256` = hash
                    // gagnant récupéré du corps 409, `attempted_sha256` = hex périmé envoyé par
                    // l'appelant (toujours Some sur le chemin RMW). Zéro `note_id` (hors contrat).
                    let conflict_payload_str =
                        serde_json::to_string(&gradatum_dto::WriteConflictDto {
                            current_sha256: current_sha256_hex.clone().unwrap_or_default(),
                            attempted_sha256: persist_req.expected_sha256.clone(),
                            timestamp_ms: Utc::now().timestamp_millis(),
                        })
                        .unwrap_or_else(|_| "{}".to_string());
                    if let Err(e) = queue
                        .mark_conflict(job_id, conflict_payload_str, duration_ms)
                        .await
                    {
                        tracing::error!(
                            job_id = %job_id,
                            error = %e,
                            "curate: mark_conflict failed — job will stay in current state"
                        );
                    }
                    let sha_suffix = current_sha256_hex
                        .as_deref()
                        .map(|sha| format!(" — current_sha256: {sha}"))
                        .unwrap_or_default();
                    return Ok(JobOutput {
                        notes_created: vec![],
                        notes_modified: vec![],
                        files: vec![],
                        result_note_md: format!(
                            "curate: optimistic-lock conflict on note {} (Admitted){sha_suffix}",
                            spec.note_id
                        ),
                    });
                }
                Err(e) => {
                    return Err(HandlerError::Business(format!(
                        "curate: persist_curated Admitted: {e}"
                    )));
                }
            }

            tracing::info!(
                job_id = %job.record.id,
                section = %decisions.canonical_section,
                "curate: note admitted and persisted"
            );
            let written_id = NoteId(spec.note_id);
            Some(written_id)
        }
        CurateOutcome::Pending {
            ref decisions,
            ref reason,
        } => {
            let _section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            // Pending path — same structure as Admitted but with PendingReview status
            let status = write_status.expect("Pending → Some(PendingReview) by outcome_to_status");
            let pending_status_str = status_to_str(status);
            let (
                pend_title,
                pend_body,
                pend_section_str,
                pend_tags,
                pend_author,
                pend_expected_sha256,
                pend_provenance,
            ) = if let Some(_existing_note_id) = note_id_for_vault {
                let dto = existing_dto_opt
                    .as_ref()
                    .expect("reclass path always has existing_dto");
                let reclass_title = gradatum_curator::extract_h1_title(&dto.body)
                    .unwrap_or_else(|| decisions.canonical_section.clone());
                let mut merged_tags = dto.tags.clone();
                for tag_str in &decisions.tags {
                    if !merged_tags.contains(tag_str) {
                        merged_tags.push(tag_str.clone());
                    }
                }
                (
                    reclass_title,
                    dto.body.clone(),
                    decisions.canonical_section.clone(),
                    merged_tags,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                )
            } else {
                let author = spec.author.clone();
                let provenance = Some(
                    gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref())
                        .to_string(),
                );
                let expected_sha256 = spec.expected_sha256.map(|h| ContentHash(h).hex());
                let mut all_tags = spec.tags.clone();
                for t in &decisions.tags {
                    if !all_tags.contains(t) {
                        all_tags.push(t.clone());
                    }
                }
                (
                    title_resolved.clone(),
                    body_for_write.clone(),
                    decisions.canonical_section.clone(),
                    all_tags,
                    author,
                    expected_sha256,
                    provenance,
                )
            };
            let note_id_str_pending = spec.note_id.to_string();
            // B5 wikilinks — résolution parallèle AVANT persist_curated (non-fatale).
            // Parité Admitted/Pending : les deux branches renseignent persist_req.links.
            let resolved_pending =
                resolve_wikilinks_via_client(&client, &tenant_id, &note_id_str_pending, &pend_body)
                    .await;
            let mut persist_req_pending = PersistCuratedRequest::new(
                note_id_str_pending.clone(),
                tenant_id.clone().into(),
                pend_title,
                pend_body,
                pend_section_str,
                pending_status_str,
            );
            persist_req_pending.tags = pend_tags;
            persist_req_pending.author = pend_author;
            persist_req_pending.expected_sha256 = pend_expected_sha256;
            persist_req_pending.links = resolved_pending.links;
            // F-147 : parité Admitted — autoritatif SSI résolution complète.
            persist_req_pending.links_authoritative = resolved_pending.complete;
            persist_req_pending.provenance = pend_provenance;
            persist_req_pending.curator_decision = Some(curator_decision.clone());
            // trust, temporal, target_vault restent aux défauts du constructeur.
            match client.persist_curated(&persist_req_pending).await {
                Ok(_ok) => {}
                Err(InternalClientError::Conflict { current_sha256_hex }) => {
                    let job_id = job.record.id;
                    let duration_ms: u32 = job
                        .record
                        .lifecycle
                        .started_at
                        .map(|s| {
                            (Utc::now() - s)
                                .num_milliseconds()
                                .max(0)
                                .min(i64::from(u32::MAX)) as u32
                        })
                        .unwrap_or(0);
                    // Contrat gelé WriteConflictDto (parité avec le bras Admitted).
                    let conflict_payload_str =
                        serde_json::to_string(&gradatum_dto::WriteConflictDto {
                            current_sha256: current_sha256_hex.clone().unwrap_or_default(),
                            attempted_sha256: persist_req_pending.expected_sha256.clone(),
                            timestamp_ms: Utc::now().timestamp_millis(),
                        })
                        .unwrap_or_else(|_| "{}".to_string());
                    if let Err(e) = queue
                        .mark_conflict(job_id, conflict_payload_str, duration_ms)
                        .await
                    {
                        tracing::error!(
                            job_id = %job_id,
                            error = %e,
                            "curate: mark_conflict failed (pending) — job will stay in current state"
                        );
                    }
                    let sha_suffix = current_sha256_hex
                        .as_deref()
                        .map(|sha| format!(" — current_sha256: {sha}"))
                        .unwrap_or_default();
                    return Ok(JobOutput {
                        notes_created: vec![],
                        notes_modified: vec![],
                        files: vec![],
                        result_note_md: format!(
                            "curate: optimistic-lock conflict (pending) on note {}{sha_suffix}",
                            spec.note_id
                        ),
                    });
                }
                Err(e) => {
                    return Err(HandlerError::Business(format!(
                        "curate: persist_curated Pending: {e}"
                    )));
                }
            }

            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note moved to Staging (manual review required)"
            );
            Some(NoteId(spec.note_id))
        }
        CurateOutcome::Rejected { ref reason } => {
            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note rejected — no vault write"
            );
            None
        }
    };

    // ── curate→embed chaining — best-effort non-fatal ────────────────────────
    // upsert_note_title + write_temporal_entry handled server-side in persist_curated.
    // B5 wikilinks renseignés dans persist_req.links (avant persist_curated) — pas de pass post-curate.
    if let Some(note_id) = &written_note_id {
        // ── curate→embed chaining — best-effort non-fatal ─────────────────────
        // Enqueues a Job::Embed for the curated note so embeddings are
        // generated in cascade after curation.
        //
        // Storage idempotence only: a double-curate re-enqueues an Embed;
        // handle_embed recomputes the embedding then INSERT OR REPLACE into
        // note_embeddings — no corruption, but non-zero compute cost.
        // Compute skip (force_regenerate=false + vector present → no-op) is deferred.
        //
        // Transient: direct enqueue (await_jobs=[]) because the cascade engine
        // (await_jobs/Cascade, gradatum_queue::find_awaiting/set_pending) is todo!()
        // in gradatum_queue.rs. A non-empty await_jobs would leave the embed in Waiting.
        // Migration to await_jobs=[JobTrigger{curate_id, OnDone}] + TriggerSource::Cascade
        // is planned when the cascade engine is implemented.
        let embed_record = build_embed_job_record(*note_id, &tenant_id, job.record.id);
        if let Err(e) = queue.enqueue(embed_record).await {
            tracing::warn!(
                note_id = %note_id,
                error = %e,
                "curate: Job::Embed enqueue failed — note curated, embed not scheduled (best-effort)"
            );
        }
    }

    let result_desc = written_note_id
        .map(|id| format!("note {} created/updated", id))
        .unwrap_or_else(|| "rejected".to_string());

    Ok(JobOutput {
        notes_created: written_note_id.map(|nid| vec![nid.0]).unwrap_or_default(),
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!("curate: {result_desc}"),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Embed
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Embed`] — embedding generation.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Embed(EmbedSpec { note_id, ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "embed")` without computation.
/// - In `Batch` mode: reads the note from the vault, computes the embedding, persists it in the index.
///
/// # Silent skip
///
/// Only skip case: empty `body_text` → returns `JobOutput` without calling the embedder.
/// If a vector already exists for this note, it is recomputed and overwritten via
/// `INSERT OR REPLACE` (storage idempotence, not compute idempotence).
/// Compute skip (`force_regenerate=false` + vector present → no-op) is not yet implemented.
pub async fn handle_embed(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "embed — simulation"));
    }

    let spec = match &job.record.spec.kind {
        Job::Embed(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Cross-tenant guard: terminally reject if tenant ≠ main (defense-in-depth).
    ensure_job_tenant(&spec.tenant_id, mt.enabled)?;

    let note_id = NoteId(spec.note_id);
    let id_str = note_id.to_string();

    // Read the note via InternalClient to obtain the body.
    // Lecture scopée sur le tenant validé juste au-dessus — même vault que le
    // `persist_embedding` en aval.
    let note_dto = client
        .get_note(&spec.tenant_id, &id_str)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: read_note: {e}")))?;

    let body_text = note_dto.body.as_str();
    if body_text.is_empty() {
        tracing::info!(
            job_id = %job.record.id,
            note_id = %spec.note_id,
            "embed: skip — empty body"
        );
        return Ok(JobOutput {
            notes_created: vec![],
            notes_modified: vec![],
            files: vec![],
            result_note_md: format!("embed: skip note {} — empty body", spec.note_id),
        });
    }

    // Truncate to 2 048 Unicode characters (UTF-8-safe via char_indices).
    // Prevents model context overflow without arbitrary byte slicing.
    let truncated = if body_text.len() > 8192 {
        let end = body_text
            .char_indices()
            .nth(2048)
            .map(|(i, _)| i)
            .unwrap_or(body_text.len());
        &body_text[..end]
    } else {
        body_text
    };

    let vec = embedder
        .embed(truncated)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: embedder: {e}")))?;

    let mut embedding_req = PersistEmbeddingRequest::new(
        id_str.clone(),
        embedder.embedder_id().to_string(),
        embedder.dim(),
        vec,
    );
    // C4-1e Slice B3 (MIGRATE) : le worker émet le vault réel du job.
    // OFF → spec.tenant_id == "main" (garde ensure_job_tenant l.791) = byte-identical.
    embedding_req.vault_id = Some(spec.tenant_id.clone().into());

    client
        .persist_embedding(&embedding_req)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: persist_embedding: {e}")))?;

    tracing::info!(
        job_id = %job.record.id,
        note_id = %spec.note_id,
        embedder_id = embedder.embedder_id(),
        dim = embedder.dim(),
        "embed: done"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: vec![note_id.0],
        files: vec![],
        result_note_md: format!(
            "embed: note {} vector dim={} persisted",
            spec.note_id,
            embedder.dim()
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::ReIndex
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::ReIndex`] — full reindex.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::ReIndex(ReIndexMode { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "reindex")` without any write.
/// - For all other modes: returns `Err(HandlerError::Business)` (deferred).
///
/// # Status
///
/// All modes are deferred. `SqliteIndex::rebuild_fts()` and
/// `get_notes_without_embedding()` are not yet implemented.
///
/// | Mode | Status |
/// |---|---|
/// | `FtsOnly` | Deferred |
/// | `MissingOnly` | Deferred |
/// | `VectorsOnly` | Deferred (requires vector backend) |
/// | `Full` | Deferred (requires vector backend) |
///
/// # `temporal_index` reconstruction
///
/// When the `Full` mode is implemented, `temporal_index` reconstruction MUST be
/// included via `SqliteIndex::backfill_temporal_index()` — a derived table that must
/// remain consistent with `notes` after a full reindex.
/// The initial migration backfill combined with per-curate `write_temporal_entry` calls
/// keeps the table current incrementally until `Full` is implemented.
pub async fn handle_reindex(
    job: GradatumJob,
    // Parameters reserved for the future reindex implementation (v0.5.3+).
    _client: Data<Arc<dyn InternalClient>>,
    _embedder: Data<Arc<dyn Embedder + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "reindex — simulation"));
    }

    let mode = match &job.record.spec.kind {
        Job::ReIndex(mode) => mode.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // All modes deferred: SqliteIndex::rebuild_fts() and get_notes_without_embedding()
    // are not yet implemented. Returns an explicit error (not a silent Ok) so the job
    // is marked as failed in the queue rather than appearing successful.
    tracing::warn!(
        job_id = %job.record.id,
        mode = ?mode,
        "reindex: not implemented in v0.4.x — job explicitly rejected"
    );

    Err(HandlerError::Business(format!(
        "reindex ({mode:?}): not implemented in v0.4.x"
    )))
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Purge
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Purge`] — lifecycle purge of `Garbage` notes.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Purge(PurgeSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` **OR** `spec.dry_run` as the FIRST instruction —
///   either flag alone forces dry-run (`is_dry_run() || spec.dry_run`). The guard is a
///   double *opportunity* to stay in simulation, not a conjunction: for an irreversible
///   operation, real mode requires **both** to be false.
/// - In dry-run: lists eligible notes and returns `JobOutput::dry_run(count, ulids)` **without deleting anything**.
/// - In real mode, through the injected [`InternalClient`] (the handler holds no `Vault`
///   and no `Index`), for each eligible `Garbage` note:
///   1. `client.get_note_status` — re-verifies the current status (TOCTOU mitigation
///      between listing and delete).
///   2. `client.delete_note` — the server removes the `.md`, purges `.history/<ulid>/`
///      and cleans the `redirect_table`.
///
/// # Eligibility
///
/// `Lifecycle` mode: notes with `status = 'garbage'` AND
/// `status_changed <= now - grace_days` (or `created` if `status_changed` is NULL).
/// `grace_days = None` → all `Garbage` notes without delay.
///
/// # Safety invariant
///
/// A note that is NOT `Garbage` is never touched, even if it appeared in an earlier
/// listing. The per-delete status re-verification guarantees this.
///
/// # Cron
///
/// The nightly purge cron schedule is INTENTIONALLY disabled in production.
/// Activation requires an operator decision with a nightly backup strategy.
pub async fn handle_purge(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    // ── Double dry-run guard — first instruction (DryRun mode + PurgeSpec.dry_run) ──
    let spec = match &job.record.spec.kind {
        Job::Purge(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Irreversible operation: dry_run required on both axes.
    // `job.record.is_dry_run()` checks JobMode::DryRun in JobSpec.
    // `spec.dry_run` checks the explicit flag in PurgeSpec (default true).
    let is_dry_run = job.record.is_dry_run() || spec.dry_run;

    // Compute the cutoff timestamp (UTC ms) from grace_days.
    // grace_days = None → no age limit (all Garbage notes).
    // C2 (EX-C2-3) : vault résolu depuis JobSpec.scope — "main" à OFF (byte-identical).
    let vault_id = resolve_job_vault(&job.record.spec.scope, mt.enabled)?;
    let vault_id = vault_id.as_str();
    let cutoff_ms: Option<i64> = spec.grace_days.map(|days| {
        Utc::now()
            .timestamp_millis()
            .saturating_sub(i64::from(days) * 24 * 3600 * 1000)
    });

    // List eligible Garbage notes (or all if cutoff_ms = None).
    //
    // Both paths go through the garbage listing (`list_garbage` → server
    // `list_garbage_older_than`), which **excludes PROTECTED_DELETE sections**
    // (F-100 P1-1, defense in depth). The no-grace case uses `i64::MAX` as the
    // cutoff so every Garbage note qualifies — semantically "no age limit" — while
    // still inheriting the protected-section exclusion. The generic
    // `list_by_status(Garbage)` is deliberately not used here (it has no protected
    // filter and is shared with non-purge callers).
    let cutoff = cutoff_ms.unwrap_or(i64::MAX);
    let grace_days = spec.grace_days.unwrap_or(0);
    let candidates: Vec<NoteIdDto> = client
        .list_garbage(vault_id, cutoff, grace_days)
        .await
        .map_err(|e| HandlerError::Business(format!("purge: list_garbage: {e}")))?;

    let count = candidates.len();

    // ── Dry-run: list candidates without deleting anything ───────────────────
    if is_dry_run {
        let ulid_list = candidates
            .iter()
            .map(|dto| dto.note_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let description = if ulid_list.is_empty() {
            "purge lifecycle dry-run — no eligible note".to_string()
        } else {
            format!("purge lifecycle dry-run — eligible notes: [{ulid_list}]")
        };
        tracing::info!(
            job_id = %job.record.id,
            count = count,
            grace_days = ?spec.grace_days,
            dry_run = true,
            "purge: dry-run — {count} note(s) would be deleted"
        );
        return Ok(JobOutput::dry_run(count, &description));
    }

    // ── Real mode: delete with per-note status re-verification ───────────────
    let mut deleted: Vec<String> = Vec::with_capacity(count);
    let mut skipped: usize = 0;

    for note_dto in candidates {
        let id_str = note_dto.note_id.clone();

        // Re-verify status at delete time (TOCTOU mitigation).
        // If the note was restored (Garbage→Live) between the listing and now,
        // it is silently skipped.
        //
        // C4-1e (W3) : re-check SCOPÉ par `vault_id` du tick (le vault d'où provient le
        // candidat, `list_garbage` étant scopé). `get_note_status` lit l'INDEX filtré
        // `WHERE vault_id = ?1 AND id = ?2` — même source que le listing. Avant ce fix,
        // `get_note(id)` résolvait par ULID seul via le singleton `main` : un candidat de
        // `vault-b` voyait son statut re-vérifié dans `main` (classe hijack cross-vault —
        // skip erroné si `main` Live, ou purge fondée sur le mauvais vault). À OFF
        // `vault_id == "main"` → byte-identical.
        let current_status_opt = match client.get_note_status(vault_id, &id_str).await {
            Ok(opt) => opt,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(
                    note_id = %id_str,
                    "purge: note absent — already deleted, skip"
                );
                skipped += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: get_note_status unreadable — note skipped, batch continues"
                );
                skipped += 1;
                continue;
            }
        };

        match current_status_opt.as_deref() {
            Some("garbage") => {
                // Status confirmed Garbage — proceed with deletion.
            }
            Some(other_status) => {
                tracing::info!(
                    note_id = %id_str,
                    status = %other_status,
                    "purge: note skipped — status changed since listing (TOCTOU mitigation)"
                );
                skipped += 1;
                continue;
            }
            None => {
                tracing::debug!(
                    note_id = %id_str,
                    "purge: note absent from index — already deleted, skip"
                );
                skipped += 1;
                continue;
            }
        }

        // Delete via server (vault + index + redirects in sequence).
        // C4-1e (Slice E) : `vault_id` (résolu du JobSpec.scope, `main` à OFF) scope la
        // cascade au vault propriétaire — plus de clobber de l'homonyme `main`.
        match client.delete_note(vault_id, &id_str).await {
            Ok(()) => {
                tracing::info!(
                    note_id = %id_str,
                    "purge: note deleted"
                );
            }
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %id_str, "purge: note already absent — skip");
                skipped += 1;
                continue;
            }
            // Section protégée (garde system-wide côté serveur, F-100 P1-1) : le
            // hard-delete est refusé (403). Distinct d'un échec technique — SKIP
            // journalisé explicite, le batch continue, la note reste en garbage.
            // Normalement inatteignable (le listing exclut déjà les sections
            // protégées) — ceinture-et-bretelles si une note protégée y parvenait.
            Err(InternalClientError::ServerError { status: 403, .. }) => {
                tracing::info!(
                    note_id = %id_str,
                    "purge: note in protected section (PROTECTED_DELETE) — SKIP, never hard-delete"
                );
                skipped += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: delete_note failed — note skipped, batch continues"
                );
                skipped += 1;
                continue;
            }
        }
        deleted.push(id_str);
    }

    let deleted_count = deleted.len();
    tracing::info!(
        job_id = %job.record.id,
        deleted = deleted_count,
        skipped = skipped,
        "purge: complete"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!(
            "purge lifecycle: {deleted_count} note(s) deleted, {skipped} skipped"
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Forget
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Forget`] — semantic forget.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Forget(ForgetSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the **first instruction**.
/// - Double guard: `job.record.is_dry_run()` OR `spec.dry_run` activates dry-run.
/// - **Non-destructive**: no physical deletion (purge is a separate operation).
///
/// # Protected sections
///
/// Source of truth: `Section::PROTECTED_FORGET`, which holds **four** sections —
/// `AgentIssues`, `Council`, `ProjectMap` and `Identity`. They are systematically excluded
/// from the batch and counted in the preview. The check is re-applied **per candidate**
/// (via the note's own section) and is fail-closed: a candidate whose section cannot be
/// read is skipped, not forgotten. The job does not fail on exclusions — it continues with
/// the eligible notes.
///
/// # Dry-run
///
/// Returns `JobOutput::dry_run` carrying **counts only** — eligible and excluded — never
/// the candidate ULIDs, and never the scope query. This is deliberate: the raw scope may
/// contain sensitive data (PII, project names, identifiers) and must not be persisted in
/// `result_note_md`. No frontmatter mutation, no index update.
///
/// # Real mode
///
/// The handler holds no `Vault` and no `Index` — `gradatum-vault` and `gradatum-index` are
/// dev-dependencies of this crate. Every operation goes through the injected
/// [`InternalClient`]. For each eligible note:
/// 1. `client.get_note` — read, to skip notes already forgotten and to resolve the section.
/// 2. `client.persist_forget` — the server mutates the frontmatter
///    (`forgotten=true`, `forgotten_at`, `forgotten_by`), writes it CoW-traced
///    (snapshot in `.history/`) and marks the index, in sequence.
/// 3. `client.resync_forget_index` — best-effort repair when the server's index marking
///    failed after a successful vault write.
///
/// # Double confirmation
///
/// In real mode, `spec.confirm_ulids` must match **exactly** the resolved ULIDs.
/// Any divergence → `HandlerError::Business`, which marks the job `Failed` in the
/// queue (no automatic retry — divergence is intentional).
pub async fn handle_forget(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    // ── Double dry-run guard — first instruction (DryRun mode + ForgetSpec.dry_run) ──
    let spec = match &job.record.spec.kind {
        Job::Forget(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let is_dry_run = job.record.is_dry_run() || spec.dry_run;
    // C2 (EX-C2-3) : vault résolu depuis JobSpec.scope — "main" à OFF (byte-identical).
    let vault_id = resolve_job_vault(&job.record.spec.scope, mt.enabled)?;
    let vault_id = vault_id.as_str();

    // A2-bis — SOURCE UNIQUE DE VAULT. `vault_id` (issu de `JobSpec.scope`) pilote
    // désormais le listing ET la mutation ; `ForgetScope.vault*` n'est plus qu'une
    // assertion de cohérence, vérifiée ici avant tout accès.
    ensure_forget_scope_vault(&spec.scope, vault_id)?;

    // ── Protected sections — never forgotten ─────────────────────────────────
    // Source of truth: Section::PROTECTED_FORGET in gradatum-core::section.
    // AgentIssues + Council excluded from the batch, reported in the preview.

    // ── Scope resolution → raw candidate list ────────────────────────────────
    // Methods return Vec<(id, section)> — only the id is extracted here.
    // The protected-section check is re-applied per candidate via get_note_section.
    //
    // Le vault de listing est `vault_id`, JAMAIS le champ du `ForgetScope` : celui-ci
    // vient d'être prouvé égal (ou absent) par `ensure_forget_scope_vault`, donc lire
    // `vault_id` est équivalent — et le reste après toute évolution du ForgetScope.
    let raw_candidates: Vec<String> = match &spec.scope {
        ForgetScope::Topic { query, limit, .. } => {
            let max_limit = limit.unwrap_or(50).min(200);
            client
                .search_fts_for_forget(vault_id, query, max_limit)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: search_fts_for_forget: {e}")))?
                .into_iter()
                .map(|dto| dto.note_id)
                .collect()
        }
        ForgetScope::Locus { locus, .. } => client
            .list_notes_by_locus(vault_id, locus)
            .await
            .map_err(|e| HandlerError::Business(format!("forget: list_notes_by_locus: {e}")))?
            .into_iter()
            .map(|dto| dto.note_id)
            .collect(),
        ForgetScope::Agent { agent_id, .. } => {
            // Un seul vault par job : la liste passée en aval est toujours `[vault_id]`,
            // quel que soit le contenu de `vaults` (prouvé compatible ci-dessus).
            let vault_strs: Vec<String> = vec![vault_id.to_string()];
            client
                .list_notes_by_agent(agent_id, &vault_strs)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: list_notes_by_agent: {e}")))?
                .into_iter()
                .map(|dto| dto.note_id)
                .collect()
        }
        // Future exhaustive case — guarded by #[non_exhaustive] on ForgetScope.
        // Déjà refusé en amont par `ensure_forget_scope_vault` ; conservé fail-closed.
        // Pas de `{:?}` : ce message est persisté en base, la variante peut porter des
        // champs non bornés.
        _ => {
            return Err(HandlerError::Business(
                "forget: unsupported ForgetScope variant".to_owned(),
            ));
        }
    };

    // ── Partition: eligible / excluded (protected sections) ──────────────────
    // Each exclusion: (ulid, section) — used for the job description.
    let mut eligible: Vec<String> = Vec::with_capacity(raw_candidates.len());
    let mut excluded_details: Vec<(String, String)> = Vec::new(); // (ulid, section)

    for ulid in raw_candidates {
        // Scopé `vault_id` : la garde « section protégée » DOIT porter sur la note qui
        // sera mutée. Non scopée, elle jugeait l'homonyme de `main` — une note protégée
        // du vault cible pouvait passer, une note libre être bloquée par un homonyme.
        let section_str = client
            .get_note(vault_id, &ulid)
            .await
            .ok()
            .map(|dto| dto.section);

        // Fail-closed: unknown section (note absent from index) = PROTECTED.
        // unwrap_or(true) ensures that any out-of-index ULID is excluded rather
        // than included — conservative behavior consistent with the
        // "protected sections are never forgotten" policy.
        let is_protected = section_str
            .as_deref()
            .map(|s| Section::PROTECTED_FORGET.iter().any(|p| p.as_str() == s))
            .unwrap_or(true);

        if is_protected {
            let section = section_str.unwrap_or_default();
            tracing::info!(
                note_id = %ulid,
                section = %section,
                "forget: note excluded — protected section"
            );
            excluded_details.push((ulid, section));
        } else {
            eligible.push(ulid);
        }
    }

    let eligible_count = eligible.len();
    let excluded_count = excluded_details.len();

    // ── Dry-run: return preview without mutation ──────────────────────────────
    if is_dry_run {
        // Do not persist the raw scope query in result_note_md:
        // it may contain sensitive data (PII, project names, identifiers).
        // The eligible note count is sufficient for poll-status on the caller side.
        let description = if eligible_count == 0 {
            format!("forget dry-run — no eligible note (exclusions: {excluded_count})")
        } else {
            format!("forget dry-run — {eligible_count} eligible note(s), {excluded_count} excluded")
        };

        tracing::info!(
            job_id = %job.record.id,
            eligible = eligible_count,
            excluded = excluded_count,
            dry_run = true,
            "forget: dry-run — {eligible_count} note(s) would be forgotten, {excluded_count} excluded"
        );
        return Ok(JobOutput::dry_run(eligible_count, &description));
    }

    // ── Real mode — confirm_ulids verification ────────────────────────────────
    // confirm_ulids must match EXACTLY the eligible ULIDs (same set, order irrelevant).
    // Any divergence = rejection to prevent accidental forget.
    //
    // Two empty sets (eligible=0 + confirm=0) = legal → empty job OK.
    // No composite guard: direct comparison covers all cases.
    {
        let mut expected_sorted = eligible.clone();
        expected_sorted.sort();
        let mut confirmed_sorted = spec.confirm_ulids.clone();
        confirmed_sorted.sort();

        if expected_sorted != confirmed_sorted {
            return Err(HandlerError::Business(format!(
                "forget: confirm_ulids does not match the resolved ULIDs — \
                 expected={}, provided={}. Re-run a preview and confirm the exact ULIDs.",
                expected_sorted.len(),
                confirmed_sorted.len()
            )));
        }
    }

    // ── Real mode: frontmatter mutation + index sync ──────────────────────────
    let forgotten_by = spec.forgotten_by.as_deref();
    let mut forgotten_ulids: Vec<ulid::Ulid> = Vec::with_capacity(eligible_count);
    let mut forgotten: Vec<String> = Vec::with_capacity(eligible_count);
    let mut skipped: usize = 0;

    for ulid in &eligible {
        let raw_ulid = match ulid::Ulid::from_string(ulid) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: invalid ULID — skipped"
                );
                skipped += 1;
                continue;
            }
        };
        let _note_id = NoteId(raw_ulid);

        // TOCTOU re-verification: if the note is already forgotten, skip idempotently.
        //
        // A7 — the flag lives in `NoteReadDto.forgotten` (frontmatter `forgotten: true`),
        // NOT in `status`: `NoteStatus` has no `Forgotten` variant, so comparing the
        // status against `"forgotten"` was always false and this skip never ran. A second
        // forget then overwrote `forgotten_at`/`forgotten_by`, losing the audit trail of
        // the first one. Same field as the distill path already reads below.
        let already_forgotten = match client.get_note(vault_id, ulid).await {
            Ok(dto) => dto.forgotten,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %ulid, "forget: note absent — skip");
                skipped += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: get_note failed — note skipped, batch continues"
                );
                skipped += 1;
                continue;
            }
        };

        if already_forgotten {
            // A7-bis — le skip garde la piste d'audit, la re-synchro garde l'index.
            //
            // A7 avait ressuscité ce skip (mort tant qu'il comparait `status`), ce qui a
            // troqué une propriété contre l'autre : `forgotten_at`/`forgotten_by` du
            // premier oubli étaient enfin préservés, mais le `continue` laissait l'index
            // désynchronisé POUR TOUJOURS — la note restait indexée vivante, donc
            // CHERCHABLE, alors que le vault la dit oubliée, et elle était malgré tout
            // comptée dans la liste `forgotten` du job.
            //
            // Cette désynchronisation n'est pas une fenêtre de course : deux chemins de
            // production ordinaires la créent — `vault_unforgot` (route publique) efface
            // la marque d'index sans toucher au frontmatter, et `persist_forget` rend 200
            // best-effort si son `mark_forgotten` échoue après un write vault réussi.
            //
            // `resync_forget_index` ne réécrit QUE `forgotten = 1` : surtout pas
            // `persist_forget`, qui ré-estamperait `forgotten_at`/`forgotten_by` à
            // l'instant présent et détruirait ce que ce skip existe pour protéger.
            //
            // Best-effort assumé, jamais silencieux : la note EST oubliée côté vault, donc
            // un échec de réparation ne doit pas faire échouer le lot — mais il est
            // journalisé en WARN, sans quoi la désynchronisation redeviendrait invisible.
            if let Err(e) = client.resync_forget_index(vault_id, ulid).await {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: index resync failed — note left desynchronised (searchable), batch continues"
                );
            }
            tracing::debug!(
                note_id = %ulid,
                "forget: already forgotten — idempotent skip, index mark re-asserted"
            );
            forgotten.push(ulid.clone());
            forgotten_ulids.push(raw_ulid);
            continue;
        }

        // Get section for persist_forget (server handles frontmatter mutation + index sync)
        let note_section = match client.get_note(vault_id, ulid).await {
            Ok(dto) => dto.section,
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: get_note (section) failed — note skipped, batch continues"
                );
                skipped += 1;
                continue;
            }
        };

        // Persist via server (vault frontmatter mutation + index mark_forgotten in sequence).
        let mut forget_req = PersistForgetRequest::new(
            ulid.clone(),
            vault_id.to_string().into(),
            String::new(), // server reads the body internally
            note_section,
        );
        forget_req.forgotten_by = forgotten_by.map(|s| s.to_string());
        match client.persist_forget(&forget_req).await {
            Ok(_) => {
                tracing::info!(note_id = %ulid, "forget: note forgotten");
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: persist_forget failed — note skipped, batch continues"
                );
                skipped += 1;
                continue;
            }
        }
        forgotten.push(ulid.clone());
        forgotten_ulids.push(raw_ulid);
    }

    let forgotten_count = forgotten.len();
    tracing::info!(
        job_id = %job.record.id,
        forgotten = forgotten_count,
        skipped = skipped,
        exclusions = excluded_count,
        "forget: complete"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: forgotten_ulids,
        files: vec![],
        result_note_md: format!(
            "semantic forget: {forgotten_count} note(s) forgotten, {skipped} skipped, {excluded_count} excluded (protected sections)"
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Distill (semantic distillation)
// ─────────────────────────────────────────────────────────────────────────────

/// Hard cap on the number of notes considered per distillation run.
///
/// Clustering is `O(n²)`; even if `batch_limit` is misconfigured, the number of notes
/// actually loaded and compared never exceeds this bound (protection against
/// combinatorial explosion and memory pressure).
pub const MAX_DISTILL_BATCH: usize = 2000;

/// Synthesis output produced for a note cluster.
///
/// Produced by a [`DistillSynthesizer`] and written as a `PendingReview` note.
pub struct ClusterSynthesis {
    /// Title of the synthesis note.
    pub title: String,
    /// Markdown body of the synthesis note.
    pub body: String,
}

/// Synthesis error — propagated to mark the job `Failed` cleanly.
#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    /// The synthesis service (LLM gateway) is unavailable or failed.
    #[error("synthesis unavailable: {0}")]
    Unavailable(String),
}

/// Cluster synthesis producer.
///
/// Abstraction that allows substituting the deterministic implementation (MVP)
/// with a dedicated LLM gateway backend without touching the handler.
///
/// # Contract
///
/// - `synthesize` receives the notes of a cluster as `[(title, body)]` (≥ 1 note).
/// - Returns `Ok(ClusterSynthesis)`: title + body of the `PendingReview` note.
/// - Returns `Err(SynthesisError::Unavailable)`: the job MUST fail cleanly
///   (no partial note written — mitigation for gateway-down scenarios).
#[async_trait::async_trait]
pub trait DistillSynthesizer: Send + Sync {
    /// Synthesizes a note cluster into a synthesis note.
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError>;
}

/// Deterministic synthesizer — MVP (no LLM call).
///
/// Produces a structured synthesis note by concatenation: title derived from the
/// first cluster element, body listing source notes with an excerpt.
/// The note is written as `PendingReview` (requires human review) — editorial quality
/// is the reviewer's responsibility, not the automated step's.
///
/// ## Why deterministic at MVP
///
/// The worker injects no free-text generation client (the only wired LLM backend is
/// `gradatum_chat::LlmBackend`, specialised for curator classification — not free
/// completion). A dedicated `distill-semantic` gateway client is deferred:
/// the `PendingReview` output combined with the cron disabled by default keeps the step
/// safe, and the [`DistillSynthesizer`] abstraction allows plugging in an LLM without
/// refactoring the handler.
#[derive(Default)]
pub struct TemplateSynthesizer;

#[async_trait::async_trait]
impl DistillSynthesizer for TemplateSynthesizer {
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError> {
        if cluster.is_empty() {
            return Err(SynthesisError::Unavailable(
                "empty cluster — nothing to synthesize".to_string(),
            ));
        }
        // Title: derived from the first non-empty title in the cluster.
        let lead_title = cluster
            .iter()
            .map(|(t, _)| t.trim())
            .find(|t| !t.is_empty())
            .unwrap_or("related notes");
        let title = format!("Distilled synthesis — {lead_title}");

        // Body: header + list of source notes with bounded excerpt.
        let mut body = format!(
            "# {title}\n\n\
             > Distilled synthesis note (F-22) — **pending review**.\n\
             > Groups {} semantically close note(s).\n\n\
             ## Distilled sources\n\n",
            cluster.len()
        );
        for (i, (src_title, src_body)) in cluster.iter().enumerate() {
            let excerpt: String = src_body.trim().chars().take(280).collect();
            let display_title = if src_title.trim().is_empty() {
                "(untitled)"
            } else {
                src_title.trim()
            };
            body.push_str(&format!("### {}. {display_title}\n\n{excerpt}\n\n", i + 1));
        }
        Ok(ClusterSynthesis { title, body })
    }
}

/// Handler for [`gradatum_core::Job::Distill`] — semantic distillation.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Distill(DistillSource)`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
///
/// # Dry-run
///
/// Lists candidate clusters (non-`processed` notes from the scope → embeddings →
/// cosine clustering) **without any mutation**. `JobScope::VaultWide` is only
/// permitted in dry-run (exploration).
///
/// # Real mode
///
/// **This handler writes nothing.** It synthesises, then hands the persistence off to a
/// `Job::Validate`. For each cluster:
/// 1. Synthesis via [`DistillSynthesizer`] (failure → clean `Failed` job, nothing enqueued).
/// 2. Compute the base trust for the synthesis and snapshot the source trusts.
/// 3. Build a `ValidateSpec` (synthesis ULID, title, body, source ulids, source texts,
///    source trusts, base trust, threshold) and `queue.enqueue` it as `Job::Validate`.
///    Enqueue failure is **best-effort**: it is logged as a warning and the cluster is
///    skipped, without failing the distill job.
///
/// Writing the synthesis note, persisting its trust and marking the sources
/// (`processed = true` + `derived-into`) all happen in `handle_validate`, not here.
///
/// # Required scope in real mode
///
/// `JobScope::VaultWide` is **rejected** outside dry-run (`HandlerError::Business`) —
/// mitigation against O(n²) clustering. `Locus` or `Notes` scope required.
///
/// # Idempotence
///
/// A note with `processed = true` is never re-collected (filtered before clustering) —
/// a double run on the same scope is idempotent (already-distilled clusters are excluded).
///
/// **Caveat**: this handler never sets `processed` itself — the flag is written by
/// `handle_validate`. Idempotence therefore only holds once the enqueued `Job::Validate`
/// has run. A second distill launched before that (or after a best-effort enqueue
/// failure) re-collects the same sources and re-synthesises the cluster.
///
/// # Injected dependencies
///
/// `client` ([`InternalClient`] — the only write path), `embedder` (reads precomputed
/// embeddings via `embedder_id`), `synthesizer` (pluggable synthesis producer), `queue`
/// (to enqueue the `Job::Validate`) and `mt` (multi-tenant config for vault resolution).
///
/// Neither `vault` nor `index` is injected: `gradatum-vault` and `gradatum-index` are
/// **dev-dependencies** of this crate, unavailable to the handler.
pub async fn handle_distill(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    synthesizer: Data<Arc<dyn DistillSynthesizer + Send + Sync>>,
    queue: Data<Arc<dyn QueueStore>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    // ── Spec extraction — first instruction ──────────────────────────────────
    let spec = match &job.record.spec.kind {
        Job::Distill(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let is_dry_run = job.record.is_dry_run();
    // C2 (EX-C2-3) : vault résolu depuis JobSpec.scope (le cron enqueue Vault(v) à ON,
    // Locus(locus) à OFF → "main", byte-identical). Le locus vit dans DistillSource.scope.
    //
    // ⚠️ HOMONYMES — deux champs `scope`, MÊME type `JobScope`, contrats OPPOSÉS. Le
    // typage ne protège de rien ici : intervertir les deux compile sans un warning.
    //   • `job.record.spec.scope` (ci-dessous) = OÙ. Doit porter `Vault(v)` à ON ;
    //     `resolve_job_vault` refuse toute autre variante.
    //   • `spec.scope` = `DistillSource.scope` (l. ~1817) = QUOI. Doit porter
    //     `Locus`/`Notes` ; `resolve_distill_scope` refuse justement `Vault`.
    // Un `Vault(v)` valide pour l'un est terminal pour l'autre, et réciproquement.
    let vault_id = resolve_job_vault(&job.record.spec.scope, mt.enabled)?;
    let embedder_id = embedder.embedder_id().to_string();

    // Clamp confidence_threshold to [0, 1].
    // An out-of-range threshold (NaN, negative, > 1) would corrupt cosine clustering.
    let confidence_threshold = if spec.confidence_threshold.is_finite() {
        spec.confidence_threshold.clamp(0.0, 1.0)
    } else {
        0.75 // NaN/inf → défaut prudent.
    };

    // Hard cap on batch_limit (anti O(n²) explosion).
    let effective_batch_limit = spec.batch_limit.min(MAX_DISTILL_BATCH);

    // ── VaultWide scope guard in real mode ───────────────────────────────────
    if !is_dry_run && matches!(spec.scope, JobScope::VaultWide) {
        return Err(HandlerError::Business(
            "distill: JobScope::VaultWide rejected outside dry-run — Locus or Notes scope required (R3)"
                .to_string(),
        ));
    }

    // Reject empty / whitespace-only Locus in real mode.
    // An empty prefix would match the entire vault via LIKE '%' → equivalent to VaultWide
    // (bypasses the VaultWide guard). Rejected outside dry-run.
    if !is_dry_run
        && let JobScope::Locus(prefix) = &spec.scope
        && prefix.trim().is_empty()
    {
        return Err(HandlerError::Business(
            "distill: empty/whitespace JobScope::Locus refused outside dry-run \
                     (would match the whole vault — bypasses R3)"
                .to_string(),
        ));
    }

    // ── Scope resolution → raw candidates (ULIDs) ────────────────────────────
    // ⚠️ `spec.scope` = `DistillSource.scope` (le QUOI), à ne pas confondre avec
    // `job.record.spec.scope` (le OÙ, consommé par `resolve_job_vault` plus haut) — même
    // type, contrats opposés. Cf. le bloc HOMONYMES en tête de handler.
    let raw_candidates: Vec<NoteId> =
        resolve_distill_scope(&**client, &vault_id, &spec.scope).await?;

    // ── Filter THEN truncate (to avoid starvation) ────────────────────────────
    // `batch_limit` truncation is applied AFTER the filters
    // (processed / forgotten / garbage / no-embedding), not before. Otherwise notes
    // beyond the first batch_limit entries are never reachable if all earlier ones
    // are filtered out.
    // Defensive skip of forgotten / Garbage notes regardless of scope
    // (an explicit Notes scope may contain stale ULIDs).
    // Notes without an embedding are silently skipped (cannot be clustered).
    let mut candidates: Vec<(NoteId, String, String, Vec<f32>)> = Vec::new();
    for note_id in raw_candidates {
        // Early exit: effective window reached (saves I/O).
        if candidates.len() >= effective_batch_limit {
            break;
        }
        let note_id_str = note_id.to_string();
        let note_dto = match client.get_note(&vault_id, &note_id_str).await {
            Ok(n) => n,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::warn!(note_id = %note_id, "distill: note absent — skipped");
                continue;
            }
            Err(e) => {
                tracing::warn!(note_id = %note_id, error = %e, "distill: get_note failed — note skipped");
                continue;
            }
        };
        // Defensive skip: never distill a forgotten note.
        if note_dto.forgotten {
            tracing::debug!(note_id = %note_id, "distill: note forgotten — skipped");
            continue;
        }
        // Defensive skip: never distill a Garbage note.
        if note_dto.status == "garbage" {
            tracing::debug!(note_id = %note_id, "distill: Garbage note — skipped");
            continue;
        }
        // Skip if already distilled (idempotence — check via processed field in NoteReadDto).
        if note_dto.processed {
            tracing::debug!(note_id = %note_id, "distill: note already processed — skipped");
            continue;
        }
        // Skip if no embedding (cannot be clustered).
        let emb = match client
            .get_note_embedding(&vault_id, &note_id_str, &embedder_id)
            .await
        {
            Ok(e) => e.vector,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %note_id, "distill: no embedding — note skipped");
                continue;
            }
            Err(e) => {
                return Err(HandlerError::Business(format!(
                    "distill: get_note_embedding {note_id}: {e}"
                )));
            }
        };
        let title = gradatum_curator::extract_h1_title(&note_dto.body).unwrap_or_default();
        candidates.push((note_id, title, note_dto.body.clone(), emb));
    }

    // ── Cosine clustering (connected components, confidence_threshold) ─────────
    let embeddings: Vec<Vec<f32>> = candidates.iter().map(|(_, _, _, e)| e.clone()).collect();
    let clusters = crate::distill_cluster::cluster_by_cosine(&embeddings, confidence_threshold);

    // ── Dry-run: list clusters without mutation ──────────────────────────────
    if is_dry_run {
        let description = format!(
            "distill dry-run — {} candidate note(s), {} cluster(s) (cosine threshold {:.2})",
            candidates.len(),
            clusters.len(),
            confidence_threshold
        );
        tracing::info!(
            job_id = %job.record.id,
            candidates = candidates.len(),
            clusters = clusters.len(),
            dry_run = true,
            "distill: dry-run"
        );
        return Ok(JobOutput::dry_run(clusters.len(), &description));
    }

    // ── Real mode: synthesis per cluster → enqueue Job::Validate ────────────
    let mut notes_created: Vec<ulid::Ulid> = Vec::new();

    for cluster in &clusters {
        // Cluster notes (titles + bodies for synthesis).
        let cluster_pairs: Vec<(String, String)> = cluster
            .iter()
            .map(|&i| (candidates[i].1.clone(), candidates[i].2.clone()))
            .collect();
        let source_ids: Vec<NoteId> = cluster.iter().map(|&i| candidates[i].0).collect();

        // Synthesis — failure = clean job Failed (no partial note written for THIS
        // cluster; previously written clusters remain committed, documented batch behaviour).
        let synthesis = synthesizer.synthesize(&cluster_pairs).await.map_err(|e| {
            HandlerError::Business(format!("distill: cluster synthesis failed: {e}"))
        })?;

        // Synthesis note frontmatter: PendingReview + provenance distilled +
        // derived-from (ExtraFields, JCS-safe: Vec of ULID strings).
        let synth_id = NoteId::new();
        let derived_from: Vec<TomlValue> = source_ids
            .iter()
            .map(|id| TomlValue::String(id.to_string()))
            .collect();
        let mut extra = ExtraFields::empty();
        extra.insert("derived-from".to_string(), TomlValue::Array(derived_from));

        let _fm = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new(&vault_id),
            locus: None,
            section: Section::Reference,
            status: NoteStatus::PendingReview,
            status_reason: Some("distilled — en attente de revue".to_string()),
            status_changed: None,
            tags: SmallVec::new(),
            author: Some(AuthorRef::system("vault-distiller")),
            created: Utc::now(),
            updated: None,
            extra,
            provenance: Some("distilled".to_string()),
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        // Compute dynamic trust before writing the synthesis note.
        // Preload source trusts via client (async), then apply synchronous compute_distill_trust.
        let mut trust_map: std::collections::HashMap<ulid::Ulid, f32> =
            std::collections::HashMap::with_capacity(source_ids.len());
        for src in &source_ids {
            if let Ok(t) = client.get_trust(&vault_id, &src.to_string()).await {
                trust_map.insert(src.0, t);
            }
        }
        // Snapshot source trusts before trust_map is moved into MapTrustLookup.
        let source_trusts: Vec<f32> = source_ids
            .iter()
            .map(|n| trust_map.get(&n.0).copied().unwrap_or(0.6))
            .collect();
        let lookup = MapTrustLookup(trust_map);
        let trust = gradatum_core::provenance::compute_distill_trust(
            &source_ids.iter().map(|n| n.0).collect::<Vec<_>>(),
            &lookup,
            confidence_threshold,
        );

        // Build the validation payload and hand off to Job::Validate.
        // Persistence (note write + source marking) is delegated to handle_validate.
        let validate_spec = ValidateSpec {
            note_id: synth_id.0,
            tenant_id: vault_id.clone(),
            title: synthesis.title.clone(),
            body: synthesis.body.clone(),
            source_ids: source_ids.iter().map(|n| n.0).collect(),
            // Bodies of source notes — needed by the quality scorer (num/entity penalties).
            source_texts: cluster_pairs.iter().map(|(_, b)| b.clone()).collect(),
            // Trusts pre-snapshotted above from trust_map before it was moved.
            source_trusts,
            base_trust: trust,
            threshold: ValidateSpec::default_threshold(),
        };
        let record = build_validate_job_record(validate_spec, &vault_id, job.record.id);
        if let Err(e) = queue.enqueue(record).await {
            tracing::warn!(
                note_id = %synth_id,
                error = %e,
                "distill: enqueue Job::Validate failed — synthesis not validated (best-effort)"
            );
        } else {
            notes_created.push(synth_id.0);
        }
        // Source `processed` marking is moved into handle_validate.
    }

    tracing::info!(
        job_id = %job.record.id,
        clusters = clusters.len(),
        enqueued = notes_created.len(),
        "distill: complete"
    );

    let enqueued_count = notes_created.len();
    Ok(JobOutput {
        notes_created,
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!(
            "distill: {} cluster(s) → {enqueued_count} synthesis/es enqueued for validation",
            clusters.len()
        ),
    })
}

/// Resolves a distillation `JobScope` into a list of candidate `NoteId`s via `InternalClient`.
///
/// - `Locus(prefix)`: notes whose locus starts with `prefix`.
/// - `Notes(ids)`: explicit set of note IDs.
/// - `VaultWide`: all notes in the vault (permitted in dry-run only —
///   the handler guard rejects `VaultWide` in real mode before this call).
/// - `Session(_)`: not supported for distillation (`HandlerError::Business`).
async fn resolve_distill_scope(
    client: &dyn InternalClient,
    vault_id: &str,
    scope: &JobScope,
) -> Result<Vec<NoteId>, HandlerError> {
    match scope {
        JobScope::Locus(prefix) => {
            let rows = client
                .list_notes_by_locus(vault_id, prefix)
                .await
                .map_err(|e| {
                    HandlerError::Business(format!("distill: list_notes_by_locus: {e}"))
                })?;
            rows.into_iter()
                .filter_map(|dto| ulid::Ulid::from_string(&dto.note_id).ok().map(NoteId))
                .map(Ok)
                .collect()
        }
        JobScope::Notes(ids) => Ok(ids.iter().copied().map(NoteId).collect()),
        JobScope::VaultWide => {
            // Reached only in dry-run (handler guard). Lists all Live + PendingReview notes.
            let mut all = Vec::new();
            for status_str in ["live", "pending-review", "staging"] {
                let ids = client
                    .list_by_status(vault_id, status_str)
                    .await
                    .map_err(|e| HandlerError::Business(format!("distill: list_by_status: {e}")))?;
                all.extend(
                    ids.into_iter()
                        .filter_map(|dto| ulid::Ulid::from_string(&dto.note_id).ok().map(NoteId)),
                );
            }
            Ok(all)
        }
        JobScope::Session(_) => Err(HandlerError::Business(
            "distill: JobScope::Session not supported".to_string(),
        )),
        // JobScope est #[non_exhaustive] (A3) : toute variante future est refusée
        // tant qu'elle n'est pas câblée explicitement (fail-closed, zéro mutation).
        _ => Err(HandlerError::Business(
            "distill: JobScope variant not supported".to_string(),
        )),
    }
}

/// Synchronous `TrustLookup` adapter backed by a preloaded in-memory map.
///
/// `compute_distill_trust` requires a synchronous `&dyn TrustLookup`; `SqliteIndex`
/// only exposes `get_trust` async. This adapter preloads source trusts
/// (async I/O) then provides the expected synchronous view.
struct MapTrustLookup(std::collections::HashMap<ulid::Ulid, f32>);

impl gradatum_core::provenance::TrustLookup for MapTrustLookup {
    fn get_trust(&self, id: &ulid::Ulid) -> Option<f32> {
        self.0.get(id).copied()
    }
}

// is_processed removed — note.processed now served by NoteReadDto.processed field.

// mark_source_processed removed — now delegated to server via persist_distill(mark_processed=true).

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Builds a complete `JobRecord` for `Job::Embed(EmbedSpec)`.
///
/// Mirrors `build_curate_job_record` in `gradatum-server/src/api_v1/write.rs`.
///
/// # Parameters
///
/// - `note_id`: ULID of the note to embed (`NoteId.0`).
/// - `tenant_id`: tenant of the parent job (inherited from the curate job).
/// - `parent_job_id`: ULID of the parent curate job (`lineage.parent_job`).
fn build_embed_job_record(
    note_id: gradatum_core::identity::NoteId,
    tenant_id: &str,
    parent_job_id: ulid::Ulid,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    JobRecord {
        id: ulid::Ulid::generate(),
        spec: JobSpec {
            kind: Job::Embed(EmbedSpec {
                note_id: note_id.0,
                tenant_id: tenant_id.to_string(),
                // Idempotence: handle_embed skips if a vector is already present.
                force_regenerate: false,
            }),
            class,
            mode: JobMode::Batch,
            scope: JobScope::Notes(vec![note_id.0]),
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            // Must be empty — the cascade engine is not yet implemented in gradatum_queue.rs.
            // A non-empty await_jobs would leave this job stuck in Waiting indefinitely.
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: Some(parent_job_id),
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Builds a [`JobRecord`] for a `Job::Validate(ValidateSpec)` job.
///
/// Mirrors `build_embed_job_record` and the pattern of `build_curate_job_record` in
/// `gradatum-server/src/api_v1/write.rs`.
///
/// The tenant is already embedded in `spec.tenant_id`; the `_tenant` parameter is
/// retained for interface symmetry with the other build helpers.
///
/// # Parameters
///
/// - `spec`: Full [`ValidateSpec`] carrying the synthesis payload + source metadata.
/// - `_tenant`: tenant string (already in `spec.tenant_id` — interface symmetry only).
/// - `parent_id`: ULID of the parent distill job (`lineage.parent_job`).
pub fn build_validate_job_record(
    spec: ValidateSpec,
    _tenant: &str,
    parent_id: ulid::Ulid,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    JobRecord {
        id: ulid::Ulid::generate(),
        spec: JobSpec {
            kind: Job::Validate(spec),
            class,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::High,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            // Must be empty — the cascade engine is not yet implemented in gradatum_queue.rs.
            // A non-empty await_jobs would leave this job stuck in Waiting indefinitely.
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: Some(parent_id),
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Converts a kebab-case section string into a `Section` enum via `serde_json`.
///
/// Returns `None` if the string is not a valid canonical section.
fn section_from_str(s: &str) -> Option<Section> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<Section>(&json_str).ok()
}

/// Converts a `NoteStatus` to its kebab-case string representation for the internal API.
fn status_to_str(status: NoteStatus) -> String {
    match status {
        NoteStatus::Live => "live".to_string(),
        NoteStatus::PendingReview => "pending-review".to_string(),
        NoteStatus::Staging => "staging".to_string(),
        NoteStatus::Garbage => "garbage".to_string(),
        NoteStatus::Draft => "draft".to_string(),
        NoteStatus::Deprecated => "deprecated".to_string(),
    }
}

/// Builds a `Frontmatter` from a `CurateSpec` and the curator decisions.
///
/// Used for the vault_write path (title/body present in the spec).
#[allow(dead_code)]
fn build_frontmatter_from_spec(
    tenant_id: &str,
    section: Section,
    status: NoteStatus,
    spec: &CurateSpec,
    curator_tags: &[String],
) -> Frontmatter {
    let mut all_tags: Vec<String> = spec.tags.clone();
    for t in curator_tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }

    // C-TAG-1 : alignement sur le régime interne `parse_tags` (persist.rs).
    // Utilise `Tag::normalize` + WARN sur transformation + dédup, au lieu du
    // `filter_map(Tag::new(...).ok())` qui silençait les tags légitimes.
    let tags: SmallVec<[Tag; 4]> = {
        let mut seen = std::collections::HashSet::with_capacity(all_tags.len());
        let mut out: SmallVec<[Tag; 4]> = SmallVec::new();
        for t in &all_tags {
            let norm = Tag::normalize(t.clone());
            if norm.as_ref().map(|n| n.as_str()) != Some(t.as_str()) {
                tracing::warn!(
                    original = %t,
                    normalized = ?norm.as_ref().map(|n| n.as_str()),
                    "build_frontmatter_from_spec: normalized tag (C-TAG-1)"
                );
            }
            if let Some(tag) = norm
                && seen.insert(tag.as_str().to_owned())
            {
                out.push(tag);
            }
        }
        out
    };

    let author = spec.author.as_deref().map(AuthorRef::system);

    // Resolve provenance from section_hint.
    // If section_hint ∈ TRUST_SCORES → provenance = section_hint.
    // Otherwise (or absent) → conservative default "agent-log" (trust 0.50).
    let provenance = gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref());

    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(tenant_id),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: None,
        tags,
        author,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: Some(provenance.to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TemporalIndex helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parses an `ExtraFields` field as a UTC epoch in milliseconds.
///
/// Accepted formats (delegated to the SSOT [`gradatum_core::parse_temporal_str_as_ms`]):
/// - ISO 8601 / RFC 3339 (with time): `2024-03-15T10:00:00Z`
/// - Date-only YYYY-MM-DD → start of day UTC: `2024-03-15`
///
/// Non-`String` `toml::Value` variants (Integer, Float, etc.) are rejected (`None`)
/// per the JCS constraint on `ExtraFields` — only the String case delegates to the SSOT.
///
/// Returns `None` if the field is absent, non-`String`, or malformed.
///
/// # Side effects
///
/// None. Pure function.
pub(crate) fn parse_extra_field_as_ms(extra: &ExtraFields, key: &str) -> Option<i64> {
    let val = extra.get(key)?;
    let s = match val {
        TomlValue::String(s) => s.as_str(),
        // Non-String formats (Integer, Float, etc.) → ignored (JCS constraint)
        _ => return None,
    };
    // Délègue au SSOT partagé serveur+worker — garantit la parité des formats acceptés.
    // Un format accepté par le serveur (HTTP 202) DOIT produire le même résultat ici
    // pour éviter un fallback silencieux anchor_src=Created dans le worker.
    gradatum_core::parse_temporal_str_as_ms(s)
}

/// Resolves the temporal anchor of a note according to the field priority order.
///
/// Priority (descending):
/// 1. `occurred_at` in `frontmatter.extra` (ISO 8601 UTC string)
/// 2. `event-date` in `frontmatter.extra`
/// 3. `valid_from` in `frontmatter.extra`
/// 4. `frontmatter.created` (universal fallback — always present)
///
/// ## Robustness
///
/// `ExtraFields` values are `toml::Value::String` (JCS constraint — see frontmatter.rs).
/// An invalid format silently falls back to the next lower-priority field,
/// and ultimately to `created` (no panic, no propagated error).
///
/// ## Returns
///
/// `(anchor_ms, AnchorSrc)` — UTC epoch in milliseconds + identified source.
///
/// # Side effects
///
/// None. Pure function.
pub(crate) fn resolve_temporal_anchor(extra: &ExtraFields, created_ms: i64) -> (i64, AnchorSrc) {
    if let Some(ms) = parse_extra_field_as_ms(extra, "occurred_at") {
        return (ms, AnchorSrc::OccurredAt);
    }
    if let Some(ms) = parse_extra_field_as_ms(extra, "event-date") {
        return (ms, AnchorSrc::EventDate);
    }
    if let Some(ms) = parse_extra_field_as_ms(extra, "valid_from") {
        return (ms, AnchorSrc::ValidFrom);
    }

    (created_ms, AnchorSrc::Created)
}

/// Extracts the validity end bound of a note from `frontmatter.extra`.
///
/// Reads the `valid_until` field (same parsers as `valid_from`: ISO 8601 / YYYY-MM-DD / ms).
///
/// ## Consistency guard
///
/// If `valid_until_ms` is present AND `≤ anchor_ms` → returns `None` and emits a warning.
/// An invalid window is ignored (the note remains visible): accuracy over coverage.
///
/// ## Returns
///
/// `Some(epoch_ms)` if `valid_until` is present and coherent, `None` otherwise (open validity).
///
/// # Side effects
///
/// None except the warning emitted on an invalid window.
#[allow(dead_code)]
pub(crate) fn extract_valid_until(extra: &ExtraFields, anchor_ms: i64) -> Option<i64> {
    let valid_until_ms = parse_extra_field_as_ms(extra, "valid_until")?;
    if valid_until_ms <= anchor_ms {
        tracing::warn!(
            anchor_ms,
            valid_until_ms,
            "valid_until ≤ anchor_ms: invalid window skipped (note stays visible)"
        );
        return None;
    }
    Some(valid_until_ms)
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Validate (F-43)
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic validation gate for a distilled note before persistence.
///
/// Computes a quality_score (grounding × f17 × f47 × num/entity penalties); if the score
/// is below `spec.threshold`, the note is stored with degraded trust and the `quality-low`
/// tag. The gate is **non-blocking**: any scoring error falls back to `base_trust` so no
/// note is ever lost due to an embedder failure.
///
/// # Steps
///
/// 1. Compute quality via `compute_quality` (embedder + heuristics).
/// 2. Determine disposition: `pass` (score ≥ threshold) or `degrade`.
/// 3. Persist synthesis note via `client.persist_distill` (mark_processed=false).
/// 4. Mark each source processed=true + derived-into (non-fatal on per-source failure).
///
/// # Errors
///
/// Returns [`HandlerError::Business`] if the synthesis persist call fails.
pub async fn handle_validate(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    mt: Data<MultiTenantCfg>,
) -> Result<JobOutput, HandlerError> {
    let spec = match &job.record.spec.kind {
        Job::Validate(s) => s.clone(),
        other => {
            return Err(HandlerError::Business(format!(
                "validate: unexpected variant {}",
                job_kind_str(other)
            )));
        }
    };

    // Cross-tenant guard: terminally reject if tenant ≠ main (defense-in-depth).
    ensure_job_tenant(&spec.tenant_id, mt.enabled)?;

    // Grounding + score — best-effort; any error ⇒ neutral score (pass, no note loss).
    let (quality, mode) = match compute_quality(&embedder, &spec).await {
        Ok(q) => (q, "ok"),
        Err(e) => {
            tracing::warn!(note_id = %spec.note_id, error = %e,
                "validate: degraded scoring — falling back to base trust");
            (
                crate::quality_score::QualityScore {
                    score: 1.0, // neutral: pass, no degradation
                    grounding: -1.0,
                    num_penalty: 1.0,
                    entity_penalty: 1.0,
                },
                "degraded",
            )
        }
    };

    let pass = quality.score >= spec.threshold;
    let (final_trust, tags) = if pass {
        (spec.base_trust, Vec::new())
    } else {
        (
            (spec.base_trust * quality.score).clamp(0.0, 1.0),
            vec!["quality-low".to_string()],
        )
    };

    tracing::info!(
        note_id = %spec.note_id,
        quality_score = quality.score,
        grounding = quality.grounding,
        num_penalty = quality.num_penalty,
        entity_penalty = quality.entity_penalty,
        base_trust = spec.base_trust,
        final_trust,
        disposition = if pass { "accept" } else { "degrade" },
        grounding_mode = mode,
        "validate: quality_score"
    );

    // Persist the synthesis note.
    let mut persist_req = PersistDistillRequest::new(
        spec.note_id.to_string(),
        spec.tenant_id.clone().into(),
        spec.title.clone(),
        spec.body.clone(),
        "reference".to_string(),
    );
    persist_req.trust = Some(final_trust);
    persist_req.derived_from = spec.source_ids.iter().map(|id| id.to_string()).collect();
    persist_req.tags = tags;
    // expected_sha256, mark_processed (false), derived_into restent aux défauts.
    if let Err(e) = client.persist_distill(&persist_req).await {
        return Err(HandlerError::Business(format!(
            "validate: persist synthesis: {e}"
        )));
    }

    // Mark sources processed=true + derived-into (non-fatal per source).
    let synth_id_str = spec.note_id.to_string();
    let mut modified = Vec::new();
    for src_id in &spec.source_ids {
        let mut src_req = PersistDistillRequest::new(
            src_id.to_string(),
            spec.tenant_id.clone().into(),
            String::new(),
            String::new(),
            String::new(),
        );
        src_req.mark_processed = true;
        src_req.derived_into = Some(synth_id_str.clone());
        if let Err(e) = client.persist_distill(&src_req).await {
            tracing::warn!(note_id = %src_id, error = %e, "validate: source marking failed — non-fatal");
        } else {
            // src_id: &Ulid; Ulid is Copy — use dereference, not .0 (which yields u128).
            modified.push(*src_id);
        }
    }

    Ok(JobOutput {
        notes_created: vec![spec.note_id],
        notes_modified: modified,
        files: vec![],
        result_note_md: format!(
            "validate: note {} — score {:.3} → {}",
            spec.note_id,
            quality.score,
            if pass {
                "accept"
            } else {
                "degrade(quality-low)"
            }
        ),
    })
}

/// Compute the quality_score for a distilled note: embedding grounding + temporal
/// recency (f17) + source trust (f47) + numeric / entity penalties.
///
/// Returns `Err(String)` on any embedder failure; the caller falls back to neutral score.
async fn compute_quality(
    embedder: &Arc<dyn Embedder + Send + Sync>,
    spec: &ValidateSpec,
) -> Result<crate::quality_score::QualityScore, String> {
    if spec.source_texts.is_empty() {
        return Err("no source to compare against".to_string());
    }

    let synth_emb = embedder
        .embed(&spec.body)
        .await
        .map_err(|e| e.to_string())?;
    let src_refs: Vec<&str> = spec.source_texts.iter().map(String::as_str).collect();
    let src_embs = embedder
        .embed_batch(&src_refs)
        .await
        .map_err(|e| e.to_string())?;
    let centroid = crate::quality_score::centroid(&src_embs);

    // f17: mean of recency_factor over source ULID timestamps.
    // Formula mirrors gradatum_search::scoring::recency_factor (dep not wired to this crate).
    // half-life ≈ 69 days (LAMBDA = 0.01 day⁻¹).
    let now_ms = Utc::now().timestamp_millis();
    let f17 = if spec.source_ids.is_empty() {
        1.0f32
    } else {
        const LAMBDA: f64 = 0.01;
        const MS_PER_DAY: f64 = 86_400_000.0;
        let sum: f64 = spec
            .source_ids
            .iter()
            .map(|id| {
                let created_ms = id.timestamp_ms() as i64;
                let delta_ms = (now_ms - created_ms).max(0);
                (-LAMBDA * (delta_ms as f64 / MS_PER_DAY)).exp()
            })
            .sum();
        (sum / spec.source_ids.len() as f64) as f32
    };

    // f47: mean of source trust scores.
    let f47 = if spec.source_trusts.is_empty() {
        0.6f32
    } else {
        spec.source_trusts.iter().sum::<f32>() / spec.source_trusts.len() as f32
    };

    Ok(crate::quality_score::score_quality(
        &crate::quality_score::QualityInputs {
            synth_embedding: &synth_emb,
            source_centroid: &centroid,
            synth_body: &spec.body,
            source_texts: &spec.source_texts,
            f17_sources: f17,
            f47_sources: f47,
        },
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, EmbedSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
        JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, PurgeSpec,
        ReIndexMode, TriggerSource, ValidateSpec,
    };
    use ulid::Ulid;

    fn make_job(kind: Job, mode: JobMode) -> GradatumJob {
        let now = Utc::now();
        let class = JobClass::Agent;
        GradatumJob {
            priority: JobPriority::default_for(&class).as_u8(),
            record: JobRecord {
                id: Ulid::generate(),
                spec: JobSpec {
                    kind,
                    class,
                    mode,
                    scope: JobScope::VaultWide,
                    priority: JobPriority::High,
                },
                scheduling: JobScheduling {
                    trigger: TriggerSource::Demand,
                    scheduled_at: now,
                    await_jobs: vec![],
                    deadline: None,
                    cron_expr: None,
                },
                lifecycle: JobLifecycle {
                    status: JobStatus::Running,
                    created_at: now,
                    started_at: Some(now),
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
            },
        }
    }

    // Les tests dry-run ne nécessitent pas les dépendances Data — le handler retourne
    // avant tout accès aux deps. Tests Data injectés = tests d'intégration dans tests/.
    // On ne peut pas facilement construire Data<T> en unit test sans Apalis runtime.
    // Pattern : tester dry-run ici, tester le chemin Batch dans monitor_integration.rs.

    #[tokio::test]
    async fn curate_dry_run_returns_output() {
        let job = make_job(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                title: Some("Test note".to_string()),
                body: Some("Test body".to_string()),
                ..Default::default()
            }),
            JobMode::DryRun,
        );
        // Dry run retourne AVANT d'accéder aux deps — on peut appeler avec des Data vides.
        // Cependant, la signature apalis::Data<T> n'est pas facilement mockable sans runtime.
        // Ce test vérifie uniquement la logique DryRun via le trait DryRunAware.
        assert!(job.record.is_dry_run());
    }

    #[tokio::test]
    async fn embed_dry_run_is_detected() {
        let job = make_job(
            Job::Embed(EmbedSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobMode::DryRun,
        );
        assert!(job.record.is_dry_run());
    }

    #[tokio::test]
    async fn reindex_dry_run_is_detected() {
        let job = make_job(Job::ReIndex(ReIndexMode::FtsOnly), JobMode::DryRun);
        assert!(job.record.is_dry_run());
    }

    /// `handle_reindex` in Batch mode returns `Err(HandlerError::Business)` — never `Ok`.
    ///
    /// All modes (FtsOnly/MissingOnly/VectorsOnly/Full) are deferred.
    /// The handler explicitly rejects the job to avoid a silent misleading `Ok`.
    #[tokio::test]
    async fn handle_reindex_batch_returns_err_not_implemented() {
        use gradatum_embed::Noop;

        // Les deps _client/_embedder ne sont jamais accédés dans le chemin non-DryRun
        // (le handler retourne Err::Business avant tout accès).
        // Un mock minimal suffit pour satisfaire la signature du handler (worker-flip).
        struct NeverCalledClient;
        #[async_trait::async_trait]
        impl crate::internal_client::InternalClient for NeverCalledClient {
            async fn persist_curated(
                &self,
                _: &gradatum_dto::PersistCuratedRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn persist_embedding(
                &self,
                _: &gradatum_dto::PersistEmbeddingRequest,
            ) -> Result<
                gradatum_dto::EmbeddingOkResponse,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn persist_forget(
                &self,
                _: &gradatum_dto::PersistForgetRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn persist_distill(
                &self,
                _: &gradatum_dto::PersistDistillRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn delete_note(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn get_note(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                crate::internal_client::NoteReadDto,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn get_note_status(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn get_note_embedding(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<
                crate::internal_client::EmbeddingReadDto,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn get_trust(
                &self,
                _: &str,
                _: &str,
            ) -> Result<f32, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn title_lookup(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn id_lookup(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn list_notes_by_locus(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_by_status(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_garbage(
                &self,
                _: &str,
                _: i64,
                _: u32,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn search_fts_for_forget(
                &self,
                _: &str,
                _: &str,
                _: usize,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_notes_by_agent(
                &self,
                _: &str,
                _: &[String],
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
        }

        let client_data = Data::new(
            Arc::new(NeverCalledClient) as Arc<dyn crate::internal_client::InternalClient>
        );
        let embedder: Arc<dyn Embedder + Send + Sync> = Arc::new(Noop::new(384));
        let embedder_data = Data::new(embedder);

        for mode in [
            ReIndexMode::FtsOnly,
            ReIndexMode::MissingOnly,
            ReIndexMode::VectorsOnly,
            ReIndexMode::Full,
        ] {
            let job = make_job(Job::ReIndex(mode.clone()), JobMode::Batch);
            let result = handle_reindex(job, client_data.clone(), embedder_data.clone()).await;
            assert!(
                matches!(result, Err(HandlerError::Business(_))),
                "handle_reindex({mode:?}) doit retourner Err(Business) en v0.4.x, obtenu : {result:?}"
            );
        }
    }

    // ── Lot 5 : ensure_main_tenant (garde worker P0 cross-tenant) ──────────────

    #[test]
    fn ensure_main_tenant_accepts_main() {
        assert!(super::ensure_main_tenant("main").is_ok());
    }

    #[test]
    fn ensure_main_tenant_rejects_non_main() {
        let r = super::ensure_main_tenant("evil");
        assert!(
            matches!(r, Err(HandlerError::Business(_))),
            "tenant ≠ main → HandlerError::Business, obtenu : {r:?}"
        );
    }

    #[test]
    fn ensure_main_tenant_rejects_empty() {
        assert!(matches!(
            super::ensure_main_tenant(""),
            Err(HandlerError::Business(_))
        ));
    }

    // ── Tests C2 (EX-C2-3) — ensure_job_tenant / resolve_job_vault ───────────

    /// OFF : strictement le comportement `ensure_main_tenant` (byte-identical).
    #[test]
    fn ensure_job_tenant_off_is_legacy() {
        assert!(super::ensure_job_tenant("main", false).is_ok());
        assert!(matches!(
            super::ensure_job_tenant("research", false),
            Err(HandlerError::Business(_))
        ));
    }

    /// ON : tout tenant bien formé passe (l'autorisation est rendue à l'enqueue) ;
    /// un tenant mal formé est rejeté terminalement (`VaultId::parse`).
    #[test]
    fn ensure_job_tenant_on_parses() {
        assert!(super::ensure_job_tenant("research", true).is_ok());
        assert!(matches!(
            super::ensure_job_tenant("Bad Vault!", true),
            Err(HandlerError::Business(_))
        ));
        assert!(matches!(
            super::ensure_job_tenant("", true),
            Err(HandlerError::Business(_))
        ));
    }

    /// OFF : un seul vault existe, donc tout scope n'en portant aucun → "main" ;
    /// `Vault(v ≠ main)` rejeté (fail-closed).
    ///
    /// Garde d'anti-régression du lot A2 : le durcissement ne vise QUE le chemin ON.
    /// Le chemin OFF doit rester byte-identical — le cron distill y enqueue
    /// `JobScope::Locus(locus)` (`schedules.rs::build_distill_job_record`).
    #[test]
    fn resolve_job_vault_off_matrix() {
        use gradatum_core::job::JobScope;
        assert_eq!(
            super::resolve_job_vault(&JobScope::VaultWide, false).expect("VaultWide"),
            "main"
        );
        assert_eq!(
            super::resolve_job_vault(&JobScope::Locus("decisions".into()), false).expect("Locus"),
            "main"
        );
        assert_eq!(
            super::resolve_job_vault(&JobScope::Notes(vec![]), false).expect("Notes"),
            "main"
        );
        assert_eq!(
            super::resolve_job_vault(&JobScope::Session(ulid::Ulid::generate()), false)
                .expect("Session"),
            "main"
        );
        assert_eq!(
            super::resolve_job_vault(&JobScope::Vault("main".into()), false).expect("Vault(main)"),
            "main"
        );
        assert!(matches!(
            super::resolve_job_vault(&JobScope::Vault("research".into()), false),
            Err(HandlerError::Business(_))
        ));
    }

    /// ON : `Vault(v)` validé et retourné ; vault mal formé rejeté.
    #[test]
    fn resolve_job_vault_on_matrix() {
        use gradatum_core::job::JobScope;
        assert_eq!(
            super::resolve_job_vault(&JobScope::Vault("research".into()), true)
                .expect("Vault(research)"),
            "research"
        );
        // `Vault("main")` à ON : `main` n'a rien de spécial une fois le flag levé, c'est
        // un vault comme un autre — il passe par `VaultId::parse` et est rendu tel quel.
        // Le cas manquait : la matrice ON ne couvrait que le vault secondaire.
        assert_eq!(
            super::resolve_job_vault(&JobScope::Vault("main".into()), true).expect("Vault(main)"),
            "main"
        );
        assert!(matches!(
            super::resolve_job_vault(&JobScope::Vault("../Evil".into()), true),
            Err(HandlerError::Business(_))
        ));
    }

    /// A2 — ON : un scope ne portant AUCUN vault est refusé terminalement au lieu de
    /// retomber en silence sur "main".
    ///
    /// Discriminant : sur le code d'avant (catch-all `_ => Ok("main")`) les quatre
    /// variantes rendaient `Ok("main")` et chaque `assert!(matches!(.., Err(..)))`
    /// échoue. Le vault résolu scope des accès destructifs (`delete_note` dans
    /// `handle_purge`, `persist_forget` dans `handle_forget`) : élire "main" parmi N
    /// vaults écrivait dans le mauvais vault sans le dire.
    #[test]
    fn resolve_job_vault_on_rejects_scopes_carrying_no_vault() {
        use gradatum_core::job::JobScope;
        for scope in [
            JobScope::VaultWide,
            JobScope::Locus("decisions".into()),
            JobScope::Notes(vec![ulid::Ulid::generate()]),
            JobScope::Session(ulid::Ulid::generate()),
        ] {
            assert!(
                matches!(
                    super::resolve_job_vault(&scope, true),
                    Err(HandlerError::Business(_))
                ),
                "scope {scope:?} doit être refusé à ON (aucun vault porté)"
            );
        }
    }

    /// A2-bis — invariant « un job = exactement un vault » sur `Job::Forget`.
    ///
    /// Accepté : le `ForgetScope` est muet (`Topic.vault = None`, `Agent.vaults = []`) ou
    /// d'accord avec le vault du job. Refusé : tout désaccord, et tout `Agent` multi-vault
    /// (le fan-out N vaults ⇒ N jobs appartient au site d'enqueue, pas au handler).
    #[test]
    fn ensure_forget_scope_vault_accepts_only_agreement_on_one_vault() {
        use gradatum_core::ForgetScope;

        let accepted = [
            ForgetScope::Topic {
                query: "q".into(),
                vault: None,
                limit: None,
            },
            ForgetScope::Topic {
                query: "q".into(),
                vault: Some("vault-b".into()),
                limit: None,
            },
            ForgetScope::Locus {
                vault: "vault-b".into(),
                locus: "inbox/".into(),
            },
            ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec![],
            },
            ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec!["vault-b".into()],
            },
        ];
        for scope in accepted {
            assert!(
                super::ensure_forget_scope_vault(&scope, "vault-b").is_ok(),
                "scope d'accord avec le vault du job doit passer : {scope:?}"
            );
        }

        let rejected = [
            ForgetScope::Topic {
                query: "q".into(),
                vault: Some("main".into()),
                limit: None,
            },
            ForgetScope::Locus {
                vault: "main".into(),
                locus: "inbox/".into(),
            },
            ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec!["main".into()],
            },
            // Multi-vault : le fan-out n'a pas été fait à l'enqueue.
            ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec!["vault-b".into(), "main".into()],
            },
        ];
        for scope in rejected {
            assert!(
                matches!(
                    super::ensure_forget_scope_vault(&scope, "vault-b"),
                    Err(HandlerError::Business(_))
                ),
                "scope divergent ou multi-vault doit être refusé terminalement : {scope:?}"
            );
        }
    }

    /// Le rendu d'un scope dans un message **persisté en base** est borné.
    ///
    /// Discriminant : avec `{scope:?}`, `Notes` sérialise ses N ULIDs (26 o pièce) et
    /// `Locus` sa chaîne entière — les deux assertions de longueur tombent.
    #[test]
    fn scope_label_is_bounded_for_persisted_messages() {
        use gradatum_core::job::JobScope;

        let many = JobScope::Notes((0..500).map(|_| ulid::Ulid::generate()).collect());
        let label = super::scope_label(&many);
        assert_eq!(label, "Notes(500 ids)");

        let long = JobScope::Locus("x".repeat(4096));
        let label = super::scope_label(&long);
        assert!(
            label.chars().count() < 80,
            "un Locus non borné ne doit pas être recopié en base : {label}"
        );
    }

    #[tokio::test]
    async fn curate_unexpected_variant_check() {
        // Vérification que Backup n'est pas un Curate spec (logique de guard variant)
        let job = make_job(Job::Backup, JobMode::Batch);
        // La vérification du variant se fait dans le handler — vérifier que le Job::Backup
        // n'est pas un Job::Curate (invariant statique du type).
        assert!(!matches!(&job.record.spec.kind, Job::Curate(_)));
    }

    // ── Tests Job::Purge ──────────────────────────────────────────────────────

    /// `DryRunAware::is_dry_run()` detects `JobMode::DryRun` for `Job::Purge`.
    #[tokio::test]
    async fn purge_dry_run_via_job_mode_is_detected() {
        let job = make_job(
            Job::Purge(PurgeSpec {
                mode: gradatum_core::PurgeMode::Lifecycle,
                dry_run: false, // spec.dry_run = false
                grace_days: Some(30),
            }),
            JobMode::DryRun, // mais JobMode = DryRun → is_dry_run() = true
        );
        assert!(
            job.record.is_dry_run(),
            "JobMode::DryRun doit activer le dry-run même si spec.dry_run=false"
        );
    }

    /// `spec.dry_run=true` (default) activates dry-run even in Batch mode.
    #[tokio::test]
    async fn purge_dry_run_via_spec_is_detected() {
        let job = make_job(
            Job::Purge(PurgeSpec::default()), // dry_run=true par défaut
            JobMode::Batch,
        );
        // is_dry_run() retourne false (JobMode::Batch), mais spec.dry_run = true.
        // La double garde dans handle_purge couvrira les deux.
        assert!(
            !job.record.is_dry_run(),
            "JobMode::Batch → is_dry_run() = false"
        );
        assert!(
            matches!(&job.record.spec.kind, Job::Purge(s) if s.dry_run),
            "PurgeSpec::default() doit avoir dry_run=true"
        );
    }

    /// `Job::Purge` with an unexpected variant → `HandlerError::UnexpectedVariant`.
    ///
    /// Verifies the variant guard: `Job::Backup` ≠ `Job::Purge`.
    #[tokio::test]
    async fn purge_unexpected_variant_is_not_purge() {
        let job = make_job(Job::Backup, JobMode::Batch);
        assert!(!matches!(&job.record.spec.kind, Job::Purge(_)));
    }

    /// `PurgeSpec::default()`: conservative default values.
    #[test]
    fn purge_spec_default_values_in_handler_tests() {
        let spec = PurgeSpec::default();
        assert!(spec.dry_run, "dry_run doit être true par défaut");
        assert_eq!(spec.grace_days, Some(30));
    }

    // ── Tests F-55 TemporalIndex ──────────────────────────────────────────────

    /// Fallback : ExtraFields vide → anchor_src='created', anchor_ms=created_ms.
    #[test]
    fn resolve_temporal_anchor_fallback_to_created() {
        let extra = ExtraFields::empty();
        let created_ms = 1_700_000_000_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(ms, created_ms, "fallback doit retourner created_ms");
        assert_eq!(
            src,
            AnchorSrc::Created,
            "fallback doit retourner AnchorSrc::Created"
        );
    }

    /// Priority: `occurred_at` > `event-date` > `valid_from` > `created`.
    #[test]
    fn resolve_temporal_anchor_priority_occurred_at_wins() {
        let mut extra = ExtraFields::empty();
        // occurred_at + event-date tous deux présents → occurred_at doit gagner.
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("2024-03-15T10:00:00Z".to_string()),
        );
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("2024-01-01T00:00:00Z".to_string()),
        );
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-01-01T00:00:00Z".to_string()),
        );

        let created_ms = 0i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);

        assert_eq!(
            src,
            AnchorSrc::OccurredAt,
            "occurred_at doit prendre priorité"
        );
        // 2024-03-15T10:00:00Z = epoch ms
        let expected = chrono::DateTime::parse_from_rfc3339("2024-03-15T10:00:00Z")
            .expect("parsing test date")
            .timestamp_millis();
        assert_eq!(ms, expected, "anchor_ms doit correspondre à occurred_at");
    }

    /// Without `occurred_at`, `event-date` takes precedence.
    #[test]
    fn resolve_temporal_anchor_priority_event_date_second() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("2024-06-01T00:00:00Z".to_string()),
        );
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-01-01T00:00:00Z".to_string()),
        );

        let (_, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::EventDate,
            "event-date doit prendre priorité sur valid_from"
        );
    }

    /// Without `occurred_at` or `event-date`, `valid_from` takes precedence.
    #[test]
    fn resolve_temporal_anchor_priority_valid_from_third() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-09-15T00:00:00Z".to_string()),
        );

        let (_, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::ValidFrom,
            "valid_from doit prendre priorité sur created"
        );
    }

    /// A date-only string `YYYY-MM-DD` is accepted and parsed as the start of that UTC day.
    #[test]
    fn resolve_temporal_anchor_date_only_format() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("2024-03-15".to_string()),
        );

        let (ms, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::OccurredAt,
            "format date seule doit être accepté"
        );
        // 2024-03-15T00:00:00Z
        let expected = chrono::DateTime::parse_from_rfc3339("2024-03-15T00:00:00Z")
            .expect("parsing expected")
            .timestamp_millis();
        assert_eq!(ms, expected, "date seule → début du jour UTC");
    }

    /// An invalid format silently falls back to the next lower-priority field or `created`.
    #[test]
    fn resolve_temporal_anchor_invalid_format_falls_back() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("not-a-date".to_string()),
        );
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("aussi-invalide".to_string()),
        );

        let created_ms = 42_000_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(
            src,
            AnchorSrc::Created,
            "formats invalides → fallback created"
        );
        assert_eq!(ms, created_ms, "anchor_ms doit être created_ms en fallback");
    }

    /// A non-`String` `ExtraFields` value (e.g. `Integer`) is ignored; falls back to `created`.
    #[test]
    fn resolve_temporal_anchor_non_string_value_ignored() {
        let mut extra = ExtraFields::empty();
        // toml::Value::Integer — ne doit pas être parsé comme date
        extra.insert("occurred_at".to_string(), TomlValue::Integer(1_700_000_000));

        let created_ms = 99_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(
            src,
            AnchorSrc::Created,
            "valeur non-String doit être ignorée"
        );
        assert_eq!(ms, created_ms);
    }

    /// `AnchorSrc::as_db_str()` returns the canonical strings expected by migration 0013.
    #[test]
    fn anchor_src_as_db_str_canonical_values() {
        assert_eq!(AnchorSrc::OccurredAt.as_db_str(), "occurred_at");
        assert_eq!(AnchorSrc::EventDate.as_db_str(), "event-date");
        assert_eq!(AnchorSrc::ValidFrom.as_db_str(), "valid_from");
        assert_eq!(AnchorSrc::Created.as_db_str(), "created");
    }

    // ── Tests Lot 1 — extraction valid_until (v0.5.1) ────────────────────────

    /// No `valid_until` field → `None`.
    #[test]
    fn extract_valid_until_absent_returns_none() {
        let extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(result.is_none(), "sans valid_until → None");
    }

    /// `valid_until` in the future relative to `anchor` → `Some(ms)`.
    #[test]
    fn extract_valid_until_future_returns_some() {
        let mut extra = ExtraFields::empty();
        // anchor_ms = 1000, valid_until bien futur
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2030-01-01T00:00:00Z".to_string()),
        );
        let anchor_ms = 1_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(result.is_some(), "valid_until futur → Some");
        let expected = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(result.unwrap(), expected);
    }

    /// Date-only `YYYY-MM-DD` format accepted for `valid_until`.
    #[test]
    fn extract_valid_until_date_only_format() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2030-06-15".to_string()),
        );
        let anchor_ms = 1_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_some(),
            "format date seule accepté pour valid_until"
        );
        let expected = chrono::DateTime::parse_from_rfc3339("2030-06-15T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(result.unwrap(), expected);
    }

    /// `valid_until ≤ anchor` → `None` (invalid window, silently ignored).
    #[test]
    fn extract_valid_until_equal_to_anchor_returns_none() {
        let mut extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        // valid_until == anchor (borne incluse → invalide)
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2023-11-14T22:13:20Z".to_string()),
        );
        // Vérifie que c'est bien la valeur attendue
        let expected_ms = chrono::DateTime::parse_from_rfc3339("2023-11-14T22:13:20Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            expected_ms, anchor_ms,
            "précondition: anchor_ms = valid_until ms"
        );
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_none(),
            "valid_until == anchor → None (fenêtre invalide)"
        );
    }

    /// `valid_until < anchor` → `None` (invalid window).
    #[test]
    fn extract_valid_until_before_anchor_returns_none() {
        let mut extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        // valid_until bien avant anchor
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2020-01-01T00:00:00Z".to_string()),
        );
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_none(),
            "valid_until < anchor → None (fenêtre invalide)"
        );
    }

    /// Invalid `valid_until` format → `None` (silent fallback).
    #[test]
    fn extract_valid_until_invalid_format_returns_none() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("pas-une-date".to_string()),
        );
        let result = extract_valid_until(&extra, 1_000i64);
        assert!(result.is_none(), "format invalide → None");
    }

    // ── Tests parité SSOT serveur/worker ────────────────────────────────────

    /// `parse_extra_field_as_ms` delegates to `gradatum_core::parse_temporal_str_as_ms` —
    /// formats accepted by the server (HTTP 202) produce exactly the same millisecond
    /// values as those accepted by the worker, eliminating any risk of a silent
    /// `anchor_src=Created` fallback after a server 202 response.
    #[test]
    fn parse_extra_field_as_ms_ssot_parity_rfc3339_and_date_only() {
        let formats = [
            "2026-01-15T10:00:00Z",
            "2026-01-15T00:00:00+00:00",
            "2026-01-15",
        ];
        for s in formats {
            let mut extra = ExtraFields::empty();
            extra.insert("occurred_at".to_string(), TomlValue::String(s.to_string()));
            let worker_result = parse_extra_field_as_ms(&extra, "occurred_at");
            let server_result = gradatum_core::parse_temporal_str_as_ms(s);
            assert_eq!(
                worker_result, server_result,
                "parité worker/serveur échouée pour le format `{s}`"
            );
            assert!(
                worker_result.is_some(),
                "format `{s}` doit être accepté par les deux couches"
            );
        }
    }

    // ── Tests F-43 build_validate_job_record ─────────────────────────────────

    /// build_validate_job_record produces kind=Validate with an empty await_jobs.
    #[test]
    fn validate_job_record_has_validate_kind() {
        let spec = ValidateSpec {
            note_id: Ulid::generate(),
            tenant_id: "main".to_string(),
            title: "test title".to_string(),
            body: "test body".to_string(),
            source_ids: vec![],
            source_texts: vec![],
            source_trusts: vec![],
            base_trust: 0.6,
            threshold: ValidateSpec::default_threshold(),
        };
        let rec = build_validate_job_record(spec, "main", Ulid::generate());
        assert_eq!(
            gradatum_core::job::job_kind_str(&rec.spec.kind),
            "Validate",
            "job kind must be Validate"
        );
        assert!(
            matches!(rec.scheduling.await_jobs.as_slice(), []),
            "await_jobs must be empty (cascade engine not yet implemented)"
        );
    }

    /// Invalid format → both layers return `None` (SSOT parity on rejections).
    #[test]
    fn parse_extra_field_as_ms_ssot_parity_invalid_returns_none() {
        let invalids = ["pas-une-date", "", "2026-13-45"];
        for s in invalids {
            let mut extra = ExtraFields::empty();
            extra.insert("occurred_at".to_string(), TomlValue::String(s.to_string()));
            let worker_result = parse_extra_field_as_ms(&extra, "occurred_at");
            let server_result = gradatum_core::parse_temporal_str_as_ms(s);
            assert_eq!(
                worker_result, server_result,
                "parité rejet worker/serveur échouée pour `{s}`"
            );
            assert!(
                worker_result.is_none(),
                "format invalide `{s}` → None attendu"
            );
        }
    }
}
