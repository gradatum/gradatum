//! Tests d'intégration — handler `handle_purge` (F-32C, volet C).
//!
//! Vérifie :
//! - Dry-run (via JobMode::DryRun) : liste les candidats sans supprimer aucun fichier.
//! - Dry-run (via spec.dry_run=true) : idem avec mode Batch mais spec.dry_run actif.
//! - Mode réel : supprime note + .history/ + redirect_table, retourne le compte.
//! - Note Garbage récente (< grace) : épargnée.
//! - Note non-Garbage : jamais touchée même si listée par erreur (TOCTOU guard).

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::error::GradatumError;
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::index::{FileChecksumEntry, NoteRecord, TemporalEntry};
use gradatum_core::index_store::{AuthorRow, LessonHitRaw, Lineage, SearchHitRaw};
use gradatum_core::note::Note;
use gradatum_core::scope::OverrideScope;
use gradatum_core::{
    frontmatter::{ExtraFields, Frontmatter},
    scope::VaultId,
    section::Section,
    status::NoteStatus,
    GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
    JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, PurgeMode, PurgeSpec, TriggerSource,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Construit un `GradatumJob` pour `Job::Purge`.
fn make_purge_job(spec: PurgeSpec, mode: JobMode) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::System;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Purge(spec),
                class,
                mode,
                scope: JobScope::VaultWide,
                priority: JobPriority::Low,
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

/// Construit un Frontmatter minimal pour les tests.
fn make_frontmatter(section: Section, status: NoteStatus) -> Frontmatter {
    let now = Utc::now();
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: Some(now),
        tags: Default::default(),
        author: None,
        created: now,
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Fixture de test : vault + index en mémoire.
struct PurgeFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

async fn make_fixture() -> PurgeFixture {
    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();
    PurgeFixture {
        vault,
        index,
        _tmp: tmp,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// handle_purge en dry-run (JobMode::DryRun + spec.dry_run=false) :
/// liste les notes Garbage éligibles sans rien supprimer.
///
/// Comportement attendu : JobOutput::dry_run retourné, notes toujours présentes
/// dans l'index après exécution.
#[tokio::test]
async fn purge_dry_run_via_job_mode_does_not_delete() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    // Créer une note Live, puis la passer en Garbage.
    let note = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "body test dry-run".to_string(),
        )
        .await
        .expect("write_note");
    let note_id = note.id;

    // Passer en Garbage (transition Live→Garbage autorisée).
    fixture
        .vault
        .update_status(note_id, NoteStatus::Garbage, None)
        .await
        .expect("update_status Live→Garbage");

    // Job dry-run via JobMode::DryRun, spec.dry_run=false, grace_days=None.
    let job = make_purge_job(
        PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: None,
        },
        JobMode::DryRun,
    );

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(Arc::clone(&fixture.index) as std::sync::Arc<dyn gradatum_core::index::Index>),
    )
    .await
    .expect("handle_purge dry-run ne doit pas échouer");

    // Dry-run : résultat contient "DRY-RUN" et aucune note créée/modifiée.
    assert!(
        result.result_note_md.contains("DRY-RUN"),
        "résultat dry-run doit contenir DRY-RUN : {}",
        result.result_note_md
    );
    assert!(
        result.notes_created.is_empty(),
        "dry-run ne doit pas créer de notes"
    );

    // La note doit toujours être présente dans l'index (pas supprimée).
    let still_there = fixture
        .index
        .get_note_status("main", &note_id.to_string())
        .await
        .expect("get_note_status")
        .expect("note doit être présente après dry-run");
    assert_eq!(
        still_there,
        NoteStatus::Garbage,
        "la note doit rester Garbage après dry-run"
    );
}

