//! Test de compilation des 3 traits de storage — mock implementation pour vérifier la shape.
//!
//! ## Étape 0.1 — refactoring
//!
//! Avant Étape 0.1 : `MockIndex` implémentait `Index` avec 10 méthodes directement.
//! Après Étape 0.1 : `Index` est une façade (supertrait sans méthodes propres).
//! `MockIndex` doit maintenant implémenter les 3 sous-traits granulaires :
//! - `DocumentStore` : write_note, get_content_hash, get_note, list_by_status,
//!   downgrade_note, patch_note_status
//! - `IndexStore` : search_fts, search_fts_scored, upsert/get_override_raw,
//!   upsert/list_file_checksums, get_note_created_and_indegree,
//!   search_fts_with_snippet, title_lookup, live_note_count, distinct_authors,
//!   distinct_tags, neighbors, backlinks, trace_lineage, list_notes, total_body_size_bytes,
//!   upsert_link
//! - `VectorStore` : insert_note_embedding, get_note_embedding, search_semantic
//!
//! Note : `seed_note`, `seed_note_with_fts`, `seed_note_with_created` sont des méthodes
//! concrètes de `SqliteIndex` (pas dans le trait `IndexStore`) — elles ne figurent donc
//! pas dans ce mock.
//!
//! La compilation de ce test PROUVE que la shape des 3 traits est correcte et que
//! le blanket impl `Index` pour tout T: DocumentStore+IndexStore+VectorStore fonctionne.

use async_trait::async_trait;
use std::sync::Mutex;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::index::{FileChecksumEntry, FileKind, NoteRecord};
use gradatum_core::index_store::{AuthorRow, Lineage, SearchHitRaw};
use gradatum_core::note::Note;
use gradatum_core::scope::{OverrideScope, VaultId};
use gradatum_core::status::NoteStatus;
use gradatum_core::{DocumentStore, IndexStore, VectorStore};

/// Mock minimaliste pour tester le dispatch des 3 traits (Étape 0.1).
pub struct MockIndex {
    /// Compteur d'appels à `write_note` (ex-`upsert_note`).
    pub write_calls: Mutex<u32>,
}

