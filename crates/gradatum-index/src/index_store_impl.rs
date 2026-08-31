//! `impl IndexStore for SqliteIndex`.
//!
//! All methods delegate to inherent methods on `SqliteIndex`
//! (defined in `sqlite.rs` and `queries.rs`).
//!
//! The impl covers the whole `IndexStore` trait. A few of note:
//! `get_note_created_and_indegree`, `search_fts_with_snippet`, `title_lookup`,
//! `live_note_count`, `distinct_authors`, `distinct_tags`, `neighbors`, `backlinks`,
//! `trace_lineage`, plus `get_trust`, `upsert_redirect` and `resolve_redirect` which
//! delegate to `links.rs`. This list is illustrative, **not** exhaustive — see the trait
//! definition in `gradatum_core::index_store` for the authoritative surface.
//!
//! ## Contention
//!
//! All three traits share a single `Arc<Mutex<Connection>>`.

use async_trait::async_trait;

use gradatum_core::{
    IndexStore,
    error::GradatumError,
    identity::NoteId,
    index::{FileChecksumEntry, NoteRecord, TemporalEntry},
    index_store::{
        AuthorRow, CodeScopeEntryRaw, CodeSelector, CuratedLinks, LessonHitRaw, Lineage,
        ReviewQueueRow, SearchHitRaw,
    },
    metric_sample::MetricSamplePoint,
    scheduled_health::{ScheduledTaskHealth, TaskOutcome},
    scope::{AclCheckedVaultId, OverrideScope, VaultId},
    status::NoteStatus,
};

use crate::SqliteIndex;