/// handle_purge en dry-run via spec.dry_run=true (mode Batch) :
/// même comportement que JobMode::DryRun — aucune suppression.
#[tokio::test]
async fn purge_dry_run_via_spec_flag_does_not_delete() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    let note = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "body test spec dry-run".to_string(),
        )
        .await
        .expect("write_note");
    let note_id = note.id;

    fixture
        .vault
        .update_status(note_id, NoteStatus::Garbage, None)
        .await
        .expect("update_status");

    // spec.dry_run=true (défaut PurgeSpec) en mode Batch
    let job = make_purge_job(PurgeSpec::default(), JobMode::Batch);

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(Arc::clone(&fixture.index) as std::sync::Arc<dyn gradatum_core::index::Index>),
    )
    .await
    .expect("handle_purge spec.dry_run ne doit pas échouer");

    assert!(
        result.result_note_md.contains("DRY-RUN"),
        "spec.dry_run=true doit produire dry-run : {}",
        result.result_note_md
    );

    // Note toujours présente.
    let status = fixture
        .index
        .get_note_status("main", &note_id.to_string())
        .await
        .expect("get_note_status")
        .expect("note présente");
    assert_eq!(status, NoteStatus::Garbage);
}

/// handle_purge mode réel (spec.dry_run=false, grace_days=None) :
/// supprime la note Garbage + .history/ + redirect_table.
#[tokio::test]
async fn purge_real_deletes_garbage_note() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    // Créer une note et la mettre en Garbage.
    let note = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "body à supprimer".to_string(),
        )
        .await
        .expect("write_note");
    let note_id = note.id;

    fixture
        .vault
        .update_status(note_id, NoteStatus::Garbage, None)
        .await
        .expect("update_status Live→Garbage");

    // Job réel : dry_run=false, grace_days=None (pas de délai).
    let job = make_purge_job(
        PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: None,
        },
        JobMode::Batch,
    );

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(Arc::clone(&fixture.index) as std::sync::Arc<dyn gradatum_core::index::Index>),
    )
    .await
    .expect("handle_purge réel ne doit pas échouer");

    // Le résultat doit indiquer 1 suppression.
    assert!(
        result.result_note_md.contains("1 note(s) supprimée(s)"),
        "résultat doit indiquer 1 suppression : {}",
        result.result_note_md
    );

    // La note doit être absente de l'index après purge.
    let status_after = fixture
        .index
        .get_note_status("main", &note_id.to_string())
        .await
        .expect("get_note_status après purge");
    assert!(
        status_after.is_none(),
        "la note doit être absente de l'index après purge réelle"
    );
}

/// handle_purge mode réel avec grace_days=30 :
/// une note mise en Garbage il y a moins de 30j (maintenant) est épargnée.
#[tokio::test]
async fn purge_spares_recent_garbage_within_grace() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    // Créer une note Live et la passer en Garbage maintenant.
    let note = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "body récent".to_string(),
        )
        .await
        .expect("write_note");
    let note_id = note.id;

    fixture
        .vault
        .update_status(note_id, NoteStatus::Garbage, None)
        .await
        .expect("update_status");

    // grace_days=30 → cutoff = now - 30j
    // La note vient d'être mise en Garbage → status_changed ≈ now > cutoff → épargnée.
    let job = make_purge_job(
        PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: Some(30),
        },
        JobMode::Batch,
    );

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(Arc::clone(&fixture.index) as std::sync::Arc<dyn gradatum_core::index::Index>),
    )
    .await
    .expect("handle_purge grace ne doit pas échouer");

    // Aucune suppression — la note est dans la grace period.
    assert!(
        result.result_note_md.contains("0 note(s) supprimée(s)"),
        "note récente doit être épargnée (grace_days=30) : {}",
        result.result_note_md
    );

    // La note doit toujours être dans l'index.
    let status = fixture
        .index
        .get_note_status("main", &note_id.to_string())
        .await
        .expect("get_note_status")
        .expect("note doit être présente après grace");
    assert_eq!(status, NoteStatus::Garbage);
}