impl MockIndex {
    fn new() -> Self {
        Self {
            write_calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl DocumentStore for MockIndex {
    async fn write_note(&self, _note: &Note) -> Result<(), GradatumError> {
        *self.write_calls.lock().expect("mutex non-poisonné") += 1;
        Ok(())
    }

    async fn get_content_hash(&self, _id: NoteId) -> Result<Option<ContentHash>, GradatumError> {
        Ok(None)
    }

    async fn get_note(
        &self,
        _tenant_id: &str,
        _note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        Ok(None)
    }

    async fn list_by_status(
        &self,
        _vault_id: &VaultId,
        _status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        Ok(vec![])
    }

    async fn downgrade_note(
        &self,
        _note_id: &NoteId,
        _reason: &str,
        _replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn patch_note_status(
        &self,
        _note_id: &NoteId,
        _status: Option<&str>,
        _status_reason: Option<&str>,
        _replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn upsert_note_title(
        &self,
        _note_id: &NoteId,
        _title: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
}

#[async_trait]
impl IndexStore for MockIndex {
    async fn search_fts(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        Ok(vec![])
    }

    async fn search_fts_scored(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
        _include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        Ok(Vec::new())
    }

    async fn upsert_override_raw(
        &self,
        _note_id: NoteId,
        _scope: &OverrideScope,
        _override_type: &str,
        _schema_version: u32,
        _payload_toml: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn get_override_raw(
        &self,
        _note_id: NoteId,
        _scope: &OverrideScope,
        _override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        Ok(None)
    }

    async fn upsert_file_checksum(&self, _entry: &FileChecksumEntry) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        Ok(vec![])
    }

    async fn get_note_created_and_indegree(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        Ok((0, 0))
    }

    async fn search_fts_with_snippet(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
        _include_downgraded: bool,
        _section: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        Ok(vec![])
    }

    async fn title_lookup(
        &self,
        _vault_id: &str,
        _title: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }

    async fn live_note_count(&self, _vault_id: &str) -> Result<u64, GradatumError> {
        Ok(0)
    }

    async fn distinct_authors(&self, _vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        Ok(vec![])
    }

    async fn distinct_tags(&self, _vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        Ok(vec![])
    }

    async fn neighbors(
        &self,
        _vault_id: &str,
        _note_id: &str,
        _depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }

    async fn backlinks(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }

    async fn trace_lineage(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Lineage, GradatumError> {
        Ok(Lineage::default())
    }

    async fn list_notes(
        &self,
        _vault_id: &str,
        _section: Option<&str>,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        Ok((vec![], 0))
    }

    async fn total_body_size_bytes(&self, _vault_id: &str) -> Result<u64, GradatumError> {
        Ok(0)
    }

    async fn upsert_link(
        &self,
        _vault_id: &str,
        _src_note_id: &str,
        _dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn get_titles_sections(
        &self,
        _vault_id: &str,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }
}

#[async_trait]
impl VectorStore for MockIndex {
    async fn insert_note_embedding(
        &self,
        _note_id: &NoteId,
        _embedder_id: &str,
        _dim: u16,
        _vector: &[f32],
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn get_note_embedding(
        &self,
        _note_id: &NoteId,
        _embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        Ok(None)
    }

    async fn search_semantic(
        &self,
        _vault_id: &str,
        _embedder_id: &str,
        _query_emb: &[f32],
        _limit: usize,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        Ok(vec![])
    }
}

// Le blanket impl `Index for T: DocumentStore+IndexStore+VectorStore` couvre MockIndex.
// Vérification de compilation : MockIndex est un Index via le blanket impl.
fn _assert_mock_is_index(_: &dyn gradatum_core::index::Index) {}

/// Vérifie que `MockIndex` implémente correctement les 3 traits et que le compteur
/// `write_calls` démarre à 0.
///
/// La compilation de ce test PROUVE que la shape des 3 traits est correcte.
#[tokio::test]
async fn mock_index_records_write_starts_at_zero() {
    let mock = MockIndex::new();
    assert_eq!(
        *mock.write_calls.lock().unwrap(),
        0,
        "compteur doit démarrer à 0"
    );
    // Vérification blanket Index via dispatch dyn (object safety)
    _assert_mock_is_index(&mock);
}

/// Vérification que `search_fts` retourne `Vec::new()` sur le mock (pas d'erreur).
#[tokio::test]
async fn mock_index_search_fts_returns_empty() {
    let mock = MockIndex::new();
    let vault = VaultId::new("main");
    let results = mock.search_fts(&vault, "test query", 10).await.unwrap();
    assert!(results.is_empty());
}

/// Vérification que `get_override_raw` retourne `None` sur le mock (pas d'erreur).
#[tokio::test]
async fn mock_index_override_raw_returns_none() {
    let mock = MockIndex::new();
    let note_id = NoteId::new();
    let scope = OverrideScope::Vault(VaultId::new("main"));
    let result = mock
        .get_override_raw(note_id, &scope, "metadata")
        .await
        .unwrap();
    assert!(result.is_none());
}

/// Vérification que `list_file_checksums` retourne `Vec::new()` sur le mock.
#[tokio::test]
async fn mock_index_file_checksums_returns_empty() {
    let mock = MockIndex::new();
    let checksums = mock.list_file_checksums().await.unwrap();
    assert!(checksums.is_empty());
}

/// Vérification que `FileKind` sérialise correctement en kebab-case.
#[test]
fn file_kind_serde_kebab_case() {
    let json = serde_json::to_string(&FileKind::Note).unwrap();
    assert_eq!(json, "\"note\"");
    let json = serde_json::to_string(&FileKind::Override).unwrap();
    assert_eq!(json, "\"override\"");
    let json = serde_json::to_string(&FileKind::Config).unwrap();
    assert_eq!(json, "\"config\"");
}

/// Vérification que `VectorStore::search_semantic` retourne Vec vide sur le mock.
#[tokio::test]
async fn mock_index_search_semantic_returns_empty() {
    let mock = MockIndex::new();
    let results = mock
        .search_semantic("main", "test-embedder", &[1.0f32, 0.0, 0.0], 5)
        .await
        .unwrap();
    assert!(results.is_empty());
}

/// Vérification que `get_note_created_and_indegree` retourne (0,0) sur le mock.
#[tokio::test]
async fn mock_index_note_created_and_indegree_returns_zero() {
    let mock = MockIndex::new();
    let (created_ms, in_degree) = mock
        .get_note_created_and_indegree("main", "01HZZZZZZZZZZZZZZZZZZZZZZA")
        .await
        .unwrap();
    assert_eq!(created_ms, 0);
    assert_eq!(in_degree, 0);
}
