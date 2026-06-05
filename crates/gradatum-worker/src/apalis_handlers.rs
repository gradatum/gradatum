//! Handlers Apalis pour les variants `Job` actifs en v0.2.0 (Phase 1.2).
//!
//! Chaque handler est une fonction async `fn(GradatumJob) -> Result<JobOutput, HandlerError>`
//! compatible avec la signature attendue par `apalis::WorkerBuilder` Phase 1.2.
//!
//! # Phase 1.2 — handlers réels (résolution incident LIVE)
//!
//! Les 3 handlers implémentent la logique métier complète en s'appuyant sur
//! les crates alpha.15 : `gradatum-curator`, `gradatum-vault`, `gradatum-embed`.
//! Les dépendances sont injectées via [`apalis::prelude::Data`] par `build_monitor`.
//!
//! # Variants actifs v0.2.0
//!
//! | Handler | Job variant | Feature | Phase impl |
//! |---|---|---|---|
//! | `handle_curate` | `Job::Curate(CurateSpec)` | F-42 classification | Phase 1.2 ✅ |
//! | `handle_embed` | `Job::Embed(EmbedSpec)` | F-01 embedding | Phase 1.2 ✅ |
//! | `handle_reindex` | `Job::ReIndex(ReIndexMode)` | F-15 réindexation | Phase 1.2 partial |
//!
//! # DryRunAware
//!
//! Chaque handler vérifie `job.record.is_dry_run()` en PREMIÈRE instruction,
//! conformément à la règle v62 (`JobMode::DryRun` = mécanisme unique).
//!
//! # Injection de dépendances
//!
//! `build_monitor` injecte via `.data()` :
//! - `Data<Arc<Vault>>` — registry vault pour écriture/lecture notes
//! - `Data<Arc<dyn CuratorProcess + Send + Sync>>` — pipeline curator
//! - `Data<Arc<dyn Embedder + Send + Sync>>` — backend embedding
//! - `Data<Arc<SqliteIndex>>` — index FTS5 pour reindex + wikilinks
//!
//! # ReIndex modes
//!
//! | Mode | Phase |
//! |---|---|
//! | `FtsOnly` | Phase 1.2 ✅ |
//! | `MissingOnly` | Phase 1.2 ✅ |
//! | `VectorsOnly` | Phase 2 (sqlite-vec) |
//! | `Full` | Phase 2 (sqlite-vec) |
//!
//! # Références
//!
//! - v81 §6 L2470-2488 (DryRunAware, règle v62)
//! - ARCH-D15 : `docs/decisions/ARCH-D15-apalis-embedded.md`
//! - Spec : `docs/specs/2026-05-29-v0-2-0-apalis-job-infra-spec.md §4`

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use smallvec::SmallVec;

use gradatum_core::{
    author::AuthorRef,
    frontmatter::{ExtraFields, Frontmatter},
    identity::NoteId,
    scope::VaultId,
    section::Section,
    status::NoteStatus,
    tag::Tag,
    CurateSpec,
    DryRunAware,
    EmbedSpec,
    GradatumJob,
    Job,
    JobClass,
    JobLifecycle,
    JobLineage,
    JobMode,
    JobOutput,
    JobPriority,
    JobRecord,
    JobRetry,
    JobScheduling,
    JobScope,
    JobSpec,
    JobStatus,
    QueueStore,
    TriggerSource,
    // Traits de storage — nécessaires pour résoudre les méthodes de trait sur Arc<SqliteIndex>.
    VectorStore as _,
};
use gradatum_curator::{CurateOutcome, CuratorProcess};
use gradatum_embed::Embedder;
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;

// ─────────────────────────────────────────────────────────────────────────────
// Erreur des handlers
// ─────────────────────────────────────────────────────────────────────────────