/// Note non-Garbage (Live) : jamais touchée, même en mode réel sans grace.
///
/// Vérifie l'invariant de sécurité : seules les notes Garbage sont éligibles.
#[tokio::test]
async fn purge_never_touches_non_garbage_note() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    // Créer une note Live — pas de transition en Garbage.
    let note = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "note Live — ne doit pas être supprimée".to_string(),
        )
        .await
        .expect("write_note");
    let note_id = note.id;

    // Job réel sans grace period — mais aucune note Garbage.
    let job = make_purge_job(
        PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: None,
        },
        JobMode::Batch,
    );

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(Arc::clone(&fixture.index) as std::sync::Arc<dyn gradatum_core::index::Index>),
    )
    .await
    .expect("handle_purge note Live ne doit pas échouer");

    // Aucune suppression.
    assert!(
        result.result_note_md.contains("0 note(s) supprimée(s)"),
        "aucune note Garbage → 0 suppression : {}",
        result.result_note_md
    );

    // La note Live est toujours présente.
    let status = fixture
        .index
        .get_note_status("main", &note_id.to_string())
        .await
        .expect("get_note_status")
        .expect("note Live doit être présente");
    assert_eq!(status, NoteStatus::Live, "note Live non touchée");
}
/// Décorateur d'index injectant une erreur sur `get_note_status` pour un ULID
/// désigné — simule une note au statut hors-enum (ex. `'downgraded'`) apparue
/// pendant la boucle de purge (TOCTOU). Tout le reste délègue à l'index réel.
struct FaultStatusIndex {
    inner: Arc<SqliteIndex>,
    fault_id: String,
}

#[async_trait::async_trait]
impl gradatum_core::DocumentStore for FaultStatusIndex {
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError> {
        self.inner.write_note(note).await
    }
    async fn get_content_hash(&self, id: NoteId) -> Result<Option<ContentHash>, GradatumError> {
        self.inner.get_content_hash(id).await
    }
    async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        self.inner.get_note(tenant_id, note_id_ulid).await
    }
    async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.inner.list_by_status(vault_id, status).await
    }
    async fn downgrade_note(
        &self,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        self.inner
            .downgrade_note(note_id, reason, replaced_by)
            .await
    }
    async fn patch_note_status(
        &self,
        note_id: &NoteId,
        status: Option<&str>,
        status_reason: Option<&str>,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        self.inner
            .patch_note_status(note_id, status, status_reason, replaced_by)
            .await
    }
    async fn upsert_note_title(&self, note_id: &NoteId, title: &str) -> Result<(), GradatumError> {
        self.inner.upsert_note_title(note_id, title).await
    }
    async fn update_note_locus(
        &self,
        note_id: &NoteId,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), GradatumError> {
        self.inner.update_note_locus(note_id, new_locus).await
    }
    async fn mark_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
        by: Option<&str>,
    ) -> Result<(), GradatumError> {
        self.inner.mark_forgotten(vault_id, note_id, by).await
    }
    async fn unmark_forgotten(&self, vault_id: &str, note_id: &str) -> Result<(), GradatumError> {
        self.inner.unmark_forgotten(vault_id, note_id).await
    }
    async fn list_forgotten(
        &self,
        vault_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<(String, Option<String>, String, i64, Option<String>)>, GradatumError> {
        self.inner.list_forgotten(vault_id, limit, cursor).await
    }
    async fn count_forgotten(&self, vault_id: &str) -> Result<usize, GradatumError> {
        self.inner.count_forgotten(vault_id).await
    }
    async fn count_notes_by_status(
        &self,
        vault_id: &str,
    ) -> Result<std::collections::HashMap<String, u64>, GradatumError> {
        self.inner.count_notes_by_status(vault_id).await
    }
}

