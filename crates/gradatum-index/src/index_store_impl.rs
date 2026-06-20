//! `impl IndexStore for SqliteIndex`.
//!
//! All methods delegate to inherent methods on `SqliteIndex`
//! (defined in `sqlite.rs` and `queries.rs`).
//!
//! Exposes: `get_note_created_and_indegree`, `search_fts_with_snippet`, `title_lookup`,
//! `live_note_count`, `distinct_authors`, `distinct_tags`, `neighbors`, `backlinks`,
//! `trace_lineage`.
//!
//! v0.4.0 adds `get_trust` and `upsert_redirect` + `resolve_redirect` (delegate to `links.rs`).
//!
//! ## Contention
//!
//! All three traits share a single `Arc<Mutex<Connection>>` (v0.3.0 design).

use async_trait::async_trait;

use gradatum_core::{
    IndexStore,
    error::GradatumError,
    identity::NoteId,
    index::{FileChecksumEntry, NoteRecord, TemporalEntry},
    index_store::{
        AuthorRow, CodeScopeEntryRaw, CodeSelector, LessonHitRaw, Lineage, ReviewQueueRow,
        SearchHitRaw,
    },
    scope::{OverrideScope, VaultId},
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
        self.search_fts_with_snippet(
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

    /// FTS5 corpus count — delegates to `SqliteIndex::count_fts_matches` (predicate parity with `search_fts_with_snippet`).
    async fn count_fts_matches(
        &self,
        vault_id: &VaultId,
        query: &str,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
    ) -> Result<(u64, bool), GradatumError> {
        self.count_fts_matches(vault_id, query, include_downgraded, section, locus, status)
            .await
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
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        self.get_titles_sections(vault_id, ids).await
    }

    /// Raw SQL status for a batch of note IDs — delegates to `SqliteIndex::get_statuses`.
    async fn get_statuses(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        self.get_statuses(vault_id, ids).await
    }

    /// Trust score from the `notes.trust` column — delegates to `SqliteIndex::get_trust`.
    async fn get_trust(&self, id: &NoteId) -> Result<Option<f32>, GradatumError> {
        self.get_trust(id).await
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
        slug: &str,
        ulid: &ulid::Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError> {
        self.upsert_redirect(slug, ulid, renamed_at_ms).await
    }

    /// Resolves a redirect slug → ULID — delegates to `SqliteIndex::lookup_redirect`.
    async fn resolve_redirect(&self, slug: &str) -> Result<Option<ulid::Ulid>, GradatumError> {
        self.lookup_redirect(slug).await
    }

    // ── Semantic Forget — scope resolution ───────────────────────────────────

    /// Delegates to `SqliteIndex::search_fts_for_forget`.
    async fn search_fts_for_forget(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.search_fts_for_forget(vault_id, query, limit).await
    }

    /// Delegates to `SqliteIndex::list_notes_by_locus_prefix`.
    async fn list_notes_by_locus_prefix(
        &self,
        vault_id: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        self.list_notes_by_locus_prefix(vault_id, prefix).await
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
    async fn set_note_trust(&self, id: &NoteId, trust: f32) -> Result<usize, GradatumError> {
        self.set_note_trust(id, trust).await
    }

    /// Delegates to `SqliteIndex::write_temporal_entry`.
    async fn write_temporal_entry(&self, entry: &TemporalEntry) -> Result<(), GradatumError> {
        self.write_temporal_entry(entry).await
    }

    /// Delegates to `SqliteIndex::timeline`.
    async fn timeline(
        &self,
        vault_id: &VaultId,
        filter: &gradatum_core::temporal_query::TimelineFilter,
    ) -> Result<Vec<gradatum_core::temporal_query::TimelineRow>, GradatumError> {
        self.timeline(vault_id, filter).await
    }

    /// Delegates to `SqliteIndex::delete_redirect_by_ulid`.
    async fn delete_redirect_by_ulid(&self, ulid_str: &str) -> Result<usize, GradatumError> {
        self.delete_redirect_by_ulid(ulid_str).await
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

    /// Impl atomique pour `persist_curated_index_atomic`.
    ///
    /// ## Atomicité
    ///
    /// Acquiert le verrou une seule fois, ouvre une transaction `unchecked_transaction`,
    /// exécute TOUTES les mutations SQL dans la transaction, puis commit.
    /// Si l'une des mutations échoue (ex: FK violation dans `note_links`),
    /// le `Drop` de `Transaction` provoque un rollback implicite.
    ///
    /// ## Borrow checker
    ///
    /// `unchecked_transaction()` borrow `&Connection` et retourne `Transaction<'_>`.
    /// On utilise `tx.execute(...)` via `Deref<Target=Connection>` sur `Transaction`.
    /// La `Connection` ne peut plus être utilisée directement après création de `tx`.
    ///
    /// ## Contrat vault
    ///
    /// Le vault write (markdown sur disque) est effectué AVANT cet appel.
    /// Si la transaction index échoue, le markdown reste cohérent (idempotent, CoW).
    /// Le worker peut re-tenter le job et retrouvera le markdown présent.
    async fn persist_curated_index_atomic(
        &self,
        note_id: &NoteId,
        title: &str,
        temporal: Option<&TemporalEntry>,
        links: &[(String, String)],
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
        tx.execute(
            "UPDATE notes SET title = ?2 WHERE id = ?1",
            rusqlite::params![note_id_str, title],
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

        // 3. Links — INSERT OR IGNORE (idempotent, FK sur src_note_id → rollback si inexistant).
        let now_ms = Utc::now().timestamp_millis();
        for (src, dst) in links {
            tx.execute(
                "INSERT OR IGNORE INTO note_links (src_note_id, dst_note_id, vault_id, created_at)                  VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![src, dst, vault_id, now_ms],
            )
            .map_err(|e| GradatumError::Storage(format!("persist_curated_index_atomic: upsert_link ({src}→{dst}): {e}")))?;
        }

        // 4. Trust optionnel.
        if let Some(trust_val) = trust {
            tx.execute(
                "UPDATE notes SET trust = ?2 WHERE id = ?1",
                rusqlite::params![note_id_str, f64::from(trust_val)],
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

    /// Override ANN backfill pour `SqliteIndex` — délègue à `sqlite_vec::backfill_ann_from_conn`.
    ///
    /// La méthode inherent `SqliteIndex::backfill_ann_index` est distincte de cet override :
    /// ici on accède directement à `self.conn` pour éviter toute ambiguïté de dispatch.
    async fn backfill_ann_index(&self) -> Result<u64, GradatumError> {
        crate::sqlite_vec::backfill_ann_from_conn(&self.conn).await
    }
}

// ── Tests TDD F-47 get_trust ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gradatum_core::IndexStore;
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::VaultId;

    use crate::SqliteIndex;

    /// F-47 — get_trust retourne la valeur de la colonne notes.trust.
    ///
    /// Teste found (0.95) et not-found (None) sur une DB in-memory migrée.
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
            .get_trust(&note_id)
            .await
            .expect("get_trust ne doit pas échouer");
        assert_eq!(
            found,
            Some(0.95_f32),
            "trust 0.95 attendu pour human-decision"
        );

        // Note inexistante → None
        let not_found = store
            .get_trust(&NoteId::new())
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

        // search_fts_with_snippet — base vide → vec vide
        let hits = store
            .search_fts_with_snippet(&vault, "foo", 10, false, None, None, None)
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
}