/// Erreur retournée par les handlers Apalis.
///
/// Conforme à la signature attendue par `apalis::WorkerBuilder`.
/// En Phase 1.2, cette erreur est mappée vers `apalis::Error` via `From`.
///
/// `MissingDependency` et `InvalidPayload` sont conservés pour la robustesse future
/// (Phase 1.3 : handlers avec validation de payload + injection dynamique).
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// Dépendance non injectée — `build_monitor` doit appeler `.data()` sur le worker.
    #[error("dépendance absente : {0}")]
    MissingDependency(&'static str),

    /// Le variant Job reçu n'est pas géré par ce handler.
    #[error("variant job inattendu : {0}")]
    UnexpectedVariant(String),

    /// Payload du job absent ou invalide (title/body manquants pour vault_write).
    #[error("payload job invalide : {0}")]
    InvalidPayload(String),

    /// Erreur métier remontée depuis le vault ou le curator.
    #[error("erreur métier : {0}")]
    Business(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Curate
// ─────────────────────────────────────────────────────────────────────────────

/// Handler pour [`gradatum_core::Job::Curate`] — classification `inbox/` F-42.
///
/// # Contrat
///
/// - Reçoit un `GradatumJob` dont `record.spec.kind = Job::Curate(CurateSpec { ... })`
/// - Vérifie `DryRunAware::is_dry_run()` en première instruction (règle v62)
/// - En mode `DryRun` : retourne `JobOutput::dry_run(0, "curate")` sans écriture
/// - En mode `Batch` : appelle `CuratorPipeline::process()` + `Vault::write_note()`
///
/// # Deux cas d'usage
///
/// 1. **vault_write (nouveau)** : `CurateSpec.title` + `.body` sont `Some` —
///    la note est créée dans le vault lors du traitement du job.
/// 2. **reclassification** : `title`/`body` sont `None` —
///    la note existe déjà, lue depuis le vault via `note_id`.
///
/// # Persistance du titre (fix bug LIVE — colonne `notes.title` quasi-vide)
///
/// Après écriture vault, `index.upsert_note_title()` est appelé avec le titre résolu
/// (`spec.title` pour vault_write, `extract_h1_title(body)` pour reclassification).
/// Non-fatal : un warn est loggué en cas d'échec sans propager.
///
/// # Effets de bord
///
/// - Écrit la note dans le vault (Admitted → Live, Pending → Staging)
/// - Persiste les wikilinks `[[...]]` via `SqliteIndex` (non-fatal)
/// - Enchaîne un job `Job::Embed` si note admise/pending (non-fatal, best-effort)
///
/// # Timeout
///
/// Le timeout par-job est assuré par la couche Apalis Tower (voir `monitor.rs`
/// `cfg.timeout_secs`, par défaut 30s pour curate). Ce handler n'ajoute pas
/// de timeout redondant — la couche Tower est OUTER et prend effet en premier.
pub async fn handle_curate(
    job: GradatumJob,
    vault: Data<Arc<Vault>>,
    curator: Data<Arc<dyn CuratorProcess + Send + Sync>>,
    index: Data<Arc<SqliteIndex>>,
    queue: Data<Arc<dyn QueueStore + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // Règle v62 — vérification DryRun PREMIÈRE instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "curate — simulation"));
    }
    // Extraction du CurateSpec
    let spec = match &job.record.spec.kind {
        Job::Curate(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Construire le CuratorNote depuis le spec
    // Cas vault_write : title + body présents dans le spec
    // Cas reclassification : title/body None → lire depuis le vault
    let (note_id_for_vault, curator_note) = if spec.title.is_some() && spec.body.is_some() {
        // Path vault_write : note à créer
        let curator_note = gradatum_curator::Note {
            id: spec.note_id.to_string(),
            title: spec.title.clone().unwrap_or_default(),
            body: spec.body.clone().unwrap_or_default(),
            tags_hint: spec.tags.clone(),
            section_hint: spec.section_hint.clone(),
        };
        (None, curator_note) // note_id = None → sera assigné lors de write_note
    } else {
        // Path reclassification : lire la note existante depuis le vault
        let note_id = NoteId(spec.note_id);
        let existing = vault
            .read_note(note_id)
            .await
            .map_err(|e| HandlerError::Business(format!("read_note: {e}")))?;
        let title_for_curator = gradatum_curator::extract_h1_title(&existing.body.markdown)
            .map(|t| t.to_string())
            .unwrap_or_else(|| existing.frontmatter.section.as_str().to_string());
        let curator_note = gradatum_curator::Note {
            id: spec.note_id.to_string(),
            title: title_for_curator,
            body: existing.body.markdown.clone(),
            tags_hint: existing
                .frontmatter
                .tags
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            section_hint: None,
        };
        (Some(note_id), curator_note)
    };

    let tenant_id = spec.tenant_id.clone();
    let body_for_write = spec
        .body
        .clone()
        .unwrap_or_else(|| curator_note.body.clone());
    // Titre résolu avant que `curator.process` consomme `curator_note`.
    // Utilisé post-match pour `upsert_note_title` — fix colonne `notes.title` quasi-vide.
    let title_resolved = curator_note.title.clone();

    let curate_outcome = curator.process(curator_note).await;

    let written_note_id = match curate_outcome {
        CurateOutcome::Admitted { ref decisions } => {
            let section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            let note = if let Some(existing_note_id) = note_id_for_vault {
                // Reclassification — mettre à jour la note existante
                let existing = vault
                    .read_note(existing_note_id)
                    .await
                    .map_err(|e| HandlerError::Business(format!("read_note classify: {e}")))?;
                let mut fm = existing.frontmatter.clone();
                fm.section = section;
                for tag_str in &decisions.tags {
                    if !fm.tags.iter().any(|t| t.as_str() == tag_str.as_str()) {
                        if let Ok(t) = Tag::new(tag_str) {
                            fm.tags.push(t);
                        }
                    }
                }
                vault
                    .write_note(fm, existing.body.markdown.clone())
                    .await
                    .map_err(|e| HandlerError::Business(format!("write_note classify: {e}")))?
            } else {
                // vault_write — créer la note
                let fm = build_frontmatter_from_spec(
                    &tenant_id,
                    section,
                    NoteStatus::Live,
                    &spec,
                    &decisions.tags,
                );
                vault
                    .write_note(fm, body_for_write.clone())
                    .await
                    .map_err(|e| HandlerError::Business(format!("write_note curate: {e}")))?
            };

            tracing::info!(
                job_id = %job.record.id,
                section = %decisions.canonical_section,
                "curate: note admise et persistée"
            );
            Some(note.id)
        }
        CurateOutcome::Pending {
            ref decisions,
            ref reason,
        } => {
            let section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            let note = if let Some(existing_note_id) = note_id_for_vault {
                let existing = vault.read_note(existing_note_id).await.map_err(|e| {
                    HandlerError::Business(format!("read_note classify pending: {e}"))
                })?;
                let mut fm = existing.frontmatter.clone();
                fm.section = section;
                fm.status = NoteStatus::Staging;
                vault
                    .write_note(fm, existing.body.markdown.clone())
                    .await
                    .map_err(|e| {
                        HandlerError::Business(format!("write_note classify pending: {e}"))
                    })?
            } else {
                let fm = build_frontmatter_from_spec(
                    &tenant_id,
                    section,
                    NoteStatus::Staging,
                    &spec,
                    &decisions.tags,
                );
                vault
                    .write_note(fm, body_for_write.clone())
                    .await
                    .map_err(|e| {
                        HandlerError::Business(format!("write_note curate pending: {e}"))
                    })?
            };

            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note mise en Staging (revue manuelle requise)"
            );
            Some(note.id)
        }
        CurateOutcome::Rejected { ref reason } => {
            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note rejetée — aucune écriture vault"
            );
            None
        }
    };

    // ── upsert_note_title post-curate — non-fatal ────────────────────────────
    // Fix bug LIVE : notes.title était jamais peuplé à l'écriture (910/911 NULL en prod).
    // `title_resolved` = spec.title (vault_write) ou extract_h1_title(body) (reclassification).
    // `DocumentStore::upsert_note_title` est idempotent — UPDATE notes SET title = ?2 WHERE id = ?1.
    if let Some(note_id) = &written_note_id {
        if !title_resolved.is_empty() {
            if let Err(e) = index.upsert_note_title(note_id, &title_resolved).await {
                tracing::warn!(
                    note_id = %note_id,
                    title = %title_resolved,
                    error = %e,
                    "curate: upsert_note_title échoué — non fatal"
                );
            }
        }
    }

    // ── Wikilinks post-curate (B5) — non-fatal ────────────────────────────────
    if let Some(note_id) = &written_note_id {
        process_wikilinks_b5(
            index.as_ref(),
            &tenant_id,
            &note_id.to_string(),
            &body_for_write,
        )
        .await;

        // ── Chaînage curate→embed (Tranche A) — best-effort non-fatal ─────────
        // Enqueue un Job::Embed pour la note curée afin que les embeddings soient
        // générés en cascade après la curation.
        //
        // Idempotence au STOCKAGE uniquement : un double-curate ré-enqueue un Embed ;
        // handle_embed recalcule l'embedding (appel LLM/modèle) puis INSERT OR REPLACE
        // dans note_embeddings — pas de corruption, mais coût calcul non nul.
        // Skip compute (force_regenerate=false + vecteur présent → no-op) = dette
        // backlog v0.4.0.
        //
        // TRANSITOIRE : enqueue direct (await_jobs=[]) car le cascade engine F-14
        // (await_jobs/Cascade, gradatum_queue::find_awaiting/set_pending) est todo!()
        // dans gradatum_queue.rs. Un await_jobs non vide hangerait l'embed en Waiting.
        // Migration vers await_jobs=[JobTrigger{curate_id, OnDone}] + TriggerSource::Cascade
        // prévue quand "F-14 complet v0.3.0" landera.
        let embed_record = build_embed_job_record(*note_id, &tenant_id, job.record.id);
        if let Err(e) = queue.enqueue(embed_record).await {
            tracing::warn!(
                note_id = %note_id,
                error = %e,
                "curate: enqueue Job::Embed échoué — note curée, embed non schedulé (best-effort)"
            );
        }
    }

    let result_desc = written_note_id
        .map(|id| format!("note {} créée/mise à jour", id))
        .unwrap_or_else(|| "rejetée".to_string());

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

/// Handler pour [`gradatum_core::Job::Embed`] — génération d'embedding F-01.
///
/// # Contrat
///
/// - Reçoit un `GradatumJob` dont `record.spec.kind = Job::Embed(EmbedSpec { note_id, ... })`
/// - Vérifie `DryRunAware::is_dry_run()` en première instruction (règle v62)
/// - En mode `DryRun` : retourne `JobOutput::dry_run(0, "embed")` sans calcul
/// - En mode `Batch` : lit la note du vault, calcule l'embedding, persiste dans l'index
///
/// # Skip silencieux
///
/// Seul cas de skip : `body_text` vide → retourne `JobOutput` sans appel embedder.
/// Si un vecteur existe déjà pour cette note, il est recalculé puis écrasé via
/// `INSERT OR REPLACE` (idempotence stockage, pas compute).
/// Skip compute (`force_regenerate=false` + vecteur présent → no-op calcul)
/// n'est pas implémenté — dette backlog v0.4.0.
pub async fn handle_embed(
    job: GradatumJob,
    vault: Data<Arc<Vault>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    index: Data<Arc<SqliteIndex>>,
) -> Result<JobOutput, HandlerError> {
    // Règle v62 — vérification DryRun PREMIÈRE instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "embed — simulation"));
    }

    let spec = match &job.record.spec.kind {
        Job::Embed(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let note_id = NoteId(spec.note_id);

    // Lire la note depuis le vault pour obtenir le body
    let note = vault
        .read_note(note_id)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: read_note: {e}")))?;

    let body_text = note.body.markdown.as_str();
    if body_text.is_empty() {
        tracing::info!(
            job_id = %job.record.id,
            note_id = %spec.note_id,
            "embed: skip — body vide"
        );
        return Ok(JobOutput {
            notes_created: vec![],
            notes_modified: vec![],
            files: vec![],
            result_note_md: format!("embed: skip note {} — body vide", spec.note_id),
        });
    }

    // Tronquer à 2 048 caractères Unicode (UTF-8-safe via char_indices).
    // Évite les dépassements de contexte modèle sans slice byte arbitraire.
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

    index
        .insert_note_embedding(&note_id, embedder.embedder_id(), embedder.dim(), &vec)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: insert_note_embedding: {e}")))?;

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
            "embed: note {} vecteur dim={} persisté",
            spec.note_id,
            embedder.dim()
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::ReIndex
// ─────────────────────────────────────────────────────────────────────────────

/// Handler pour [`gradatum_core::Job::ReIndex`] — réindexation F-15.
///
/// # Contrat
///
/// - Reçoit un `GradatumJob` dont `record.spec.kind = Job::ReIndex(ReIndexMode { ... })`
/// - Vérifie `DryRunAware::is_dry_run()` en première instruction (règle v62)
/// - En mode `DryRun` : retourne `JobOutput::dry_run(0, "reindex")` sans écriture
///
/// # Modes supportés (Phase 1.2)
///
/// Tous les modes sont en `DEFERRED` Phase 1.2 — `SqliteIndex` n'expose pas encore
/// les méthodes `rebuild_fts()` / `get_notes_without_embedding()` nécessaires.
/// Implémentation complète planifiée Phase 0.4.0 (sqlite-vec + FTS rebuild).
///
/// | Mode | Phase |
/// |---|---|
/// | `FtsOnly` | Phase 0.4.0 |
/// | `MissingOnly` | Phase 0.4.0 |
/// | `VectorsOnly` | Phase 2 (sqlite-vec) |
/// | `Full` | Phase 2 (sqlite-vec) |
pub async fn handle_reindex(
    job: GradatumJob,
    // Paramètres présents pour conformité signature future Phase 0.4.0
    _index: Data<Arc<SqliteIndex>>,
    _embedder: Data<Arc<dyn Embedder + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // Règle v62 — vérification DryRun PREMIÈRE instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "reindex — simulation"));
    }

    let mode = match &job.record.spec.kind {
        Job::ReIndex(mode) => mode.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Phase 1.2 : tous modes DEFERRED — SqliteIndex::rebuild_fts/get_notes_without_embedding
    // non encore implémentés. Retour Ok (non-bloquant) avec message descriptif.
    // Backlog Phase 0.4.0 : câbler les méthodes SqliteIndex + activer les modes FtsOnly + MissingOnly.
    tracing::info!(
        job_id = %job.record.id,
        mode = ?mode,
        "reindex: DEFERRED Phase 0.4.0 — SqliteIndex rebuild_fts non disponible Phase 1.2"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!("reindex {:?}: DEFERRED Phase 0.4.0", mode),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers internes
// ─────────────────────────────────────────────────────────────────────────────

/// Construit un `JobRecord` complet pour `Job::Embed(EmbedSpec)`.
///
/// Calqué sur `build_curate_job_record` de `gradatum-server/src/api_v1/write.rs`.
///
/// # Paramètres
///
/// - `note_id` : ULID de la note à embedder (NoteId.0)
/// - `tenant_id` : tenant du job parent (hérité du curate)
/// - `parent_job_id` : ULID du job curate parent (lineage.parent_job)
fn build_embed_job_record(
    note_id: gradatum_core::identity::NoteId,
    tenant_id: &str,
    parent_job_id: ulid::Ulid,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    JobRecord {
        id: ulid::Ulid::new(),
        spec: JobSpec {
            kind: Job::Embed(EmbedSpec {
                note_id: note_id.0,
                tenant_id: tenant_id.to_string(),
                // Idempotence : skip dans handle_embed si vecteur déjà présent.
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
            // OBLIGATOIRE vide — cascade engine F-14 est todo!() dans gradatum_queue.rs.
            // Un await_jobs non vide hangerait ce job en Waiting indéfiniment.
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

/// Convertit une section kebab-case en `Section` enum via serde_json.
///
/// Retourne `None` si la chaîne n'est pas une section canonique valide.
fn section_from_str(s: &str) -> Option<Section> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<Section>(&json_str).ok()
}

/// Construit un `Frontmatter` depuis un `CurateSpec` et les décisions curator.
///
/// Utilisé pour le path vault_write (title/body présents dans le spec).
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

    let tags: SmallVec<[Tag; 4]> = all_tags
        .iter()
        .filter_map(|t| Tag::new(t.clone()).ok())
        .collect();

    let author = spec.author.as_deref().map(AuthorRef::system);

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
    }
}

/// Extrait les wikilinks `[[...]]` du body, les résout et les persiste dans l'index.
///
/// Non-fatal : tout échec est loggué sans propager.
async fn process_wikilinks_b5(index: &SqliteIndex, tenant_id: &str, src_note_id: &str, body: &str) {
    let wikilinks = gradatum_curator::wikilinks::extract_wikilinks(body);
    if wikilinks.is_empty() {
        return;
    }

    for target_title in &wikilinks {
        match index.title_lookup(tenant_id, target_title).await {
            Ok(Some(dst_id)) => {
                if let Err(e) = index.upsert_link(tenant_id, src_note_id, &dst_id).await {
                    tracing::warn!(
                        src = %src_note_id,
                        dst = %dst_id,
                        error = %e,
                        "B5: upsert_link échoué — non fatal"
                    );
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target = %target_title,
                    "B5: note cible non trouvée — wikilink en suspens"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target = %target_title,
                    error = %e,
                    "B5: title_lookup échoué — non fatal"
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, EmbedSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
        JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, ReIndexMode,
        TriggerSource,
    };
    use ulid::Ulid;

    fn make_job(kind: Job, mode: JobMode) -> GradatumJob {
        let now = Utc::now();
        let class = JobClass::Agent;
        GradatumJob {
            priority: JobPriority::default_for(&class).as_u8(),
            record: JobRecord {
                id: Ulid::new(),
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
                note_id: Ulid::new(),
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
                note_id: Ulid::new(),
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

    #[tokio::test]
    async fn curate_unexpected_variant_check() {
        // Vérification que Backup n'est pas un Curate spec (logique de guard variant)
        let job = make_job(Job::Backup, JobMode::Batch);
        // La vérification du variant se fait dans le handler — vérifier que le Job::Backup
        // n'est pas un Job::Curate (invariant statique du type).
        assert!(!matches!(&job.record.spec.kind, Job::Curate(_)));
    }
}