#[async_trait::async_trait]
impl gradatum_core::IndexStore for FaultStatusIndex {
    async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.inner.search_fts(vault_id, query, limit).await
    }
    async fn search_fts_scored(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        self.inner
            .search_fts_scored(vault_id, query, limit, include_downgraded)
            .await
    }
    async fn upsert_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
        schema_version: u32,
        payload_toml: &str,
    ) -> Result<(), GradatumError> {
        self.inner
            .upsert_override_raw(note_id, scope, override_type, schema_version, payload_toml)
            .await
    }
    async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        self.inner
            .get_override_raw(note_id, scope, override_type)
            .await
    }
    async fn upsert_file_checksum(&self, entry: &FileChecksumEntry) -> Result<(), GradatumError> {
        self.inner.upsert_file_checksum(entry).await
    }
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        self.inner.list_file_checksums().await
    }
    async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        self.inner
            .get_note_created_and_indegree(vault_id, note_id)
            .await
    }
    async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        self.inner
            .search_fts_with_snippet(
                vault_id,
                query,
                limit,
                include_downgraded,
                section,
                locus,
                status,
            )
            .await
    }
    async fn recall_lessons(
        &self,
        vault_id: &VaultId,
        class: &str,
        limit: usize,
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        self.inner.recall_lessons(vault_id, class, limit).await
    }
    async fn list_review_queue(
        &self,
        vault_id: &VaultId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<gradatum_core::ReviewQueueRow>, GradatumError> {
        self.inner.list_review_queue(vault_id, cursor, limit).await
    }
    async fn count_review_queue(&self, vault_id: &VaultId) -> Result<u64, GradatumError> {
        self.inner.count_review_queue(vault_id).await
    }
    async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.inner.title_lookup(vault_id, title).await
    }
    async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.inner.live_note_count(vault_id).await
    }
    async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        self.inner.distinct_authors(vault_id).await
    }
    async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        self.inner.distinct_tags(vault_id).await
    }
    async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        self.inner.neighbors(vault_id, note_id, depth).await
    }
    async fn backlinks(&self, vault_id: &str, note_id: &str) -> Result<Vec<String>, GradatumError> {
        self.inner.backlinks(vault_id, note_id).await
    }
    async fn trace_lineage(&self, vault_id: &str, note_id: &str) -> Result<Lineage, GradatumError> {
        self.inner.trace_lineage(vault_id, note_id).await
    }
    async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        self.inner
            .list_notes(vault_id, section, limit, cursor)
            .await
    }
    async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.inner.total_body_size_bytes(vault_id).await
    }
    async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        self.inner
            .upsert_link(vault_id, src_note_id, dst_note_id)
            .await
    }
    async fn get_titles_sections(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        self.inner.get_titles_sections(vault_id, ids).await
    }
    async fn get_trust(&self, id: &NoteId) -> Result<Option<f32>, GradatumError> {
        self.inner.get_trust(id).await
    }
    async fn get_trust_and_provenance(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(Option<f32>, Option<String>), GradatumError> {
        self.inner.get_trust_and_provenance(vault_id, note_id).await
    }
    async fn upsert_redirect(
        &self,
        slug: &str,
        ulid: &Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError> {
        self.inner.upsert_redirect(slug, ulid, renamed_at_ms).await
    }
    async fn resolve_redirect(&self, slug: &str) -> Result<Option<Ulid>, GradatumError> {
        self.inner.resolve_redirect(slug).await
    }
    async fn search_fts_for_forget(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.inner
            .search_fts_for_forget(vault_id, query, limit)
            .await
    }
    async fn list_notes_by_locus_prefix(
        &self,
        vault_id: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.inner
            .list_notes_by_locus_prefix(vault_id, prefix)
            .await
    }
    async fn list_notes_by_agent(
        &self,
        agent_id: &str,
        vaults: &[String],
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.inner.list_notes_by_agent(agent_id, vaults).await
    }
    async fn set_note_trust(&self, id: &NoteId, trust: f32) -> Result<usize, GradatumError> {
        self.inner.set_note_trust(id, trust).await
    }
    async fn write_temporal_entry(&self, entry: &TemporalEntry) -> Result<(), GradatumError> {
        self.inner.write_temporal_entry(entry).await
    }
    async fn timeline(
        &self,
        vault_id: &VaultId,
        filter: &gradatum_core::temporal_query::TimelineFilter,
    ) -> Result<Vec<gradatum_core::temporal_query::TimelineRow>, GradatumError> {
        self.inner.timeline(vault_id, filter).await
    }
    async fn delete_redirect_by_ulid(&self, ulid_str: &str) -> Result<usize, GradatumError> {
        self.inner.delete_redirect_by_ulid(ulid_str).await
    }
    async fn delete_note_from_index(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        self.inner.delete_note_from_index(vault_id, note_id).await
    }
    async fn list_garbage_older_than(
        &self,
        vault_id: &str,
        cutoff_ms: i64,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.inner
            .list_garbage_older_than(vault_id, cutoff_ms)
            .await
    }
    async fn get_note_section(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.inner.get_note_section(vault_id, note_id).await
    }
    async fn is_note_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        self.inner.is_note_forgotten(vault_id, note_id).await
    }
    async fn get_note_status(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<NoteStatus>, GradatumError> {
        if note_id == self.fault_id {
            return Err(GradatumError::Storage(format!(
                "parse NoteStatus 'downgraded' : unknown variant (fault injecté) pour {note_id}"
            )));
        }
        self.inner.get_note_status(vault_id, note_id).await
    }
}

#[async_trait::async_trait]
impl gradatum_core::VectorStore for FaultStatusIndex {
    async fn insert_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError> {
        self.inner
            .insert_note_embedding(note_id, embedder_id, dim, vector)
            .await
    }
    async fn get_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        self.inner.get_note_embedding(note_id, embedder_id).await
    }
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        self.inner
            .search_semantic(vault_id, embedder_id, query_emb, limit, locus)
            .await
    }
}