#[async_trait]
impl IndexStore for SqliteIndex {
    /// FTS5 search — delegates to the `search_fts` inherent method.
    async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.search_fts(vault_id, query, limit).await
    }

    /// Scored FTS5 search (BM25 + status) — delegates to `search_fts_scored`.
    async fn search_fts_scored(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        self.search_fts_scored(vault_id, query, limit, include_downgraded)
            .await
    }

    /// Generic override upsert — delegates to `upsert_override_raw`.
    async fn upsert_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
        schema_version: u32,
        payload_toml: &str,
    ) -> Result<(), GradatumError> {
        self.upsert_override_raw(note_id, scope, override_type, schema_version, payload_toml)
            .await
    }

    /// Retrieves a generic override — delegates to `get_override_raw`.
    async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        self.get_override_raw(note_id, scope, override_type).await
    }

    /// Upserts a file checksum — delegates to `upsert_file_checksum`.
    async fn upsert_file_checksum(&self, entry: &FileChecksumEntry) -> Result<(), GradatumError> {
        self.upsert_file_checksum(entry).await
    }

    /// Lists file checksums — delegates to `list_file_checksums`.
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        self.list_file_checksums().await
    }

    /// Returns `(created_ms, in_degree)` — promoted from `queries.rs`.
    ///
    /// Delegates to the `get_note_created_and_indegree` concrete method on `SqliteIndex`
    /// (defined in `queries.rs`). No name collision: this method did not previously exist
    /// in the `Index` trait — it is a pure promotion.
    async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        self.get_note_created_and_indegree(vault_id, note_id).await
    }

    // ── Promoted methods ──────────────────────────────────────────────────────
    //
    // Methods below are promoted from their inherent equivalents.
    // Pattern: direct delegation `self.method(...)` — Rust resolves to the inherent
    // method (higher priority than the trait method), no infinite recursion.

    /// FTS5 search with snippet — delegates to `SqliteIndex::search_fts_with_snippet`.
    // 10 non-self args: orthogonal search filters — trait method signature cannot change.
    // #[allow] not #[expect]: clippy does not trigger too_many_arguments on trait impls.
    #[allow(clippy::too_many_arguments)]
    async fn search_fts_with_snippet(
        &self,
        vault_id: &AclCheckedVaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        self.search_fts_with_snippet(
            vault_id.vault_id(),
            query,
            limit,
            include_downgraded,
            section,
            locus,
            status,
            from_ms,
            to_ms,
        )
        .await
    }

    /// FTS5 corpus count — delegates to `SqliteIndex::count_fts_matches` (predicate parity with `search_fts_with_snippet`).
    async fn count_fts_matches(
        &self,
        vault_id: &AclCheckedVaultId,
        query: &str,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
    ) -> Result<(u64, bool), GradatumError> {
        self.count_fts_matches(
            vault_id.vault_id(),
            query,
            include_downgraded,
            section,
            locus,
            status,
        )
        .await
    }

    /// Batch anchor_ms lookup — delegates to `SqliteIndex::get_anchor_ms_batch`.
    async fn get_anchor_ms_batch(
        &self,
        vault_id: &AclCheckedVaultId,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, i64>, GradatumError> {
        self.get_anchor_ms_batch(vault_id.as_str(), ids).await
    }

    /// Lesson recall — delegates to `SqliteIndex::recall_lessons`.
    async fn recall_lessons(
        &self,
        vault_id: &VaultId,
        class: &str,
        limit: usize,
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        self.recall_lessons(vault_id, class, limit).await
    }

    /// Hydrate lessons by ULID — delegates to `SqliteIndex::hydrate_lessons_by_ulids`.
    async fn hydrate_lessons_by_ulids(
        &self,
        vault_id: &VaultId,
        ulids: &[&str],
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        self.hydrate_lessons_by_ulids(vault_id, ulids).await
    }

    /// Review queue listing — delegates to `SqliteIndex::list_review_queue`.
    async fn list_review_queue(
        &self,
        vault_id: &VaultId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReviewQueueRow>, GradatumError> {
        self.list_review_queue(vault_id, cursor, limit).await
    }

    /// Review queue count — delegates to `SqliteIndex::count_review_queue`.
    async fn count_review_queue(&self, vault_id: &VaultId) -> Result<u64, GradatumError> {
        self.count_review_queue(vault_id).await
    }

    /// Promotable notes from review statuses — delegates to `SqliteIndex::find_promotable`.
    async fn find_promotable(
        &self,
        cutoff_ms: i64,
        limit: usize,
    ) -> Result<Vec<(String, gradatum_core::status::NoteStatus)>, GradatumError> {
        self.find_promotable(cutoff_ms, limit).await
    }

    /// Per-vault promotable notes — delegates to `SqliteIndex::find_promotable_in_vault`.
    ///
    /// The inherent SQLite method takes a `&str`, so `vault_id.as_str()` is forwarded; the
    /// trait itself exposes the `VaultId` newtype.
    async fn find_promotable_in_vault(
        &self,
        vault_id: &VaultId,
        cutoff_ms: i64,
        limit: usize,
    ) -> Result<Vec<(String, gradatum_core::status::NoteStatus)>, GradatumError> {
        self.find_promotable_in_vault(vault_id.as_str(), cutoff_ms, limit)
            .await
    }

    /// Active vaults (`tenants.status='active'`) — delegates to `SqliteIndex::list_active_vaults`.
    ///
    /// The inherent SQLite method returns raw `String` values; they are re-typed into
    /// `VaultId` at the trait boundary.
    async fn list_active_vaults(&self) -> Result<Vec<VaultId>, GradatumError> {
        Ok(self
            .list_active_vaults()
            .await?
            .into_iter()
            .map(VaultId::new)
            .collect())
    }

    /// Vault provisioning — delegates to `SqliteIndex::provision_vault`.
    async fn provision_vault(&self, vault_id: &str) -> Result<bool, GradatumError> {
        self.provision_vault(vault_id).await
    }

    /// Tenant status change — delegates to `SqliteIndex::set_tenant_status`.
    async fn set_tenant_status(
        &self,
        vault_id: &str,
        status: gradatum_core::scope::TenantStatus,
    ) -> Result<Option<bool>, GradatumError> {
        self.set_tenant_status(vault_id, status).await
    }

    /// Tenant status read — delegates to `SqliteIndex::get_tenant_status`.
    async fn get_tenant_status(
        &self,
        vault_id: &str,
    ) -> Result<Option<gradatum_core::scope::TenantStatus>, GradatumError> {
        self.get_tenant_status(vault_id).await
    }

    /// Purge eligibility listing — delegates to `SqliteIndex::list_vault_note_ulids`.
    async fn list_vault_note_ulids(
        &self,
        vault_id: &str,
        limit: usize,
    ) -> Result<(Vec<String>, u64), GradatumError> {
        self.list_vault_note_ulids(vault_id, limit).await
    }

    /// Audit scan — delegates to `SqliteIndex::audit_scan_inner`.
    async fn audit_scan(
        &self,
        vault_id: &str,
        limit: usize,
    ) -> Result<Vec<gradatum_core::AuditScanRow>, GradatumError> {
        self.audit_scan_inner(vault_id, limit).await
    }

    /// Title-based lookup — delegates to `SqliteIndex::title_lookup`.
    async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.title_lookup(vault_id, title).await
    }

    /// Id-based lookup — delegates to `SqliteIndex::id_lookup`.
    async fn id_lookup(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.id_lookup(vault_id, note_id).await
    }

    /// Counts live notes — delegates to `SqliteIndex::live_note_count`.
    async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.live_note_count(vault_id).await
    }

    /// Most recent live-note indexation timestamp — delegates to
    /// `SqliteIndex::last_indexed_at`.
    async fn last_indexed_at(&self, vault_id: &str) -> Result<Option<i64>, GradatumError> {
        self.last_indexed_at(vault_id).await
    }

    /// Allow-list grants of a tenant — delegates to `SqliteIndex::tenant_grants`,
    /// overriding the fail-closed default provided by the trait.
    async fn tenant_grants(
        &self,
        tenant_id: &gradatum_core::scope::TenantId,
    ) -> Result<Vec<gradatum_core::scope::VaultGrant>, GradatumError> {
        self.tenant_grants(tenant_id).await
    }

    /// Allow-list grants of an agent — delegates to `SqliteIndex::agent_grants`,
    /// overriding the fail-closed default provided by the trait.
    async fn agent_grants(
        &self,
        agent_id: &gradatum_core::scope::AgentId,
    ) -> Result<Vec<gradatum_core::scope::AgentVaultGrant>, GradatumError> {
        self.agent_grants(agent_id).await
    }

    /// Upserts a row into `agent_vault_grants` — delegates to
    /// `SqliteIndex::upsert_agent_grant`.
    async fn upsert_agent_grant(
        &self,
        agent_id: &gradatum_core::scope::AgentId,
        vault_id: &gradatum_core::scope::VaultId,
        access: gradatum_core::scope::GrantAccess,
        section: Option<&str>,
    ) -> Result<(), GradatumError> {
        self.upsert_agent_grant(agent_id, vault_id, access, section)
            .await
    }

    /// Distinct authors — delegates to `SqliteIndex::distinct_authors`.
    async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        self.distinct_authors(vault_id).await
    }

    /// Distinct tags — delegates to `SqliteIndex::distinct_tags`.
    async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        self.distinct_tags(vault_id).await
    }

    /// BFS neighbors — delegates to `SqliteIndex::neighbors`.
    async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        self.neighbors(vault_id, note_id, depth).await
    }

    /// Backlinks — delegates to `SqliteIndex::backlinks`.
    async fn backlinks(&self, vault_id: &str, note_id: &str) -> Result<Vec<String>, GradatumError> {
        self.backlinks(vault_id, note_id).await
    }

    /// Lineage (parents + children) — delegates to `SqliteIndex::trace_lineage`.
    async fn trace_lineage(&self, vault_id: &str, note_id: &str) -> Result<Lineage, GradatumError> {
        self.trace_lineage(vault_id, note_id).await
    }

    /// Paginated note listing — delegates to `SqliteIndex::list_notes`.
    async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        self.list_notes(vault_id, section, limit, cursor).await
    }

    /// Atomic feature-number allocation — delegates to
    /// `SqliteIndex::allocate_feature_number` (overrides the loud-error trait default).
    async fn allocate_feature_number(&self, vault_id: &VaultId) -> Result<u32, GradatumError> {
        self.allocate_feature_number(vault_id.as_str()).await
    }

    /// Status-filtered note listing, `downgraded` included — delegates to
    /// `SqliteIndex::list_notes_by_status`.
    async fn list_notes_by_status(
        &self,
        vault_id: &str,
        statuses: &[&str],
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        self.list_notes_by_status(vault_id, statuses, section, limit, cursor)
            .await
    }

    /// Role-filtered note listing — delegates to the inherent
    /// `SqliteIndex::list_notes_filtered` (inherent method takes priority over this trait
    /// method, so this is delegation, not recursion — same shape as `list_notes_by_status`).
    async fn list_notes_filtered(
        &self,
        vault_id: &str,
        section: Option<&str>,
        role_kind: Option<&str>,
        role_status: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        self.list_notes_filtered(vault_id, section, role_kind, role_status, limit, cursor)
            .await
    }

    /// Lists `k` most recently active notes — delegates to `SqliteIndex::list_recent_notes`.
    async fn list_recent_notes(
        &self,
        vault_id: &str,
        k: usize,
    ) -> Result<Vec<NoteRecord>, GradatumError> {
        self.list_recent_notes(vault_id, k).await
    }

    /// Total `body_text` size in bytes — delegates to `SqliteIndex::total_body_size_bytes`.
    async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.total_body_size_bytes(vault_id).await
    }

    /// Upserts a wikilink — delegates to `SqliteIndex::upsert_link`.
    async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        self.upsert_link(vault_id, src_note_id, dst_note_id).await
    }

    /// Batch title + section fetch — delegates to `SqliteIndex::get_titles_sections`.
    async fn get_titles_sections(
        &self,
        vault_id: &AclCheckedVaultId,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        self.get_titles_sections(vault_id.as_str(), ids).await
    }

    /// Raw SQL status for a batch of note IDs — delegates to `SqliteIndex::get_statuses`.
    async fn get_statuses(
        &self,
        vault_id: &AclCheckedVaultId,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        self.get_statuses(vault_id.as_str(), ids).await
    }

    /// Trust score from the `notes.trust` column — delegates to `SqliteIndex::get_trust`.
    async fn get_trust(&self, vault_id: &str, id: &NoteId) -> Result<Option<f32>, GradatumError> {
        self.get_trust(vault_id, id).await
    }

    /// Combined `(trust, provenance)` — delegates to `SqliteIndex::get_trust_and_provenance`.
    async fn get_trust_and_provenance(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(Option<f32>, Option<String>), GradatumError> {
        self.get_trust_and_provenance(vault_id, note_id).await
    }

    /// Upserts a redirect slug → ULID — delegates to `SqliteIndex::upsert_redirect`.
    async fn upsert_redirect(
        &self,
        vault_id: &str,
        slug: &str,
        ulid: &ulid::Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError> {
        self.upsert_redirect(vault_id, slug, ulid, renamed_at_ms)
            .await
    }

    /// Resolves a redirect slug → ULID — delegates to `SqliteIndex::lookup_redirect`.
    async fn resolve_redirect(
        &self,
        vault_id: &str,
        slug: &str,
    ) -> Result<Option<ulid::Ulid>, GradatumError> {
        self.lookup_redirect(vault_id, slug).await
    }

    // ── Semantic Forget — scope resolution ───────────────────────────────────

    /// Delegates to `SqliteIndex::search_fts_for_forget`.
    async fn search_fts_for_forget(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        // Délègue à la méthode inhérente `SqliteIndex::search_fts_for_forget`,
        // dont la signature `&str` reste inchangée (frontière SQL, Task 21).
        self.search_fts_for_forget(vault_id.as_str(), query, limit)
            .await
    }

    /// Delegates to `SqliteIndex::list_notes_by_locus_prefix`.
    async fn list_notes_by_locus_prefix(
        &self,
        vault_id: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.list_notes_by_locus_prefix(vault_id, prefix).await
    }

    /// Delegates to `SqliteIndex::list_notes_by_section`.
    async fn list_notes_by_section(
        &self,
        vault_id: &str,
        section: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.list_notes_by_section(vault_id, section).await
    }

    /// Delegates to `SqliteIndex::list_notes_by_agent`.
    async fn list_notes_by_agent(
        &self,
        agent_id: &str,
        vaults: &[String],
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.list_notes_by_agent(agent_id, vaults).await
    }

    // ── Index lifecycle / mutations promoted to trait (v0.4.5) ────────────────
    // Override the trait's neutral default impls with real delegation.

    /// Delegates to `SqliteIndex::set_note_trust`.
    async fn set_note_trust(
        &self,
        vault_id: &str,
        id: &NoteId,
        trust: f32,
    ) -> Result<usize, GradatumError> {
        self.set_note_trust(vault_id, id, trust).await
    }

    /// Delegates to `SqliteIndex::write_temporal_entry`.
    async fn write_temporal_entry(&self, entry: &TemporalEntry) -> Result<(), GradatumError> {
        self.write_temporal_entry(entry).await
    }

    /// Delegates to `SqliteIndex::timeline`.
    async fn timeline(
        &self,
        vault_id: &AclCheckedVaultId,
        filter: &gradatum_core::temporal_query::TimelineFilter,
    ) -> Result<Vec<gradatum_core::temporal_query::TimelineRow>, GradatumError> {
        self.timeline(vault_id.vault_id(), filter).await
    }

    /// Delegates to `SqliteIndex::delete_redirect_by_ulid`.
    async fn delete_redirect_by_ulid(
        &self,
        vault_id: &str,
        ulid_str: &str,
    ) -> Result<usize, GradatumError> {
        self.delete_redirect_by_ulid(vault_id, ulid_str).await
    }

    /// Delegates to `SqliteIndex::delete_note_from_index`.
    async fn delete_note_from_index(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        self.delete_note_from_index(vault_id, note_id).await
    }

    /// Delegates to `SqliteIndex::list_garbage_older_than`.
    async fn list_garbage_older_than(
        &self,
        vault_id: &str,
        cutoff_ms: i64,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.list_garbage_older_than(vault_id, cutoff_ms).await
    }

    /// Delegates to `SqliteIndex::get_note_status`.
    async fn get_note_status(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<NoteStatus>, GradatumError> {
        self.get_note_status(vault_id, note_id).await
    }

    /// Delegates to `SqliteIndex::get_note_section`.
    async fn get_note_section(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.get_note_section(vault_id, note_id).await
    }

    /// Delegates to `SqliteIndex::is_note_forgotten`.
    async fn is_note_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        self.is_note_forgotten(vault_id, note_id).await
    }

    // ── v0.5.2 code-map — delegation to inherent methods ─────────────────────

    /// Delegates to `SqliteIndex::code_scope_query`.
    async fn code_scope_query(
        &self,
        vault_id: &str,
        selector: &CodeSelector,
        limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        self.code_scope_query(vault_id, selector, limit).await
    }

    /// Delegates to `SqliteIndex::code_scope_reverse_deps`.
    async fn code_scope_reverse_deps(
        &self,
        vault_id: &str,
        qualified_name: &str,
        limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        self.code_scope_reverse_deps(vault_id, qualified_name, limit)
            .await
    }

    /// Delegates to `SqliteIndex::code_scope_reverse_deps_batch`.
    async fn code_scope_reverse_deps_batch(
        &self,
        vault_id: &str,
        names: &[&str],
        limit: usize,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, GradatumError> {
        self.code_scope_reverse_deps_batch(vault_id, names, limit)
            .await
    }

    /// Delegates to `SqliteIndex::get_last_ingested_sha`.
    async fn get_last_ingested_sha(&self, vault_id: &str) -> Result<Option<String>, GradatumError> {
        self.get_last_ingested_sha(vault_id).await
    }

    /// Delegates to `SqliteIndex::get_code_vault_repo_path`.
    async fn get_code_vault_repo_path(
        &self,
        vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.get_code_vault_repo_path(vault_id).await
    }

    /// Delegates to `SqliteIndex::code_freshness_hashes_for`.
    async fn code_freshness_hashes(
        &self,
        vault_id: &str,
        source_paths: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        self.code_freshness_hashes_for(vault_id, source_paths).await
    }

    /// Atomic implementation of `persist_curated_index_atomic`.
    ///
    /// ## Atomicity
    ///
    /// Takes the lock once, opens an `unchecked_transaction`, runs EVERY SQL mutation
    /// inside it, then commits. If any mutation fails (for instance a foreign-key
    /// violation in `note_links`), dropping the `Transaction` rolls the whole batch back.
    ///
    /// ## Borrow checker
    ///
    /// `unchecked_transaction()` borrows the `&Connection` and returns a `Transaction<'_>`;
    /// the statements go through `tx.execute(...)` via `Deref<Target = Connection>`. The
    /// `Connection` itself can no longer be used directly once `tx` exists.
    ///
    /// ## Vault contract
    ///
    /// The vault write (Markdown on disk) happens BEFORE this call. If the index
    /// transaction fails, the Markdown stays consistent — the write is idempotent and
    /// copy-on-write — so the worker can retry the job and will find the file in place.
    async fn persist_curated_index_atomic(
        &self,
        note_id: &NoteId,
        title: &str,
        temporal: Option<&TemporalEntry>,
        links: CuratedLinks<'_>,
        trust: Option<f32>,
        vault_id: &str,
    ) -> Result<(), GradatumError> {
        use chrono::Utc;

        let note_id_str = note_id.to_string();
        let conn = self.conn.lock().await;

        // Ouvre une transaction — borrow `conn`, tout accès direct à `conn` après
        // cette ligne est interdit (borrow partagé actif via `tx: Deref<Connection>`).
        let tx = conn.unchecked_transaction().map_err(|e| {
            GradatumError::Storage(format!("persist_curated_index_atomic: begin tx: {e}"))
        })?;

        // 1. Upsert titre dans `notes.title` (et FTS5 via trigger).
        // C4 (caveat C2 INFO, council 01KXTRART) : épinglé `AND vault_id = ?` — durcissement
        // de la voie curation loopback (parité mutations index par ULID). Byte-identical :
        // `note_id` est résolu dans ce vault en amont, le prédicat ne retire aucune ligne légitime.
        tx.execute(
            "UPDATE notes SET title = ?2 WHERE id = ?1 AND vault_id = ?3",
            rusqlite::params![note_id_str, title, vault_id],
        )
        .map_err(|e| {
            GradatumError::Storage(format!(
                "persist_curated_index_atomic: upsert_note_title: {e}"
            ))
        })?;

        // 2. Entrée temporelle optionnelle.
        if let Some(t) = temporal {
            tx.execute(
                "INSERT OR REPLACE INTO temporal_index                  (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms)                  VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    t.note_id,
                    t.vault_id,
                    t.anchor_ms,
                    t.anchor_src.as_db_str(),
                    t.doc_kind,
                    t.valid_until_ms,
                ],
            )
            .map_err(|e| GradatumError::Storage(format!("persist_curated_index_atomic: write_temporal_entry: {e}")))?;
        }

        // 3. Links.
        //
        // F-147 : les arêtes sortantes d'une note doivent REFLÉTER son corps courant, pas
        // s'accumuler. Sous `links_authoritative`, on efface d'abord toutes les arêtes
        // sortantes de cette note (scopé `src_note_id` + `vault_id`, même durcissement que
        // l'UPDATE titre/trust ci-dessus — une suppression non scopée par vault serait une
        // fuite inter-locataires), puis on réinsère la liste courante, dans la MÊME
        // transaction (atomicité déjà acquise). Le DELETE porte sur `note_id`, qui EST le
        // `src` de toutes les arêtes de `links` (invariant `resolve_wikilinks*`).
        //
        // Absent `links_authoritative` (défaut sûr : chemins qui NE recalculent PAS les liens
        // — reclassification titre/section/statut, downgrade), aucune suppression : simple
        // upsert idempotent, comportement historique préservé. Une liste vide + autorité est
        // légitime (note dont le corps n'a plus aucun lien) et nettoie les arêtes périmées ;
        // une liste vide SANS autorité ne touche rien.
        let now_ms = Utc::now().timestamp_millis();
        if links.authoritative {
            // `note_id_str` : réutilise la résolution faite en tête de fonction (upsert titre).
            tx.execute(
                "DELETE FROM note_links WHERE src_note_id = ?1 AND vault_id = ?2",
                rusqlite::params![note_id_str, vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!(
                    "persist_curated_index_atomic: delete stale links ({note_id_str}): {e}"
                ))
            })?;
        }
        for (src, dst) in links.edges {
            tx.execute(
                "INSERT OR IGNORE INTO note_links (src_note_id, dst_note_id, vault_id, created_at)                  VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![src, dst, vault_id, now_ms],
            )
            .map_err(|e| GradatumError::Storage(format!("persist_curated_index_atomic: upsert_link ({src}→{dst}): {e}")))?;
        }

        // 4. Trust optionnel.
        // C4 (caveat C2 INFO, council 01KXTRART) : épinglé `AND vault_id = ?` — même durcissement
        // que l'upsert titre ci-dessus (voie curation loopback, byte-identical mono-vault).
        if let Some(trust_val) = trust {
            tx.execute(
                "UPDATE notes SET trust = ?2 WHERE id = ?1 AND vault_id = ?3",
                rusqlite::params![note_id_str, f64::from(trust_val), vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!("persist_curated_index_atomic: set_note_trust: {e}"))
            })?;
        }

        // Commit — si cette ligne atteint Ok(()), TOUTES les mutations sont persisted.
        // En cas d'échec avant ce point → Drop de `tx` = rollback implicite.
        tx.commit().map_err(|e| {
            GradatumError::Storage(format!("persist_curated_index_atomic: commit: {e}"))
        })?;

        Ok(())
    }

    /// ANN backfill override for `SqliteIndex` — delegates to
    /// `sqlite_vec::backfill_ann_from_conn`.
    ///
    /// The inherent `SqliteIndex::backfill_ann_index` method is distinct from this
    /// override: here `self.conn` is accessed directly to avoid any dispatch ambiguity.
    async fn backfill_ann_index(&self) -> Result<u64, GradatumError> {
        crate::sqlite_vec::backfill_ann_from_conn(&self.conn).await
    }

    /// Orphan-ANN-vector GC override for `SqliteIndex`, scoped to one partition.
    ///
    /// The inherent SQLite method takes a `&str`, so `vault_id.as_str()` is forwarded;
    /// `self.conn` is used directly, the vec0 extension being already loaded on it.
    async fn gc_orphan_ann(&self, vault_id: &VaultId) -> Result<u64, GradatumError> {
        SqliteIndex::gc_orphan_ann(self, vault_id.as_str()).await
    }

    /// ANN boot health gate override for `SqliteIndex`.
    ///
    /// Delegates to the inherent method, which owns the `ann_enabled` flag and therefore the
    /// fail-closed downgrade: through `Arc<dyn Index>` the concrete type is erased and
    /// `set_ann_enabled` is out of reach, so the decision cannot live in the caller.
    async fn ann_health_gate(
        &self,
    ) -> Result<Vec<gradatum_core::index_store::AnnPartitionDeficit>, GradatumError> {
        SqliteIndex::ann_health_gate(self).await
    }

    // ── Santé des tâches récurrentes (v0.7.5 F-85) ──────────────────────────

    /// Delegates to the inherent `SqliteIndex::record_task_run` method.
    async fn record_task_run(
        &self,
        task_name: &str,
        outcome: TaskOutcome,
        duration_ms: i64,
        error: Option<&str>,
        now_ms: i64,
    ) -> Result<(), GradatumError> {
        self.record_task_run(task_name, outcome, duration_ms, error, now_ms)
            .await
    }

    /// Delegates to the inherent `SqliteIndex::seed_scheduled_task` method.
    async fn seed_scheduled_task(&self, task_name: &str) -> Result<(), GradatumError> {
        self.seed_scheduled_task(task_name).await
    }

    /// Delegates to the inherent `SqliteIndex::list_scheduled_health` method.
    async fn list_scheduled_health(
        &self,
        now_ms: i64,
    ) -> Result<Vec<ScheduledTaskHealth>, GradatumError> {
        self.list_scheduled_health(now_ms).await
    }

    /// Delegates to the inherent `SqliteIndex::insert_metric_samples` method.
    async fn insert_metric_samples(
        &self,
        ts_ms: i64,
        samples: &[(String, f64)],
    ) -> Result<usize, GradatumError> {
        self.insert_metric_samples(ts_ms, samples).await
    }

    /// Delegates to the inherent `SqliteIndex::query_metric_timeseries` method.
    async fn query_metric_timeseries(
        &self,
        series: &[String],
        from_ms: i64,
        to_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<MetricSamplePoint>, GradatumError> {
        self.query_metric_timeseries(series, from_ms, to_ms, bucket_ms)
            .await
    }

    /// Delegates to the inherent `SqliteIndex::purge_metric_samples` method.
    async fn purge_metric_samples(&self, cutoff_ms: i64) -> Result<usize, GradatumError> {
        self.purge_metric_samples(cutoff_ms).await
    }

    /// Delegates to the inherent `SqliteIndex::list_distinct_metric_series` method.
    async fn list_distinct_metric_series(&self) -> Result<Vec<String>, GradatumError> {
        self.list_distinct_metric_series().await
    }
}

