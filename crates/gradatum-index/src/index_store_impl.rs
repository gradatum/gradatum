//! `impl IndexStore for SqliteIndex` — carve additif Étape 0.1 + extension Étape 0.2a.
//!
//! Toutes les méthodes délèguent aux méthodes inhérentes de `SqliteIndex`
//! (définies dans `sqlite.rs` et `queries.rs`).
//!
//! `get_note_created_and_indegree` est une **promotion** depuis `queries.rs` (Étape 0.1).
//!
//! Étape 0.2a ajoute 8 promotions supplémentaires :
//! `search_fts_with_snippet`, `title_lookup`, `live_note_count`, `distinct_authors`,
//! `distinct_tags`, `neighbors`, `backlinks`, `trace_lineage`.
//!
//! ## Contention
//!
//! Les 3 traits partagent un `Arc<Mutex<Connection>>` unique en v0.3.0.

use async_trait::async_trait;

use gradatum_core::{
    error::GradatumError,
    identity::NoteId,
    index::{FileChecksumEntry, NoteRecord},
    index_store::{AuthorRow, Lineage, SearchHitRaw},
    scope::{OverrideScope, VaultId},
    IndexStore,
};

use crate::SqliteIndex;

#[async_trait]
impl IndexStore for SqliteIndex {
    /// Recherche FTS5 — délègue à la méthode inhérente `search_fts`.
    async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.search_fts(vault_id, query, limit).await
    }

    /// Recherche FTS5 scorée (BM25 + status) — délègue à `search_fts_scored`.
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

    /// Upsert override générique — délègue à `upsert_override_raw`.
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

    /// Récupère un override générique — délègue à `get_override_raw`.
    async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        self.get_override_raw(note_id, scope, override_type).await
    }

    /// Upsert checksum fichier — délègue à `upsert_file_checksum`.
    async fn upsert_file_checksum(&self, entry: &FileChecksumEntry) -> Result<(), GradatumError> {
        self.upsert_file_checksum(entry).await
    }

    /// Liste les checksums fichiers — délègue à `list_file_checksums`.
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        self.list_file_checksums().await
    }

    /// Retourne `(created_ms, in_degree)` — **promotion** depuis `queries.rs`.
    ///
    /// Délègue à la méthode concrète `get_note_created_and_indegree` de `SqliteIndex`
    /// (définie dans `queries.rs`). Pas de collision : cette méthode n'existait PAS
    /// dans le `trait Index` historique — c'est une promotion pure.
    async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        self.get_note_created_and_indegree(vault_id, note_id).await
    }

    // ── Promotions Étape 0.2a ─────────────────────────────────────────────────
    //
    // Les méthodes ci-dessous sont promues depuis leurs équivalents inhérents.
    // Pattern : délégation directe `self.method(...)` — Rust résout vers la méthode
    // inhérente (priorité sur la méthode trait), pas de récursion infinie.

    /// Recherche FTS5 avec snippet — délègue à `SqliteIndex::search_fts_with_snippet`.
    async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        self.search_fts_with_snippet(vault_id, query, limit, include_downgraded, section)
            .await
    }

    /// Résolution par titre — délègue à `SqliteIndex::title_lookup`.
    async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError> {
        self.title_lookup(vault_id, title).await
    }

    /// Compte les notes live — délègue à `SqliteIndex::live_note_count`.
    async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.live_note_count(vault_id).await
    }

    /// Auteurs distincts — délègue à `SqliteIndex::distinct_authors`.
    async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        self.distinct_authors(vault_id).await
    }

    /// Tags distincts — délègue à `SqliteIndex::distinct_tags`.
    async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        self.distinct_tags(vault_id).await
    }

    /// Voisins BFS — délègue à `SqliteIndex::neighbors`.
    async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        self.neighbors(vault_id, note_id, depth).await
    }

    /// Backlinks — délègue à `SqliteIndex::backlinks`.
    async fn backlinks(&self, vault_id: &str, note_id: &str) -> Result<Vec<String>, GradatumError> {
        self.backlinks(vault_id, note_id).await
    }

    /// Lignée (parents + enfants) — délègue à `SqliteIndex::trace_lineage`.
    async fn trace_lineage(&self, vault_id: &str, note_id: &str) -> Result<Lineage, GradatumError> {
        self.trace_lineage(vault_id, note_id).await
    }

    /// Liste paginée des notes — délègue à `SqliteIndex::list_notes`.
    async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        self.list_notes(vault_id, section, limit, cursor).await
    }

    /// Taille totale body_text — délègue à `SqliteIndex::total_body_size_bytes`.
    async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError> {
        self.total_body_size_bytes(vault_id).await
    }

    /// Lien wikilink — délègue à `SqliteIndex::upsert_link`.
    async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        self.upsert_link(vault_id, src_note_id, dst_note_id).await
    }

    /// Batch title+section — délègue à `SqliteIndex::get_titles_sections`.
    async fn get_titles_sections(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        self.get_titles_sections(vault_id, ids).await
    }
}

// ── Tests TDD Étape 0.2a ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gradatum_core::scope::VaultId;
    use gradatum_core::IndexStore;

    use crate::SqliteIndex;

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
            .search_fts_with_snippet(&vault, "foo", 10, false, None)
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