/// handle_purge mode réel : une note candidate dont `get_note_status` est illisible
/// (statut hors-enum apparu pendant la boucle, ex. `'downgraded'`) NE DOIT PAS
/// avorter le batch. La note fautive est comptée ignorée, les autres notes Garbage
/// sont purgées normalement. (P2 audit W1/W2 — robustesse TOCTOU.)
#[tokio::test]
async fn purge_tolerates_unparseable_status_and_continues_batch() {
    use gradatum_worker::apalis_handlers::handle_purge;

    let fixture = make_fixture().await;

    // Deux notes Garbage légitimes (status='garbage' → listées comme candidates).
    let bad = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "note au statut illisible".to_string(),
        )
        .await
        .expect("write_note bad");
    let good = fixture
        .vault
        .write_note(
            make_frontmatter(Section::Decisions, NoteStatus::Live),
            "note purgeable normale".to_string(),
        )
        .await
        .expect("write_note good");
    for id in [bad.id, good.id] {
        fixture
            .vault
            .update_status(id, NoteStatus::Garbage, None)
            .await
            .expect("update_status Live→Garbage");
    }

    // Index décoré : get_note_status erre pour `bad.id` (simule statut hors-enum).
    let faulty: Arc<dyn gradatum_core::index::Index> = Arc::new(FaultStatusIndex {
        inner: Arc::clone(&fixture.index),
        fault_id: bad.id.to_string(),
    });

    let job = make_purge_job(
        PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: None,
        },
        JobMode::Batch,
    );

    let result = handle_purge(
        job,
        Data::new(Arc::clone(&fixture.vault)),
        Data::new(faulty),
    )
    .await
    .expect("le batch NE DOIT PAS avorter sur un statut illisible");

    // 1 supprimée (good), 1 ignorée (bad) — batch complété malgré l'erreur.
    assert!(
        result.result_note_md.contains("1 note(s) supprimée(s)"),
        "1 suppression attendue (good) : {}",
        result.result_note_md
    );
    assert!(
        result.result_note_md.contains("1 ignorée(s)"),
        "1 note ignorée attendue (bad, statut illisible) : {}",
        result.result_note_md
    );

    // good réellement purgée (absente de l'index réel) ; bad toujours présente.
    assert!(
        fixture
            .index
            .get_note_status("main", &good.id.to_string())
            .await
            .expect("get_note_status good")
            .is_none(),
        "la note good doit être purgée"
    );
    assert_eq!(
        fixture
            .index
            .get_note_status("main", &bad.id.to_string())
            .await
            .expect("get_note_status bad"),
        Some(NoteStatus::Garbage),
        "la note bad (ignorée) reste en Garbage"
    );
}