// ── Tests TDD F-47 get_trust ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gradatum_core::IndexStore;
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::{AclCheckedVaultId, TenantId, VaultId};

    use crate::SqliteIndex;

    /// `get_trust` returns the value of the `notes.trust` column.
    ///
    /// Tests found (0.95) and not-found (`None`) on a migrated in-memory DB.
    #[tokio::test]
    async fn get_trust_returns_column_value() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — invariant test");

        // Insérer une note minimale avec trust=0.95 via SQL direct (hook de test uniquement).
        // On n'a pas encore d'API d'upsert avec trust exposée — on insère directement.
        let note_id = NoteId::new();
        let id_str = note_id.to_string();
        {
            let conn = idx.conn.lock().await;
            let content_hash = [0u8; 32];
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, provenance, trust)
                 VALUES (?1, 'main', 'decisions', 'live', 1, 0, ?2, '', 'human-decision', 0.95)",
                rusqlite::params![id_str, content_hash.as_ref()],
            )
            .expect("INSERT note avec trust");
        }

        // get_trust via trait IndexStore (dynamic dispatch)
        let store: Arc<dyn IndexStore> = Arc::new(idx);
        let found = store
            .get_trust("main", &note_id)
            .await
            .expect("get_trust ne doit pas échouer");
        assert_eq!(
            found,
            Some(0.95_f32),
            "trust 0.95 attendu pour human-decision"
        );

        // Note inexistante → None
        let not_found = store
            .get_trust("main", &NoteId::new())
            .await
            .expect("get_trust note absente ne doit pas échouer");
        assert!(not_found.is_none(), "note absente → None");
    }

    /// Vérifie que `Arc<dyn IndexStore>` peut appeler les 8 nouvelles méthodes promues.
    ///
    /// Test round-trip : on instancie un `SqliteIndex` in-memory, on le coerce en
    /// `Arc<dyn IndexStore>`, et on appelle chaque méthode promue — si le dispatch
    /// dynamique est correctement câblé, les appels retournent `Ok(_)` sur une base vide.
    #[tokio::test]
    async fn arc_dyn_index_store_round_trip_promoted_methods() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let vault = VaultId::new("test");
        let vault_checked = AclCheckedVaultId::for_system_task(vault.clone());

        // search_fts_with_snippet — base vide → vec vide
        let hits = store
            .search_fts_with_snippet(
                &vault_checked,
                "foo",
                10,
                false,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("search_fts_with_snippet dyn");
        assert!(hits.is_empty(), "base vide → 0 hits");

        // title_lookup — base vide → None
        let lookup = store
            .title_lookup("test", "intro")
            .await
            .expect("title_lookup dyn");
        assert!(lookup.is_none(), "base vide → None");

        // live_note_count — base vide → 0
        let count = store
            .live_note_count("test")
            .await
            .expect("live_note_count dyn");
        assert_eq!(count, 0, "base vide → 0");

        // distinct_authors — base vide → vec vide
        let authors = store
            .distinct_authors("test")
            .await
            .expect("distinct_authors dyn");
        assert!(authors.is_empty(), "base vide → 0 auteurs");

        // distinct_tags — base vide → vec vide
        let tags = store
            .distinct_tags("test")
            .await
            .expect("distinct_tags dyn");
        assert!(tags.is_empty(), "base vide → 0 tags");

        // neighbors — base vide → vec vide
        let neighbors = store
            .neighbors("test", "01FAKEID000000000000000001", 1)
            .await
            .expect("neighbors dyn");
        assert!(neighbors.is_empty(), "base vide → 0 voisins");

        // backlinks — base vide → vec vide
        let backlinks = store
            .backlinks("test", "01FAKEID000000000000000001")
            .await
            .expect("backlinks dyn");
        assert!(backlinks.is_empty(), "base vide → 0 backlinks");

        // trace_lineage — base vide → Lineage vide
        let lineage = store
            .trace_lineage("test", "01FAKEID000000000000000001")
            .await
            .expect("trace_lineage dyn");
        assert!(lineage.parents.is_empty());
        assert!(lineage.children.is_empty());
    }

    // ── Tests TDD C1 (F-63) tenant_grants ────────────────────────────────────

    /// Seed 0030 : le tenant racine `main` détient exactement le grant write sur `main`.
    #[tokio::test]
    async fn tenant_grants_seed_main_write() {
        use gradatum_core::scope::GrantAccess;

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let grants = store
            .tenant_grants(&TenantId::new("main"))
            .await
            .expect("tenant_grants main");
        assert_eq!(grants.len(), 1, "seed = exactement 1 grant pour main");
        assert_eq!(grants[0].tenant_id, "main");
        assert_eq!(grants[0].vault_id.as_str(), "main");
        assert_eq!(grants[0].access, GrantAccess::Write);
        assert!(grants[0].access.allows_write());
    }

    /// Tenant inconnu → liste vide (l'absence de grant est un refus fail-closed).
    #[tokio::test]
    async fn tenant_grants_unknown_tenant_empty() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let grants = store
            .tenant_grants(&TenantId::new("ghost"))
            .await
            .expect("tenant_grants ghost");
        assert!(grants.is_empty(), "tenant inconnu → aucun grant");
    }

    /// Grant read-only : retourné mais `allows_write() == false` ; tenant suspendu :
    /// tous ses grants disparaissent du lookup (JOIN sur `tenants.status = 'active'`).
    #[tokio::test]
    async fn tenant_grants_read_only_and_suspended() {
        use gradatum_core::scope::GrantAccess;

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO tenants (id, status, created_at) VALUES
                   ('reader', 'active', 0),
                   ('frozen', 'suspended', 0);
                 INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES
                   ('reader', 'reader', 'read'),
                   ('frozen', 'frozen', 'write');",
            )
            .expect("seed tenants de test");
        }
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let reader = store
            .tenant_grants(&TenantId::new("reader"))
            .await
            .expect("tenant_grants reader");
        assert_eq!(reader.len(), 1);
        assert_eq!(reader[0].access, GrantAccess::Read);
        assert!(!reader[0].access.allows_write(), "read-only n'écrit pas");

        let frozen = store
            .tenant_grants(&TenantId::new("frozen"))
            .await
            .expect("tenant_grants frozen");
        assert!(frozen.is_empty(), "tenant suspendu → aucun grant visible");
    }

    /// L3 (F-121, migration 0040) : la colonne `section` fait l'aller-retour telle quelle.
    ///
    /// Les lignes héritées (`section` absente de l'INSERT → `NULL`) remontent en `None`
    /// = grant vault-entier, sémantique C1 stricte ; une ligne bornée remonte sa section.
    #[tokio::test]
    async fn tenant_grants_carries_section_scope() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO tenants (id, status, created_at) VALUES ('scoped', 'active', 0);
                 INSERT INTO tenant_vault_grants (tenant_id, vault_id, access, section) VALUES
                   ('scoped', 'main', 'read', 'lessons-learned');",
            )
            .expect("seed grant section-scopé");
        }
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        // Ligne héritée (seed 0030, INSERT sans `section`) → None = vault-entier.
        let main = store
            .tenant_grants(&TenantId::new("main"))
            .await
            .expect("tenant_grants main");
        assert_eq!(
            main[0].section, None,
            "un grant seed 0030 doit rester vault-entier"
        );
        assert!(main[0].covers_section(Some("lessons-learned")));

        // Ligne bornée → section remontée + couverture restreinte.
        let scoped = store
            .tenant_grants(&TenantId::new("scoped"))
            .await
            .expect("tenant_grants scoped");
        assert_eq!(scoped[0].section.as_deref(), Some("lessons-learned"));
        assert!(scoped[0].covers_section(Some("lessons-learned")));
        assert!(!scoped[0].covers_section(Some("decisions")));
        assert!(
            !scoped[0].covers_section(None),
            "fail-closed : un grant borné ne couvre pas une demande vault-entier"
        );
    }

    // ── Tests TDD B6 agent_grants ─────────────────────────────────────────────

    /// Seed 0042 : l'agent racine `main-agent` détient exactement le grant write
    /// sur `main`.
    #[tokio::test]
    async fn agent_grants_seed_main_write() {
        use gradatum_core::scope::{AgentId, GrantAccess};

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let grants = store
            .agent_grants(&AgentId::new("main-agent"))
            .await
            .expect("agent_grants main-agent");
        assert_eq!(grants.len(), 1, "seed = exactement 1 grant pour main-agent");
        assert_eq!(grants[0].agent_id.as_str(), "main-agent");
        assert_eq!(grants[0].vault_id.as_str(), "main");
        assert_eq!(grants[0].access, GrantAccess::Write);
        assert!(grants[0].access.allows_write());
        // Seed sans section → vault-wide.
        assert!(grants[0].covers_section(Some("lessons-learned")));
        assert!(grants[0].covers_section(None));
    }

    /// Agent inconnu → liste vide (l'absence de grant est un refus fail-closed).
    #[tokio::test]
    async fn agent_grants_unknown_agent_empty() {
        use gradatum_core::scope::AgentId;

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let grants = store
            .agent_grants(&AgentId::new("ghost-agent"))
            .await
            .expect("agent_grants ghost-agent");
        assert!(grants.is_empty(), "agent inconnu → aucun grant");
    }

    /// Grant read-only : retourné mais `allows_write() == false`.
    #[tokio::test]
    async fn agent_grants_read_only() {
        use gradatum_core::scope::{AgentId, GrantAccess};

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO agent_vault_grants (agent_id, vault_id, access) VALUES
                   ('reader-agent', 'reader', 'read');",
            )
            .expect("seed agent_grants de test");
        }
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        let grants = store
            .agent_grants(&AgentId::new("reader-agent"))
            .await
            .expect("agent_grants reader-agent");
        assert_eq!(grants.len(), 1, "1 grant pour reader-agent");
        assert_eq!(grants[0].access, GrantAccess::Read);
        assert!(!grants[0].access.allows_write());
    }

    /// Grant avec section scope : la colonne `section` fait l'aller-retour,
    /// `covers_section` restreint correctement.
    #[tokio::test]
    async fn agent_grants_carries_section_scope() {
        use gradatum_core::scope::AgentId;

        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test");
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO agent_vault_grants (agent_id, vault_id, access, section) VALUES
                   ('scoped-agent', 'main', 'read', 'lessons-learned');",
            )
            .expect("seed agent_grants section-scopé");
        }
        let store: Arc<dyn IndexStore> = Arc::new(idx);

        // Ligne héritée (seed 0042, INSERT sans `section`) → None = vault-entier.
        let main = store
            .agent_grants(&AgentId::new("main-agent"))
            .await
            .expect("agent_grants main-agent");
        assert_eq!(main[0].section, None, "seed 0042 doit rester vault-entier");
        assert!(main[0].covers_section(Some("lessons-learned")));

        // Ligne bornée → section remontée + couverture restreinte.
        let scoped = store
            .agent_grants(&AgentId::new("scoped-agent"))
            .await
            .expect("agent_grants scoped-agent");
        assert_eq!(scoped[0].section.as_deref(), Some("lessons-learned"));
        assert!(scoped[0].covers_section(Some("lessons-learned")));
        assert!(!scoped[0].covers_section(Some("decisions")));
        assert!(
            !scoped[0].covers_section(None),
            "fail-closed : un grant borné ne couvre pas une demande vault-entier"
        );
    }
}
