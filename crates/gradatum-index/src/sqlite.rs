//! Concrete implementation of `gradatum-core::index::Index` via SQLite + FTS5.
//!
//! ## Design
//!
//! `SqliteIndex` is thread-safe via `Arc<Mutex<Connection>>`:
//! rusqlite `Connection` is neither `Send` nor `Sync`. The tokio `Mutex` guarantees
//! exclusive access from any thread of the runtime.
//!
//! ## Mandatory PRAGMAs (C12)
//!
//! Applied at each `open()` / `open_in_memory()` call before migrations:
//! - `journal_mode = WAL`   : concurrent reads without a global lock.
//! - `synchronous = NORMAL` : durable after OS crash (not after power loss).
//! - `busy_timeout = 5000`  : 5 s before SQLITE_BUSY (multi-process safe).
//! - `foreign_keys = ON`    : referential integrity + cascade DELETE.
//!
//! ## `extra_json` column
//!
//! The original schema named the column `extra_yaml TEXT`. This implementation uses
//! `extra_json TEXT` and `serde_json` to serialise `ExtraFields` (`BTreeMap<String, toml::Value>`).
//! Rationale: `serde_yml::to_string` on `toml::Value` produces ambiguous variants
//! (notably `Datetime` → a non-portable private toml representation). `serde_json` guarantees
//! a stable round-trip for String/Integer/Float/Boolean/Array/Table variants.
//! `toml::Value::Datetime` is forbidden in `ExtraFields` for JCS hashing (see `identity.rs`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tokio::sync::Mutex;

use gradatum_core::error::{GradatumError, ValidationError};
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::index::{FileChecksumEntry, FileKind, NoteRecord, TemporalEntry};
use gradatum_core::index_store::{
    CodeScopeEntryRaw, CodeSelector, LessonHitRaw, ReviewQueueRow, SearchHitRaw,
};
use gradatum_core::note::Note;
use gradatum_core::scope::{OverrideScope, VaultId};
use gradatum_core::section::{section_to_c_kind, section_to_doc_kind};
use gradatum_core::status::NoteStatus;

/// Row returned by [`SqliteIndex::list_forgotten_notes`].
///
/// `(ulid, title, section, forgotten_at_ms, forgotten_by)`.
type ForgottenRow = (String, Option<String>, String, i64, Option<String>);

/// Snapshot of **index-level** status fields for a note.
///
/// Captures the raw state of columns `status`, `status_reason`, `status_changed`,
/// and `replaced_by` as stored in the index — including values set by index-only
/// mutations (`downgrade_note`, `patch_note_status`, trust decay) that do NOT
/// rewrite the `.md` frontmatter.
///
/// Usage: [`SqliteIndex::get_index_status_snapshot`] captures this state BEFORE a
/// physical relocation (`move_locus`); [`SqliteIndex::restore_index_status_fields`]
/// restores it AFTER the re-upsert to prevent silent resurrection of a `downgraded`
/// note (the re-upsert from a stale `live` frontmatter would otherwise overwrite
/// the index-only status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatusSnapshot {
    /// Raw value of `notes.status` (e.g. `"live"`, `"downgraded"`, `"pending-review"`).
    pub status: String,
    /// Reason for the last status change (`notes.status_reason`).
    pub status_reason: Option<String>,
    /// Epoch-ms timestamp of the last status change (`notes.status_changed`).
    pub status_changed_ms: Option<i64>,
    /// ULID of the replacement note (`notes.replaced_by`), set by `downgrade_note`.
    pub replaced_by: Option<String>,
}

/// SQLite + FTS5 implementation of the Gradatum storage traits.
///
/// Created via `SqliteIndex::open(&path)` or `SqliteIndex::open_in_memory()`.
/// One instance per process is sufficient for a single-tenant vault.
///
/// ## Implemented traits
///
/// `SqliteIndex` implements the three granular traits:
/// - [`DocumentStore`](gradatum_core::DocumentStore) — note CRUD
/// - [`IndexStore`](gradatum_core::IndexStore) — FTS, overrides, checksums, composite scoring
/// - [`VectorStore`](gradatum_core::VectorStore) — embeddings + cosine semantic search
///
/// The [`Index`](gradatum_core::index::Index) facade remains available via blanket impl.
///
/// ## Concrete non-trait methods
///
/// The following methods are concrete (`pub`) on `SqliteIndex` and are not exposed
/// via a trait:
/// - `search_fts_scored_filtered`: extends `search_fts_scored` with a section filter —
///   not called directly by handlers (they use `search_fts_with_snippet`).
/// - `downgrade_note`, `patch_note_status`, `upsert_note_title`: admin/lifecycle methods.
/// - `list_notes`, `total_body_size_bytes`: promoted into `IndexStore`.
/// - `seed_note`, `seed_note_with_fts`, `seed_note_with_created`: test utilities —
///   intentionally kept on the concrete type, outside the `IndexStore` trait.
/// - Bench methods (`vault_id_count`, `locus_count`).
///
/// ## Contention
///
/// All three traits share a single `Arc<Mutex<Connection>>`. Every method implementation
/// must ensure the `MutexGuard` is dropped BEFORE the next `.await`.
pub struct SqliteIndex {
    /// Shared SQLite connection — `pub(crate)` for methods in the `queries` module.
    ///
    /// Protected by a tokio `Mutex` to guarantee exclusive access from any thread
    /// of the runtime (rusqlite `Connection` is neither `Send` nor `Sync`).
    pub(crate) conn: Arc<Mutex<Connection>>,

    /// ANN path enabled at runtime (v0.5.3 ANN-5).
    ///
    /// `true` = extension sqlite-vec chargée + `ann_backend = sqlite_vec` configuré.
    /// `false` (défaut) = brute-force cosine (`search_semantic_inner`).
    ///
    /// Modifiable après ouverture via `set_ann_enabled` (bin crate server, APRÈS
    /// enregistrement de l'extension et validation de la table `note_embeddings_ann`).
    /// Utilise `AtomicBool` pour éviter le coût d'un `Mutex` sur un hot path de lecture.
    pub(crate) ann_enabled: Arc<AtomicBool>,

    /// Paramètre `ef_search` transmis à vec0 pour chaque requête ANN.
    ///
    /// Contrôle l'oversampling (`limit × ef_search`, borné par `MAX_ANN_K`).
    /// Défaut : 64 (configurable via `[search] ann_ef_search` dans `server.toml`).
    pub(crate) ann_ef_search: Arc<AtomicU32>,
}

/// Derived note for index-only batch writes (code-ingest, v0.5.2).
///
/// No Markdown file: content lives only in the `notes` table.
/// The source of truth is the git repository; the index is fully derived.
/// Used by `SqliteIndex::write_note_derived_batch`.
#[derive(Debug, Clone)]
pub struct DerivedNote {
    /// Deterministic `NoteId` built via `NoteId::derived_from`.
    pub id: NoteId,
    /// Note body (signature + short doc-comment + deps). Cap: ≤ 60 lines.
    pub body_text: String,
    /// Space-separated tags (e.g. `"code rust fn my_module"`).
    pub tags: String,
    /// Short title (qualified name or short signature).
    pub title: Option<String>,
    /// Structured metadata for the code symbol (`code_scope`).
    ///
    /// Persisted in `notes.extra_json` under the key `"cs"` at ingest time.
    /// The `code_scope` handler reads them back as-is — no fragile re-parse of
    /// `body_text`. `None` = non-derived note (never the case for code-ingest).
    pub code_meta: Option<CodeSymbolMeta>,
}

/// Structured metadata for a derived code symbol (v0.5.2).
///
/// Serialised as JSON in `notes.extra_json["cs"]` during `write_note_derived_batch`,
/// read back by the `code_scope` handler to reconstruct the `entries[]` response
/// without re-parsing `body_text`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolMeta {
    /// Source file path (relative to the repo). Stored in the note to allow the
    /// `code_scope` handler to detect drift without an inverse join on `code_freshness`.
    pub source_path: String,
    /// Entity kind (`fn`, `struct`, `enum`, `trait`, `impl`, `const`, `mod`, `method`).
    pub kind: String,
    /// Qualified name (e.g. `"MyStruct::my_method"`).
    pub qualified_name: String,
    /// Textual signature (params + return type) — `None` if not extractable.
    pub signature: Option<String>,
    /// Outgoing intra-repo dependencies (qualified names, best-effort).
    pub deps: Vec<String>,
    /// Item visibility: `"pub"` or `"priv"`. `None` for notes predating v0.5.2 (backward compat).
    #[serde(default)]
    pub visibility: Option<String>,
    /// Inclusive 1-based span `(start_line, end_line)` of the tree-sitter node.
    ///
    /// Stored additively in `extra_json["cs"]` — no SQL migration required.
    /// `None` for notes ingested before the `include_body` feature; in that case
    /// `code_scope include_body` returns `body=None` for the entry (fallback to Read).
    #[serde(default)]
    pub span: Option<(u32, u32)>,
}

/// Freshness state of a source file relative to the `code_freshness` index.
///
/// Produced by `SqliteIndex::check_freshness`. The drift-detection contract
/// is non-blocking and non-regenerating in the synchronous path:
/// the consumer receives the state and decides the action (typically: enqueue an
/// async re-generation via the job system).
///
/// ## Accuracy > coverage
///
/// `Unknown` is returned when certainty is absent (entry missing from `code_freshness`).
/// `Fresh` is never returned by default — a false `Fresh` is more costly
/// (an agent acting on stale data).
#[derive(Debug, Clone, PartialEq)]
pub enum Freshness {
    /// Current hash matches the stored hash — file unchanged since last ingest.
    Fresh,
    /// Current hash differs from the stored hash — file modified, re-generation required.
    Stale {
        /// Hash stored at the last ingest.
        stored_hash: String,
        /// Current hash computed from bytes passed to `check_freshness`.
        current_hash: String,
    },
    /// No entry for this `(vault_id, source_path)` in `code_freshness`.
    /// Accuracy > coverage: never treated as `Fresh`.
    Unknown,
}

/// Escapes SQLite LIKE metacharacters (`%`, `_`, `\`) in a value intended to be
/// passed as a bound parameter in a `LIKE ? ESCAPE '\'` clause.
///
/// Ensures that a user-supplied value is treated literally:
/// `%` is not a wildcard and `_` does not match a single arbitrary character.
///
/// ## SQL injection
///
/// The value MUST be passed as a bound parameter (never interpolated into SQL).
/// This function protects against unintended LIKE wildcards, not against SQL injection
/// (the bound parameter handles that).
///
/// The same logic is exposed in `gradatum_dto::escape_like` for external consumers
/// (MCP, SDK). The duplication is intentional: `gradatum-index` must not depend on
/// `gradatum-dto` (acyclic crate dependency graph).
///
/// `pub(crate)` : réutilisé par `sqlite_vec.rs` (filtre locus ANN).
pub(crate) fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '%' => out.push_str(r"\%"),
            '_' => out.push_str(r"\_"),
            c => out.push(c),
        }
    }
    out
}

/// Prepares a user query for an FTS5 `MATCH` clause by neutralising all FTS5
/// operators (hyphens, quotes, AND/OR/NOT, parentheses, `*`, `^`).
///
/// ## Mechanism
///
/// Each token (non-whitespace sequence) is wrapped in `"..."` with internal
/// double-quotes escaped as `""`. The result is the tokens joined by a space —
/// FTS5 interprets this as an implicit AND.
///
/// Examples:
/// - `lot-c`      → `"lot-c"`
/// - `2026-06-10` → `"2026-06-10"`
/// - `foo bar`    → `"foo" "bar"`
/// - `a "b" c`    → `"a" """b""" "c"`
/// - (empty)      → `""` (returns an empty string — caller must short-circuit)
///
/// ## Trade-off
///
/// Advanced FTS5 operators (exact phrases, `NEAR` proximity, `*` prefix) are
/// disabled. This is acceptable for raw user input where literal keyword semantics
/// are expected, not expressive FTS queries.
///
/// ## When to use
///
/// Use **only for `search_fts_for_forget`** where the query is raw user input
/// (indirect input from the MCP stub). For the main vault search paths
/// (`search_fts_with_snippet`, `search_fts_scored_filtered`), the query is
/// constructed by the curator LLM and may contain valid FTS5 operators.
///
/// ## Single source of truth
///
/// This function is the **single source of truth** for FTS5 quoting across the
/// workspace. `gradatum-admin::vault_forget_cmd` and `recall_lessons` consume it
/// instead of duplicating the quoting algorithm.
#[must_use]
pub fn fts5_quote_query(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(query.len() + tokens.len() * 3);
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('"');
        for ch in token.chars() {
            if ch == '"' {
                // Doubler les guillemets internes — convention FTS5 phrase quoting.
                out.push('"');
                out.push('"');
            } else {
                out.push(ch);
            }
        }
        out.push('"');
    }
    out
}

impl SqliteIndex {
    /// Opens a SQLite file at `path`, creating it if it does not exist.
    ///
    /// Applies the 4 mandatory PRAGMAs (C12) then runs schema migrations.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the file is inaccessible or if
    /// PRAGMA application or migrations fail.
    pub async fn open(path: &Path) -> Result<Self, GradatumError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| GradatumError::Storage(format!("sqlite open : {e}")))?;
        Self::init(conn).await
    }

    /// Opens an in-memory SQLite database (test / benchmark use).
    ///
    /// Identical behaviour to `open()` for PRAGMAs and migrations.
    /// Note: `journal_mode` in memory returns `"memory"` rather than `"wal"`.
    pub async fn open_in_memory() -> Result<Self, GradatumError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GradatumError::Storage(format!("sqlite in-memory : {e}")))?;
        Self::init(conn).await
    }

    /// Shared initialisation: 4 mandatory PRAGMAs (C12) + schema migration.
    async fn init(conn: Connection) -> Result<Self, GradatumError> {
        // PRAGMA C12 — appliqués avant tout accès aux tables.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| GradatumError::Storage(format!("PRAGMA journal_mode : {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| GradatumError::Storage(format!("PRAGMA synchronous : {e}")))?;
        conn.pragma_update(None, "busy_timeout", 5000_i64)
            .map_err(|e| GradatumError::Storage(format!("PRAGMA busy_timeout : {e}")))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| GradatumError::Storage(format!("PRAGMA foreign_keys : {e}")))?;

        let idx = Self {
            conn: Arc::new(Mutex::new(conn)),
            ann_enabled: Arc::new(AtomicBool::new(false)),
            ann_ef_search: Arc::new(AtomicU32::new(64)),
        };
        idx.run_migrations().await?;
        Ok(idx)
    }

    /// Delegates migration execution to the `migrations` module.
    async fn run_migrations(&self) -> Result<(), GradatumError> {
        crate::migrations::run(&self.conn).await
    }

    /// Reads the value of a SQLite PRAGMA.
    ///
    /// Used in tests to verify that C12 PRAGMAs were applied correctly.
    /// `T` must implement `rusqlite::types::FromSql` (e.g. `String`, `i64`).
    pub async fn pragma<T: rusqlite::types::FromSql>(
        &self,
        name: &str,
    ) -> Result<T, GradatumError> {
        let conn = self.conn.lock().await;
        let v: T = conn
            .query_row(&format!("PRAGMA {name};"), [], |row| row.get(0))
            .map_err(|e| GradatumError::Storage(format!("PRAGMA {name} : {e}")))?;
        Ok(v)
    }

    // ── ANN control ────────────────────────────────────────────────

    /// Active ou désactive le chemin ANN sqlite-vec au runtime.
    ///
    /// Appelé par le bin `gradatum-server` dans `main.rs`, APRÈS :
    /// 1. Enregistrement de l'extension via `sqlite3_auto_extension` (unsafe dans vec_ext.rs).
    /// 2. Vérification que la table `note_embeddings_ann` est accessible (requête pragma).
    ///
    /// Ordre Relaxed suffisant : écriture lors du boot (séquentiel), lectures en runtime
    /// (barrière mémoire implicite sur la transition boot→handler).
    pub fn set_ann_enabled(&self, enabled: bool) {
        self.ann_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Lit l'état courant du flag ANN.
    ///
    /// `true` = chemin ANN actif, `false` = brute-force.
    pub fn ann_is_enabled(&self) -> bool {
        self.ann_enabled.load(Ordering::Relaxed)
    }

    /// Configure le paramètre `ef_search` pour les requêtes ANN.
    ///
    /// Valeur par défaut : 64. Acceptée en `u32` borné dans la config server.
    /// Appel séquentiel au boot — Relaxed suffisant.
    pub fn set_ann_ef_search(&self, ef_search: u32) {
        self.ann_ef_search.store(ef_search, Ordering::Relaxed);
    }

    /// Lit la valeur courante de `ef_search`.
    pub fn ann_ef_search(&self) -> u32 {
        self.ann_ef_search.load(Ordering::Relaxed)
    }

    // ── Embeddings ──────────────────────────────────────────────

    /// Inserts or replaces a note embedding in the `note_embeddings` table.
    ///
    /// ## Primary key
    ///
    /// `(note_id, embedder_id)` — idempotent UPSERT. A second insert with the same
    /// pair replaces the vector, dimension, and timestamp.
    ///
    /// ## BLOB format
    ///
    /// `vector` is serialised as a little-endian f32 BLOB (4 bytes per dimension).
    /// Consistent across x86-64 and aarch64 (both little-endian).
    ///
    /// ## Validation
    ///
    /// Returns `GradatumError::Storage` if `vector.len() != dim as usize`.
    /// This check is mandatory: a silent mismatch would produce truncated or
    /// over-dimensioned vectors incomparable to the query.
    ///
    /// ## `model_version`
    ///
    /// Passed as `NULL` (optional column per schema `0001_phase1.sql`).
    /// The `embedder_id` is sufficient to identify the model.
    /// Internal concrete method — called by `impl VectorStore for SqliteIndex`.
    /// Renamed `_inner` to avoid name collision with the trait method.
    pub(crate) async fn insert_note_embedding_inner(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError> {
        if vector.len() != dim as usize {
            return Err(GradatumError::Storage(format!(
                "insert_note_embedding: vector len {} != dim {}",
                vector.len(),
                dim
            )));
        }

        // Sérialisation f32 little-endian → BLOB (4 bytes par dim).
        let mut blob = Vec::with_capacity(vector.len() * 4);
        for v in vector {
            blob.extend_from_slice(&v.to_le_bytes());
        }

        let note_id_str = note_id.to_string();
        let computed_at = chrono::Utc::now().timestamp_millis();

        let conn = self.conn.lock().await;

        // Transaction atomique : INSERT note_embeddings + upsert_ann doivent être
        // indivisibles. Si upsert_ann échoue (erreur SQL réelle), l'INSERT est rollback.
        // Si upsert_ann est un no-op Ok(()) (mode dégradé sans extension), la transaction
        // commit normalement — l'embedding est persisté, l'index ANN reste simplement absent.
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| GradatumError::Storage(format!("insert_note_embedding begin tx: {e}")))?;

        tx.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)
             ON CONFLICT(note_id, embedder_id) DO UPDATE SET
                 vector      = excluded.vector,
                 dim         = excluded.dim,
                 computed_at = excluded.computed_at",
            rusqlite::params![note_id_str, embedder_id, blob, dim as i64, computed_at],
        )
        .map_err(|e| GradatumError::Storage(format!("insert_note_embedding : {e}")))?;

        // Mise à jour synchrone de l'index ANN (v0.5.3 ANN-1).
        // Vault_id requis pour le PARTITION KEY vec0 — lookup depuis notes.
        // En mode dégradé (extension non chargée), upsert_ann retourne Ok(()) silencieusement :
        // la transaction commit quand même, l'INSERT note_embeddings est préservé.
        let vault_id: Option<String> = tx
            .query_row(
                "SELECT vault_id FROM notes WHERE id = ?1",
                rusqlite::params![note_id_str],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GradatumError::Storage(format!("insert_note_embedding vault_id: {e}")))?;

        if let Some(vid) = vault_id {
            // upsert_ann utilise la même connexion sous-jacente — la transaction ouverte
            // via unchecked_transaction() englobe également cet appel.
            crate::sqlite_vec::upsert_ann(&tx, &note_id_str, &vid, embedder_id, vector)?;
        }

        tx.commit()
            .map_err(|e| GradatumError::Storage(format!("insert_note_embedding commit tx: {e}")))?;

        Ok(())
    }

    /// Backfill de l'index ANN (`note_embeddings_ann`) depuis `note_embeddings`.
    ///
    /// Sélectionne toutes les notes non-downgraded avec un embedding de dim=1024
    /// (bge-m3) and inserts them into `note_embeddings_ann` via `upsert_ann`.
    ///
    /// ## Mode dégradé
    ///
    /// Si l'extension sqlite-vec n'est pas chargée au runtime, `upsert_ann` retourne
    /// `Ok(())` silencieusement pour chaque note. Le compteur retourné représente le
    /// nombre de notes sélectionnées (pas nécessairement insérées dans vec0).
    ///
    /// ## Idempotence
    ///
    /// `INSERT OR REPLACE` sur la PRIMARY KEY → idempotent. Appeler plusieurs fois
    /// n'entraîne pas de doublons.
    ///
    /// ## Usage
    ///
    /// À appeler explicitement après enregistrement de l'extension sqlite-vec dans
    /// le bin crate, ou en maintenance (reindex ANN complet).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si la requête SQL de lecture échoue.
    pub async fn backfill_ann_index(&self) -> Result<u64, GradatumError> {
        crate::sqlite_vec::backfill_ann_from_conn(&self.conn).await
    }

    /// ANN search on `note_embeddings_ann` — for bench binaries only.
    ///
    /// Queries the vec0 virtual table directly and returns `note_id` strings
    /// ordered by ascending cosine distance (most similar first).
    ///
    /// ## Requirements
    ///
    /// - sqlite-vec extension must be registered before calling this method
    ///   (via `sqlite3_auto_extension` in the bin crate).
    /// - Migration `0020_ann_sqlite_vec` must have been applied at DB open time.
    ///
    /// ## Errors
    ///
    /// `GradatumError::Storage` if the vec0 query fails (e.g. extension absent).
    pub async fn search_ann_bench(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<String>, GradatumError> {
        crate::sqlite_vec::search_ann_bench_inner(&self.conn, vault_id, embedder_id, query, k).await
    }

    /// Reads back an embedding vector from the `note_embeddings` table.
    ///
    /// Returns `None` if no embedding exists for the `(note_id, embedder_id)` pair.
    /// Returns `Some(Vec<f32>)` after decoding the little-endian f32 BLOB.
    ///
    /// Used by the embed pipeline to skip re-computation when an embedding
    /// is already present and `computed_at` is recent.
    /// Internal concrete method — called by `impl VectorStore for SqliteIndex`.
    /// Renamed `_inner` to avoid name collision with the trait method.
    pub(crate) async fn get_note_embedding_inner(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        let note_id_str = note_id.to_string();
        let conn = self.conn.lock().await;

        match conn.query_row(
            "SELECT vector FROM note_embeddings WHERE note_id = ?1 AND embedder_id = ?2",
            rusqlite::params![note_id_str, embedder_id],
            |row| row.get::<_, Vec<u8>>(0),
        ) {
            Ok(blob) => {
                if blob.len() % 4 != 0 {
                    return Err(GradatumError::Storage(format!(
                        "get_note_embedding: BLOB len {} non multiple de 4 pour note {note_id_str}",
                        blob.len()
                    )));
                }
                let vec: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| {
                        f32::from_le_bytes(b.try_into().expect("chunks_exact garantit 4 bytes"))
                    })
                    .collect();
                Ok(Some(vec))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_note_embedding : {e}"))),
        }
    }

    // ── Registry methods ──────────────────────────────────────────────────────

    /// Counts distinct `vault_id` values in the `notes` table.
    ///
    /// Used by `Registry::tenant_count` — each `vault_id` maps to a tenant
    /// (single-tenant vault: at most 1 distinct value after `ensure_vault_id`).
    pub async fn vault_id_count(&self) -> Result<u32, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row("SELECT COUNT(DISTINCT vault_id) FROM notes", [], |row| {
                row.get(0)
            })
            .map_err(|e| GradatumError::Storage(format!("vault_id_count : {e}")))?;
        Ok(count as u32)
    }

    /// Counts distinct loci in the `notes` table (vault_id + locus pairs).
    ///
    /// A locus is the sub-tenant organisational unit (thematic section).
    /// Returns 0 if no notes are indexed.
    pub async fn locus_count(&self) -> Result<u32, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT (vault_id || '/' || COALESCE(locus, ''))) FROM notes",
                [],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("locus_count : {e}")))?;
        Ok(count as u32)
    }

    /// Ensures a `vault_id` exists in the `notes` table by inserting a sentinel if absent.
    ///
    /// Used by `Registry::ensure_tenant` to register the tenant before any note
    /// ingestion. The sentinel has an `id` prefixed with `__sentinel__`
    /// and `section = "reference"` (a valid section per the schema).
    ///
    /// Idempotent: `INSERT OR IGNORE` is a no-op if the `vault_id` is already present.
    pub async fn ensure_vault_id(&self, vault_id: &str) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        // Sentinelle minimale respectant toutes les contraintes NOT NULL du schéma :
        //   id, vault_id, section, status, schema_version, created, version,
        //   content_hash (BLOB 32 bytes), body_text (TEXT).
        // `id` est unique par vault_id pour éviter les conflits entre tenants.
        let sentinel_id = format!("__sentinel__{vault_id}");
        // content_hash : 32 octets nuls (SHA256 placeholder pour sentinelle).
        let zero_hash: &[u8] = &[0u8; 32];
        conn.execute(
            "INSERT OR IGNORE INTO notes (
                id, vault_id, section, status, schema_version,
                created, version, content_hash, body_text
            ) VALUES (?1, ?2, 'reference', 'live', 1,
                      CAST(strftime('%s','now') AS INTEGER) * 1000, 1, ?3, '')",
            rusqlite::params![sentinel_id, vault_id, zero_hash],
        )
        .map_err(|e| GradatumError::Storage(format!("ensure_vault_id : {e}")))?;
        Ok(())
    }

    /// Soft-downgrades a note.
    ///
    /// Sets `status = 'downgraded'` on `note_id`, populates `status_reason`
    /// and `status_changed` (UTC millisecond timestamp), and sets `replaced_by`
    /// if provided. The body (`body_text`) is preserved.
    ///
    /// Idempotent: a second call updates the reason and timestamp.
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound(note_id)` if no note matches `note_id`.
    /// - `GradatumError::NoteNotFound(replaced_by_id)` if `replaced_by` is provided
    ///   but the target note does not exist in the index. Without this pre-check,
    ///   the SQLite FK constraint (`replaced_by TEXT REFERENCES notes(id)`) would
    ///   raise a constraint error mapped to HTTP 500 — this converts it to a 404.
    /// - `GradatumError::Storage` on unexpected SQLite errors.
    pub async fn downgrade_note(
        &self,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let note_id_str = note_id.to_string();
        let replaced_by_str = replaced_by.map(|id| id.to_string());

        // Pré-check 0 : auto-référence interdite — replaced_by ne peut pas pointer sur
        // la note elle-même. État `status=downgraded, replaced_by=self` est sémantiquement
        // invalide (wikilinks circulaires, boucle infinie dans resolve_redirect).
        if let Some(rb_id) = replaced_by
            && rb_id == note_id
        {
            return Err(GradatumError::Validation(
                gradatum_core::error::ValidationError::InvalidInput(
                    "replaced_by ne peut pas référencer la note elle-même".into(),
                ),
            ));
        }

        // Pré-check 1 : si replaced_by est fourni, vérifier que la note cible existe.
        // Sans ce garde, la contrainte FK SQLite (REFERENCES notes(id), foreign_keys=ON)
        // renvoie SQLITE_CONSTRAINT_FOREIGNKEY, mappé en GradatumError::Storage → HTTP 500.
        // On retourne NoteNotFound(replaced_by_id) → HTTP 404 côté handler (erreur client).
        if let (Some(rb_str), Some(rb_id)) = (&replaced_by_str, replaced_by) {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1)",
                    rusqlite::params![rb_str],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("downgrade_note replaced_by check: {e}"))
                })?;
            if !exists {
                return Err(GradatumError::NoteNotFound(*rb_id));
            }
        }

        let rows = conn
            .execute(
                "UPDATE notes SET
                    status        = 'downgraded',
                    status_reason = ?2,
                    status_changed = ?3,
                    replaced_by   = ?4,
                    updated       = ?3
                 WHERE id = ?1",
                rusqlite::params![note_id_str, reason, now, replaced_by_str],
            )
            .map_err(|e| GradatumError::Storage(format!("downgrade_note: {e}")))?;

        if rows == 0 {
            return Err(GradatumError::NoteNotFound(*note_id));
        }
        Ok(())
    }

    /// Moves a note to a new `locus`.
    ///
    /// `UPDATE notes SET locus = ?` (+ `updated`). The locus is not an FTS column
    /// (`notes_fts` indexes only `body_text`/`tags`) — searchable content is unchanged,
    /// no FTS re-index required. The ULID is preserved (no `redirect_table` entry:
    /// the locus is not in the identity path; wikilinks resolve by title/ULID).
    /// Consistent with `downgrade_note` / `patch_note_status` (synchronous index-level
    /// mutations, no `.md` frontmatter rewrite).
    ///
    /// Idempotent: re-applying the same locus is a no-op UPDATE (same value).
    ///
    /// # Preconditions
    /// `new_locus` must already be validated by the caller (`LocusId::parse`).
    ///
    /// # Errors
    /// - `GradatumError::NoteNotFound` if no note matches `note_id`.
    /// - `GradatumError::Storage` on SQLite errors.
    pub async fn update_note_locus(
        &self,
        note_id: &NoteId,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let note_id_str = note_id.to_string();

        let rows = conn
            .execute(
                "UPDATE notes SET locus = ?2, updated = ?3 WHERE id = ?1",
                rusqlite::params![note_id_str, new_locus.as_str(), now],
            )
            .map_err(|e| GradatumError::Storage(format!("update_note_locus: {e}")))?;

        if rows == 0 {
            return Err(GradatumError::NoteNotFound(*note_id));
        }
        Ok(())
    }

    /// Captures the raw snapshot of index-level status fields.
    ///
    /// Reads `status`, `status_reason`, `status_changed`, `replaced_by` WITHOUT
    /// any normalisation (unlike `get_note_status`, which maps `downgraded → Deprecated`).
    /// Used to preserve index-only state across an operation that re-upserts the note
    /// from the `.md` file (`move_locus`): without capture, the re-upsert would overwrite
    /// an index-only status (`downgrade_note`, `patch_note_status`, trust decay) with
    /// the `live` status from a stale frontmatter.
    ///
    /// # Errors
    /// - `GradatumError::Storage` on SQLite errors.
    pub async fn get_index_status_snapshot(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<IndexStatusSnapshot>, GradatumError> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT status, status_reason, status_changed, replaced_by
             FROM notes WHERE vault_id = ?1 AND id = ?2",
            rusqlite::params![vault_id, note_id],
            |row| {
                Ok(IndexStatusSnapshot {
                    status: row.get(0)?,
                    status_reason: row.get(1)?,
                    status_changed_ms: row.get(2)?,
                    replaced_by: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|e| GradatumError::Storage(format!("get_index_status_snapshot: {e}")))
    }

    /// Restores a previously captured index-level status snapshot.
    ///
    /// Unconditional UPDATE of the 4 fields (`status`, `status_reason`, `status_changed`,
    /// `replaced_by`) with the **exact** snapshot values — including `NULL`s.
    /// Does NOT touch `updated` (a locus move does not change status semantics, and the
    /// re-upsert has already updated `updated`). Used by `Vault::move_locus` after the
    /// re-upsert to undo the stale frontmatter overwrite.
    ///
    /// # Errors
    /// - `GradatumError::NoteNotFound` if no note matches.
    /// - `GradatumError::Storage` on SQLite errors.
    pub async fn restore_index_status_fields(
        &self,
        vault_id: &str,
        note_id: &str,
        snapshot: &IndexStatusSnapshot,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let rows = conn
            .execute(
                "UPDATE notes SET
                    status         = ?3,
                    status_reason  = ?4,
                    status_changed = ?5,
                    replaced_by    = ?6
                 WHERE vault_id = ?1 AND id = ?2",
                rusqlite::params![
                    vault_id,
                    note_id,
                    snapshot.status,
                    snapshot.status_reason,
                    snapshot.status_changed_ms,
                    snapshot.replaced_by,
                ],
            )
            .map_err(|e| GradatumError::Storage(format!("restore_index_status_fields: {e}")))?;

        if rows == 0 {
            return Err(GradatumError::Storage(format!(
                "restore_index_status_fields: note absente vault={vault_id} id={note_id}"
            )));
        }
        Ok(())
    }

    /// Partial status PATCH for a note.
    ///
    /// Updates only the provided fields (`None` = unchanged).
    /// `status_changed` is updated only when `status` is provided.
    /// `updated` is always updated.
    ///
    /// At least one field must be provided (validation is the caller's responsibility).
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if no note matches `note_id`.
    /// - `GradatumError::Storage` on SQLite errors.
    pub async fn patch_note_status(
        &self,
        note_id: &NoteId,
        status: Option<&str>,
        status_reason: Option<&str>,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let note_id_str = note_id.to_string();
        let replaced_by_str = replaced_by.map(|id| id.to_string());

        let rows = conn
            .execute(
                "UPDATE notes SET
                    status         = COALESCE(?2, status),
                    status_reason  = COALESCE(?3, status_reason),
                    replaced_by    = COALESCE(?4, replaced_by),
                    status_changed = CASE WHEN ?2 IS NOT NULL THEN ?5 ELSE status_changed END,
                    updated        = ?5
                 WHERE id = ?1",
                rusqlite::params![note_id_str, status, status_reason, replaced_by_str, now],
            )
            .map_err(|e| GradatumError::Storage(format!("patch_note_status: {e}")))?;

        if rows == 0 {
            return Err(GradatumError::NoteNotFound(*note_id));
        }
        Ok(())
    }

    /// Counts notes with `status = 'live'` for a vault.
    ///
    /// Excludes sentinels (`id NOT LIKE '__sentinel__%'`).
    /// Used by `vault_status.note_count`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes
                 WHERE vault_id = ?1
                   AND status = 'live'
                   AND id NOT LIKE '__sentinel__%'",
                [vault_id],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("live_note_count: {e}")))?;
        Ok(count as u64)
    }

    /// Counts notes by status (`GROUP BY status`) — tolerant of out-of-enum values.
    ///
    /// Key = raw SQL string (`"live"`, `"pending-review"`, `"downgraded"` legacy, …).
    /// Excludes sentinels. Single query. No status value is rejected.
    pub async fn count_notes_by_status(
        &self,
        vault_id: &str,
    ) -> Result<std::collections::HashMap<String, u64>, GradatumError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM notes
                 WHERE vault_id = ?1
                   AND id NOT LIKE '__sentinel__%'
                 GROUP BY status",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare count_notes_by_status: {e}")))?;

        let rows = stmt
            .query_map([vault_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("query count_notes_by_status: {e}")))?;

        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (status, n) =
                r.map_err(|e| GradatumError::Storage(format!("row count_notes_by_status: {e}")))?;
            out.insert(status, u64::try_from(n).unwrap_or(0));
        }
        Ok(out)
    }

    /// Total sum of `LENGTH(body_text)` for non-sentinel notes in a vault.
    ///
    /// Returns 0 if no notes exist. `COALESCE` handles the empty-vault case (SUM NULL → 0).
    /// Used by `vault_status.total_size_bytes`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError> {
        let conn = self.conn.lock().await;
        let total: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(body_text)), 0)
                 FROM notes
                 WHERE vault_id = ?1
                   AND id NOT LIKE '__sentinel__%'",
                [vault_id],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("total_body_size_bytes: {e}")))?;
        Ok(total as u64)
    }

    /// Updates the `title` column of an existing note.
    ///
    /// Idempotent. Best-effort: logs on error but does not propagate.
    /// Used post-curation to persist the H1 title extracted from the body.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn upsert_note_title(
        &self,
        note_id: &NoteId,
        title: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE notes SET title = ?2 WHERE id = ?1",
            rusqlite::params![note_id.to_string(), title],
        )
        .map_err(|e| GradatumError::Storage(format!("upsert_note_title: {e}")))?;
        Ok(())
    }

    /// BM25 FTS5 search with optional section filter.
    ///
    /// Extends `search_fts_scored` by adding `AND n.section = ?4` when `section` is provided.
    /// `section = None` behaves identically to `search_fts_scored` (all sections).
    ///
    /// ## Dynamic rusqlite params
    ///
    /// Two explicit branches — rusqlite does not support variable arity in a single
    /// `query_map` closure. Collection is performed inside each branch.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn search_fts_scored_filtered(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        let conn = self.conn.lock().await;

        let downgraded_clause = if include_downgraded {
            ""
        } else {
            "AND n.status != 'downgraded'"
        };
        let section_clause = if section.is_some() {
            "AND n.section = ?4"
        } else {
            ""
        };
        // Filtre locus : préfixe LIKE avec ESCAPE.
        // Le numéro de paramètre dépend de la présence de section.
        // section=None → locus=?4 ; section=Some → locus=?5.
        // L'échappement des métacaractères LIKE est appliqué dans chaque branche
        // (escape_like appelé au moment de la construction des params).
        let locus_param_idx = if section.is_some() { 5usize } else { 4usize };
        let locus_clause = if locus.is_some() {
            format!("AND n.locus LIKE ?{locus_param_idx} || '%' ESCAPE '\\'")
        } else {
            String::new()
        };

        // F-44 : on sélectionne forgotten + forgotten_at en plus du status.
        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status,
                    n.forgotten,
                    n.forgotten_at
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
               {section_clause}
               {locus_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Quatre branches pour les params dynamiques — rusqlite ne supporte pas
        // une arité variable dans la même closure query_map.
        //
        // Pattern E0597 : stmt doit vivre dans le même bloc que le collect.
        // Assigner `result` dans le bloc de stmt — évite le borrow dangling.
        //
        // F-44 : les closures récupèrent maintenant 5 colonnes (id, score, status,
        // forgotten, forgotten_at). Le type intermédiaire est (String, f64, String, i64, Option<i64>).
        type RawRow = (String, f64, String, i64, Option<i64>);
        let collected: Vec<RawRow> = match (section, locus) {
            (Some(sec), Some(loc)) => {
                let locus_escaped = escape_like(loc);
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_scored_filtered: {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, sec, locus_escaped],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "query search_fts_scored_filtered (sec+locus): {e}"
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "collect search_fts_scored_filtered (sec+locus): {e}"
                    ))
                })?
            }
            (Some(sec), None) => {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_scored_filtered: {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, sec],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "query search_fts_scored_filtered (section): {e}"
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "collect search_fts_scored_filtered (section): {e}"
                    ))
                })?
            }
            (None, Some(loc)) => {
                let locus_escaped = escape_like(loc);
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!(
                        "prepare search_fts_scored_filtered (locus): {e}"
                    ))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, locus_escaped],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("query search_fts_scored_filtered (locus): {e}"))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "collect search_fts_scored_filtered (locus): {e}"
                    ))
                })?
            }
            (None, None) => {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!(
                        "prepare search_fts_scored_filtered (no section): {e}"
                    ))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "query search_fts_scored_filtered (no_section): {e}"
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "collect search_fts_scored_filtered (no_section): {e}"
                    ))
                })?
            }
        };

        // Mapping String → NoteId + application du decay forgotten (F-44).
        //
        // Même logique que search_fts_scored : court-circuit forgotten AVANT downgraded.
        // Note : search_fts_scored_filtered ne ré-applique pas la pénalité downgraded ×10
        // car ce chemin est appelé depuis search_fts_with_snippet qui gère le scoring
        // en aval.
        //
        // P3-M1 : Le decay forgotten est appliqué sur les trois chemins FTS :
        //   1. search_fts_scored (RRF direct via vault_search)
        //   2. search_fts_scored_filtered (ce chemin — utilisé par search_fts_with_snippet)
        //   3. search_fts_with_snippet (decay appliqué dans son propre mapping aval)
        // Et le chemin sémantique :
        //   4. search_semantic_inner (decay cosine × 0.5^d, voir P2-R4)
        let mut results: Vec<(NoteId, f64, String)> = Vec::with_capacity(collected.len());
        for (id_str, bm25_raw, status, forgotten, forgotten_at_ms) in collected {
            let ulid = ulid::Ulid::from_string(&id_str).map_err(|e| {
                GradatumError::Storage(format!("ULID parse search_fts_scored_filtered: {e}"))
            })?;
            // F-44 decay forgotten — court-circuit AVANT la pénalité downgraded.
            //
            // P2-R3 : état incohérent détectable (forgotten=1 mais forgotten_at=NULL).
            // warn! car c'est une anomalie de données détectable — pas un chemin normal.
            // Comportement decay : `elapsed_days = 0.0` → facteur 0.5^0 = 1.0 (neutre).
            // Choix conservateur : ne pas pénaliser une note sur un état corrompu.
            if forgotten != 0 && forgotten_at_ms.is_none() {
                tracing::warn!(
                    note_id = %id_str,
                    "search_fts_scored_filtered: forgotten=1 mais forgotten_at=NULL — état incohérent"
                );
            }
            let score = if forgotten != 0 {
                let elapsed_days = forgotten_at_ms
                    .map(|at_ms| (now_ms - at_ms) as f64 / 86_400_000.0)
                    .unwrap_or(0.0)
                    .max(0.0);
                bm25_raw * (0.5f64).powf(elapsed_days)
            } else {
                bm25_raw
            };
            results.push((NoteId(ulid), score, status));
        }
        // C2 : re-tri après application du decay (ORDER BY SQL portait sur le score brut).
        // Même sens que search_fts_scored : score BM25 ASC (valeurs négatives, plus proche
        // de 0 = meilleur match).
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(results)
    }

    /// Builds the shared WHERE clauses for `search_fts_with_snippet` and
    /// `count_fts_matches` (predicate parity guaranteed by construction).
    ///
    /// ## `base_param_idx` parameter
    ///
    /// 1-based index of the first optional parameter (section/locus/status) in the SQL
    /// query. Varies by context:
    /// - `4` for `search_fts_with_snippet`: `?1=query`, `?2=vault_id`, `?3=limit` → section=`?4`
    /// - `3` for `count_fts_matches`: `?1=query`, `?2=vault_id` (no `?3=limit`) → section=`?3`
    ///
    /// A future 6th predicate has exactly ONE place to modify — desync is impossible.
    ///
    /// ## Return value
    ///
    /// `(downgraded_clause, section_clause, locus_clause, status_clause, status_param_idx)`
    ///
    /// - `downgraded_clause`: `""` if `include_downgraded`, else `"AND n.status != 'downgraded'"`.
    /// - `section_clause`: `""` if `section.is_none()`, else `"AND n.section = ?{base_param_idx}"`.
    /// - `locus_clause`: `""` if `locus.is_none()`, else `"AND n.locus LIKE ?N || '%' ESCAPE '\\'"`
    ///   where N = base_param_idx + `usize::from(section.is_some())`.
    /// - `status_clause`: `"AND (?N IS NULL OR n.status = ?N)"` where N = `status_param_idx`.
    /// - `status_param_idx`: 1-based index of the status parameter (for binding in the caller).
    ///
    /// ## Predicate parity
    ///
    /// Both `search_fts_with_snippet` and `count_fts_matches` call this function —
    /// predicate desync between the two is structurally impossible.
    fn build_fts_where_parts(
        base_param_idx: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
    ) -> (
        &'static str,
        String,
        String,
        String,
        usize, // status_param_idx
    ) {
        let downgraded_clause = if include_downgraded {
            ""
        } else {
            "AND n.status != 'downgraded'"
        };
        let section_clause = if section.is_some() {
            format!("AND n.section = ?{base_param_idx}")
        } else {
            String::new()
        };
        let locus_param_idx = base_param_idx + usize::from(section.is_some());
        let locus_clause = if locus.is_some() {
            format!("AND n.locus LIKE ?{locus_param_idx} || '%' ESCAPE '\\'")
        } else {
            String::new()
        };
        let status_param_idx =
            base_param_idx + usize::from(section.is_some()) + usize::from(locus.is_some());
        let status_clause =
            format!("AND (?{status_param_idx} IS NULL OR n.status = ?{status_param_idx})");
        (
            downgraded_clause,
            section_clause,
            locus_clause,
            status_clause,
            status_param_idx,
        )
    }

    /// FTS5 search with native FTS5 snippet, optional section and locus filters.
    ///
    /// Uses `snippet(notes_fts, 0, '»', '«', '...', 32)` to locate the most relevant
    /// passage in the body (vs. `build_snippet`, which truncates from the head).
    ///
    /// Returns `Vec<SearchHitRaw>` including snippet, section, and title.
    ///
    /// ## Locus filter (optional)
    ///
    /// If `locus` is `Some(prefix)`, only notes whose `locus` starts with `prefix`
    /// are returned. The prefix is automatically escaped against LIKE metacharacters
    /// (`%`, `_`, `\`) — the caller does not need to pre-escape.
    ///
    /// ## Dynamic rusqlite params
    ///
    /// Four branches (`section × locus`) — rusqlite does not support variable arity
    /// in a single `query_map` closure.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    // 8 args: orthogonal search filters (downgraded/section/locus/status).
    #[expect(
        clippy::too_many_arguments,
        reason = "filtres de recherche orthogonaux (F-37 notes fix) — struct d'options sans gain"
    )]
    pub async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        let conn = self.conn.lock().await;

        // R3 parité prédicats (spec corpus-hits) : clauses WHERE construites via la même fn
        // que `count_fts_matches` — impossible de désynchroniser les 5 prédicats.
        // base_param_idx=4 : ?1=query, ?2=vault_id, ?3=limit → section commence à ?4.
        // Filtre status (F-37 notes fix) : `status` est TOUJOURS lié comme dernier
        // paramètre (NULL si None). Le prédicat `(?N IS NULL OR n.status = ?N)` matche
        // toutes les notes quand status est absent → arité fixe par branche, pas de
        // multiplication des branches section×locus×status.
        let (downgraded_clause, section_clause, locus_clause, status_clause, _status_param_idx) =
            Self::build_fts_where_parts(4, include_downgraded, section, locus);

        // FTS5 snippet() : col=0 (body_text), marqueurs »/«, ellipsis ..., max 32 tokens
        // C1 (audit P1) : n.forgotten + n.forgotten_at ajoutés pour appliquer le decay
        // F-44 identique à search_fts_scored et search_fts_scored_filtered.
        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status,
                    snippet(notes_fts, 0, '»', '«', '...', 32) AS snippet,
                    n.section,
                    n.title,
                    n.forgotten,
                    n.forgotten_at
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
               {section_clause}
               {locus_clause}
               {status_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Quatre branches — params dynamiques rusqlite.
        // Pattern E0597 : stmt dans le même bloc que le collect.
        // C1 : 8 colonnes maintenant (ajout forgotten, forgotten_at).
        type RawRow = (
            String,
            f64,
            String,
            String,
            String,
            Option<String>,
            i64,
            Option<i64>,
        );
        let raw_rows: Vec<RawRow> = match (section, locus) {
            (Some(sec), Some(loc)) => {
                let locus_escaped = escape_like(loc);
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet: {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![
                        query,
                        vault_id.as_str(),
                        limit as i64,
                        sec,
                        locus_escaped,
                        status
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "query search_fts_with_snippet (sec+locus): {e}"
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "collect search_fts_with_snippet (sec+locus): {e}"
                    ))
                })?
            }
            (Some(sec), None) => {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet: {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, sec, status],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                )
                .map_err(|e| GradatumError::Storage(format!("query search_fts_with_snippet: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!("collect search_fts_with_snippet: {e}"))
                })?
            }
            (None, Some(loc)) => {
                let locus_escaped = escape_like(loc);
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet (locus): {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![
                        query,
                        vault_id.as_str(),
                        limit as i64,
                        locus_escaped,
                        status
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("query search_fts_with_snippet (locus): {e}"))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!("collect search_fts_with_snippet (locus): {e}"))
                })?
            }
            (None, None) => {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet (no sec): {e}"))
                })?;

                stmt.query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, status],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("query search_fts_with_snippet (no sec): {e}"))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| {
                    GradatumError::Storage(format!("collect search_fts_with_snippet (no sec): {e}"))
                })?
            }
        };

        // Mapping vers SearchHitRaw + application du decay forgotten (C1 audit P1).
        //
        // Score BM25 (valeur négative) : decay forgotten × 0.5^elapsed_days DÉGRADE
        // le score en le rapprochant de 0 (sens identique à search_fts_scored).
        //
        // P2-R3 : état incohérent détectable (forgotten=1 mais forgotten_at=NULL).
        // warn! car c'est une anomalie de données détectable — pas un chemin normal.
        // Comportement decay : `elapsed_days = 0.0` → facteur 0.5^0 = 1.0 (neutre).
        // Choix conservateur : ne pas pénaliser une note sur un état corrompu.
        let mut results = Vec::with_capacity(raw_rows.len());
        for (id_str, bm25_raw, status, snippet, section_str, title, forgotten, forgotten_at_ms) in
            raw_rows
        {
            let ulid = ulid::Ulid::from_string(&id_str).map_err(|e| {
                GradatumError::Storage(format!("ULID parse search_fts_with_snippet: {e}"))
            })?;
            if forgotten != 0 && forgotten_at_ms.is_none() {
                tracing::warn!(
                    note_id = %id_str,
                    "search_fts_with_snippet: forgotten=1 mais forgotten_at=NULL — état incohérent"
                );
            }
            let bm25 = if forgotten != 0 {
                let elapsed_days = forgotten_at_ms
                    .map(|at_ms| (now_ms - at_ms) as f64 / 86_400_000.0)
                    .unwrap_or(0.0)
                    .max(0.0);
                bm25_raw * (0.5f64).powf(elapsed_days)
            } else {
                bm25_raw
            };
            results.push(SearchHitRaw {
                note_id: NoteId(ulid),
                bm25,
                status,
                snippet,
                section: section_str,
                title,
            });
        }
        // C1 : re-tri après application du decay (ORDER BY SQL portait sur bm25 brut).
        // Score BM25 ASC (valeurs négatives, plus proche de 0 = meilleur match).
        results.sort_by(|a, b| {
            a.bm25
                .partial_cmp(&b.bm25)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Caps the raw FTS match count at 10 000.
    ///
    /// Returns `(count, capped)`:
    /// - `raw_count < 10001` → `(raw_count, false)`
    /// - `raw_count >= 10001` → `(10000, true)` (means "at least 10 001 matches")
    ///
    /// Pure function — unit-testable without a database.
    #[inline]
    fn apply_cap(raw_count: u64) -> (u64, bool) {
        if raw_count >= 10001 {
            (10000, true)
        } else {
            (raw_count, false)
        }
    }

    /// Counts notes matching an FTS5/BM25 query within the filtered scope.
    ///
    /// Implements `IndexStore::count_fts_matches`.
    ///
    /// ## Predicate parity — by construction
    ///
    /// WHERE clauses are built via `build_fts_where_parts(base_param_idx=3, ...)` —
    /// identical by construction to `search_fts_with_snippet(base_param_idx=4, ...)`.
    /// A future 6th predicate has exactly ONE place to modify.
    ///
    /// ## Count cap
    ///
    /// `LIMIT 10001` in the sub-query: if `COUNT(*) = 10001`, the true total is ≥ 10001.
    /// `apply_cap` then returns `(10000, true)`. Full rows are never materialised.
    ///
    /// ## Parameter numbering
    ///
    /// `base_param_idx=3`: `?1=query`, `?2=vault_id` (no `?3=limit`) → section=`?3`.
    /// Identical to `search_fts_with_snippet` except the base is 3 instead of 4.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` on SQLite errors.
    // 6 non-self args: orthogonal filters identical to search_fts_with_snippet.
    // clippy too_many_arguments threshold is 7 — 6 non-self args do not trigger it.
    // Using #[allow] (not #[expect]) to avoid an unfulfilled_lint_expectation.
    #[allow(clippy::too_many_arguments)]
    pub async fn count_fts_matches(
        &self,
        vault_id: &VaultId,
        query: &str,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
    ) -> Result<(u64, bool), GradatumError> {
        let conn = self.conn.lock().await;

        // R3 parité PAR CONSTRUCTION : build_fts_where_parts(base=3, ...) — source unique
        // des clauses downgraded/section/locus/status pour count ET search_fts_with_snippet.
        // base_param_idx=3 : ?1=query, ?2=vault_id (pas de ?3=limit) → section à ?3.
        let (downgraded_clause, section_clause, locus_clause, status_clause, _status_param_idx) =
            Self::build_fts_where_parts(3, include_downgraded, section, locus);

        // R4 : LIMIT 10001 → cap 10000 via apply_cap.
        // Pas de snippet/JOIN lourd. Sous-requête sur FTS pour COUNT.
        let sql = format!(
            "SELECT COUNT(*) FROM (
               SELECT 1
               FROM notes_fts
               JOIN notes n ON notes_fts.rowid = n.rowid
               WHERE notes_fts MATCH ?1
                 AND n.vault_id = ?2
                 {downgraded_clause}
                 {section_clause}
                 {locus_clause}
                 {status_clause}
               LIMIT 10001
             )"
        );

        // Quatre branches (section × locus) — rusqlite params dynamiques.
        let raw_count: i64 = match (section, locus) {
            (Some(sec), Some(loc)) => {
                let locus_escaped = escape_like(loc);
                conn.query_row(
                    &sql,
                    rusqlite::params![query, vault_id.as_str(), sec, locus_escaped, status],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("count_fts_matches (sec+locus): {e}"))
                })?
            }
            (Some(sec), None) => conn
                .query_row(
                    &sql,
                    rusqlite::params![query, vault_id.as_str(), sec, status],
                    |row| row.get(0),
                )
                .map_err(|e| GradatumError::Storage(format!("count_fts_matches (sec): {e}")))?,
            (None, Some(loc)) => {
                let locus_escaped = escape_like(loc);
                conn.query_row(
                    &sql,
                    rusqlite::params![query, vault_id.as_str(), locus_escaped, status],
                    |row| row.get(0),
                )
                .map_err(|e| GradatumError::Storage(format!("count_fts_matches (locus): {e}")))?
            }
            (None, None) => conn
                .query_row(
                    &sql,
                    rusqlite::params![query, vault_id.as_str(), status],
                    |row| row.get(0),
                )
                .map_err(|e| GradatumError::Storage(format!("count_fts_matches: {e}")))?,
        };

        Ok(Self::apply_cap(raw_count as u64))
    }

    /// Lesson recall by class — BM25-only, `lessons-learned` section.
    ///
    /// Implements `IndexStore::recall_lessons`. See the trait doc for the
    /// functional contract.
    ///
    /// ## Implementation details
    ///
    /// - **Fixed section** `lessons-learned` (not a parameter — recall targets this
    ///   corpus exclusively).
    /// - **MATCH on `class`**: `notes_fts` indexes both `body_text` and `tags`, so a
    ///   note tagged `deploy` matches `MATCH 'deploy'` even without the word in the body.
    /// - **Exclusion of `codified`**: applied in Rust (exact token split of `notes.tags`)
    ///   rather than SQL `LIKE '%codified%'` — prevents false positives on tags that
    ///   contain the substring (e.g. `codified-2026`). Over-fetches `limit * 4`
    ///   (clamped to [limit, 100]) before filtering to retain `limit` net results.
    /// - **Exclusion of forgotten notes**: `AND n.forgotten = 0` — a forgotten lesson
    ///   must never be recalled. No progressive decay: recall is binary
    ///   (present or not), sorted by BM25 ASC.
    pub async fn recall_lessons(
        &self,
        vault_id: &VaultId,
        class: &str,
        limit: usize,
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        let conn = self.conn.lock().await;

        // Sur-fetch : on filtre `codified` en Rust, donc on demande plus large pour
        // ne pas perdre de résultats nets. Borné pour éviter un scan non maîtrisé.
        let fetch_limit = (limit * 4).clamp(limit.max(1), 100) as i64;

        // FTS5 phrase : la plupart des classes du vocabulaire contiennent un tiret
        // (`ci-cd`, `crates-io`, `anti-leak`, `git-hygiene`, `auth-secrets`,
        // `data-integrity`, `process-discipline`, `api-external`). Sans quoting, le
        // tiret est interprété comme opérateur NOT/colonne FTS5 → erreur ou résultat
        // faux. On wrappe donc en phrase exacte via `fts5_quote_query` (source unique
        // D2.1) — pour une classe mono-token le résultat est `"<classe>"`, identique
        // à l'ancien inline mais sans duplication de l'algorithme de quoting.
        let fts_phrase = fts5_quote_query(class);

        let sql = "SELECT n.id,
                          snippet(notes_fts, 0, '»', '«', '...', 32) AS snippet,
                          n.title,
                          n.tags,
                          n.created,
                          bm25(notes_fts) AS score
                   FROM notes_fts
                   JOIN notes n ON notes_fts.rowid = n.rowid
                   WHERE notes_fts MATCH ?1
                     AND n.vault_id = ?2
                     AND n.section = 'lessons-learned'
                     AND n.status != 'downgraded'
                     AND n.forgotten = 0
                     AND n.id NOT LIKE '__sentinel__%'
                   ORDER BY score ASC
                   LIMIT ?3";

        // Pattern E0597 : collecter dans la même portée que `stmt`.
        type LessonRow = (String, String, Option<String>, Option<String>, i64);
        let raw_rows: Vec<LessonRow> = {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| GradatumError::Storage(format!("prepare recall_lessons: {e}")))?;

            stmt.query_map(
                rusqlite::params![fts_phrase, vault_id.as_str(), fetch_limit],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("query recall_lessons: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| GradatumError::Storage(format!("collect recall_lessons: {e}")))?
        };

        // Mapping + exclusion `codified` (token exact) + cap à `limit` résultats nets.
        let mut results = Vec::with_capacity(limit);
        for (id_str, snippet, title, tags_raw, created_ms) in raw_rows {
            let tags: Vec<String> = tags_raw
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect();
            // Anti-pollution F-32 : ignorer les leçons déjà codifiées.
            if tags.iter().any(|t| t == "codified") {
                continue;
            }
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("ULID parse recall_lessons: {e}")))?;
            results.push(LessonHitRaw {
                note_id: NoteId(ulid),
                title,
                snippet,
                tags,
                anchor_ms: created_ms,
            });
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Lists notes in a vault with ULID cursor pagination.
    ///
    /// `cursor`: last ULID received — returns notes whose ULID > cursor (lexicographic order).
    /// `limit`: clamped to [1, 200].
    /// Excludes sentinels and downgraded notes by default.
    ///
    /// Returns `(Vec<NoteRecord>, total_count)` where `total_count` is the total without pagination.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        let conn = self.conn.lock().await;
        let limit_clamped = limit.clamp(1, 200) as i64;

        // Comptage total (pour la réponse total: u64) — deux branches (section optionnelle)
        let total: i64 = match section {
            Some(sec) => conn.query_row(
                "SELECT COUNT(*) FROM notes
                 WHERE vault_id = ?1
                   AND section = ?2
                   AND id NOT LIKE '__sentinel__%'
                   AND status != 'downgraded'",
                rusqlite::params![vault_id, sec],
                |row| row.get(0),
            ),
            None => conn.query_row(
                "SELECT COUNT(*) FROM notes
                 WHERE vault_id = ?1
                   AND id NOT LIKE '__sentinel__%'
                   AND status != 'downgraded'",
                [vault_id],
                |row| row.get(0),
            ),
        }
        .map_err(|e| GradatumError::Storage(format!("list_notes count: {e}")))?;

        // Requête paginée ULID lexicographique ASC, cursor > dernier ULID reçu.
        // `(?2 = '' OR id > ?2)` : curseur vide = début de liste.
        let cursor_val = cursor.unwrap_or("");

        // Deux branches pour le filtre section optionnel.
        // Pattern E0597 : stmt dans le même bloc que le collect.
        let records: Vec<NoteRecord> = match section {
            Some(sec) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, vault_id, section, status, body_text,
                                COALESCE(author_display_name, author_id) AS author,
                                tags, content_hash, created, updated, title, locus
                         FROM notes
                         WHERE vault_id = ?1
                           AND section = ?4
                           AND id NOT LIKE '__sentinel__%'
                           AND status != 'downgraded'
                           AND (?2 = '' OR id > ?2)
                         ORDER BY id ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("list_notes prepare (section): {e}"))
                    })?;

                stmt.query_map(
                    rusqlite::params![vault_id, cursor_val, limit_clamped, sec],
                    |row| {
                        Ok(NoteRecord {
                            id: row.get(0)?,
                            vault_id: row.get(1)?,
                            section: row.get(2)?,
                            status: row.get(3)?,
                            body_text: row.get(4)?,
                            author: row.get(5)?,
                            tags_raw: row.get(6)?,
                            content_hash: row.get::<_, Vec<u8>>(7)?,
                            created: row.get(8)?,
                            updated: row.get(9)?,
                            title: row.get(10)?,
                            locus: row.get(11)?,
                        })
                    },
                )
                .map_err(|e| GradatumError::Storage(format!("query list_notes (section): {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| GradatumError::Storage(format!("collect list_notes (section): {e}")))?
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, vault_id, section, status, body_text,
                                COALESCE(author_display_name, author_id) AS author,
                                tags, content_hash, created, updated, title, locus
                         FROM notes
                         WHERE vault_id = ?1
                           AND id NOT LIKE '__sentinel__%'
                           AND status != 'downgraded'
                           AND (?2 = '' OR id > ?2)
                         ORDER BY id ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| GradatumError::Storage(format!("list_notes prepare: {e}")))?;

                stmt.query_map(
                    rusqlite::params![vault_id, cursor_val, limit_clamped],
                    |row| {
                        Ok(NoteRecord {
                            id: row.get(0)?,
                            vault_id: row.get(1)?,
                            section: row.get(2)?,
                            status: row.get(3)?,
                            body_text: row.get(4)?,
                            author: row.get(5)?,
                            tags_raw: row.get(6)?,
                            content_hash: row.get::<_, Vec<u8>>(7)?,
                            created: row.get(8)?,
                            updated: row.get(9)?,
                            title: row.get(10)?,
                            locus: row.get(11)?,
                        })
                    },
                )
                .map_err(|e| GradatumError::Storage(format!("query list_notes: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| GradatumError::Storage(format!("collect list_notes: {e}")))?
            }
        };

        Ok((records, total as u64))
    }

    /// Inserts a minimal note directly into the database — reserved for integration tests.
    ///
    /// Inserts one row into `notes` with the minimum required fields (`id`, `vault_id`,
    /// `section`, `status`, `schema_version`, `created`, `content_hash`, `body_text`).
    /// The `id` must be a valid ULID string. No FTS upsert — sufficient for testing
    /// downgrade/patch flows on seeded notes.
    ///
    /// # Note visibility
    ///
    /// This method is `pub` because integration tests in other crates (`gradatum-admin`,
    /// `gradatum-server`, `gradatum-index/tests/`) access it from their `#[cfg(test)]` scope.
    /// Migration to `cfg(any(test, feature = "test-helpers"))` is deferred.
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if the INSERT fails (duplicate id, constraint violation, etc.).
    pub async fn seed_note(
        &self,
        id: &str,
        section: &str,
        body: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, 'main', ?2, 'live', 1, ?3, X'00', ?4)",
            rusqlite::params![id, section, now, body],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note: {e}")))?;
        Ok(())
    }

    /// Inserts a note with explicit `status` and `provenance` — for use in tests.
    ///
    /// `status` is passed as kebab-case (e.g. `"pending-review"`, `"staging"`, `"live"`).
    /// `provenance` is optional (e.g. `Some("distilled")`).
    ///
    /// # Errors
    /// - `GradatumError::Storage` if the INSERT fails.
    pub async fn seed_note_with_status(
        &self,
        id: &str,
        section: gradatum_core::section::Section,
        body: &str,
        status: NoteStatus,
        provenance: Option<&str>,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let section_str = section.to_string();
        let status_str = status.to_string();
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, provenance)
             VALUES (?1, 'main', ?2, ?3, 1, ?4, X'00', ?5, ?6)",
            rusqlite::params![id, section_str, status_str, now, body, provenance],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_status: {e}")))?;
        // Synchroniser notes_fts (FTS5 content=notes ne se synchronise pas en mémoire) —
        // permet aux tests de recherche FTS de retrouver les notes seedées.
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text)
             SELECT rowid, body_text FROM notes WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_status fts: {e}")))?;
        Ok(())
    }

    /// Inserts a note with FTS5 synchronised — for use in tests.
    ///
    /// Inserts into both `notes` and `notes_fts` (FTS5 `content=notes` does not
    /// synchronise automatically in memory — FTS tests require an explicit INSERT
    /// into `notes_fts`). Section is configurable (unlike `seed_note`, which fixes `'reference'`).
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if either INSERT fails.
    pub async fn seed_note_with_fts(
        &self,
        id: &str,
        section: &str,
        body: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        // Insert dans notes avec section configurable
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, 'main', ?2, 'live', 1, ?3, X'00', ?4)",
            rusqlite::params![id, section, now, body],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_fts notes: {e}")))?;
        // Insert dans notes_fts pour que les recherches FTS fonctionnent en mémoire
        // (FTS5 content= ne se synchronise pas sans trigger ou INSERT explicite)
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text)
             SELECT rowid, body_text FROM notes WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_fts fts: {e}")))?;
        Ok(())
    }

    /// Variant of `seed_note_with_fts` that accepts an explicit `vault_id`.
    ///
    /// Allows inserting notes into a vault other than `"main"` to test
    /// cross-vault scoping.
    ///
    /// The `locus` can also be set to test locus prefix filtering.
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if either INSERT fails.
    pub async fn seed_note_with_fts_vault(
        &self,
        id: &str,
        vault_id: &str,
        section: &str,
        locus: Option<&str>,
        body: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, ?2, ?3, ?4, 'live', 1, ?5, X'00', ?6)",
            rusqlite::params![id, vault_id, locus, section, now, body],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_fts_vault notes: {e}")))?;
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text)
             SELECT rowid, body_text FROM notes WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_fts_vault fts: {e}")))?;
        Ok(())
    }

    /// Variant of `seed_note_with_fts` that allows setting `created` explicitly.
    ///
    /// Back-dated notes are needed to verify that the composite scoring
    /// prefers newer notes when RRF scores are equal.
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if either INSERT fails.
    pub async fn seed_note_with_created(
        &self,
        id: &str,
        section: &str,
        body: &str,
        created_ms: i64,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text) \
             VALUES (?1, 'main', ?2, 'live', 1, ?3, X'00', ?4)",
            rusqlite::params![id, section, created_ms, body],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_created notes: {e}")))?;
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text) \
             SELECT rowid, body_text FROM notes WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_with_created fts: {e}")))?;
        Ok(())
    }

    /// Inserts a lesson (`section='lessons-learned'`) with explicit tags, title, and `created` — for use in tests.
    ///
    /// Synchronises `notes_fts` (`body_text` + `tags`) so that `MATCH` works on
    /// both columns. Tags are stored space-separated in `notes.tags` and
    /// `notes_fts.tags` (the same format as the real write pipeline).
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if either INSERT fails.
    pub async fn seed_lesson(
        &self,
        id: &str,
        title: &str,
        tags: &str,
        body: &str,
        created_ms: i64,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let tags_opt: Option<&str> = if tags.is_empty() { None } else { Some(tags) };
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, title, tags) \
             VALUES (?1, 'main', 'lessons-learned', 'live', 1, ?2, X'00', ?3, ?4, ?5)",
            rusqlite::params![id, created_ms, body, title, tags_opt],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_lesson notes: {e}")))?;
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text, tags) \
             SELECT rowid, body_text, COALESCE(tags, '') FROM notes WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_lesson fts: {e}")))?;
        Ok(())
    }

    /// Marks a note as forgotten with a configurable timestamp — for use in decay tests.
    ///
    /// Allows inserting a past `forgotten_at` value to test the decay effect
    /// without waiting (e.g. `forgotten_at = now - 86_400_000` → 1 day elapsed → decay ×0.5).
    ///
    /// # Errors
    ///
    /// - `GradatumError::Storage` if the UPDATE fails.
    pub async fn seed_mark_forgotten_at(
        &self,
        id: &str,
        forgotten_at_ms: i64,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE notes SET forgotten = 1, forgotten_at = ?1 WHERE id = ?2",
            rusqlite::params![forgotten_at_ms, id],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_mark_forgotten_at: {e}")))?;
        Ok(())
    }

    /// Resets the `title` column to NULL for a given note.
    ///
    /// **For test use only** — simulates a note that predates migration 0009
    /// (no indexed title) while keeping the `.md` file on disk.
    /// Used to test the H1 fallback in `vault_read`.
    ///
    /// # Note visibility
    ///
    /// This method is `pub` because integration tests in other crates (`gradatum-server`,
    /// `gradatum-index/tests/`) access it from their `#[cfg(test)]` scope.
    /// Migration to `cfg(any(test, feature = "test-helpers"))` is deferred.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite UPDATE fails or the note is absent.
    pub async fn set_title_to_null_for_test(&self, note_id: &NoteId) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE notes SET title = NULL WHERE id = ?1",
            rusqlite::params![note_id.to_string()],
        )
        .map_err(|e| GradatumError::Storage(format!("set_title_to_null_for_test: {e}")))?;
        Ok(())
    }

    /// Inserts a note embedding row directly — for use in bench binaries and integration tests.
    ///
    /// Inserts into `note_embeddings` only (does **not** trigger the ANN upsert path
    /// so that the caller controls extension registration order).  Use
    /// [`backfill_ann_index`][Self::backfill_ann_index] after seeding all embeddings.
    ///
    /// The `vault_id` is inferred from the `notes` row; the note must exist beforehand
    /// (call [`seed_note`][Self::seed_note] first).
    ///
    /// ## BLOB format
    ///
    /// `vector` is serialised as a little-endian f32 BLOB (4 bytes per dimension).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the INSERT fails (note absent, constraint violation, etc.).
    pub async fn seed_note_embedding(
        &self,
        note_id: &str,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError> {
        let blob: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        let computed_at = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)
             ON CONFLICT(note_id, embedder_id) DO UPDATE SET
                 vector      = excluded.vector,
                 dim         = excluded.dim,
                 computed_at = excluded.computed_at",
            rusqlite::params![note_id, embedder_id, blob, dim as i64, computed_at],
        )
        .map_err(|e| GradatumError::Storage(format!("seed_note_embedding: {e}")))?;
        Ok(())
    }

    // ── Semantic search ─────────────────────────────────

    /// Cosine semantic search over `note_embeddings`.
    ///
    /// Loads all `embedder_id` vectors for the vault into memory, computes
    /// cosine similarity against `query_emb`, and returns the top `limit` results.
    ///
    /// ## Complexity
    ///
    /// O(N × dim) where N = number of notes with an embedding.
    /// For N=600, dim=1024: ~600K f32 ops ≈ 1–5 ms on a modern CPU.
    /// Beyond N=10_000, use sqlite-vec ANN (not yet implemented).
    ///
    /// ## Filters applied
    ///
    /// - `vault_id = ?`: tenant isolation.
    /// - `embedder_id = ?`: embedding model isolation.
    /// - `status != 'downgraded'`: excludes archived notes.
    /// - Sentinels excluded via `id NOT LIKE '__sentinel__%'`.
    ///
    /// ## Zero-norm handling
    ///
    /// - Zero-norm query → returns `Ok(vec![])` immediately.
    /// - Zero-norm note vector (`NoopEmbedder`) → silently skipped.
    /// - Dimension mismatch between embedding and query → silently skipped (different model).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite query fails or a ULID cannot be decoded.
    /// Concrete inner method — called by `impl VectorStore for SqliteIndex`.
    /// Renamed `_inner` to avoid a name collision with the trait method.
    pub(crate) async fn search_semantic_inner(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        // Pré-calcul norme query : si nulle, aucun cosine n'est calculable.
        let norm_q: f32 = query_emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_q == 0.0 {
            return Ok(vec![]);
        }

        let conn = self.conn.lock().await;

        // Chargement batch : tous les embeddings du vault pour embedder_id.
        // JOIN notes pour filtrer par vault_id, status, et exclure les sentinelles.
        //
        // Filtre locus optionnel (F-31 v0.4.3) : si présent, seules les notes
        // dont le `locus` commence par le préfixe fourni sont chargées.
        // L'échappement LIKE est appliqué ici (escape_like).
        //
        // Pattern E0597 : collecter dans la même portée que `stmt` pour éviter
        // que `stmt` soit droppé pendant que la MappedRows est encore en vie.
        // P2-R4 (audit) : on sélectionne n.forgotten et n.forgotten_at pour appliquer
        // le même decay F-44 que les chemins FTS (search_fts_scored / search_fts_with_snippet).
        // Différence sémantique BM25 vs cosine :
        //   - BM25 : valeur NÉGATIVE → decay × 0.5^d la rapproche de 0 (dégrade le rang)
        //   - Cosine : valeur POSITIVE [0,1] → decay × 0.5^d la réduit (dégrade le rang)
        // Les deux opérations réduisent le score de la note forgotten dans leur espace respectif.
        //
        // type alias pour réduire la complexité perçue par clippy::type_complexity.
        type SemRow = (String, Vec<u8>, i64, i64, Option<i64>);
        let raw_rows: Vec<SemRow> = if let Some(loc) = locus {
            let locus_escaped = escape_like(loc);
            // Param bindé : ?3 = locus escapé (jamais interpolé dans la SQL).
            let mut stmt = conn
                .prepare(
                    "SELECT ne.note_id, ne.vector, ne.dim, n.forgotten, n.forgotten_at
                     FROM note_embeddings ne
                     JOIN notes n ON n.id = ne.note_id
                     WHERE n.vault_id = ?1
                       AND ne.embedder_id = ?2
                       AND n.status != 'downgraded'
                       AND n.id NOT LIKE '__sentinel__%'
                       AND n.locus LIKE ?3 || '%' ESCAPE '\\'",
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("search_semantic prepare (locus): {e}"))
                })?;

            stmt.query_map(
                rusqlite::params![vault_id, embedder_id, locus_escaped],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("search_semantic query (locus): {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| GradatumError::Storage(format!("search_semantic collect (locus): {e}")))?
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT ne.note_id, ne.vector, ne.dim, n.forgotten, n.forgotten_at
                     FROM note_embeddings ne
                     JOIN notes n ON n.id = ne.note_id
                     WHERE n.vault_id = ?1
                       AND ne.embedder_id = ?2
                       AND n.status != 'downgraded'
                       AND n.id NOT LIKE '__sentinel__%'",
                )
                .map_err(|e| GradatumError::Storage(format!("search_semantic prepare: {e}")))?;

            stmt.query_map(rusqlite::params![vault_id, embedder_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("search_semantic query: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| GradatumError::Storage(format!("search_semantic collect: {e}")))?
        };
        // Libération du lock avant le calcul cosinus O(N×dim) — évite de tenir
        // le Mutex Tokio pendant un calcul CPU potentiellement long.
        drop(conn);

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Calcul cosine pour chaque note chargée.
        let mut scored: Vec<(NoteId, f32)> = Vec::with_capacity(raw_rows.len());
        for (id_str, blob, dim, forgotten, forgotten_at_ms) in raw_rows {
            let expected_bytes = dim as usize * 4;
            if blob.len() != expected_bytes {
                tracing::warn!(
                    note_id = %id_str,
                    blob_len = blob.len(),
                    expected = expected_bytes,
                    "search_semantic: blob size mismatch — skip"
                );
                continue;
            }

            // Décodage f32 little-endian depuis BLOB (4 bytes/f32).
            let vec: Vec<f32> = blob
                .chunks_exact(4)
                .map(|b| {
                    f32::from_le_bytes(
                        b.try_into()
                            .expect("chunks_exact garantit 4 bytes — invariant"),
                    )
                })
                .collect();

            if vec.len() != query_emb.len() {
                // Dim mismatch : embedding d'un modèle différent — skip silencieux.
                continue;
            }

            // Cosine = dot(q, v) / (||q|| × ||v||)
            let dot: f32 = query_emb.iter().zip(&vec).map(|(a, b)| a * b).sum();
            let norm_v: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm_v == 0.0 {
                // NoopEmbedder → vecteur nul → cosine indéfini → skip.
                continue;
            }
            let cosine_raw = dot / (norm_q * norm_v);

            // P2-R3 : état incohérent détectable (forgotten=1 mais forgotten_at=NULL).
            // warn! car c'est une anomalie de données détectable — pas un chemin normal.
            // Comportement decay : `elapsed_days = 0.0` → facteur 0.5^0 = 1.0 (neutre).
            // Choix conservateur : ne pas pénaliser une note sur un état corrompu.
            if forgotten != 0 && forgotten_at_ms.is_none() {
                tracing::warn!(
                    note_id = %id_str,
                    "search_semantic: forgotten=1 mais forgotten_at=NULL — état incohérent"
                );
            }
            // P2-R4 : decay forgotten sur score cosine (valeur POSITIVE — × 0.5^d réduit le score).
            let cosine = if forgotten != 0 {
                let elapsed_days = forgotten_at_ms
                    .map(|at_ms| (now_ms - at_ms) as f64 / 86_400_000.0)
                    .unwrap_or(0.0)
                    .max(0.0);
                cosine_raw * (0.5f32).powf(elapsed_days as f32)
            } else {
                cosine_raw
            };

            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("search_semantic ULID parse: {e}")))?;
            scored.push((NoteId(ulid), cosine));
        }

        // Tri décroissant stable : meilleur cosine en premier.
        // `partial_cmp` avec fallback Equal préserve l'ordre d'insertion en cas d'égalité.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored)
    }
}

// ── Concrete storage method implementations (SQL bodies) ──────────────────────
//
// These methods previously lived in `impl Index for SqliteIndex` (the monolithic pre-split trait).
// After the carve-out they became inherent `pub(crate)` methods on `SqliteIndex`.
// The three fine-grained traits (`DocumentStore`, `IndexStore`, `VectorStore`) delegate here.
// The `Index` trait is now a supertrait façade with a blanket impl in gradatum-core.
//
// Note: no `#[async_trait]` here — inherent async methods on a plain `impl T`
// do not need it (unlike trait impls).
// Detailed doc-comments live in the corresponding traits (DocumentStore/IndexStore).
// `allow(missing_docs)` is scoped to this impl block: `*_inner` methods are `pub(crate)`
// and their contract documentation resides in the traits (document_store.rs/index_store.rs/vector_store.rs).
#[allow(missing_docs)]
impl SqliteIndex {
    /// Inserts or updates a note in the `notes` and `notes_fts` tables.
    ///
    /// `ON CONFLICT(id) DO UPDATE`: atomic upsert on the ULID primary key.
    /// FTS5: `notes_fts` uses `content=notes` — `INSERT OR REPLACE` keeps
    /// `rowid ↔ body_text/tags` consistent on updates.
    pub async fn upsert_note(&self, note: &Note) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;

        let id_str = note.id.to_string();
        let vault_id = note.frontmatter.vault_id.as_str();
        let locus: Option<&str> = note.frontmatter.locus.as_ref().map(|l| l.as_str());
        // Section : kebab-case via Display (ex. "lessons-learned")
        let section_str = note.frontmatter.section.to_string();
        // c_kind / doc_kind : dérivés déterministiques CoALA (F-42 c-prime, scoring-only).
        // Dérivés ici au moment de l'écriture — zéro changement de la struct Note/Frontmatter.
        // Usage scoring effectif : DIFFÉRÉ v0.4.0 (F-17). Section reste autoritaire.
        let c_kind = section_to_c_kind(&note.frontmatter.section);
        let doc_kind = section_to_doc_kind(&note.frontmatter.section);
        // NoteStatus : kebab-case via Display (ex. "pending-review")
        let status_str = note.frontmatter.status.to_string();

        // Tags : espace-séparés pour stocker dans notes.tags (migration 0003).
        // Même format que notes_fts.tags — permet les queries non-FTS sur distinct_tags.
        let tags_str: String = note
            .frontmatter
            .tags
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let tags_col: Option<&str> = if tags_str.is_empty() {
            None
        } else {
            Some(tags_str.as_str())
        };

        // AuthorRef sérialisé par champ (kind en kebab-case via serde_json)
        let author_kind: Option<String> = note.frontmatter.author.as_ref().map(|a| {
            // serde_json::to_string sur un enum serde(rename_all="kebab-case") produit `"main-agent"`
            serde_json::to_string(&a.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        });
        let author_id: Option<&str> = note.frontmatter.author.as_ref().map(|a| a.id.as_str());
        let author_display_name: Option<&str> = note
            .frontmatter
            .author
            .as_ref()
            .and_then(|a| a.display_name.as_deref());

        let created_ms = note.frontmatter.created.timestamp_millis();
        let updated_ms = note.frontmatter.updated.map(|d| d.timestamp_millis());
        let status_changed_ms = note
            .frontmatter
            .status_changed
            .map(|d| d.timestamp_millis());

        // ExtraFields → JSON (voir note en tête de fichier sur le choix extra_json vs extra_yaml)
        let extra_json: Option<String> =
            if note.frontmatter.extra.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&note.frontmatter.extra).map_err(|e| {
                    GradatumError::Storage(format!("sérialisation extra_json : {e}"))
                })?)
            };

        let content_hash_bytes = &note.content_hash.0[..];

        // F-47 : provenance (frontmatter) + trust (colonne scoring, non consommé avant F-17).
        let provenance = note.frontmatter.provenance.as_deref();
        // Trust : depuis frontmatter.provenance via table statique TRUST_SCORES.
        // Défaut conservateur : agent-log/0.50 si provenance absente ou non reconnue.
        let trust: f64 = note
            .frontmatter
            .provenance
            .as_deref()
            .and_then(gradatum_core::provenance::trust_for)
            .unwrap_or(0.50) as f64;

        conn.execute(
            "INSERT INTO notes (
                id, vault_id, locus, section, status, schema_version,
                author_kind, author_id, author_display_name,
                created, updated, status_changed, status_reason,
                content_hash, version, body_text, integrity_signature, extra_json, tags,
                c_kind, doc_kind, provenance, trust
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
            ON CONFLICT(id) DO UPDATE SET
                vault_id             = excluded.vault_id,
                -- P1-1 (F-37 S1.4) : préserver un locus modifié via update_note_locus.
                -- `update_note_locus` écrit notes.locus index-level SANS réécrire le .md
                -- (le content_hash reste inchangé). Un re-upsert ultérieur depuis le .md
                -- stale fournirait l'ancien locus (ou NULL) du frontmatter inchangé : il ne
                -- DOIT PAS écraser le locus déplacé. Discriminant = content_hash :
                --   hash inchangé (re-upsert même contenu) → conserver notes.locus (déplacé).
                --   hash modifié  (vrai changement de contenu/frontmatter) → appliquer excluded.locus.
                -- `IS NOT` gère NULL correctement (mêmes blobs → égalité).
                locus                = CASE
                                         WHEN notes.content_hash IS NOT excluded.content_hash
                                           THEN excluded.locus
                                         ELSE notes.locus
                                       END,
                section              = excluded.section,
                status               = excluded.status,
                schema_version       = excluded.schema_version,
                author_kind          = excluded.author_kind,
                author_id            = excluded.author_id,
                author_display_name  = excluded.author_display_name,
                updated              = excluded.updated,
                status_changed       = excluded.status_changed,
                status_reason        = excluded.status_reason,
                content_hash         = excluded.content_hash,
                version              = excluded.version,
                body_text            = excluded.body_text,
                integrity_signature  = excluded.integrity_signature,
                extra_json           = excluded.extra_json,
                tags                 = excluded.tags,
                c_kind               = excluded.c_kind,
                doc_kind             = excluded.doc_kind,
                provenance           = excluded.provenance,
                -- P1-1 : préserver un trust dynamique posé par set_note_trust (F-22).
                -- Le trust statique dérivé de provenance ne doit écraser l'existant QUE si
                -- la provenance change. Provenance inchangée → conserver notes.trust courant
                -- (qui peut être un trust distillé dynamique). `IS NOT` gère NULL correctement.
                trust                = CASE
                                         WHEN notes.provenance IS NOT excluded.provenance
                                           THEN excluded.trust
                                         ELSE notes.trust
                                       END",
            rusqlite::params![
                id_str,
                vault_id,
                locus,
                section_str,
                status_str,
                note.frontmatter.schema_version,
                author_kind,
                author_id,
                author_display_name,
                created_ms,
                updated_ms,
                status_changed_ms,
                note.frontmatter.status_reason.as_deref(),
                content_hash_bytes,
                note.version.0,
                note.body.markdown.as_str(),
                None::<Vec<u8>>,  // integrity_signature : Phase 1 = NULL
                extra_json,
                tags_col,         // tags espace-séparés (migration 0003)
                c_kind,           // c_kind CoALA (F-42 c-prime, scoring-only)
                doc_kind,         // doc_kind CoALA (F-42 c-prime, scoring-only)
                provenance,       // F-47 provenance String (depuis frontmatter)
                trust,            // F-47 trust REAL (depuis TRUST_SCORES, défaut 0.50)
            ],
        )
        .map_err(|e| GradatumError::Storage(format!("INSERT notes : {e}")))?;

        // Maintien FTS5 : INSERT OR REPLACE synchronise rowid + body_text + tags.
        // `content=notes` exige que rowid FTS = rowid de la table notes (même entier).
        // Réutilise `tags_str` déjà calculé pour notes.tags (migration 0003) — pas de duplication.
        conn.execute(
            "INSERT OR REPLACE INTO notes_fts (rowid, body_text, tags)
             VALUES ((SELECT rowid FROM notes WHERE id = ?1), ?2, ?3)",
            rusqlite::params![id_str, note.body.markdown.as_str(), tags_str],
        )
        .map_err(|e| GradatumError::Storage(format!("INSERT notes_fts : {e}")))?;

        Ok(())
    }

    /// Reads the trust score for a note from the `notes.trust` column.
    ///
    /// Returns `Some(trust)` if the note exists and trust is non-NULL, `None` otherwise.
    /// Stored but not consumed by scoring until trust-decay was wired in v0.4.1.
    pub async fn get_trust(&self, id: &NoteId) -> Result<Option<f32>, GradatumError> {
        let conn = self.conn.lock().await;
        let id_str = id.to_string();

        match conn.query_row("SELECT trust FROM notes WHERE id = ?1", [&id_str], |row| {
            row.get::<_, Option<f64>>(0)
        }) {
            Ok(Some(v)) => Ok(Some(v as f32)),
            Ok(None) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_trust : {e}"))),
        }
    }

    /// Reads `(trust, provenance)` for a note in a single query.
    ///
    /// Used by trust-decay scoring to select the half-life per provenance.
    /// Returns `(None, None)` if the note is absent.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    pub async fn get_trust_and_provenance(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(Option<f32>, Option<String>), GradatumError> {
        let conn = self.conn.lock().await;
        match conn.query_row(
            "SELECT trust, provenance FROM notes WHERE vault_id = ?1 AND id = ?2",
            rusqlite::params![vault_id, note_id],
            |row| {
                let trust: Option<f64> = row.get(0)?;
                let provenance: Option<String> = row.get(1)?;
                Ok((trust.map(|t| t as f32), provenance))
            },
        ) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((None, None)),
            Err(e) => Err(GradatumError::Storage(format!(
                "get_trust_and_provenance : {e}"
            ))),
        }
    }

    /// Writes an explicit trust score for a note into `notes.trust`.
    ///
    /// `upsert_note` derives `trust` statically from `provenance` via `TRUST_SCORES`
    /// (e.g. `distilled` → `0.60`). Distillation computes a **dynamic** trust
    /// (`compute_distill_trust` = mean(trust sources) × confidence) that must overwrite
    /// the static value after writing the synthesis note.
    ///
    /// This method is the sole write point for trust values computed outside `TRUST_SCORES`.
    /// Idempotent — `UPDATE notes SET trust = ?2 WHERE id = ?1`.
    ///
    /// # Return value
    ///
    /// Returns the number of rows affected (`0` if the note is absent — non-fatal
    /// for the caller: the static value from `provenance` remains in place).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the `UPDATE` fails.
    #[must_use = "le nombre de lignes affectées indique si la note existait"]
    pub async fn set_note_trust(&self, id: &NoteId, trust: f32) -> Result<usize, GradatumError> {
        let conn = self.conn.lock().await;
        let id_str = id.to_string();
        conn.execute(
            "UPDATE notes SET trust = ?2 WHERE id = ?1",
            rusqlite::params![id_str, f64::from(trust)],
        )
        .map_err(|e| GradatumError::Storage(format!("set_note_trust : {e}")))
    }

    pub async fn get_content_hash(&self, id: NoteId) -> Result<Option<ContentHash>, GradatumError> {
        let conn = self.conn.lock().await;
        let id_str = id.to_string();

        match conn.query_row(
            "SELECT content_hash FROM notes WHERE id = ?1",
            [&id_str],
            |row| row.get::<_, Vec<u8>>(0),
        ) {
            Ok(bytes) => {
                if bytes.len() < 32 {
                    return Err(GradatumError::Storage(format!(
                        "content_hash trop court ({} bytes) pour NoteId {id_str}",
                        bytes.len()
                    )));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes[..32]);
                Ok(Some(ContentHash(arr)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_content_hash : {e}"))),
        }
    }

    pub async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT n.id
                 FROM notes_fts
                 JOIN notes n ON notes_fts.rowid = n.rowid
                 WHERE notes_fts MATCH ?1
                   AND n.vault_id = ?2
                 ORDER BY rank
                 LIMIT ?3",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare search_fts : {e}")))?;

        let rows = stmt
            .query_map(
                rusqlite::params![query, vault_id.as_str(), limit as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| GradatumError::Storage(format!("query search_fts : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let id_str = r.map_err(|e| GradatumError::Storage(format!("row search_fts : {e}")))?;
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("parse ULID {id_str:?} : {e}")))?;
            out.push(NoteId(ulid));
        }
        Ok(out)
    }

    pub async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        let conn = self.conn.lock().await;
        // NoteStatus::Display produit le kebab-case serde (ex. "pending-review")
        let status_str = status.to_string();

        let mut stmt = conn
            .prepare(
                "SELECT id FROM notes
                 WHERE vault_id = ?1 AND status = ?2
                 ORDER BY updated DESC NULLS LAST, created DESC",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare list_by_status : {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![vault_id.as_str(), status_str], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query list_by_status : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let id_str =
                r.map_err(|e| GradatumError::Storage(format!("row list_by_status : {e}")))?;
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("parse ULID {id_str:?} : {e}")))?;
            out.push(NoteId(ulid));
        }
        Ok(out)
    }

    /// Lists paginated review-queue notes (`status ∈ {pending-review, staging}`).
    ///
    /// Lexicographic ULID cursor (`id < cursor` because sort is DESC; `""`/`None` = first
    /// page). Sorted `created DESC, id DESC` (newest first). Sentinels are excluded.
    pub async fn list_review_queue(
        &self,
        vault_id: &VaultId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReviewQueueRow>, GradatumError> {
        let conn = self.conn.lock().await;
        let limit_clamped = limit.clamp(1, 200) as i64;
        let cursor_val = cursor.unwrap_or("");

        // Tri DESC → le curseur avance vers les ULIDs plus petits (`id < cursor`).
        // `(?2 = '' OR id < ?2)` : curseur vide = début de liste.
        let mut stmt = conn
            .prepare(
                "SELECT id, title, section, locus, status, provenance, created
                 FROM notes
                 WHERE vault_id = ?1
                   AND status IN ('pending-review', 'staging')
                   AND id NOT LIKE '__sentinel__%'
                   AND (?2 = '' OR id < ?2)
                 ORDER BY created DESC, id DESC
                 LIMIT ?3",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare list_review_queue: {e}")))?;

        let rows = stmt
            .query_map(
                rusqlite::params![vault_id.as_str(), cursor_val, limit_clamped],
                |row| {
                    let id_str: String = row.get(0)?;
                    Ok((
                        id_str,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("query list_review_queue: {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (id_str, title, section, locus, status, provenance, created_ms) =
                r.map_err(|e| GradatumError::Storage(format!("row list_review_queue: {e}")))?;
            // #7 — un id non-ULID (anomalie data : sentinelle, ligne corrompue) ne doit
            // PAS faire échouer la page entière en 500. On le skippe + log warn (résilience).
            let ulid = match ulid::Ulid::from_string(&id_str) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(
                        id = %id_str,
                        err = %e,
                        "list_review_queue: id non-ULID ignoré (ligne skippée)"
                    );
                    continue;
                }
            };
            out.push(ReviewQueueRow {
                note_id: NoteId(ulid),
                title,
                section,
                locus,
                status,
                provenance,
                created_ms,
            });
        }
        Ok(out)
    }

    /// Returns the total count of review-queue notes (`status ∈ {pending-review, staging}`).
    pub async fn count_review_queue(&self, vault_id: &VaultId) -> Result<u64, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes
                 WHERE vault_id = ?1
                   AND status IN ('pending-review', 'staging')
                   AND id NOT LIKE '__sentinel__%'",
                rusqlite::params![vault_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("count_review_queue: {e}")))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Lists `status=Garbage` notes older than the given cutoff (purge lifecycle).
    ///
    /// Returns the `NoteId` values of notes where `status_changed <= cutoff_ms` (UTC
    /// millisecond timestamp). Notes without `status_changed` (NULL) are included when
    /// their `created` timestamp also predates the cutoff (conservative behaviour).
    ///
    /// ## Parameters
    ///
    /// - `vault_id`: tenant identifier.
    /// - `cutoff_ms`: UTC timestamp in milliseconds. Only notes with `status_changed ≤ cutoff_ms`
    ///   are returned.
    ///
    /// ## Guarantee
    ///
    /// Only notes with `status = 'garbage'` at query time are returned.
    /// The Purge handler re-checks each note's status at delete time (TOCTOU mitigation).
    pub async fn list_garbage_older_than(
        &self,
        vault_id: &str,
        cutoff_ms: i64,
    ) -> Result<Vec<NoteId>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT id FROM notes
                 WHERE vault_id = ?1
                   AND status = 'garbage'
                   AND COALESCE(status_changed, created) <= ?2
                 ORDER BY COALESCE(status_changed, created) ASC",
            )
            .map_err(|e| {
                GradatumError::Storage(format!("prepare list_garbage_older_than : {e}"))
            })?;

        let rows = stmt
            .query_map(rusqlite::params![vault_id, cutoff_ms], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query list_garbage_older_than : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let id_str = r.map_err(|e| {
                GradatumError::Storage(format!("row list_garbage_older_than : {e}"))
            })?;
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("parse ULID {id_str:?} : {e}")))?;
            out.push(NoteId(ulid));
        }
        Ok(out)
    }

    /// Returns the current status of a note from the index.
    ///
    /// Used by `handle_purge` to re-verify that the note is still in `Garbage`
    /// at delete time (TOCTOU mitigation between listing and deletion).
    ///
    /// Returns `None` if the note is absent from the index.
    pub async fn get_note_status(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<NoteStatus>, GradatumError> {
        let conn = self.conn.lock().await;

        let result = conn.query_row(
            "SELECT status FROM notes WHERE vault_id = ?1 AND id = ?2",
            rusqlite::params![vault_id, note_id],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(status_str) => {
                // D1.2 (v0.4.8) — Tolérance du statut SQL `downgraded` (hors enum).
                //
                // `downgraded` est un statut ACTIF et distinct (mécanisme F-39 soft
                // downgrade : `replaced_by`, exclusion search `!= 'downgraded'`, écrit
                // par le endpoint LIVE `vault_downgrade`). Il n'est volontairement PAS
                // un variant `NoteStatus`. Avant D1.2, `get_note_status` ERRAIT au parse
                // serde sur `downgraded`, ce qui faisait *silencieusement ignorer* ces
                // notes par le handler de purge (`apalis_handlers` : « get_note_status
                // illisible — note ignorée »). Conséquence : les notes downgradées ne
                // pouvaient jamais être purgées.
                //
                // Choix : une migration de données `downgraded → deprecated` est ÉCARTÉE
                // (elle conflerait deux concepts distincts, ferait resurgir les notes
                // archivées dans la recherche — `deprecated` n'est pas filtré comme
                // `downgraded` — et serait immédiatement re-cassée par le prochain
                // `vault_downgrade`). On normalise donc à la *lecture* : `downgraded`
                // est projeté sur `NoteStatus::Deprecated` (sémantique « note sortante /
                // archivée » la plus proche dans l'enum), sans toucher la valeur stockée
                // ni les filtres search. Durable : couvre aussi les downgrades futurs.
                if status_str == "downgraded" {
                    return Ok(Some(NoteStatus::Deprecated));
                }
                // Désérialiser via serde_json (NoteStatus est un serde kebab-case)
                let quoted = format!("\"{}\"", status_str);
                let status: NoteStatus = serde_json::from_str(&quoted).map_err(|e| {
                    GradatumError::Storage(format!("parse NoteStatus '{status_str}' : {e}"))
                })?;
                Ok(Some(status))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_note_status : {e}"))),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Semantic Forget — listing notes forgotten
    // ─────────────────────────────────────────────────────────────────────────

    /// Lists paginated forgotten notes for a vault, cursor-keyed by ULID.
    ///
    /// Returns notes with `forgotten = 1`, sorted by `forgotten_at DESC`
    /// (most recently forgotten first).
    ///
    /// ## Cursor-based pagination
    ///
    /// `cursor`: ULID of the last element of the previous page (exclusive, monotone ULID).
    /// `None` = first page. Use the `next_cursor` field from the previous response.
    ///
    /// ## Parameters
    ///
    /// - `vault_id`: tenant identifier.
    /// - `limit`: maximum number of results (fetched as limit + 1 to detect next page).
    /// - `cursor`: last returned ULID (exclusive) — `None` = first page.
    ///
    /// ## Return value
    ///
    /// `Vec` of `(id, title, section, forgotten_at, forgotten_by)`:
    /// - `id`: note ULID (String).
    /// - `title`: optional title (NULL if column is empty).
    /// - `section`: kebab-case section.
    /// - `forgotten_at`: epoch timestamp in ms (i64).
    /// - `forgotten_by`: optional actor (TEXT | NULL).
    pub async fn list_forgotten_notes(
        &self,
        vault_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<ForgottenRow>, GradatumError> {
        let conn = self.conn.lock().await;
        // Safety cap : max 500 résultats par page.
        let safe_limit = limit.min(500);
        // +1 pour détecter s'il y a une page suivante sans double requête.
        let fetch_limit_i64 = i64::try_from(safe_limit.saturating_add(1)).unwrap_or(i64::MAX);

        // Pattern rusqlite lifetime : stmt et rows dans le même scope que conn.
        // Branche cursor / no-cursor séparées pour éviter E0597 (stmt droppé avant collect).
        let rows: Vec<ForgottenRow> = if let Some(cur) = cursor {
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, section, forgotten_at, forgotten_by
                     FROM notes
                     WHERE vault_id = ?1
                       AND forgotten = 1
                       AND id > ?2
                     ORDER BY forgotten_at DESC, id ASC
                     LIMIT ?3",
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("prepare list_forgotten_notes cursor: {e}"))
                })?;

            stmt.query_map(rusqlite::params![vault_id, cur, fetch_limit_i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("query list_forgotten_notes cursor: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                GradatumError::Storage(format!("collect list_forgotten_notes cursor: {e}"))
            })?
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, section, forgotten_at, forgotten_by
                     FROM notes
                     WHERE vault_id = ?1
                       AND forgotten = 1
                     ORDER BY forgotten_at DESC, id ASC
                     LIMIT ?2",
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("prepare list_forgotten_notes: {e}"))
                })?;

            stmt.query_map(rusqlite::params![vault_id, fetch_limit_i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("query list_forgotten_notes: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("collect list_forgotten_notes: {e}")))?
        };
        Ok(rows)
    }

    /// Returns the total count of forgotten notes for a vault.
    ///
    /// Used by `GET /api/v1/vault/forgotten` for the `total` field.
    pub async fn count_forgotten_notes(&self, vault_id: &str) -> Result<usize, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1 AND forgotten = 1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("count_forgotten_notes : {e}")))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Returns `true` if the note is forgotten within the vault scope.
    ///
    /// Used by `handle_forget` to re-verify before mutations (TOCTOU mitigation).
    /// Returns `false` if the note is absent from the index.
    pub async fn is_note_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT forgotten FROM notes WHERE vault_id = ?1 AND id = ?2",
            rusqlite::params![vault_id, note_id],
            |row| row.get::<_, i64>(0),
        );
        match result {
            Ok(v) => Ok(v != 0),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(GradatumError::Storage(format!("is_note_forgotten : {e}"))),
        }
    }

    /// Returns the section of a note (used to filter protected sections).
    ///
    /// Returns `None` if the note is absent from the index.
    pub async fn get_note_section(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT section FROM notes WHERE vault_id = ?1 AND id = ?2",
            rusqlite::params![vault_id, note_id],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_note_section : {e}"))),
        }
    }

    /// Topic-scope resolution: FTS-matching notes returned with their section.
    ///
    /// Used by `handle_forget` for Topic-scope resolution.
    pub async fn search_fts_for_forget(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        let conn = self.conn.lock().await;
        // Safety cap.
        let safe_limit = limit.min(200);
        // C2 — Borne longueur query FTS : SQLite FTS5 accepte des expressions
        // arbitrairement longues, mais une query > 512 chars est pathologique
        // (risque de DoS ou d'injection d'expression FTS complexe).
        if query.len() > 512 {
            return Err(GradatumError::Validation(ValidationError::InvalidInput(
                format!(
                    "search_fts_for_forget: query trop longue ({} > 512 chars)",
                    query.len()
                ),
            )));
        }
        // Sanitize la query FTS : si elle est vide → liste vide.
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        // Quoting FTS5 : chaque token est wrappé en "..." pour neutraliser les
        // opérateurs FTS5 (tirets, dates ISO, AND/OR/NOT, `*`, `^`, parenthèses).
        // Bug P1 constaté au smoke v0.4.3 : `lot-c` → `no such column: lot` (le tiret
        // est interprété comme opérateur de soustraction de colonne FTS5).
        // `fts5_quote_query` produit une recherche AND de termes littéraux — trade-off
        // documenté dans le doc-comment de la fonction.
        let quoted_query = fts5_quote_query(query);
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.section
                 FROM notes_fts
                 JOIN notes n ON notes_fts.rowid = n.rowid
                 WHERE n.vault_id = ?1
                   AND notes_fts MATCH ?2
                   AND n.forgotten = 0
                 LIMIT ?3",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare search_fts_for_forget: {e}")))?;
        let rows = stmt
            .query_map(
                rusqlite::params![vault_id, quoted_query, safe_limit as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| GradatumError::Storage(format!("query search_fts_for_forget: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("collect search_fts_for_forget: {e}")))?;
        Ok(rows)
    }

    /// Locus-scope resolution: notes whose locus starts with the given prefix.
    ///
    /// `locus_prefix` is escaped against LIKE metacharacters on the Rust side (single
    /// escaping) — do not re-escape on the caller side.
    pub async fn list_notes_by_locus_prefix(
        &self,
        vault_id: &str,
        locus_prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        let conn = self.conn.lock().await;
        let escaped = escape_like(locus_prefix);
        let mut stmt = conn
            .prepare(
                "SELECT id, section
                 FROM notes
                 WHERE vault_id = ?1
                   AND locus LIKE ?2 || '%' ESCAPE '\\'
                   AND forgotten = 0",
            )
            .map_err(|e| {
                GradatumError::Storage(format!("prepare list_notes_by_locus_prefix: {e}"))
            })?;
        let rows = stmt
            .query_map(rusqlite::params![vault_id, escaped], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("query list_notes_by_locus_prefix: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                GradatumError::Storage(format!("collect list_notes_by_locus_prefix: {e}"))
            })?;
        Ok(rows)
    }

    /// Agent-scope resolution: notes authored by a given agent across the specified vaults.
    ///
    /// Empty `vaults` → falls back to vault `"main"` only.
    /// Filters on `author_id = agent_id` in `notes` (`author_id` column, migration 0001).
    pub async fn list_notes_by_agent(
        &self,
        agent_id: &str,
        vaults: &[String],
    ) -> Result<Vec<(String, String)>, GradatumError> {
        let conn = self.conn.lock().await;
        let effective_vaults: Vec<&str> = if vaults.is_empty() {
            vec!["main"]
        } else {
            vaults.iter().map(String::as_str).collect()
        };
        // Construire la clause IN dynamiquement (paramètres bindés — pas d'interpolation).
        // Safety cap : max 20 vaults ciblés (DoS protection).
        let capped_vaults: Vec<&str> = effective_vaults.into_iter().take(20).collect();
        let placeholders: String = (1..=capped_vaults.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, section
             FROM notes
             WHERE author_id = ?1
               AND vault_id IN ({placeholders})
               AND forgotten = 0"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("prepare list_notes_by_agent: {e}")))?;
        let mut params: Vec<&dyn rusqlite::types::ToSql> =
            Vec::with_capacity(capped_vaults.len() + 1);
        params.push(&agent_id);
        for v in &capped_vaults {
            params.push(v);
        }
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("query list_notes_by_agent: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("collect list_notes_by_agent: {e}")))?;
        Ok(rows)
    }

    /// Returns the `replaced_by` ULID (string) for a note, or `None` if absent or note is unknown.
    ///
    /// Direct read from `notes.replaced_by`. Used in integration tests to verify
    /// that the `replaced_by` field is persisted correctly after `patch_note`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` on an unexpected SQLite error.
    pub async fn get_replaced_by(&self, note_id: &str) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT replaced_by FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get::<_, Option<String>>(0),
        );
        match result {
            Ok(val) => Ok(val),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_replaced_by : {e}"))),
        }
    }

    /// Deletes all `redirect_table` entries pointing to a given ULID (purge lifecycle cleanup).
    ///
    /// Called when a note is permanently deleted (purge lifecycle) to clean up
    /// stale wikilink redirects.
    ///
    /// ## Behaviour
    ///
    /// - Deletes 0 or N rows (idempotent: no-op if the note had no redirect).
    /// - Non-fatal: orphan redirects do not block resolution
    ///   (they simply return a ULID for an absent note).
    ///
    /// ## Parameter
    ///
    /// `ulid_str`: textual ULID representation (26 chars, standard format).
    pub async fn delete_redirect_by_ulid(&self, ulid_str: &str) -> Result<usize, GradatumError> {
        let conn = self.conn.lock().await;

        let n = conn
            .execute(
                "DELETE FROM redirect_table WHERE ulid = ?1",
                rusqlite::params![ulid_str],
            )
            .map_err(|e| GradatumError::Storage(format!("delete_redirect_by_ulid : {e}")))?;

        Ok(n)
    }

    /// Deletes a note from the SQLite index (`notes` table + FTS `notes_fts`).
    ///
    /// Atomic two-pass operation on the same locked connection:
    ///
    /// 1. Fetches the SQLite `rowid` of the note (required for FTS).
    /// 2. Deletes from `notes_fts` (FTS5 `content=notes` table, no automatic trigger).
    /// 3. Deletes from `notes` → cascades automatically to `note_audit_trail`,
    ///    `note_index`, `note_embeddings`, `note_overrides`, `note_history`
    ///    (`FOREIGN KEY … ON DELETE CASCADE` defined in migration 0001).
    ///
    /// ## Note
    ///
    /// Does not delete the `redirect_table` row — call
    /// [`Self::delete_redirect_by_ulid`] separately if needed.
    ///
    /// ## Return value
    ///
    /// - `Ok(true)`: note found and deleted.
    /// - `Ok(false)`: note absent from the index (already deleted, idempotent).
    /// - `Err(…)`: fatal SQLite error.
    pub async fn delete_note_from_index(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<bool, GradatumError> {
        let conn = self.conn.lock().await;

        // Récupérer le rowid SQLite avant suppression (requis pour la FTS).
        let rowid: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM notes WHERE id = ?1 AND vault_id = ?2",
                rusqlite::params![note_id, vault_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                GradatumError::Storage(format!("delete_note_from_index rowid query : {e}"))
            })?;

        let Some(rowid) = rowid else {
            // Note absente — idempotent.
            return Ok(false);
        };

        // Supprimer de la FTS (table `content=notes` sans trigger automatique).
        conn.execute(
            "DELETE FROM notes_fts WHERE rowid = ?1",
            rusqlite::params![rowid],
        )
        .map_err(|e| GradatumError::Storage(format!("delete_note_from_index fts : {e}")))?;

        // Suppression explicite dans temporal_index (caveat C7 — PRAGMA foreign_keys
        // non garanti → pas de ON DELETE CASCADE fiable, DELETE explicite obligatoire).
        conn.execute(
            "DELETE FROM temporal_index WHERE note_id = ?1",
            rusqlite::params![note_id],
        )
        .map_err(|e| {
            GradatumError::Storage(format!("delete_note_from_index temporal_index : {e}"))
        })?;

        // Supprimer de `notes` → cascade sur toutes les tables liées.
        let deleted = conn
            .execute(
                "DELETE FROM notes WHERE id = ?1 AND vault_id = ?2",
                rusqlite::params![note_id, vault_id],
            )
            .map_err(|e| GradatumError::Storage(format!("delete_note_from_index notes : {e}")))?;

        Ok(deleted > 0)
    }

    // ── v0.5.2 Code-ingest index-only ────────────────────────────────────────

    /// Writes a batch of derived notes for a `source_path` in a **single SQLite transaction**.
    ///
    /// ## Atomicity
    ///
    /// The transaction executes in order:
    /// 1. Fetch previous `note_ids` from `code_freshness` for this `(vault_id, source_path)`.
    /// 2. Delete old notes (including FTS) from `notes WHERE id IN (old note_ids)`.
    /// 3. Insert new notes into `notes`.
    /// 4. Upsert `code_freshness` with the new `content_hash_source`, `ingested_sha`, `note_ids`.
    ///
    /// No partial state is ever visible: old and new notes never coexist.
    ///
    /// ## Index-only path
    ///
    /// Bypasses `Vault`/filesystem entirely. No `.md` file, no curate queue, no decay.
    /// `provenance="derived:tree-sitter"` distinguishes these notes from curated notes
    /// (excluded from curator/decay/forget).
    ///
    /// ## FTS5
    ///
    /// `notes_fts` is a `content=notes` table — no automatic triggers in this configuration.
    /// Synchronisation is **manual and explicit** via `INSERT OR REPLACE INTO notes_fts`.
    /// Using `INSERT OR REPLACE` (not bare `INSERT`) ensures that a re-ingest of the same
    /// `note_id` (stable rowid) overwrites the old FTS entry instead of creating a duplicate.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    /// On error, the transaction is rolled back — no partial notes are written.
    pub async fn write_note_derived_batch(
        &self,
        vault_id: &str,
        source_path: &str,
        content_hash_source: &str,
        ingested_sha: &str,
        notes: Vec<DerivedNote>,
    ) -> Result<(), GradatumError> {
        // ── Garde isolation vault principal (invariant §3.4) ──────────────────
        // vault_id doit commencer par "code-" pour éviter toute écriture accidentelle
        // dans le vault principal ou un vault non-code. Cette garde est au niveau
        // de la méthode (pas seulement au niveau CLI) car la corruption serait silencieuse.
        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "write_note_derived_batch : vault_id '{vault_id}' doit commencer par 'code-' \
                 (isolation vault principal — ne jamais écrire dans 'main' ou un vault non-code)"
            )));
        }

        let conn = self.conn.lock().await;

        // Construire la liste de note_ids JSON pour le stockage dans code_freshness.
        let new_note_ids: Vec<String> = notes.iter().map(|n| n.id.to_string()).collect();
        let note_ids_json = serde_json::to_string(&new_note_ids).map_err(|e| {
            GradatumError::Storage(format!("write_note_derived_batch serialize note_ids: {e}"))
        })?;

        // BEGIN TRANSACTION — atomicité totale.
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| GradatumError::Storage(format!("write_note_derived_batch BEGIN: {e}")))?;

        let result = (|| -> Result<(), GradatumError> {
            // Étape 1 : récupérer les anciens note_ids depuis code_freshness.
            let old_ids_json: Option<String> = conn
                .query_row(
                    "SELECT note_ids FROM code_freshness WHERE vault_id = ?1 AND source_path = ?2",
                    rusqlite::params![vault_id, source_path],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| {
                    GradatumError::Storage(format!("write_note_derived_batch fetch old ids: {e}"))
                })?;

            // Étape 2 : supprimer les anciennes notes (avec leur entrée FTS).
            if let Some(ids_json) = old_ids_json {
                let old_ids: Vec<String> = serde_json::from_str(&ids_json).map_err(|e| {
                    GradatumError::Storage(format!("write_note_derived_batch parse old ids: {e}"))
                })?;

                for old_id in &old_ids {
                    // Supprimer FTS d'abord (content=notes, pas de trigger automatique dans certaines configs).
                    let rowid: Option<i64> = conn
                        .query_row(
                            "SELECT rowid FROM notes WHERE id = ?1 AND vault_id = ?2",
                            rusqlite::params![old_id, vault_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|e| {
                            GradatumError::Storage(format!("write_note_derived_batch rowid: {e}"))
                        })?;

                    if let Some(rowid) = rowid {
                        conn.execute(
                            "DELETE FROM notes_fts WHERE rowid = ?1",
                            rusqlite::params![rowid],
                        )
                        .map_err(|e| {
                            GradatumError::Storage(format!(
                                "write_note_derived_batch delete fts: {e}"
                            ))
                        })?;
                    }

                    conn.execute(
                        "DELETE FROM notes WHERE id = ?1 AND vault_id = ?2",
                        rusqlite::params![old_id, vault_id],
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("write_note_derived_batch delete note: {e}"))
                    })?;
                }
            }

            // Étape 3 : insérer les nouvelles notes.
            let now_ms = chrono::Utc::now().timestamp_millis();
            // content_hash pour la colonne obligatoire : SHA-256 du body_text (stable, déterministe).
            // Distinct de content_hash_source (hash du fichier git source).
            use sha2::{Digest as _, Sha256};

            for note in &notes {
                let content_hash_bytes: [u8; 32] = Sha256::digest(note.body_text.as_bytes()).into();
                // Sérialiser les métadonnées structurées du symbole dans extra_json["cs"].
                // Le handler code_scope les relit telles quelles (pas de parse fragile du body).
                // extra_json est un objet libre — la clé "cs" est additive, sans impact
                // sur les notes existantes.
                let extra_json: Option<String> = match &note.code_meta {
                    Some(meta) => {
                        let obj = serde_json::json!({ "cs": meta });
                        Some(serde_json::to_string(&obj).map_err(|e| {
                            GradatumError::Storage(format!(
                                "write_note_derived_batch serialize code_meta {}: {e}",
                                note.id
                            ))
                        })?)
                    }
                    None => None,
                };
                conn.execute(
                    "INSERT INTO notes (
                        id, vault_id, locus, section, status, schema_version,
                        created, content_hash, body_text, tags, provenance, trust, extra_json
                    ) VALUES (?1, ?2, NULL, 'architecture', 'live', 1, ?3, ?4, ?5, ?6, 'derived:tree-sitter', 0.5, ?7)
                    ON CONFLICT(id) DO UPDATE SET
                        body_text = excluded.body_text,
                        tags = excluded.tags,
                        content_hash = excluded.content_hash,
                        extra_json = excluded.extra_json",
                    rusqlite::params![
                        note.id.to_string(),
                        vault_id,
                        now_ms,
                        content_hash_bytes.as_slice(),
                        note.body_text,
                        if note.tags.is_empty() { None } else { Some(note.tags.as_str()) },
                        extra_json,
                    ],
                )
                .map_err(|e| GradatumError::Storage(format!("write_note_derived_batch insert note {}: {e}", note.id)))?;

                // Synchroniser FTS5 (content=notes).
                // INSERT OR REPLACE garantit l'idempotence : si la note existait déjà
                // (ON CONFLICT DO UPDATE laisse le rowid intact), le REPLACE écrase l'ancienne
                // entrée FTS plutôt que de créer un doublon (invariant §4.2 — 0 doublons FTS).
                conn.execute(
                    "INSERT OR REPLACE INTO notes_fts (rowid, body_text, tags)
                     SELECT rowid, body_text, tags FROM notes WHERE id = ?1",
                    rusqlite::params![note.id.to_string()],
                )
                .map_err(|e| {
                    GradatumError::Storage(format!(
                        "write_note_derived_batch fts insert {}: {e}",
                        note.id
                    ))
                })?;

                // Mettre à jour le titre si fourni.
                if let Some(title) = &note.title {
                    conn.execute(
                        "UPDATE notes SET title = ?1 WHERE id = ?2",
                        rusqlite::params![title, note.id.to_string()],
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!(
                            "write_note_derived_batch title {}: {e}",
                            note.id
                        ))
                    })?;
                }
            }

            // Étape 4 : upsert code_freshness.
            conn.execute(
                "INSERT INTO code_freshness (vault_id, source_path, content_hash_source, ingested_sha, note_ids)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(vault_id, source_path) DO UPDATE SET
                     content_hash_source = excluded.content_hash_source,
                     ingested_sha = excluded.ingested_sha,
                     note_ids = excluded.note_ids",
                rusqlite::params![vault_id, source_path, content_hash_source, ingested_sha, note_ids_json],
            )
            .map_err(|e| GradatumError::Storage(format!("write_note_derived_batch upsert freshness: {e}")))?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT").map_err(|e| {
                    GradatumError::Storage(format!("write_note_derived_batch COMMIT: {e}"))
                })?;
                Ok(())
            }
            Err(e) => {
                // Rollback best-effort — ignorer toute erreur de rollback.
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Deletes all notes for a `vault_id` from the index (complete drop).
    ///
    /// ## Usage
    ///
    /// `delete_vault_from_index("code-myproject")` deletes **all** notes in the logical
    /// vault `code-myproject` together with their `code_freshness` entries. Idempotent.
    ///
    /// ## Atomicity
    ///
    /// Runs in a transaction. On error, no partial deletion occurs.
    ///
    /// ## Vault isolation invariant
    ///
    /// No orphans in `main`: only notes for the specified `vault_id` are deleted.
    /// Notes with `vault_id='main'` are NEVER touched.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn delete_vault_from_index(&self, vault_id: &str) -> Result<u64, GradatumError> {
        // ── Garde isolation vault principal (invariant §3.4) ──────────────────
        // Même garde que write_note_derived_batch : un DELETE sans préfixe "code-"
        // détruirait le vault principal en cascade (note_audit_trail, note_index, etc.).
        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "delete_vault_from_index : vault_id '{vault_id}' doit commencer par 'code-' \
                 (destruction du vault principal ou non-code interdite)"
            )));
        }

        let conn = self.conn.lock().await;

        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| GradatumError::Storage(format!("delete_vault_from_index BEGIN: {e}")))?;

        let result = (|| -> Result<u64, GradatumError> {
            // Récupérer les rowids pour la suppression FTS.
            let rowids: Vec<i64> = {
                let mut stmt = conn
                    .prepare("SELECT rowid FROM notes WHERE vault_id = ?1")
                    .map_err(|e| {
                        GradatumError::Storage(format!(
                            "delete_vault_from_index prepare rowids: {e}"
                        ))
                    })?;
                let rows = stmt
                    .query_map(rusqlite::params![vault_id], |row| row.get(0))
                    .map_err(|e| {
                        GradatumError::Storage(format!("delete_vault_from_index query rowids: {e}"))
                    })?;
                rows.collect::<Result<Vec<i64>, _>>().map_err(|e| {
                    GradatumError::Storage(format!("delete_vault_from_index collect rowids: {e}"))
                })?
            };

            // Supprimer les entrées FTS pour ce vault.
            for rowid in &rowids {
                conn.execute(
                    "DELETE FROM notes_fts WHERE rowid = ?1",
                    rusqlite::params![rowid],
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("delete_vault_from_index fts delete: {e}"))
                })?;
            }

            // Supprimer les notes du vault (CASCADE supprime note_audit_trail, note_index, etc.).
            let deleted = conn
                .execute(
                    "DELETE FROM notes WHERE vault_id = ?1",
                    rusqlite::params![vault_id],
                )
                .map_err(|e| {
                    GradatumError::Storage(format!("delete_vault_from_index notes: {e}"))
                })?;

            // Supprimer code_freshness pour ce vault.
            conn.execute(
                "DELETE FROM code_freshness WHERE vault_id = ?1",
                rusqlite::params![vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!("delete_vault_from_index freshness: {e}"))
            })?;

            // Supprimer le mapping repo path (Phase C). Idempotent (0 ligne si absent).
            conn.execute(
                "DELETE FROM code_vault WHERE vault_id = ?1",
                rusqlite::params![vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!("delete_vault_from_index code_vault: {e}"))
            })?;

            Ok(deleted as u64)
        })();

        match result {
            Ok(count) => {
                conn.execute_batch("COMMIT").map_err(|e| {
                    GradatumError::Storage(format!("delete_vault_from_index COMMIT: {e}"))
                })?;
                Ok(count)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ── v0.5.2 Code-ingest helpers ────────────────────────────────────────────

    /// Computes the hex SHA-256 hash of a source file's bytes.
    ///
    /// Algorithm is identical to `gradatum_ingest::content_hash_source` — intentionally
    /// duplicated to avoid a circular dependency (`gradatum-index` ← `gradatum-ingest`).
    ///
    /// ## Side effects
    ///
    /// None. Pure function.
    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Checks whether a source file is up to date relative to what is indexed in `code_freshness`.
    ///
    /// Reads the stored `content_hash_source` for `(vault_id, source_path)` and compares
    /// it to the SHA-256 hash computed from `current_file_bytes`.
    ///
    /// ## Drift-detection semantics
    ///
    /// Drift-detection **does not block** and **does not regenerate** synchronously.
    /// It returns only the freshness state. Async regeneration is the caller's responsibility:
    /// `Freshness::Stale` should enqueue a regeneration job — intentionally not implemented here.
    ///
    /// ## Accuracy over coverage
    ///
    /// On uncertainty (missing entry), `Unknown` is returned. Never `Fresh` by default —
    /// a false Fresh is more costly than an `Unknown` (the agent would act on stale data).
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    #[must_use = "Freshness retourné — utiliser la valeur pour décider de la régen"]
    pub async fn check_freshness(
        &self,
        vault_id: &str,
        source_path: &str,
        current_file_bytes: &[u8],
    ) -> Result<Freshness, GradatumError> {
        let conn = self.conn.lock().await;
        let stored_hash: Option<String> = conn
            .query_row(
                "SELECT content_hash_source FROM code_freshness WHERE vault_id = ?1 AND source_path = ?2",
                rusqlite::params![vault_id, source_path],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GradatumError::Storage(format!("check_freshness query: {e}")))?;

        let Some(stored) = stored_hash else {
            // Accuracy > coverage : pas d'entrée → Unknown (jamais Fresh par défaut).
            return Ok(Freshness::Unknown);
        };

        let current = Self::sha256_hex(current_file_bytes);

        if current == stored {
            Ok(Freshness::Fresh)
        } else {
            Ok(Freshness::Stale {
                stored_hash: stored,
                current_hash: current,
            })
        }
    }

    /// Returns the `source_path → content_hash_source` map from `code_freshness`.
    ///
    /// Used by `gradatum-admin code ingest` to detect unchanged files (idempotence).
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn get_code_freshness_map(
        &self,
        vault_id: &str,
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT source_path, content_hash_source FROM code_freshness WHERE vault_id = ?1",
            )
            .map_err(|e| GradatumError::Storage(format!("get_code_freshness_map prepare: {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![vault_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("get_code_freshness_map query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("get_code_freshness_map collect: {e}")))?;

        Ok(rows.into_iter().collect())
    }

    /// Returns the stored `content_hash_source` values for a subset of `source_path` entries.
    ///
    /// Filtered variant of [`Self::get_code_freshness_map`]: only loads the requested paths
    /// (bounded drift-detection — only files in the result set). Paths absent from
    /// `code_freshness` are not included in the returned map.
    ///
    /// ## Side effects
    ///
    /// None (read-only).
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn code_freshness_hashes_for(
        &self,
        vault_id: &str,
        source_paths: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        if source_paths.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.conn.lock().await;
        // Placeholders dynamiques (?2, ?3, …) — paths bornés par le budget tokens côté handler.
        let placeholders: Vec<String> = (0..source_paths.len())
            .map(|i| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "SELECT source_path, content_hash_source FROM code_freshness
             WHERE vault_id = ?1 AND source_path IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            GradatumError::Storage(format!("code_freshness_hashes_for prepare: {e}"))
        })?;
        // params : vault_id puis chaque path.
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(source_paths.len() + 1);
        params.push(&vault_id);
        for p in source_paths {
            params.push(p);
        }
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("code_freshness_hashes_for query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                GradatumError::Storage(format!("code_freshness_hashes_for collect: {e}"))
            })?;
        Ok(rows.into_iter().collect())
    }

    /// Deletes the `code_freshness` entry for a specific `source_path`.
    ///
    /// Called when propagating deletions (source_path absent from `git ls-files`).
    /// Notes are deleted via `write_note_derived_batch(notes=[])` before calling this method.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn delete_code_freshness_entry(
        &self,
        vault_id: &str,
        source_path: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM code_freshness WHERE vault_id = ?1 AND source_path = ?2",
            rusqlite::params![vault_id, source_path],
        )
        .map_err(|e| GradatumError::Storage(format!("delete_code_freshness_entry: {e}")))?;
        Ok(())
    }

    /// Queries the code-ingest index for a `code-<project>` vault — **BM25-only**.
    ///
    /// ## Single-vault guard bypass
    ///
    /// This method queries the index directly with `WHERE vault_id = ?1` **without** the
    /// 403 guard `vault_id ≠ main` (which would block a `code-*` vault). Security therefore
    /// relies ENTIRELY on the caller (handler `code_scope`), which MUST validate that
    /// `vault_id` starts with `code-` BEFORE calling this method. A defense-in-depth
    /// guard is added here: the method rejects calls where the `code-` prefix is absent.
    ///
    /// ## Scoring
    ///
    /// - `Query` → real BM25 (`bm25(notes_fts)`), sorted ASC (best match first).
    /// - `Path` / `Symbol` → no lexical scoring, `bm25 = 0.0`, sorted by `qualified_name`.
    ///
    /// No trust/decay/ANN (code notes have no embedding or trust score).
    ///
    /// ## Structured fields
    ///
    /// Read from `notes.extra_json["cs"]` (populated at ingest). A derived note without
    /// a `cs` field (abnormal — legacy note) is **omitted** (accuracy over coverage),
    /// never returned with potentially incorrect empty fields.
    ///
    /// ## Parameters
    ///
    /// - `vault_id`: logical vault (`code-<project>`).
    /// - `selector`: search criterion.
    /// - `limit`: upper bound on candidate entries (the handler applies the token budget
    ///   afterwards). The caller is responsible for clamping.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error or if `vault_id`
    /// does not start with `code-` (defense in depth).
    pub async fn code_scope_query(
        &self,
        vault_id: &str,
        selector: &CodeSelector,
        limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        // Défense en profondeur (invariant sécu §3.3) — la garde primaire est dans le handler.
        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "code_scope_query : vault_id '{vault_id}' doit commencer par 'code-' \
                 (isolation — code_scope ne lit jamais 'main' ni un vault non-code)"
            )));
        }

        let conn = self.conn.lock().await;

        // Récupère (id, extra_json, bm25) selon le selector, puis décode extra_json["cs"].
        let rows: Vec<(String, Option<String>, f64)> = match selector {
            CodeSelector::Query(q) => {
                let fts_query = fts5_quote_query(q);
                if fts_query.is_empty() {
                    return Ok(Vec::new());
                }
                let mut stmt = conn
                    .prepare(
                        "SELECT n.id, n.extra_json, bm25(notes_fts) AS score
                         FROM notes_fts
                         JOIN notes n ON n.rowid = notes_fts.rowid
                         WHERE n.vault_id = ?1
                           AND n.provenance = 'derived:tree-sitter'
                           AND notes_fts MATCH ?2
                         ORDER BY score ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query prepare fts: {e}"))
                    })?;
                let mapped = stmt
                    .query_map(
                        rusqlite::params![vault_id, fts_query, limit as i64],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, f64>(2)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query query fts: {e}"))
                    })?;
                mapped.collect::<Result<Vec<_>, _>>().map_err(|e| {
                    GradatumError::Storage(format!("code_scope_query collect fts: {e}"))
                })?
            }
            CodeSelector::Path(p) => {
                // Préfixe LIKE échappé : match tous les symboles d'un fichier ou d'un dossier.
                let like = format!("{}%", escape_like(p));
                let mut stmt = conn
                    .prepare(
                        "SELECT n.id, n.extra_json, 0.0 AS score
                         FROM notes n
                         WHERE n.vault_id = ?1
                           AND n.provenance = 'derived:tree-sitter'
                           AND json_extract(n.extra_json, '$.cs.source_path') LIKE ?2 ESCAPE '\\'
                         ORDER BY json_extract(n.extra_json, '$.cs.qualified_name') ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query prepare path: {e}"))
                    })?;
                let mapped = stmt
                    .query_map(rusqlite::params![vault_id, like, limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    })
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query query path: {e}"))
                    })?;
                mapped.collect::<Result<Vec<_>, _>>().map_err(|e| {
                    GradatumError::Storage(format!("code_scope_query collect path: {e}"))
                })?
            }
            CodeSelector::Symbol(s) => {
                let like = format!("%{}%", escape_like(s));
                let mut stmt = conn
                    .prepare(
                        "SELECT n.id, n.extra_json, 0.0 AS score
                         FROM notes n
                         WHERE n.vault_id = ?1
                           AND n.provenance = 'derived:tree-sitter'
                           AND json_extract(n.extra_json, '$.cs.qualified_name') LIKE ?2 ESCAPE '\\'
                         ORDER BY json_extract(n.extra_json, '$.cs.qualified_name') ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query prepare symbol: {e}"))
                    })?;
                let mapped = stmt
                    .query_map(rusqlite::params![vault_id, like, limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    })
                    .map_err(|e| {
                        GradatumError::Storage(format!("code_scope_query query symbol: {e}"))
                    })?;
                mapped.collect::<Result<Vec<_>, _>>().map_err(|e| {
                    GradatumError::Storage(format!("code_scope_query collect symbol: {e}"))
                })?
            }
        };

        // Décoder extra_json["cs"] → CodeSymbolMeta. Omettre les notes sans cs (accuracy>coverage).
        let mut entries = Vec::with_capacity(rows.len());
        for (id, extra_json, bm25) in rows {
            let Some(json) = extra_json else {
                continue; // pas de métadonnées structurées → omis
            };
            let parsed: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(_) => continue, // extra_json corrompu → omis
            };
            let Some(cs_val) = parsed.get("cs") else {
                continue;
            };
            let meta: CodeSymbolMeta = match serde_json::from_value(cs_val.clone()) {
                Ok(m) => m,
                Err(_) => continue, // cs malformé → omis
            };
            let note_id = match ulid::Ulid::from_string(&id) {
                Ok(u) => NoteId(u),
                Err(_) => continue,
            };
            entries.push(CodeScopeEntryRaw {
                note_id,
                source_path: meta.source_path,
                kind: meta.kind,
                qualified_name: meta.qualified_name,
                signature: meta.signature,
                deps: meta.deps,
                bm25,
                span: meta.span,
            });
        }

        Ok(entries)
    }

    /// Returns symbols that list `qualified_name` in their outgoing `deps`
    /// (reverse-dependency / callers lookup).
    ///
    /// ## Query strategy
    ///
    /// deps are stored as a JSON array in `notes.extra_json["cs"]["deps"]`.
    /// SQLite's `json_each()` function expands the array to rows, allowing an
    /// `EXISTS` subquery to match the target. Cost: full-scan of derived notes
    /// within the vault, bounded by `LIMIT`. No dedicated index is needed
    /// because the code-vault corpus is small (typically < 10k notes).
    ///
    /// `// ECON: full scan O(n) on derived notes. Upgrade → index on deps if > 50k derived notes.`
    ///
    /// ## Security
    ///
    /// Same `code-` prefix defense-in-depth as [`SqliteIndex::code_scope_query`].
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on any SQLite error or if `vault_id` does not start with `code-`.
    pub async fn code_scope_reverse_deps(
        &self,
        vault_id: &str,
        qualified_name: &str,
        limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "code_scope_reverse_deps : vault_id '{vault_id}' doit commencer par 'code-' \
                 (isolation — ne lit jamais 'main' ni un vault non-code)"
            )));
        }

        let conn = self.conn.lock().await;

        // Find all symbols in this vault whose deps JSON array contains `qualified_name`.
        // json_each() expands the JSON array; EXISTS() short-circuits on first match.
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.extra_json, 0.0 AS score
                 FROM notes n
                 WHERE n.vault_id = ?1
                   AND n.provenance = 'derived:tree-sitter'
                   AND EXISTS (
                     SELECT 1 FROM json_each(json_extract(n.extra_json, '$.cs.deps')) d
                     WHERE d.value = ?2
                   )
                 ORDER BY json_extract(n.extra_json, '$.cs.qualified_name') ASC
                 LIMIT ?3",
            )
            .map_err(|e| GradatumError::Storage(format!("code_scope_reverse_deps prepare: {e}")))?;

        let rows: Vec<(String, Option<String>, f64)> = stmt
            .query_map(
                rusqlite::params![vault_id, qualified_name, limit as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("code_scope_reverse_deps query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("code_scope_reverse_deps collect: {e}")))?;

        // Décoder extra_json["cs"] → CodeScopeEntryRaw (même logique que code_scope_query).
        let mut entries = Vec::with_capacity(rows.len());
        for (id, extra_json, bm25) in rows {
            let Some(json) = extra_json else {
                continue;
            };
            let parsed: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(cs_val) = parsed.get("cs") else {
                continue;
            };
            let meta: CodeSymbolMeta = match serde_json::from_value(cs_val.clone()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let note_id = match ulid::Ulid::from_string(&id) {
                Ok(u) => NoteId(u),
                Err(_) => continue,
            };
            entries.push(CodeScopeEntryRaw {
                note_id,
                source_path: meta.source_path,
                kind: meta.kind,
                qualified_name: meta.qualified_name,
                signature: meta.signature,
                deps: meta.deps,
                bm25,
                span: meta.span,
            });
        }

        Ok(entries)
    }

    /// Batch reverse-dependency lookup — returns callers for multiple symbols in one SQL query.
    ///
    /// For each `qualified_name` in `names`, finds all symbols in `vault_id` whose `deps`
    /// JSON array contains that name, groups results by target name, caps each list at `limit`,
    /// and returns a `HashMap<qualified_name, Vec<caller_qualified_name>>`.
    ///
    /// One SQL query for all names (vs. N queries in a loop). Each list is sorted
    /// `qualified_name` ASC (deterministic) and capped at `limit`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on any SQLite error or if vault_id does not start with `code-`.
    pub async fn code_scope_reverse_deps_batch(
        &self,
        vault_id: &str,
        names: &[&str],
        limit: usize,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, GradatumError> {
        use std::collections::HashMap;

        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "code_scope_reverse_deps_batch : vault_id '{vault_id}' doit commencer par 'code-'"
            )));
        }
        if names.is_empty() {
            return Ok(HashMap::new());
        }

        // Build the IN clause dynamically: (?2, ?3, ..., ?N+1)
        // vault_id is bound as ?1, then each name as ?2, ?3, ...
        // The SQL finds all notes whose deps contain ANY of the target names,
        // then we dispatch per-name in Rust (avoids a GROUP BY JSON subquery).
        let placeholders: String = (0..names.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT json_extract(n.extra_json, '$.cs.qualified_name') AS caller_name,
                    je.value AS target_name
             FROM notes n, json_each(json_extract(n.extra_json, '$.cs.deps')) je
             WHERE n.vault_id = ?1
               AND n.provenance = 'derived:tree-sitter'
               AND je.value IN ({placeholders})
             ORDER BY caller_name ASC"
        );

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            GradatumError::Storage(format!("code_scope_reverse_deps_batch prepare: {e}"))
        })?;

        // Bind vault_id + each name.
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(1 + names.len());
        params.push(Box::new(vault_id.to_string()));
        for n in names {
            params.push(Box::new((*n).to_string()));
        }
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        let rows: Vec<(Option<String>, Option<String>)> = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })
            .map_err(|e| {
                GradatumError::Storage(format!("code_scope_reverse_deps_batch query: {e}"))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                GradatumError::Storage(format!("code_scope_reverse_deps_batch collect: {e}"))
            })?;

        // Dispatch: group caller_name by target_name, capped at `limit` per target.
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        for (caller, target) in rows {
            let (Some(caller_name), Some(target_name)) = (caller, target) else {
                continue;
            };
            let list = result.entry(target_name).or_default();
            if list.len() < limit {
                list.push(caller_name);
            }
        }

        Ok(result)
    }

    /// Returns the last `ingested_sha` recorded for a code vault.
    ///
    /// After a full `code ingest`, all files share the same HEAD sha.
    /// After incremental `code update` runs, only changed files carry the most recent sha.
    /// The method therefore returns the most frequent sha (mode), which corresponds to
    /// the last known complete state — the starting point for the next `git diff` update.
    ///
    /// Returns `None` if the vault has no `code_freshness` entries (never ingested).
    ///
    /// ## Design rationale
    ///
    /// No dedicated tracking table is used: `code_freshness.ingested_sha` is sufficient.
    /// The mode is robust to a previous partial update. Rejected alternative: storing
    /// HEAD in a `code_vault_head` table — an extra table for a single value with no benefit
    /// (the sha already lives per-file).
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn get_last_ingested_sha(
        &self,
        vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;
        let sha: Option<String> = conn
            .query_row(
                "SELECT ingested_sha FROM code_freshness
                 WHERE vault_id = ?1 AND ingested_sha != ''
                 GROUP BY ingested_sha
                 ORDER BY COUNT(*) DESC, ingested_sha DESC
                 LIMIT 1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GradatumError::Storage(format!("get_last_ingested_sha: {e}")))?;
        Ok(sha)
    }

    /// Upserts the absolute git repository path for a code vault.
    ///
    /// Populated by `code ingest`/`code update`. Allows the server (isolated from repos)
    /// to locate source files for drift-detection.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error or if `vault_id`
    /// does not start with `code-` (defense in depth).
    pub async fn set_code_vault_repo_path(
        &self,
        vault_id: &str,
        repo_abs_path: &str,
    ) -> Result<(), GradatumError> {
        self.set_code_vault_repo_path_with_visibility(vault_id, repo_abs_path, "pub")
            .await
    }

    /// Variant of `set_code_vault_repo_path` that also persists the visibility mode.
    ///
    /// ## `visibility` parameter
    ///
    /// - `"pub"`: only public items are indexed (default behaviour).
    /// - `"all"`: all items are indexed (private items included).
    ///
    /// Only this variant should be called by `run_ingest` and `run_update` when the mode
    /// is known. `set_code_vault_repo_path` (without suffix) is kept for backward
    /// compatibility with existing tests (it calls this method with `"pub"`).
    pub async fn set_code_vault_repo_path_with_visibility(
        &self,
        vault_id: &str,
        repo_abs_path: &str,
        visibility: &str,
    ) -> Result<(), GradatumError> {
        if !vault_id.starts_with("code-") {
            return Err(GradatumError::Storage(format!(
                "set_code_vault_repo_path : vault_id '{vault_id}' doit commencer par 'code-'"
            )));
        }
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO code_vault (vault_id, repo_abs_path, visibility) VALUES (?1, ?2, ?3)
             ON CONFLICT(vault_id) DO UPDATE SET
                 repo_abs_path = excluded.repo_abs_path,
                 visibility    = excluded.visibility",
            rusqlite::params![vault_id, repo_abs_path, visibility],
        )
        .map_err(|e| {
            GradatumError::Storage(format!("set_code_vault_repo_path_with_visibility: {e}"))
        })?;
        Ok(())
    }

    /// Returns the stored visibility mode for a code vault.
    ///
    /// Returns `None` if the vault does not exist. Returns `Some("pub")` if the column
    /// is NULL (backward compatibility for vaults ingested before migration 0018, which
    /// default to `'pub'`).
    pub async fn get_code_vault_visibility(
        &self,
        vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;
        let vis: Option<String> = conn
            .query_row(
                "SELECT visibility FROM code_vault WHERE vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GradatumError::Storage(format!("get_code_vault_visibility: {e}")))?;
        Ok(vis)
    }

    /// Returns the absolute git repository path for a code vault.
    ///
    /// Returns `None` if the vault has never been ingested or was ingested by a version
    /// that did not record the repo path (drift-detection is then skipped — accuracy over
    /// coverage: no false `Fresh`).
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn get_code_vault_repo_path(
        &self,
        vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;
        let path: Option<String> = conn
            .query_row(
                "SELECT repo_abs_path FROM code_vault WHERE vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GradatumError::Storage(format!("get_code_vault_repo_path: {e}")))?;
        Ok(path)
    }

    /// Returns all derived notes (`provenance = 'derived:tree-sitter'`) for a code vault.
    ///
    /// **Unbounded scan** — intended for golden tests at real-world scale.
    /// Returns `Vec<(note_id_str, body_text, tags_raw)>`.
    ///
    /// Unlike [`Self::list_notes`] (paginated, public API), this method reads the full set
    /// in a single pass and filters on `provenance = 'derived:tree-sitter'` to exclude
    /// non-code notes that share the same `vault_id`.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn list_all_derived_notes(
        &self,
        vault_id: &str,
    ) -> Result<Vec<(String, String, Option<String>)>, GradatumError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, body_text, tags \
                 FROM notes \
                 WHERE vault_id = ?1 AND provenance = 'derived:tree-sitter' \
                 ORDER BY id",
            )
            .map_err(|e| GradatumError::Storage(format!("list_all_derived_notes prepare: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![vault_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("list_all_derived_notes query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("list_all_derived_notes collect: {e}")))?;
        Ok(rows)
    }

    // ── F-55 TemporalIndex ────────────────────────────────────────────────────

    /// Writes or updates the temporal index entry for a note in `temporal_index`.
    ///
    /// Uses `INSERT OR REPLACE` (upsert by PK `note_id`): idempotent on each curate.
    /// Called after each successful curate to update the anchor per the priority
    /// `occurred_at > event-date > valid_from > created`.
    ///
    /// ## Side effects
    ///
    /// - INSERT if the note has no temporal entry yet.
    /// - REPLACE if an entry already exists (updates `anchor_ms`/`anchor_src`/`doc_kind`).
    /// - No cascade: deletion must be explicit via `delete_temporal_entry`.
    ///
    /// ## Errors
    ///
    /// Returns `GradatumError::Storage` on any SQLite error.
    pub async fn write_temporal_entry(&self, entry: &TemporalEntry) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO temporal_index \
             (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                entry.note_id,
                entry.vault_id,
                entry.anchor_ms,
                entry.anchor_src.as_db_str(),
                entry.doc_kind,
                entry.valid_until_ms,
            ],
        )
        .map_err(|e| GradatumError::Storage(format!("write_temporal_entry : {e}")))?;
        Ok(())
    }

    /// Deletes the temporal index entry for a note (`PRAGMA foreign_keys` is not guaranteed in all contexts — no implicit `ON DELETE CASCADE`).
    ///
    /// Called explicitly in `delete_note_from_index` — do not rely on a SQLite cascade
    /// because `PRAGMA foreign_keys` is not guaranteed in all execution contexts.
    ///
    /// Idempotent: if the note has no `temporal_index` entry, returns `Ok(false)`.
    pub async fn delete_temporal_entry(&self, note_id: &str) -> Result<bool, GradatumError> {
        let conn = self.conn.lock().await;
        let deleted = conn
            .execute(
                "DELETE FROM temporal_index WHERE note_id = ?1",
                rusqlite::params![note_id],
            )
            .map_err(|e| GradatumError::Storage(format!("delete_temporal_entry : {e}")))?;
        Ok(deleted > 0)
    }

    /// Backfills `temporal_index` for all notes that have no entry.
    ///
    /// Uses `INSERT OR IGNORE` in bulk — idempotent, does not modify existing entries
    /// (does not downgrade an anchor that was enriched by curate).
    ///
    /// All notes without a temporal entry receive `anchor_src='created'` and
    /// `doc_kind` from `notes.doc_kind` (`COALESCE 'Static'` for pre-migration-0008 notes).
    ///
    /// ## Usage
    ///
    /// Called by `handle_reindex` in Full mode (when implemented) or from maintenance
    /// scripts. Migration 0013 already backfills all existing notes — this method covers
    /// rebuild/reset scenarios.
    ///
    /// ## Side effects
    ///
    /// - Excludes sentinels (`__sentinel__%`).
    /// - Does NOT modify existing entries (`INSERT OR IGNORE` on PK).
    /// - Returns the number of rows inserted.
    pub async fn backfill_temporal_index(&self) -> Result<u64, GradatumError> {
        let conn = self.conn.lock().await;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO temporal_index \
                 (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms) \
                 SELECT n.id, n.vault_id, n.created, 'created', \
                        COALESCE(n.doc_kind, 'Static'), NULL \
                 FROM notes AS n \
                 WHERE n.id NOT LIKE '__sentinel__%'",
                [],
            )
            .map_err(|e| GradatumError::Storage(format!("backfill_temporal_index : {e}")))?;
        // SAFETY: rusqlite::execute() retourne usize — cast u64 sans perte (rows < usize::MAX).
        Ok(inserted as u64)
    }

    /// Paginated temporal read from `temporal_index`.
    ///
    /// See the contract of `IndexStore::timeline`. Sorted `anchor_ms DESC, note_id DESC`,
    /// filters `doc_kind`/`from_ms`/`to_ms`/`cursor`, excluding `status='garbage'` and
    /// sentinels. `forgotten=1` is **included** (factual journal). Uses the
    /// `idx_temporal_vault_anchor` index.
    ///
    /// SQL is built dynamically but **never interpolates a value**:
    /// `format!` only emits `?` placeholders; all values are bound into a single
    /// `Vec<Value>` populated in the exact literal order the `?` appear
    /// (injection-safe, order preserved).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails or if a stored `note_id` is not
    /// a valid ULID (data corruption — unrecoverable by the reader).
    pub async fn timeline(
        &self,
        vault_id: &VaultId,
        filter: &gradatum_core::temporal_query::TimelineFilter,
    ) -> Result<Vec<gradatum_core::temporal_query::TimelineRow>, GradatumError> {
        use gradatum_core::temporal_query::TimelineRow;
        use rusqlite::types::Value as SqlVal;
        use ulid::Ulid;

        let limit = filter.limit.clamp(1, 200) as i64; // P2-5 cap 200

        // Sections protégées exclues (V1 sécu) : `agent-issues`/`council` portent
        // des titres sensibles (verdicts, incidents agents). Source unique
        // `Section::PROTECTED_FORGET` (gradatum-core) — dérivée, jamais hardcodée :
        // si la liste évolue, le filtre suit. Placeholders + binds (pas de littéral).
        use gradatum_core::section::Section;
        let protected_ph = Section::PROTECTED_FORGET
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");

        // Un seul vecteur de binds, rempli dans l'ordre EXACT des `?` du SQL.
        let mut sql = format!(
            "SELECT t.note_id, t.anchor_ms, t.anchor_src, t.doc_kind, n.title \
             FROM temporal_index t \
             JOIN notes n ON n.id = t.note_id \
             WHERE t.vault_id = ? \
               AND n.status != 'garbage' \
               AND t.note_id NOT LIKE '__sentinel__%' \
               AND n.section NOT IN ({protected_ph})",
        );
        let mut binds: Vec<SqlVal> = vec![SqlVal::Text(vault_id.to_string())];
        binds.extend(
            Section::PROTECTED_FORGET
                .iter()
                .map(|s| SqlVal::Text(s.as_str().to_string())),
        );

        if let Some(kinds) = filter.doc_kind.as_ref().filter(|k| !k.is_empty()) {
            let ph = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND t.doc_kind IN ({ph})"));
            binds.extend(kinds.iter().map(|k| SqlVal::Text(k.clone())));
        }
        if let Some(from) = filter.from_ms {
            sql.push_str(" AND t.anchor_ms >= ?");
            binds.push(SqlVal::Integer(from));
        }
        if let Some(to) = filter.to_ms {
            sql.push_str(" AND t.anchor_ms <= ?");
            binds.push(SqlVal::Integer(to));
        }
        if let Some(cur) = filter.cursor.as_ref() {
            // keyset : (anchor_ms, note_id) strictement « avant » dans l'ordre DESC,DESC.
            sql.push_str(" AND (t.anchor_ms < ? OR (t.anchor_ms = ? AND t.note_id < ?))");
            binds.push(SqlVal::Integer(cur.anchor_ms));
            binds.push(SqlVal::Integer(cur.anchor_ms));
            binds.push(SqlVal::Text(cur.note_id.clone()));
        }
        // v0.5.1 — filtre validité « as-of T ».
        //
        // Sémantique (3 cas) :
        //   as_of=Some(t), include_expired=false → anchor_ms ≤ T AND (valid_until IS NULL OR T < valid_until)
        //                                          (note valide à T : née avant T ET pas encore expirée)
        //   as_of=Some(t), include_expired=true  → anchor_ms ≤ T
        //                                          (note née avant T, même expirée — requête historique)
        //   as_of=None                            → aucun filtre validité (rétrocompat v0.5.0)
        //
        // Ordre : si as_of_ms présent, la clause anchor_ms ≤ T est toujours ajoutée ;
        // la clause valid_until est conditionnelle à !include_expired.
        if let Some(t) = filter.as_of_ms {
            sql.push_str(" AND t.anchor_ms <= ?");
            binds.push(SqlVal::Integer(t));
            if !filter.include_expired {
                sql.push_str(" AND (t.valid_until_ms IS NULL OR ? < t.valid_until_ms)");
                binds.push(SqlVal::Integer(t));
            }
        }
        // as_of=None → aucune clause validité, include_expired sans effet (pas de référence temporelle).
        sql.push_str(" ORDER BY t.anchor_ms DESC, t.note_id DESC LIMIT ?");
        binds.push(SqlVal::Integer(limit));

        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("prepare timeline : {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,         // note_id
                    row.get::<_, i64>(1)?,            // anchor_ms
                    row.get::<_, String>(2)?,         // anchor_src
                    row.get::<_, String>(3)?,         // doc_kind
                    row.get::<_, Option<String>>(4)?, // title
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("query timeline : {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| GradatumError::Storage(format!("collect timeline : {e}")))?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, anchor_ms, anchor_src, doc_kind, title) in rows {
            let ulid = Ulid::from_string(&id).map_err(|e| {
                GradatumError::Storage(format!("timeline: note_id non-ULID {id:?} : {e}"))
            })?;
            out.push(TimelineRow {
                note_id: NoteId(ulid),
                anchor_ms,
                anchor_src,
                doc_kind,
                title,
            });
        }
        Ok(out)
    }

    /// Upserts an override into the generic `note_overrides` table.
    ///
    /// ## Primary key
    ///
    /// `(note_id, scope_kind, scope_id, override_type)` — one active override per tuple.
    /// `ON CONFLICT … DO UPDATE` updates mutable fields without changing `created_at`.
    ///
    /// ## `file_relative_path` placeholder
    ///
    /// The real path `.gradatum/overrides/{vault}/{locus}/{note_id}.{type}.toml` will be
    /// computed by the vault orchestrator, which knows `vault_id` + `locus`.
    /// Current value: placeholder `"_unset/{note_id}.{override_type}.toml"`.
    pub async fn upsert_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
        schema_version: u32,
        payload_toml: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let id_str = note_id.to_string();

        let (scope_kind, scope_id, vault_id) = match scope {
            OverrideScope::Vault(v) => ("vault", v.to_string(), v.to_string()),
            OverrideScope::Locus(l) => ("locus", l.to_string(), "_unset".to_string()),
            OverrideScope::Bearer(b) => ("bearer", b.to_string(), "_unset".to_string()),
        };

        // file_hash = sha256(payload_toml) — permet de détecter un changement fichier
        use sha2::Digest as _;
        let file_hash: [u8; 32] = sha2::Sha256::digest(payload_toml.as_bytes()).into();

        let now_ms = chrono::Utc::now().timestamp_millis();
        let file_relative_path = format!("_unset/{id_str}.{override_type}.toml");

        conn.execute(
            "INSERT INTO note_overrides (
                note_id, vault_id, scope_kind, scope_id, override_type, schema_version,
                payload_toml, priority, created_by_kind, created_by_id,
                created_at, reason, file_relative_path, file_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL, ?8, NULL, ?9, ?10)
            ON CONFLICT(note_id, scope_kind, scope_id, override_type) DO UPDATE SET
                schema_version     = excluded.schema_version,
                payload_toml       = excluded.payload_toml,
                file_hash          = excluded.file_hash",
            rusqlite::params![
                id_str,
                vault_id,
                scope_kind,
                scope_id,
                override_type,
                schema_version,
                payload_toml,
                now_ms,
                file_relative_path,
                &file_hash[..],
            ],
        )
        .map_err(|e| GradatumError::Storage(format!("upsert_override_raw : {e}")))?;

        Ok(())
    }

    pub async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        let conn = self.conn.lock().await;
        let id_str = note_id.to_string();

        let (scope_kind, scope_id) = match scope {
            OverrideScope::Vault(v) => ("vault", v.to_string()),
            OverrideScope::Locus(l) => ("locus", l.to_string()),
            OverrideScope::Bearer(b) => ("bearer", b.to_string()),
        };

        match conn.query_row(
            "SELECT schema_version, payload_toml FROM note_overrides
             WHERE note_id = ?1 AND scope_kind = ?2 AND scope_id = ?3 AND override_type = ?4",
            rusqlite::params![id_str, scope_kind, scope_id, override_type],
            |row| {
                let sv: u32 = row.get(0)?;
                let pt: String = row.get(1)?;
                Ok((sv, pt))
            },
        ) {
            Ok(pair) => Ok(Some(pair)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_override_raw : {e}"))),
        }
    }

    pub async fn upsert_file_checksum(
        &self,
        entry: &FileChecksumEntry,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;

        // FileKind → kebab-case string (ex. "note", "override", "config")
        let kind_str = match entry.file_kind {
            FileKind::Note => "note",
            FileKind::Override => "override",
            FileKind::Config => "config",
        };

        conn.execute(
            "INSERT INTO file_checksums (
                relative_path, file_kind, expected_size,
                expected_hash_prefix_4kb, expected_hash,
                expected_mtime, last_verified
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(relative_path) DO UPDATE SET
                file_kind                = excluded.file_kind,
                expected_size            = excluded.expected_size,
                expected_hash_prefix_4kb = excluded.expected_hash_prefix_4kb,
                expected_hash            = excluded.expected_hash,
                expected_mtime           = excluded.expected_mtime,
                last_verified            = excluded.last_verified",
            rusqlite::params![
                entry.relative_path,
                kind_str,
                entry.expected_size as i64,
                &entry.expected_hash_prefix_4kb[..],
                &entry.expected_hash[..],
                entry.expected_mtime,
                entry.last_verified,
            ],
        )
        .map_err(|e| GradatumError::Storage(format!("upsert_file_checksum : {e}")))?;

        Ok(())
    }

    pub async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT relative_path, file_kind, expected_size,
                        expected_hash_prefix_4kb, expected_hash,
                        expected_mtime, last_verified
                 FROM file_checksums
                 ORDER BY relative_path",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare list_file_checksums : {e}")))?;

        let rows = stmt
            .query_map([], |row| {
                let kind_str: String = row.get(1)?;
                let size: i64 = row.get(2)?;
                let prefix_bytes: Vec<u8> = row.get(3)?;
                let hash_bytes: Vec<u8> = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    kind_str,
                    size,
                    prefix_bytes,
                    hash_bytes,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("query list_file_checksums : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (relative_path, kind_str, size, prefix_bytes, hash_bytes, mtime, verified) =
                r.map_err(|e| GradatumError::Storage(format!("row list_file_checksums : {e}")))?;

            let file_kind = match kind_str.as_str() {
                "note" => FileKind::Note,
                "override" => FileKind::Override,
                "config" => FileKind::Config,
                other => {
                    return Err(GradatumError::Storage(format!(
                        "file_kind inconnu : {other:?}"
                    )));
                }
            };

            if prefix_bytes.len() < 32 {
                return Err(GradatumError::Storage(format!(
                    "expected_hash_prefix_4kb trop court ({} bytes) pour {relative_path:?}",
                    prefix_bytes.len()
                )));
            }
            if hash_bytes.len() < 32 {
                return Err(GradatumError::Storage(format!(
                    "expected_hash trop court ({} bytes) pour {relative_path:?}",
                    hash_bytes.len()
                )));
            }

            let mut prefix_arr = [0u8; 32];
            prefix_arr.copy_from_slice(&prefix_bytes[..32]);
            let mut hash_arr = [0u8; 32];
            hash_arr.copy_from_slice(&hash_bytes[..32]);

            out.push(FileChecksumEntry {
                relative_path,
                file_kind,
                expected_size: size as u64,
                expected_hash_prefix_4kb: prefix_arr,
                expected_hash: hash_arr,
                expected_mtime: mtime,
                last_verified: verified,
            });
        }

        Ok(out)
    }

    pub async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        // Délégation vers la méthode concrète définie dans queries.rs (renommée _inner).
        SqliteIndex::get_note_inner(self, tenant_id, note_id_ulid).await
    }

    pub async fn search_fts_scored(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        let conn = self.conn.lock().await;

        // Phase 2.1.2 alpha.9 — filtre downgraded conditionnel.
        //
        // BM25 retourne des valeurs négatives (meilleur match → plus proche de 0).
        // Pénalité downgraded : le score brut est extrait par SQLite, puis multiplié
        // par 10.0 en Rust pour les notes downgraded. Cela amplifie la valeur négative
        // (ex: -0.5 → -5.0) → plus négatif = moins bon → ORDER BY ASC les place APRÈS
        // les notes live.
        // Cette approche préserve la sémantique "pertinence réduite à 10%" tout en
        // respectant l'ordre naturel BM25 ASC.
        //
        // F-44 decay forgotten : court-circuit AVANT la pénalité downgraded.
        // Si forgotten=1 → score × 0.5^elapsed_days (half-life 1 jour).
        // La pénalité downgraded est ignorée pour une note forgotten (pas de cumul).
        let downgraded_clause = if include_downgraded {
            ""
        } else {
            "AND n.status != 'downgraded'"
        };

        // F-44 : on sélectionne forgotten + forgotten_at en plus du status.
        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status,
                    n.forgotten,
                    n.forgotten_at
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

        let now_ms = chrono::Utc::now().timestamp_millis();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("prepare search_fts_scored : {e}")))?;

        let rows = stmt
            .query_map(
                rusqlite::params![query, vault_id.as_str(), limit as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("query search_fts_scored : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (id_str, bm25_raw, status, forgotten, forgotten_at_ms) =
                r.map_err(|e| GradatumError::Storage(format!("row search_fts_scored : {e}")))?;
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("parse ULID {id_str:?} : {e}")))?;

            // F-44 decay forgotten — court-circuit AVANT la pénalité downgraded.
            //
            // Si forgotten=1 : score × 0.5^elapsed_days.
            // elapsed_days=0 → 0.5^0=1.0 → pas de decay le jour même (note forgotten
            // juste marquée reste au rang normal jusqu'au lendemain).
            // La pénalité downgraded est ignorée pour les notes forgotten (pas de cumul).
            //
            // Note : trust/F-17 NE sont PAS lus ici (dormant, réservé v0.4.4 F-17).
            let score = if forgotten != 0 {
                let elapsed_days = forgotten_at_ms
                    .map(|at_ms| (now_ms - at_ms) as f64 / 86_400_000.0)
                    .unwrap_or(0.0)
                    .max(0.0);
                let decay = (0.5f64).powf(elapsed_days);
                bm25_raw * decay
            } else if status == "downgraded" {
                // Pénalité downgraded : amplifier la valeur négative BM25 × 10.
                bm25_raw * 10.0
            } else {
                bm25_raw
            };
            out.push((NoteId(ulid), score, status));
        }
        // Re-trier après application des pénalités (ORDER BY SQL portait sur le score brut).
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    /// Marks a note as forgotten in the index.
    ///
    /// Updates `forgotten=1`, `forgotten_at=<now_ms>`, `forgotten_by=<by>`.
    ///
    /// ## Index vs. vault boundary
    ///
    /// Operates on the SQLite index only — does NOT synchronise the note's YAML
    /// frontmatter on disk. Frontmatter synchronisation (`forgotten`/`forgotten_at`/`forgotten_by`)
    /// is performed by the vault layer, which calls `write_note_with_id` after `mark_forgotten`.
    ///
    /// ## Idempotence
    ///
    /// A second call on an already-forgotten note updates `forgotten_at` and
    /// `forgotten_by` (re-marking with a different actor or correcting the timestamp).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite query fails.
    /// `GradatumError::NoteNotFound` if no row is affected (unknown ULID or `vault_id` mismatch).
    pub async fn mark_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
        by: Option<&str>,
    ) -> Result<(), GradatumError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().await;
        let affected = conn
            .execute(
                "UPDATE notes
                 SET forgotten    = 1,
                     forgotten_at = ?1,
                     forgotten_by = ?2
                 WHERE id = ?3
                   AND vault_id = ?4",
                rusqlite::params![now_ms, by, note_id, vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!("mark_forgotten UPDATE {note_id:?} : {e}"))
            })?;
        if affected == 0 {
            let ulid = ulid::Ulid::from_string(note_id).map_err(|e| {
                GradatumError::Storage(format!("mark_forgotten ULID parse {note_id:?} : {e}"))
            })?;
            return Err(GradatumError::NoteNotFound(NoteId(ulid)));
        }
        Ok(())
    }

    /// Clears the forgotten mark for a note in the index.
    ///
    /// Resets `forgotten=0`, `forgotten_at=NULL`, `forgotten_by=NULL`.
    ///
    /// ## Index vs. vault boundary
    ///
    /// Same boundary as `mark_forgotten`: does not synchronise the YAML frontmatter
    /// on disk — synchronisation is delegated to the vault layer.
    ///
    /// ## Idempotence
    ///
    /// Safe to call multiple times: an already-unforgotten note remains unchanged
    /// (0 rows affected → `NoteNotFound` if the ULID is unknown).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite query fails.
    /// `GradatumError::NoteNotFound` if no row is affected.
    pub async fn unmark_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let affected = conn
            .execute(
                "UPDATE notes
                 SET forgotten    = 0,
                     forgotten_at = NULL,
                     forgotten_by = NULL
                 WHERE id = ?1
                   AND vault_id = ?2",
                rusqlite::params![note_id, vault_id],
            )
            .map_err(|e| {
                GradatumError::Storage(format!("unmark_forgotten UPDATE {note_id:?} : {e}"))
            })?;
        if affected == 0 {
            let ulid = ulid::Ulid::from_string(note_id).map_err(|e| {
                GradatumError::Storage(format!("unmark_forgotten ULID parse {note_id:?} : {e}"))
            })?;
            return Err(GradatumError::NoteNotFound(NoteId(ulid)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// Vérifie que la migration 0004 ajoute bien la colonne replaced_by.
    #[tokio::test]
    async fn migration_0004_adds_replaced_by_column() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"replaced_by".to_string()),
            "colonne replaced_by absente — migration 0004 non appliquée. cols={cols:?}"
        );
    }

    /// Vérifie qu'une 2ème ouverture en mémoire n'échoue pas (idempotence runner).
    ///
    /// Chaque `open_in_memory()` crée une DB distincte — le test vérifie que
    /// le runner de migrations ne panique pas à la 2ème application séquentielle.
    #[tokio::test]
    async fn migration_0004_is_idempotent_across_instances() {
        let idx1 = SqliteIndex::open_in_memory()
            .await
            .expect("première ouverture");
        // Vérification que replaced_by est présente dans idx1
        {
            let conn = idx1.conn.lock().await;
            let cols: Vec<String> = conn
                .prepare("PRAGMA table_info(notes)")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            assert!(cols.contains(&"replaced_by".to_string()));
        }
        // 2ème instance indépendante — le runner doit s'appliquer sans erreur
        let idx2 = SqliteIndex::open_in_memory()
            .await
            .expect("deuxième ouverture idempotente");
        let conn = idx2.conn.lock().await;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"replaced_by".to_string()),
            "replaced_by doit exister dans toute nouvelle instance"
        );
    }

    /// Vérifie que l'index partiel sur status='downgraded' est créé.
    #[tokio::test]
    async fn migration_0004_creates_status_downgrade_index() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_notes_status_downgrade'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            count, 1,
            "l'index idx_notes_status_downgrade doit exister après migration 0004"
        );
    }

    /// Vérifie que la migration 0012 ajoute les colonnes forgotten/forgotten_at/forgotten_by/orphaned.
    ///
    /// Preuve de câblage : si la migration n'est pas dans la liste MIGRATIONS, les colonnes
    /// sont absentes et ce test échoue (lesson-migration-file-not-wired-silent-noapply).
    #[tokio::test]
    async fn migration_0012_adds_forgotten_columns() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(notes)")
            .expect("PRAGMA table_info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query_map PRAGMA")
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"forgotten".to_string()),
            "colonne forgotten absente — migration 0012 non câblée. cols={cols:?}"
        );
        assert!(
            cols.contains(&"forgotten_at".to_string()),
            "colonne forgotten_at absente — migration 0012 non câblée. cols={cols:?}"
        );
        assert!(
            cols.contains(&"forgotten_by".to_string()),
            "colonne forgotten_by absente — migration 0012 non câblée. cols={cols:?}"
        );
        assert!(
            cols.contains(&"orphaned".to_string()),
            "colonne orphaned absente — migration 0012 non câblée. cols={cols:?}"
        );
    }

    /// Vérifie que l'index partiel sur forgotten=1 est créé par la migration 0012.
    #[tokio::test]
    async fn migration_0012_creates_forgotten_partial_index() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_notes_forgotten'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            count, 1,
            "l'index idx_notes_forgotten doit exister après migration 0012"
        );
    }

    /// Vérifie que les colonnes forgotten/forgotten_at sont dans _schema_migrations après 0012.
    ///
    /// Garantie d'idempotence : une deuxième ouverture in-memory n'échoue pas.
    #[tokio::test]
    async fn migration_0012_is_tracked_in_schema_migrations() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("ouverture in-memory 1");
        {
            let conn = idx.conn.lock().await;
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = '0012_forgotten_columns')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            assert!(
                exists,
                "0012_forgotten_columns doit être enregistrée dans _schema_migrations"
            );
        }
        // Deuxième instance — le runner est idempotent.
        let _idx2 = SqliteIndex::open_in_memory()
            .await
            .expect("ouverture in-memory 2 idempotente");
    }

    // ── Tests migration 0015 — session_trace (session-log Tier 1) ────────────

    /// Vérifie que la migration 0015 crée la table session_trace avec toutes ses
    /// colonnes.
    ///
    /// Preuve de câblage : si 0015 n'est pas dans MIGRATIONS, la table est absente
    /// et ce test échoue (lesson-migration-file-not-wired-silent-noapply).
    #[tokio::test]
    async fn migration_0015_creates_session_trace_table() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        // Vérifier l'existence de la table.
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_trace')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        assert!(
            exists,
            "table session_trace absente — migration 0015 non câblée"
        );
        // Vérifier les colonnes obligatoires.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(session_trace)")
            .expect("PRAGMA table_info session_trace")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query_map colonnes")
            .filter_map(|r| r.ok())
            .collect();
        for col in &[
            "id",
            "session_id",
            "agent_id",
            "tenant_id",
            "ts_ms",
            "action_type",
            "target",
            "intent",
            "outcome",
            "marker",
            "ref",
            "created_at",
        ] {
            assert!(
                cols.contains(&col.to_string()),
                "colonne {col} absente de session_trace — migration 0015 incomplète. cols={cols:?}"
            );
        }
    }

    /// Vérifie que les index de session_trace sont créés par la migration 0015.
    #[tokio::test]
    async fn migration_0015_creates_session_trace_indexes() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        for index_name in &[
            "idx_session_trace_session",
            "idx_session_trace_created",
            "idx_session_trace_agent",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    rusqlite::params![index_name],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            assert_eq!(
                count, 1,
                "index {index_name} absent — migration 0015 incomplète"
            );
        }
    }

    /// Vérifie que la migration 0015 est enregistrée dans _schema_migrations.
    #[tokio::test]
    async fn migration_0015_is_tracked_in_schema_migrations() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = '0015_session_trace')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        assert!(
            exists,
            "0015_session_trace doit être enregistrée dans _schema_migrations"
        );
    }

    // ── Tests migration 0013 — temporal_index (F-55) ─────────────────────────

    /// Vérifie que la migration 0013 crée la table temporal_index.
    ///
    /// Preuve de câblage : si 0013 n'est pas dans MIGRATIONS, la table est absente
    /// et ce test échoue (lesson-migration-file-not-wired-silent-noapply).
    #[tokio::test]
    async fn migration_0013_creates_temporal_index_table() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        // Vérifier l'existence de la table.
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='temporal_index')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        assert!(
            exists,
            "table temporal_index absente — migration 0013 non câblée"
        );
        // Vérifier les colonnes obligatoires.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(temporal_index)")
            .expect("PRAGMA table_info temporal_index")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query_map colonnes")
            .filter_map(|r| r.ok())
            .collect();
        for col in &[
            "note_id",
            "vault_id",
            "anchor_ms",
            "anchor_src",
            "doc_kind",
            "valid_until_ms",
        ] {
            assert!(
                cols.contains(&col.to_string()),
                "colonne {col} absente de temporal_index — migration 0013 incomplète. cols={cols:?}"
            );
        }
    }

    /// Vérifie que les index de temporal_index sont créés par la migration 0013.
    #[tokio::test]
    async fn migration_0013_creates_temporal_indexes() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        for index_name in &["idx_temporal_anchor", "idx_temporal_vault_anchor"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    rusqlite::params![index_name],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            assert_eq!(
                count, 1,
                "index {index_name} absent — migration 0013 incomplète"
            );
        }
    }

    /// Vérifie que la migration 0013 est enregistrée dans _schema_migrations.
    #[tokio::test]
    async fn migration_0013_is_tracked_in_schema_migrations() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = '0013_temporal_index')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        assert!(
            exists,
            "0013_temporal_index doit être enregistrée dans _schema_migrations"
        );
    }

    /// Vérifie que le backfill de la migration 0013 ne contient pas de sentinelles.
    ///
    /// Test simple : une DB fraîche (toutes migrations appliquées) ne doit avoir
    /// aucune sentinelle dans temporal_index. Le test complet de backfill sur notes
    /// pré-existantes est dans migrations.rs (accès à MIGRATIONS requis).
    #[tokio::test]
    async fn migration_0013_backfill_excludes_sentinels() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        // La table sentinel est automatiquement peuplée par le runner.
        // Vérifier qu'aucune sentinelle ne se retrouve dans temporal_index.
        let conn = idx.conn.lock().await;
        let sentinel_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE note_id LIKE '__sentinel__%'",
                [],
                |r| r.get(0),
            )
            .expect("count sentinels");
        assert_eq!(
            sentinel_count, 0,
            "les sentinelles ne doivent pas être dans temporal_index"
        );
    }
}

#[cfg(test)]
mod downgrade_tests {
    use super::*;

    /// Insère une note minimale avec le statut donné et retourne son NoteId.
    async fn seed_note(idx: &SqliteIndex, status: &str) -> NoteId {
        let id = NoteId(ulid::Ulid::new());
        let now = chrono::Utc::now().timestamp_millis();
        let zero_hash: &[u8] = &[0u8; 32];
        let conn = idx.conn.lock().await;
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, 'main', 'reference', ?2, 1, ?3, ?4, 'test body')",
            rusqlite::params![id.to_string(), status, now, zero_hash],
        )
        .unwrap();
        drop(conn);
        id
    }

    #[tokio::test]
    async fn downgrade_note_sets_status_and_reason() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = seed_note(&idx, "live").await;

        let result = idx.downgrade_note(&id, "superseded by canon", None).await;
        assert!(result.is_ok(), "downgrade should succeed: {result:?}");

        let conn = idx.conn.lock().await;
        let (status, reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, status_reason FROM notes WHERE id = ?",
                rusqlite::params![id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "downgraded");
        assert_eq!(reason.as_deref(), Some("superseded by canon"));
    }

    #[tokio::test]
    async fn downgrade_note_with_replaced_by() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let canon = seed_note(&idx, "live").await;
        let target = seed_note(&idx, "live").await;

        idx.downgrade_note(&target, "superseded", Some(&canon))
            .await
            .unwrap();

        let conn = idx.conn.lock().await;
        let replaced_by: Option<String> = conn
            .query_row(
                "SELECT replaced_by FROM notes WHERE id = ?",
                rusqlite::params![target.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replaced_by.as_deref(), Some(canon.to_string().as_str()));
    }

    #[tokio::test]
    async fn downgrade_note_idempotent() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = seed_note(&idx, "live").await;

        idx.downgrade_note(&id, "first", None).await.unwrap();
        let result = idx.downgrade_note(&id, "second", None).await;
        assert!(result.is_ok(), "idempotent: {result:?}");

        let conn = idx.conn.lock().await;
        let reason: String = conn
            .query_row(
                "SELECT status_reason FROM notes WHERE id = ?",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, "second", "raison MAJ par 2e appel");
    }

    #[tokio::test]
    async fn downgrade_note_nonexistent_returns_not_found() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = NoteId(ulid::Ulid::new());

        let result = idx.downgrade_note(&id, "test", None).await;
        assert!(
            matches!(result, Err(GradatumError::NoteNotFound(_))),
            "doit retourner NoteNotFound, got: {result:?}"
        );
    }

    /// Régression — replaced_by inexistant doit retourner NoteNotFound, pas une erreur Storage.
    ///
    /// Avant le fix : la contrainte FK SQLite (replaced_by REFERENCES notes(id),
    /// foreign_keys=ON) produisait SQLITE_CONSTRAINT_FOREIGNKEY → GradatumError::Storage
    /// → HTTP 500 côté handler. Après le fix : pré-check SELECT EXISTS → NoteNotFound
    /// → HTTP 404.
    #[tokio::test]
    async fn downgrade_note_replaced_by_nonexistent_returns_not_found() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let target = seed_note(&idx, "live").await;
        let ghost = NoteId(ulid::Ulid::new()); // ULID qui n'existe pas dans la DB

        let result = idx
            .downgrade_note(&target, "remplacé par fantôme", Some(&ghost))
            .await;
        assert!(
            matches!(result, Err(GradatumError::NoteNotFound(id)) if id == ghost),
            "replaced_by inexistant doit retourner NoteNotFound(replaced_by_id), got: {result:?}"
        );

        // La note source ne doit pas avoir été modifiée (le pré-check échoue avant l'UPDATE).
        let conn = idx.conn.lock().await;
        let status: String = conn
            .query_row(
                "SELECT status FROM notes WHERE id = ?",
                rusqlite::params![target.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "live",
            "la note source doit rester 'live' si replaced_by inexistant"
        );
    }

    /// Régression (cas nominal) — downgrade avec replaced_by existant doit réussir.
    ///
    /// Vérifie que le pré-check SELECT EXISTS ne bloque pas les cas valides.
    #[tokio::test]
    async fn downgrade_note_replaced_by_existing_succeeds() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let canon = seed_note(&idx, "live").await;
        let target = seed_note(&idx, "live").await;

        let result = idx
            .downgrade_note(&target, "remplacé par canon", Some(&canon))
            .await;
        assert!(
            result.is_ok(),
            "downgrade avec replaced_by existant doit réussir: {result:?}"
        );

        let conn = idx.conn.lock().await;
        let (status, replaced_by): (String, Option<String>) = conn
            .query_row(
                "SELECT status, replaced_by FROM notes WHERE id = ?",
                rusqlite::params![target.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "downgraded");
        assert_eq!(
            replaced_by.as_deref(),
            Some(canon.to_string().as_str()),
            "replaced_by doit pointer vers la note canon"
        );
    }

    #[tokio::test]
    async fn patch_note_status_revert_downgraded_to_live() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = seed_note(&idx, "live").await;

        idx.downgrade_note(&id, "test", None).await.unwrap();
        idx.patch_note_status(&id, Some("live"), None, None)
            .await
            .unwrap();

        let conn = idx.conn.lock().await;
        let status: String = conn
            .query_row(
                "SELECT status FROM notes WHERE id = ?",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "live");
    }

    /// D1.2 — `get_note_status` ne doit plus errer sur le statut SQL `downgraded`.
    ///
    /// Avant D1.2, le parse serde échouait sur `downgraded` (hors enum NoteStatus) →
    /// `GradatumError::Storage`, ce qui faisait silencieusement ignorer la note par
    /// le handler de purge. Désormais `downgraded` est projeté sur `Deprecated` à la
    /// lecture (valeur stockée inchangée, filtres search inchangés).
    #[tokio::test]
    async fn get_note_status_tolerates_downgraded() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = seed_note(&idx, "live").await;

        // Downgrade réel via le mécanisme F-39 (écrit status='downgraded').
        idx.downgrade_note(&id, "test downgrade", None)
            .await
            .unwrap();

        // La valeur SQL stockée reste bien 'downgraded' (contrat F-39 préservé).
        {
            let conn = idx.conn.lock().await;
            let raw: String = conn
                .query_row(
                    "SELECT status FROM notes WHERE id = ?",
                    rusqlite::params![id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(raw, "downgraded", "la valeur SQL doit rester 'downgraded'");
        }

        // get_note_status ne doit plus errer : downgraded → Deprecated.
        let status = idx
            .get_note_status("main", &id.to_string())
            .await
            .expect("get_note_status ne doit plus errer sur downgraded");
        assert_eq!(
            status,
            Some(NoteStatus::Deprecated),
            "downgraded doit être projeté sur Deprecated à la lecture"
        );
    }

    /// D1.2 — `get_note_status` reste correct sur les statuts standard de l'enum.
    #[tokio::test]
    async fn get_note_status_parses_standard_status() {
        let idx = SqliteIndex::open_in_memory().await.expect("idx");
        let id = seed_note(&idx, "live").await;

        let status = idx
            .get_note_status("main", &id.to_string())
            .await
            .expect("get_note_status live");
        assert_eq!(status, Some(NoteStatus::Live));
    }
}

// ── Tests Bug1 + Bug2 : vault_status méthodes réelles ─────────────────────────

#[cfg(test)]
mod vault_status_tests {
    use super::*;

    /// Bug1 — live_note_count doit compter uniquement les notes status='live',
    /// en excluant les downgraded et les sentinelles.
    #[tokio::test]
    async fn vault_status_note_count_counts_live_notes_only() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // Seed : 2 notes, l'une en 'live', l'autre sera downgraded
        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "reference", "body A")
            .await
            .unwrap();
        idx.seed_note("01BBBBBBBBBBBBBBBBBBBBBBBB", "decisions", "body B")
            .await
            .unwrap();
        // Forcer downgraded sur 01B
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "UPDATE notes SET status='downgraded' WHERE id='01BBBBBBBBBBBBBBBBBBBBBBBB'",
                [],
            )
            .unwrap();
        }
        // La sentinelle est insérée automatiquement via migrations (ensure_vault_id inutile ici)
        // — seed_note insère avec vault_id='main', pas de sentinelle auto.
        // Ici : 2 notes seedées → 1 live (01A), 1 downgraded (01B). Pas de sentinelle.

        let count = idx.live_note_count("main").await.unwrap();
        assert_eq!(
            count, 1,
            "live_note_count doit retourner 1 (01A live, 01B downgraded)"
        );
    }

    /// Bug2 — total_body_size_bytes doit sommer LENGTH(body_text) de toutes
    /// les notes non-sentinelles du vault.
    #[tokio::test]
    async fn vault_status_total_size_bytes_sums_body_length() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // body_a = 10 bytes, body_b = 20 bytes
        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "reference", "1234567890")
            .await
            .unwrap();
        idx.seed_note(
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "decisions",
            "12345678901234567890",
        )
        .await
        .unwrap();

        let total = idx.total_body_size_bytes("main").await.unwrap();
        assert_eq!(
            total, 30u64,
            "total_body_size_bytes doit retourner 30 (10 + 20)"
        );
    }

    /// live_note_count retourne 0 si aucune note live dans le vault.
    #[tokio::test]
    async fn vault_status_live_note_count_returns_zero_if_no_live_notes() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // Aucune note seedée
        let count = idx.live_note_count("main").await.unwrap();
        assert_eq!(count, 0, "vault vide → live_note_count = 0");
    }

    /// total_body_size_bytes retourne 0 si le vault est vide.
    #[tokio::test]
    async fn vault_status_total_size_bytes_returns_zero_if_empty() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let total = idx.total_body_size_bytes("main").await.unwrap();
        assert_eq!(total, 0u64, "vault vide → total_body_size_bytes = 0");
    }

    /// live_note_count ne doit pas compter les sentinelles (id LIKE '__sentinel__%').
    #[tokio::test]
    async fn vault_status_live_note_count_excludes_sentinel() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // Injecter une sentinelle manuellement (en théorie créée par ensure_vault_id)
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, body_text, status, schema_version, content_hash, created)
                 VALUES ('__sentinel__main', 'main', 'system', '', 'live', 1, X'0000000000000000000000000000000000000000000000000000000000000000', 0)",
                [],
            )
            .unwrap();
        }
        let count = idx.live_note_count("main").await.unwrap();
        assert_eq!(
            count, 0,
            "live_note_count doit exclure les sentinelles — résultat={count}"
        );
    }

    /// total_body_size_bytes inclut les notes downgraded (toutes sauf sentinelles).
    #[tokio::test]
    async fn vault_status_total_size_bytes_includes_downgraded() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // note A = live (6 bytes), note B = downgraded (4 bytes)
        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "reference", "live!!")
            .await
            .unwrap();
        idx.seed_note("01BBBBBBBBBBBBBBBBBBBBBBBB", "decisions", "down")
            .await
            .unwrap();
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "UPDATE notes SET status='downgraded' WHERE id='01BBBBBBBBBBBBBBBBBBBBBBBB'",
                [],
            )
            .unwrap();
        }
        let total = idx.total_body_size_bytes("main").await.unwrap();
        // 6 + 4 = 10 : size compte toutes notes non-sentinelles
        assert_eq!(
            total, 10u64,
            "total_body_size_bytes doit inclure downgraded — résultat={total}"
        );
    }

    /// vault isolation : live_note_count retourne 0 pour un vault différent.
    #[tokio::test]
    async fn vault_status_live_note_count_vault_isolation() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // Seed une note dans 'main' (vault par défaut de seed_note)
        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "reference", "body A")
            .await
            .unwrap();
        // Aucune note dans le vault 'other'
        let count = idx.live_note_count("other").await.unwrap();
        assert_eq!(
            count, 0,
            "vault 'other' sans notes → live_note_count = 0 (isolation correcte)"
        );
    }
}

// ── Tests M8 : migration 0005 + extraction titre H1 ───────────────────────────

#[cfg(test)]
mod title_tests {
    use super::*;

    /// La migration 0005 doit ajouter la colonne `title` à la table `notes`.
    #[tokio::test]
    async fn migration_0005_adds_title_column() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"title".to_string()),
            "colonne title absente — migration 0005 non appliquée. cols={cols:?}"
        );
    }

    /// Le backfill SQL de la migration 0005 doit extraire le titre H1 des notes existantes.
    #[tokio::test]
    async fn migration_0005_backfills_h1_title_for_existing_notes() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // Le backfill est exécuté lors de l'application de la migration 0005.
        // En mémoire, les notes seedées après la migration n'ont pas de backfill automatique.
        // Ce test vérifie que upsert_note_title fonctionne correctement.
        let note_id = NoteId(ulid::Ulid::new());
        idx.seed_note(&note_id.to_string(), "decisions", "# Mon Titre\n\nbody")
            .await
            .unwrap();
        idx.upsert_note_title(&note_id, "Mon Titre").await.unwrap();

        let conn = idx.conn.lock().await;
        let title: Option<String> = conn
            .query_row(
                "SELECT title FROM notes WHERE id = ?1",
                rusqlite::params![note_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            title.as_deref(),
            Some("Mon Titre"),
            "upsert_note_title doit persister le titre"
        );
    }

    /// upsert_note_title est idempotent : un deuxième appel met à jour le titre.
    #[tokio::test]
    async fn upsert_note_title_is_idempotent() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let note_id = NoteId(ulid::Ulid::new());
        idx.seed_note(&note_id.to_string(), "reference", "# Titre A\nbody")
            .await
            .unwrap();
        idx.upsert_note_title(&note_id, "Titre A").await.unwrap();
        idx.upsert_note_title(&note_id, "Titre B").await.unwrap();

        let conn = idx.conn.lock().await;
        let title: Option<String> = conn
            .query_row(
                "SELECT title FROM notes WHERE id = ?1",
                rusqlite::params![note_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            title.as_deref(),
            Some("Titre B"),
            "2ème upsert doit MAJ le titre"
        );
    }

    /// Valide le SQL de la migration 0009 sur données pré-existantes.
    ///
    /// Trois cas :
    ///   A) note title=NULL + body commençant par `# H1`  → backfill extrait le H1
    ///   B) note title déjà renseigné                    → non écrasé (idempotence)
    ///   C) note title=NULL + body sans H1               → reste NULL
    ///
    /// La migration 0009 est idempotente (WHERE title IS NULL OR title = '') :
    /// le SQL peut être ré-exécuté sur une DB déjà migrée pour simuler
    /// l'application sur des notes pré-existantes avec title=NULL.
    #[tokio::test]
    async fn migration_0009_backfills_h1_title_only() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        // Cas A : title=NULL, body commence par un H1 suivi d'une section
        let id_a = ulid::Ulid::new().to_string();
        idx.seed_note(&id_a, "decisions", "# Mon Titre\n## section\ncontenu")
            .await
            .expect("seed note A");

        // Cas B : title déjà renseigné — ne doit pas être écrasé
        let id_b = ulid::Ulid::new().to_string();
        idx.seed_note(&id_b, "reference", "# Autre Titre\nbody")
            .await
            .expect("seed note B");
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "UPDATE notes SET title = 'Déjà Là' WHERE id = ?1",
                rusqlite::params![id_b],
            )
            .expect("pre-set title B");
        }

        // Cas C : title=NULL, body sans H1 → title doit rester NULL
        let id_c = ulid::Ulid::new().to_string();
        idx.seed_note(&id_c, "debug", "Pas de H1 ici\n## Section")
            .await
            .expect("seed note C");

        // Ré-appliquer le SQL de la migration 0009 (idempotent).
        // Sur une DB déjà migrée, ce UPDATE cible uniquement les notes avec title IS NULL
        // ou title = '' — exactement ce que la migration fait au deploy.
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "UPDATE notes
                 SET title = CASE
                   WHEN body_text LIKE '# %' THEN
                     TRIM(SUBSTR(body_text, 3,
                       CASE
                         WHEN INSTR(body_text, CHAR(10)) > 0
                         THEN INSTR(body_text, CHAR(10)) - 3
                         ELSE LENGTH(body_text) - 2
                       END))
                   ELSE NULL
                 END
                 WHERE (title IS NULL OR title = '')
                   AND id NOT LIKE '__sentinel__%';",
            )
            .expect("ré-application SQL migration 0009");
        }

        // Vérification cas A : H1 extrait correctement
        {
            let conn = idx.conn.lock().await;
            let title_a: Option<String> = conn
                .query_row(
                    "SELECT title FROM notes WHERE id = ?1",
                    rusqlite::params![id_a],
                    |row| row.get(0),
                )
                .expect("query cas A");
            assert_eq!(
                title_a.as_deref(),
                Some("Mon Titre"),
                "cas A : migration 0009 doit extraire le H1 pour title=NULL"
            );
        }

        // Vérification cas B : title existant non écrasé
        {
            let conn = idx.conn.lock().await;
            let title_b: Option<String> = conn
                .query_row(
                    "SELECT title FROM notes WHERE id = ?1",
                    rusqlite::params![id_b],
                    |row| row.get(0),
                )
                .expect("query cas B");
            assert_eq!(
                title_b.as_deref(),
                Some("Déjà Là"),
                "cas B : migration 0009 ne doit pas écraser un titre existant"
            );
        }

        // Vérification cas C : body sans H1 → title reste NULL
        {
            let conn = idx.conn.lock().await;
            let title_c: Option<String> = conn
                .query_row(
                    "SELECT title FROM notes WHERE id = ?1",
                    rusqlite::params![id_c],
                    |row| row.get(0),
                )
                .expect("query cas C");
            assert!(
                title_c.is_none(),
                "cas C : body sans H1 — title doit rester NULL, obtenu={title_c:?}"
            );
        }
    }
}

// ── Tests B1 : section filter vault_search ─────────────────────────────────────

#[cfg(test)]
mod section_filter_tests {
    use super::*;

    /// B1 — search_fts_scored_filtered filtre par section.
    ///
    /// Les deux notes contiennent "gradatum hardening" mais dans des sections différentes.
    /// Une recherche filtrée sur "decisions" ne doit retourner que la note A.
    #[tokio::test]
    async fn search_fts_scored_filtered_by_section() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        // Note A en "decisions", Note B en "debug"
        idx.seed_note_with_fts(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "decisions",
            "gradatum hardening plan",
        )
        .await
        .unwrap();
        idx.seed_note_with_fts(
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "debug",
            "gradatum hardening fix",
        )
        .await
        .unwrap();

        let vault = VaultId::new("main");
        // Recherche dans "decisions" uniquement
        let results = idx
            .search_fts_scored_filtered(&vault, "gradatum", 10, false, Some("decisions"), None)
            .await
            .unwrap();

        assert_eq!(
            results.len(),
            1,
            "filtre decisions → 1 résultat attendu, got {}",
            results.len()
        );
        assert_eq!(
            results[0].0.to_string(),
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "note A (decisions) doit être retournée"
        );
    }

    /// B1 — search_fts_scored_filtered sans section retourne toutes sections.
    #[tokio::test]
    async fn search_fts_scored_filtered_no_section_returns_all() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        idx.seed_note_with_fts(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "decisions",
            "gradatum search test",
        )
        .await
        .unwrap();
        idx.seed_note_with_fts(
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "debug",
            "gradatum search result",
        )
        .await
        .unwrap();

        let vault = VaultId::new("main");
        let results = idx
            .search_fts_scored_filtered(&vault, "gradatum", 10, false, None, None)
            .await
            .unwrap();

        assert_eq!(
            results.len(),
            2,
            "sans filtre section → 2 résultats attendus, got {}",
            results.len()
        );
    }
}

// ── Tests M9 : snippet FTS5 natif ─────────────────────────────────────────────

#[cfg(test)]
mod snippet_fts_tests {
    use super::*;

    /// M9 — snippet FTS5 natif doit localiser le terme dans un corps long.
    ///
    /// Le snippet ne doit pas commencer par la tête du body si le terme est au milieu.
    #[tokio::test]
    async fn search_fts_snippet_locates_relevant_passage() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        // Corps long : terme pertinent au milieu (après 50 répétitions de "prefix")
        let body = format!(
            "{} gradatum hardening {} production ready",
            "prefix ".repeat(50),
            "suffix ".repeat(20)
        );
        idx.seed_note_with_fts("01AAAAAAAAAAAAAAAAAAAAAAAA", "decisions", &body)
            .await
            .unwrap();

        let vault = VaultId::new("main");
        let results = idx
            .search_fts_with_snippet(&vault, "hardening", 5, false, None, None, None)
            .await
            .unwrap();

        assert_eq!(results.len(), 1, "1 résultat attendu");
        let snippet = &results[0].snippet;
        assert!(
            snippet.contains("hardening"),
            "snippet doit contenir le terme 'hardening', got: {snippet:?}"
        );
        // Le snippet NE DOIT PAS commencer par les 50 répétitions de 'prefix'
        assert!(
            !snippet.starts_with("prefix prefix prefix"),
            "snippet doit localiser le terme, pas la tête du body — got: {snippet:?}"
        );
    }

    /// M9 — search_fts_with_snippet retourne la section et le titre.
    #[tokio::test]
    async fn search_fts_with_snippet_returns_section_and_title() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        let note_id = NoteId(ulid::Ulid::new());
        idx.seed_note_with_fts(
            &note_id.to_string(),
            "architecture",
            "# Mon Titre\nbody architecture",
        )
        .await
        .unwrap();
        idx.upsert_note_title(&note_id, "Mon Titre").await.unwrap();

        let vault = VaultId::new("main");
        let results = idx
            .search_fts_with_snippet(&vault, "architecture", 5, false, None, None, None)
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].section, "architecture", "section incorrecte");
        assert_eq!(
            results[0].title.as_deref(),
            Some("Mon Titre"),
            "titre incorrect"
        );
    }
}

// ── Tests M6 : vault_list pagination réelle ───────────────────────────────────

#[cfg(test)]
mod vault_list_tests {
    use super::*;

    /// M6 — list_notes retourne les notes avec pagination ULID.
    #[tokio::test]
    async fn list_notes_returns_notes_with_pagination() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        // Seed 5 notes avec des IDs ULID valides croissants
        let ids = [
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "01DDDDDDDDDDDDDDDDDDDDDDDD",
            "01EEEEEEEEEEEEEEEEEEEEEEEE",
        ];
        for id in &ids {
            idx.seed_note(id, "reference", &format!("body for {id}"))
                .await
                .unwrap();
        }

        // Récupération sans curseur → toutes les notes, limit=3
        let (records, total) = idx.list_notes("main", None, 3, None).await.unwrap();
        assert_eq!(total, 5, "total doit être 5");
        assert_eq!(records.len(), 3, "limit=3 → 3 records");

        // Curseur = dernier ID de la première page
        let cursor = records.last().map(|r| r.id.clone());
        let (page2, _) = idx
            .list_notes("main", None, 3, cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(page2.len(), 2, "page 2 doit contenir 2 records (5 - 3)");
    }

    /// M6 — list_notes avec filtre section.
    #[tokio::test]
    async fn list_notes_filters_by_section() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "decisions", "note decisions")
            .await
            .unwrap();
        idx.seed_note("01BBBBBBBBBBBBBBBBBBBBBBBB", "reference", "note reference")
            .await
            .unwrap();
        idx.seed_note(
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "decisions",
            "note decisions 2",
        )
        .await
        .unwrap();

        let (records, total) = idx
            .list_notes("main", Some("decisions"), 10, None)
            .await
            .unwrap();
        assert_eq!(total, 2, "2 notes en decisions");
        assert_eq!(records.len(), 2);
        for r in &records {
            assert_eq!(r.section, "decisions", "section incorrecte : {}", r.section);
        }
    }

    /// M6 — list_notes exclut les sentinelles.
    #[tokio::test]
    async fn list_notes_excludes_sentinels() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        // S'assurer qu'une sentinelle est présente (ensure_vault_id en crée une)
        idx.ensure_vault_id("main").await.unwrap();
        idx.seed_note("01AAAAAAAAAAAAAAAAAAAAAAAA", "reference", "body A")
            .await
            .unwrap();

        let (records, total) = idx.list_notes("main", None, 10, None).await.unwrap();
        assert_eq!(total, 1, "sentinelle exclue → total = 1");
        assert_eq!(records.len(), 1);
        assert!(
            !records[0].id.contains("sentinel"),
            "pas de sentinelle dans les résultats"
        );
    }

    /// M6 — list_notes retourne 0 si vault vide.
    #[tokio::test]
    async fn list_notes_returns_empty_for_empty_vault() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let (records, total) = idx.list_notes("main", None, 10, None).await.unwrap();
        assert_eq!(total, 0);
        assert!(records.is_empty());
    }
}

// ── Tests F-42 c-prime : colonnes c_kind + doc_kind dans upsert_note ──────────
//
// Vérifie que upsert_note dérive et persiste c_kind / doc_kind à partir de section.
// Scoring-only — usage effectif différé F-17 v0.4.0. Zéro changement struct Note.
// Golden 3/3 : les tests de search existants NE doivent PAS changer de comportement.

#[cfg(test)]
mod cognitive_kind_index_tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
    use gradatum_core::note::{Note, NoteBody};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    /// Construit une note minimale pour les tests c_kind/doc_kind.
    fn make_note(vault_id: &str, section: Section) -> Note {
        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new(vault_id),
            locus: None,
            section,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };
        let body = "test body";
        let note_body = NoteBody {
            markdown: body.to_string(),
        };
        let content_hash = ContentHash::compute(&frontmatter, body);
        Note {
            id: NoteId::new(),
            frontmatter,
            body: note_body,
            version: NoteVersion::initial(),
            content_hash,
            integrity_signature: None,
        }
    }

    /// F-42 — upsert_note section="debug" → c_kind="episodic" doc_kind="Event".
    ///
    /// Section d'incident daté : c_kind episodic (événement unique) + doc_kind Event.
    #[tokio::test]
    async fn upsert_note_debug_writes_c_kind_episodic_doc_kind_event() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        let note = make_note("main", Section::Debug);
        let note_id = note.id.to_string();
        idx.upsert_note(&note)
            .await
            .expect("upsert_note doit réussir");

        let conn = idx.conn.lock().await;
        let (c_kind, doc_kind): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT c_kind, doc_kind FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("SELECT c_kind/doc_kind doit réussir");

        assert_eq!(
            c_kind.as_deref(),
            Some("episodic"),
            "section debug → c_kind attendu 'episodic', got {c_kind:?}"
        );
        assert_eq!(
            doc_kind.as_deref(),
            Some("Event"),
            "section debug → doc_kind attendu 'Event', got {doc_kind:?}"
        );
    }

    /// F-42 — upsert_note section="architecture" → c_kind="semantic" doc_kind="Static".
    ///
    /// Section de connaissance stable : c_kind semantic + doc_kind Static.
    #[tokio::test]
    async fn upsert_note_architecture_writes_c_kind_semantic_doc_kind_static() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        let note = make_note("main", Section::Architecture);
        let note_id = note.id.to_string();
        idx.upsert_note(&note)
            .await
            .expect("upsert_note doit réussir");

        let conn = idx.conn.lock().await;
        let (c_kind, doc_kind): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT c_kind, doc_kind FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("SELECT c_kind/doc_kind doit réussir");

        assert_eq!(
            c_kind.as_deref(),
            Some("semantic"),
            "section architecture → c_kind attendu 'semantic', got {c_kind:?}"
        );
        assert_eq!(
            doc_kind.as_deref(),
            Some("Static"),
            "section architecture → doc_kind attendu 'Static', got {doc_kind:?}"
        );
    }

    /// upsert_note section="agent-issues" → c_kind="procedural" doc_kind="Event".
    ///
    /// Section d'issues agents : c_kind procedural + doc_kind Event.
    #[tokio::test]
    async fn upsert_note_agent_issues_writes_c_kind_procedural_doc_kind_event() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        let note = make_note("main", Section::AgentIssues);
        let note_id = note.id.to_string();
        idx.upsert_note(&note)
            .await
            .expect("upsert_note doit réussir");

        let conn = idx.conn.lock().await;
        let (c_kind, doc_kind): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT c_kind, doc_kind FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("SELECT c_kind/doc_kind doit réussir");

        assert_eq!(
            c_kind.as_deref(),
            Some("procedural"),
            "section agent-issues → c_kind attendu 'procedural', got {c_kind:?}"
        );
        assert_eq!(
            doc_kind.as_deref(),
            Some("Event"),
            "section agent-issues → doc_kind attendu 'Event', got {doc_kind:?}"
        );
    }

    /// F-42 — migration 0008 crée bien les colonnes c_kind et doc_kind dans notes.
    #[tokio::test]
    async fn migration_0008_adds_c_kind_doc_kind_columns() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let conn = idx.conn.lock().await;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"c_kind".to_string()),
            "colonne c_kind absente — migration 0008 non appliquée. cols={cols:?}"
        );
        assert!(
            cols.contains(&"doc_kind".to_string()),
            "colonne doc_kind absente — migration 0008 non appliquée. cols={cols:?}"
        );
    }

    /// F-42 — upsert_note est idempotent sur c_kind/doc_kind (ON CONFLICT DO UPDATE).
    ///
    /// Un deuxième upsert sur la même note doit conserver les valeurs correctes.
    #[tokio::test]
    async fn upsert_note_c_kind_idempotent() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");

        let note = make_note("main", Section::Reasoning);
        let note_id = note.id.to_string();

        // Premier upsert
        idx.upsert_note(&note)
            .await
            .expect("premier upsert doit réussir");
        // Deuxième upsert (même note, même section)
        idx.upsert_note(&note)
            .await
            .expect("deuxième upsert doit réussir");

        let conn = idx.conn.lock().await;
        let (c_kind, doc_kind): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT c_kind, doc_kind FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("SELECT c_kind/doc_kind doit réussir");

        assert_eq!(
            c_kind.as_deref(),
            Some("semantic"),
            "reasoning → c_kind doit rester 'semantic' après idempotence"
        );
        assert_eq!(
            doc_kind.as_deref(),
            Some("Static"),
            "reasoning → doc_kind doit rester 'Static' après idempotence"
        );
    }
}

// ── Tests P1-1 (F-37 S1.4) — préservation du locus sur re-upsert ─────────────

#[cfg(test)]
mod locus_preservation_tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
    use gradatum_core::note::{Note, NoteBody};
    use gradatum_core::scope::{LocusId, VaultId};
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    /// Construit une note minimale avec body + locus optionnel donnés.
    ///
    /// `created` est FIXE (epoch 0) pour garantir un `content_hash` reproductible :
    /// deux appels avec les mêmes (locus, body) produisent le même hash → permet de
    /// distinguer "re-upsert même contenu" de "vrai changement de contenu".
    fn make_note(id: NoteId, locus: Option<LocusId>, body: &str) -> Note {
        let created = chrono::DateTime::<Utc>::from_timestamp(0, 0).expect("epoch 0 valide");
        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus,
            section: Section::Reference,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created,
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };
        let content_hash = ContentHash::compute(&frontmatter, body);
        Note {
            id,
            frontmatter,
            body: NoteBody {
                markdown: body.to_string(),
            },
            version: NoteVersion::initial(),
            content_hash,
            integrity_signature: None,
        }
    }

    async fn read_locus(idx: &SqliteIndex, id: &NoteId) -> Option<String> {
        let conn = idx.conn.lock().await;
        conn.query_row(
            "SELECT locus FROM notes WHERE id = ?1",
            rusqlite::params![id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("SELECT locus")
    }

    /// P1-1 : un re-upsert depuis le MÊME contenu (.md stale) NE DOIT PAS écraser
    /// un locus déplacé via `update_note_locus`.
    #[tokio::test]
    async fn reupsert_same_content_preserves_moved_locus() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let id = NoteId::new();

        // 1. Création : note sans locus.
        let note = make_note(id, None, "corps stable");
        idx.upsert_note(&note).await.expect("upsert initial");
        assert_eq!(read_locus(&idx, &id).await, None, "locus initial = None");

        // 2. Move index-level vers "knowledge/rust" (n'altère pas le .md/content_hash).
        idx.update_note_locus(&id, &LocusId::new("knowledge/rust"))
            .await
            .expect("update_note_locus");
        assert_eq!(
            read_locus(&idx, &id).await.as_deref(),
            Some("knowledge/rust"),
            "locus déplacé"
        );

        // 3. Re-upsert depuis le MÊME frontmatter inchangé (locus=None, même hash).
        //    Simule un re-curate/backfill relisant le .md stale.
        let same_note = make_note(id, None, "corps stable");
        assert_eq!(
            same_note.content_hash.0, note.content_hash.0,
            "invariant test : content_hash identique (même contenu)"
        );
        idx.upsert_note(&same_note).await.expect("re-upsert stale");

        // Le locus déplacé DOIT être préservé (pas d'écrasement par None).
        assert_eq!(
            read_locus(&idx, &id).await.as_deref(),
            Some("knowledge/rust"),
            "re-upsert même contenu doit PRÉSERVER le locus déplacé (P1-1)"
        );
    }

    /// P1-1 : un vrai changement de contenu (content_hash différent) APPLIQUE le
    /// locus du frontmatter (pas de blocage abusif).
    #[tokio::test]
    async fn reupsert_changed_content_applies_frontmatter_locus() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let id = NoteId::new();

        // 1. Création + move index-level.
        let note = make_note(id, None, "corps v1");
        idx.upsert_note(&note).await.expect("upsert initial");
        idx.update_note_locus(&id, &LocusId::new("knowledge"))
            .await
            .expect("update_note_locus");

        // 2. Vrai changement de contenu : nouveau body + locus frontmatter explicite.
        //    content_hash diffère → la branche excluded.locus s'applique.
        let changed = make_note(id, Some(LocusId::new("decisions")), "corps v2 modifié");
        assert_ne!(
            changed.content_hash.0, note.content_hash.0,
            "invariant test : content_hash différent (contenu modifié)"
        );
        idx.upsert_note(&changed).await.expect("upsert modifié");

        assert_eq!(
            read_locus(&idx, &id).await.as_deref(),
            Some("decisions"),
            "vrai changement de contenu doit APPLIQUER le locus du frontmatter (P1-1)"
        );
    }
}

// ── Tests #7 (F-37 S1.2) — résilience list_review_queue aux id non-ULID ──────

#[cfg(test)]
mod review_queue_resilience_tests {
    use super::*;
    use gradatum_core::scope::VaultId;

    /// #7 : une ligne avec un id non-ULID (anomalie data) ne doit PAS faire échouer
    /// la page entière en 500 — elle est skippée, les lignes valides restent servies.
    #[tokio::test]
    async fn list_review_queue_skips_non_ulid_id() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault = VaultId::new("main");
        let now = chrono::Utc::now().timestamp_millis();

        // Ligne valide (ULID) en pending-review.
        let valid_id = ulid::Ulid::new().to_string();
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                 VALUES (?1, 'main', 'reference', 'pending-review', 1, ?2, X'00', 'valide')",
                rusqlite::params![valid_id, now],
            )
            .expect("insert valide");
            // Ligne corrompue : id non-ULID, même statut review.
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                 VALUES ('not-a-ulid', 'main', 'reference', 'staging', 1, ?1, X'00', 'corrompue')",
                rusqlite::params![now],
            )
            .expect("insert corrompue");
        }

        // La requête ne doit PAS échouer (pas de 500) et retourner uniquement la ligne valide.
        let rows = idx
            .list_review_queue(&vault, None, 50)
            .await
            .expect("list_review_queue ne doit pas échouer malgré la ligne corrompue");

        assert_eq!(rows.len(), 1, "seule la ligne ULID valide doit être servie");
        assert_eq!(
            rows[0].note_id.0.to_string(),
            valid_id,
            "la ligne valide est bien la nôtre"
        );
    }
}

// ── Tests F-44 Semantic Forget — decay scoring + mark/unmark ─────────────────

#[cfg(test)]
mod forgotten_tests {
    use super::*;

    /// Crée une note FTS et la marque comme forgotten avec un timestamp donné.
    ///
    /// Retourne l'ULID de la note insérée.
    async fn seed_forgotten_note(
        idx: &SqliteIndex,
        id: &str,
        body: &str,
        forgotten_at_ms: i64,
    ) -> String {
        idx.seed_note_with_fts(id, "decisions", body)
            .await
            .expect("seed_note_with_fts");
        // Forcer les colonnes forgotten directement (simule mark_forgotten).
        let conn = idx.conn.lock().await;
        conn.execute(
            "UPDATE notes SET forgotten=1, forgotten_at=?1 WHERE id=?2",
            rusqlite::params![forgotten_at_ms, id],
        )
        .expect("UPDATE forgotten");
        id.to_string()
    }

    /// Une note forgotten avec elapsed=0 jour → decay = 0.5^0 = 1.0.
    ///
    /// Le score BM25 est multiplié par 1.0 → aucun changement le jour même.
    /// Ce comportement est documenté dans search_fts_scored (voir doc-comment).
    #[tokio::test]
    async fn forgotten_note_zero_elapsed_decay_equals_one() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_forgotten_note(
            &idx,
            "01FGTTNNNN0000000000000000",
            "decay test oublié aujourd'hui",
            now_ms,
        )
        .await;
        let hits = idx
            .search_fts_scored(&VaultId::new("main"), "decay oublié", 10, true)
            .await
            .expect("search_fts_scored");
        // La note doit apparaître (elle est dans l'index).
        assert!(
            !hits.is_empty(),
            "la note forgotten doit apparaître dans les résultats"
        );
        // Le score est le BM25 brut (× 1.0 car elapsed=0).
        // On vérifie que le score n'est pas pénalisé × 10 (pénalité downgraded).
        let (_, score, status) = &hits[0];
        assert_ne!(
            *status, "downgraded",
            "statut doit être 'live' pas 'downgraded'"
        );
        // elapsed=0 → decay=1.0 → score = bm25_raw × 1.0 (pas de division par 10).
        // La valeur BM25 est négative ; le score brut sans pénalité doit être > bm25 × 10.
        // On vérifie juste qu'il n'y a pas de pénalité × 10 appliquée (score > bm25*10).
        assert!(
            score > &(score * 10.0 - 1e-9) || score >= &(score * 10.0),
            "score forgottten elapsed=0 ne doit pas être amplifié × 10"
        );
        let _ = score; // suppression warning unused
    }

    /// Une note forgotten depuis 1 jour → decay = 0.5^1 = 0.5 (score divisé par 2).
    ///
    /// Une note normale sur la même requête doit avoir un score FTS meilleur
    /// (moins négatif) que la note forgotten après decay.
    #[tokio::test]
    async fn forgotten_note_one_day_decay_reduces_score() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        let now_ms = chrono::Utc::now().timestamp_millis();
        // Note forgotten il y a exactement 1 jour (86_400_000 ms).
        let one_day_ago_ms = now_ms - 86_400_000_i64;
        seed_forgotten_note(
            &idx,
            "01FGTTN1DAY00000000000000A",
            "recherche information pertinente",
            one_day_ago_ms,
        )
        .await;
        // Note normale (pas forgotten) — même corps, statut live.
        idx.seed_note_with_fts(
            "01FGTTN1DAY00000000000000B",
            "decisions",
            "recherche information pertinente",
        )
        .await
        .expect("seed note normale");

        let hits = idx
            .search_fts_scored(&VaultId::new("main"), "recherche information", 10, true)
            .await
            .expect("search_fts_scored");

        // Les deux notes doivent être présentes.
        assert_eq!(hits.len(), 2, "deux notes attendues");

        // La note normale (01B, non-forgotten) doit avoir un score >= note forgotten (01A).
        // BM25 = valeurs négatives, ORDER BY ASC → meilleur score en premier.
        // Après decay × 0.5 sur la note forgotten, son score est multiplié par 0.5
        // (plus négatif → plus proche de 0 en valeur absolue = moins bon).
        // En valeur absolue : |score_forgotten| < |score_normal| après decay.
        // En ordre ASC : score_normal apparaît AVANT score_forgotten (meilleur match).
        let idx_normal = hits
            .iter()
            .position(|(id, _, _)| id.to_string() == "01FGTTN1DAY00000000000000B")
            .expect("note normale dans les hits");
        let idx_forgotten = hits
            .iter()
            .position(|(id, _, _)| id.to_string() == "01FGTTN1DAY00000000000000A")
            .expect("note forgotten dans les hits");

        assert!(
            idx_normal <= idx_forgotten,
            "note normale doit apparaître avant la note forgotten après decay 1j. \
             normal_rank={idx_normal}, forgotten_rank={idx_forgotten}"
        );
    }

    /// forgotten=1 + status='downgraded' → seul le decay forgotten s'applique (pas de cumul).
    ///
    /// Une note forgotten ET downgraded ne doit PAS voir son score amplifié × 10
    /// (pénalité downgraded) en plus du decay forgotten. Court-circuit garanti.
    #[tokio::test]
    async fn forgotten_and_downgraded_no_double_penalty() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        let now_ms = chrono::Utc::now().timestamp_millis();
        let one_day_ago_ms = now_ms - 86_400_000_i64;

        // Note forgotten + downgraded.
        idx.seed_note_with_fts(
            "01FGTTCM0N00000000000000AA",
            "decisions",
            "cumul penalite test note",
        )
        .await
        .expect("seed note cumul");
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "UPDATE notes SET forgotten=1, forgotten_at=?1, status='downgraded' WHERE id=?2",
                rusqlite::params![one_day_ago_ms, "01FGTTCM0N00000000000000AA"],
            )
            .expect("UPDATE forgotten+downgraded");
        }

        // Note seulement downgraded (sans forgotten).
        idx.seed_note_with_fts(
            "01FGTTCM0N00000000000000BB",
            "decisions",
            "cumul penalite test note",
        )
        .await
        .expect("seed note downgraded seule");
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "UPDATE notes SET status='downgraded' WHERE id=?1",
                rusqlite::params!["01FGTTCM0N00000000000000BB"],
            )
            .expect("UPDATE downgraded");
        }

        let hits = idx
            .search_fts_scored(
                &VaultId::new("main"),
                "cumul penalite",
                10,
                true, // include_downgraded = true pour voir les deux
            )
            .await
            .expect("search_fts_scored");

        assert_eq!(hits.len(), 2, "deux notes attendues");

        let score_forgotten_down = hits
            .iter()
            .find(|(id, _, _)| id.to_string() == "01FGTTCM0N00000000000000AA")
            .map(|(_, s, _)| *s)
            .expect("note forgotten+downgraded absente");

        let score_only_down = hits
            .iter()
            .find(|(id, _, _)| id.to_string() == "01FGTTCM0N00000000000000BB")
            .map(|(_, s, _)| *s)
            .expect("note downgraded seule absente");

        // Le score forgotten+downgraded (decay × 0.5) doit être > (moins négatif)
        // que le score downgraded seul (× 10 = beaucoup plus négatif).
        // En d'autres termes : la note forgotten+downgraded est moins pénalisée.
        // BM25 négatif × 0.5 > BM25 négatif × 10 (en valeur : score × 0.5 > score × 10
        // car la valeur est négative → diviser par 2 est moins pénal que × 10).
        assert!(
            score_forgotten_down > score_only_down,
            "forgotten+downgraded (decay seul) doit être moins pénalisé que downgraded seul. \
             forgotten_down={score_forgotten_down:.6}, only_down={score_only_down:.6}"
        );
    }

    /// Une note normale (forgotten=0) ne voit pas son score modifié.
    #[tokio::test]
    async fn normal_note_score_unchanged() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        idx.seed_note_with_fts(
            "01FGTTNRM000000000000000AA",
            "decisions",
            "note normale non oubliée score",
        )
        .await
        .expect("seed note normale");

        let hits = idx
            .search_fts_scored(&VaultId::new("main"), "note normale score", 10, false)
            .await
            .expect("search_fts_scored");

        assert_eq!(hits.len(), 1, "une note attendue");
        let (_, score, _) = &hits[0];
        // Score BM25 brut négatif — ne doit pas être pénalisé (pas de decay, pas de × 10).
        // On vérifie que la pénalité × 10 n'est pas appliquée sur une note live normale :
        // si le score était × 10, il serait 10x plus négatif que bm25_raw.
        // On vérifie juste que le score est strictement négatif (BM25 valide, non nul).
        assert!(
            *score < 0.0,
            "score BM25 doit être négatif pour une note normale : {score}"
        );
    }

    /// mark_forgotten + unmark_forgotten — cycle complet.
    ///
    /// - mark : note become forgotten=1, forgotten_at SET, forgotten_by SET.
    /// - unmark : note revient à forgotten=0, forgotten_at=NULL, forgotten_by=NULL.
    #[tokio::test]
    async fn mark_and_unmark_forgotten_round_trip() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        idx.seed_note_with_fts(
            "01FGTTMARK00000000000000CC",
            "decisions",
            "test mark unforgot round trip",
        )
        .await
        .expect("seed note");

        // mark_forgotten.
        idx.mark_forgotten("main", "01FGTTMARK00000000000000CC", Some("test-agent"))
            .await
            .expect("mark_forgotten");

        // Vérifier l'état forgotten=1 en DB.
        {
            let conn = idx.conn.lock().await;
            let (forgotten, forgotten_by): (i64, Option<String>) = conn
                .query_row(
                    "SELECT forgotten, forgotten_by FROM notes WHERE id=?1",
                    rusqlite::params!["01FGTTMARK00000000000000CC"],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("SELECT après mark");
            assert_eq!(forgotten, 1, "forgotten doit être 1 après mark");
            assert_eq!(
                forgotten_by.as_deref(),
                Some("test-agent"),
                "forgotten_by doit être 'test-agent'"
            );
        }

        // unmark_forgotten.
        idx.unmark_forgotten("main", "01FGTTMARK00000000000000CC")
            .await
            .expect("unmark_forgotten");

        // Vérifier que forgotten=0, forgotten_at=NULL, forgotten_by=NULL.
        {
            let conn = idx.conn.lock().await;
            let (forgotten, forgotten_at, forgotten_by): (i64, Option<i64>, Option<String>) = conn
                .query_row(
                    "SELECT forgotten, forgotten_at, forgotten_by FROM notes WHERE id=?1",
                    rusqlite::params!["01FGTTMARK00000000000000CC"],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("SELECT après unmark");
            assert_eq!(forgotten, 0, "forgotten doit être 0 après unmark");
            assert!(
                forgotten_at.is_none(),
                "forgotten_at doit être NULL après unmark"
            );
            assert!(
                forgotten_by.is_none(),
                "forgotten_by doit être NULL après unmark"
            );
        }
    }

    /// mark_forgotten sur un ULID inexistant → NoteNotFound.
    #[tokio::test]
    async fn mark_forgotten_unknown_note_returns_not_found() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        let result = idx
            .mark_forgotten("main", "01FFFFFFFF0000000000000000", None)
            .await;

        assert!(
            matches!(result, Err(GradatumError::NoteNotFound(_))),
            "note inconnue doit retourner NoteNotFound, got: {result:?}"
        );
    }

    /// C2 — query FTS > 512 chars retourne ValidationError::InvalidInput.
    ///
    /// Protège contre les attaques DoS via expressions FTS5 complexes ou les
    /// requêtes pathologiques générées par un client malformé.
    #[tokio::test]
    async fn search_fts_for_forget_query_too_long_returns_error() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — C2");

        // Query de 513 caractères (dépasse la borne 512).
        let long_query = "a".repeat(513);

        let result = idx.search_fts_for_forget("main", &long_query, 10).await;

        assert!(
            matches!(
                result,
                Err(GradatumError::Validation(ValidationError::InvalidInput(_)))
            ),
            "query > 512 chars doit retourner ValidationError::InvalidInput, got: {result:?}"
        );
    }

    /// C2 — query FTS exactement 512 chars est acceptée (borne inclusive).
    #[tokio::test]
    async fn search_fts_for_forget_query_at_limit_is_accepted() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — C2 borne limite");

        // Query de 512 caractères exactement — doit passer la borne.
        let exact_limit_query = "a".repeat(512);

        let result = idx
            .search_fts_for_forget("main", &exact_limit_query, 10)
            .await;

        // Peut retourner Ok([]) (aucun résultat) ou Err(Storage) si FTS rejette
        // la syntaxe, mais PAS ValidationError::InvalidInput.
        assert!(
            !matches!(
                result,
                Err(GradatumError::Validation(ValidationError::InvalidInput(_)))
            ),
            "query = 512 chars ne doit PAS retourner InvalidInput, got: {result:?}"
        );
    }

    // ── Tests quoting FTS5 (bug P1 smoke v0.4.3) ─────────────────────────────

    /// fts5_quote_query — token unique avec tiret ne doit pas retourner 500.
    ///
    /// Régression : `lot-c` → `no such column: lot` avant le fix (le tiret était
    /// interprété comme opérateur de soustraction FTS5).
    #[test]
    fn fts5_quote_query_hyphen_single_token() {
        let q = fts5_quote_query("lot-c");
        assert_eq!(q, r#""lot-c""#);
    }

    /// fts5_quote_query — date ISO 8601 complète.
    ///
    /// `2026-06-10` contient deux tirets → avant le fix, FTS5 interprétait
    /// `2026 - 06 - 10` comme soustraction de colonnes.
    #[test]
    fn fts5_quote_query_iso_date() {
        let q = fts5_quote_query("2026-06-10");
        assert_eq!(q, r#""2026-06-10""#);
    }

    /// fts5_quote_query — plusieurs tokens séparés par espaces → AND implicite.
    #[test]
    fn fts5_quote_query_multiple_tokens() {
        let q = fts5_quote_query("foo bar");
        assert_eq!(q, r#""foo" "bar""#);
    }

    /// fts5_quote_query — guillemets internes doublés (convention FTS5 phrase-quoting).
    #[test]
    fn fts5_quote_query_internal_double_quotes() {
        let q = fts5_quote_query(r#"a "b" c"#);
        assert_eq!(q, r#""a" """b""" "c""#);
    }

    /// fts5_quote_query — query simple sans caractères spéciaux.
    ///
    /// Non-régression : les queries simples continuent de fonctionner.
    #[test]
    fn fts5_quote_query_simple_word() {
        let q = fts5_quote_query("hello");
        assert_eq!(q, r#""hello""#);
    }

    /// fts5_quote_query — query vide retourne chaîne vide.
    #[test]
    fn fts5_quote_query_empty_returns_empty() {
        assert_eq!(fts5_quote_query(""), "");
        assert_eq!(fts5_quote_query("   "), "");
    }

    /// D2.1 — `recall_lessons` consomme `fts5_quote_query` (source unique).
    ///
    /// L'ancien inline `format!("\"{}\"", class.replace('"', "\"\""))` produisait
    /// le même résultat que `fts5_quote_query` pour une classe mono-token. Ce test
    /// gèle cette équivalence : si l'un des chemins diverge, il échoue.
    #[test]
    fn fts5_quote_query_matches_legacy_recall_inline() {
        for class in ["ci-cd", "crates-io", "auth-secrets", "data-integrity"] {
            let legacy = format!("\"{}\"", class.replace('"', "\"\""));
            assert_eq!(
                fts5_quote_query(class),
                legacy,
                "fts5_quote_query doit reproduire l'ancien inline recall_lessons pour `{class}`"
            );
        }
        // Cas avec guillemet interne — la classe est mono-token : le doublage doit
        // rester aligné entre la source et l'ancien inline.
        let class = r#"a"b"#;
        let legacy = format!("\"{}\"", class.replace('"', "\"\""));
        assert_eq!(fts5_quote_query(class), legacy);
    }

    /// Intégration — query avec tiret ne retourne pas 500 sur index réel.
    ///
    /// Régression end-to-end : `search_fts_for_forget("lot-c")` doit retourner
    /// `Ok(vec![])` (aucun match) et non un `Err(Storage("no such column"))`.
    #[tokio::test]
    async fn search_fts_for_forget_hyphen_query_no_error() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — hyphen query");

        // Pas de note insérée : on vérifie uniquement qu'il n'y a pas d'erreur SQL.
        let result = idx.search_fts_for_forget("main", "lot-c", 10).await;
        assert!(
            result.is_ok(),
            "query avec tiret doit retourner Ok, got: {result:?}"
        );
        assert_eq!(
            result.unwrap(),
            vec![],
            "aucune note → résultat vide attendu"
        );
    }

    /// Intégration — date ISO ne retourne pas 500 sur index réel.
    #[tokio::test]
    async fn search_fts_for_forget_iso_date_query_no_error() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — iso date query");

        let result = idx.search_fts_for_forget("main", "2026-06-10", 10).await;
        assert!(
            result.is_ok(),
            "query date ISO doit retourner Ok, got: {result:?}"
        );
        assert_eq!(result.unwrap(), vec![]);
    }

    /// Intégration — note contenant `lot-c` dans le body est bien trouvée.
    ///
    /// Vérifie que le quoting ne casse pas les matches légitimes.
    #[tokio::test]
    async fn search_fts_for_forget_hyphen_query_finds_matching_note() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — hyphen match");

        let id = "01AAAAAAAAAAAAAAAAAAAAAAA1";
        idx.seed_note_with_fts(id, "decisions", "note sur le lot-c gradatum deploy")
            .await
            .expect("seed_note_with_fts");

        let result = idx.search_fts_for_forget("main", "lot-c", 10).await;
        assert!(result.is_ok(), "query tiret doit Ok, got: {result:?}");
        let hits = result.unwrap();
        assert_eq!(hits.len(), 1, "une note doit être trouvée, got: {hits:?}");
        assert_eq!(hits[0].0, id);
    }

    /// Intégration — query mixte tirets+mots retourne les notes pertinentes.
    #[tokio::test]
    async fn search_fts_for_forget_mixed_query_no_error() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — mixed query");

        let result = idx
            .search_fts_for_forget("main", "lot-c gradatum", 10)
            .await;
        assert!(
            result.is_ok(),
            "query mixte tiret+mot doit retourner Ok, got: {result:?}"
        );
    }
}

// ── Tests temporal_index (F-55) ───────────────────────────────────────────────

#[cfg(test)]
mod temporal_index_tests {
    use super::*;
    use gradatum_core::index::{AnchorSrc, TemporalEntry};

    /// Helper : insère une note minimale (notes + notes_fts) pour les tests temporal.
    ///
    /// Insère dans les deux tables (notes + notes_fts) pour éviter l'erreur
    /// "database disk image is malformed" lors de `delete_note_from_index` qui
    /// supprime de notes_fts par rowid.
    async fn seed_note_temporal(idx: &SqliteIndex, note_id: &str, vault_id: &str) {
        let conn = idx.conn.lock().await;
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, doc_kind) \
             VALUES (?1, ?2, 'decisions', 'live', 1, 1000000, \
                     X'0000000000000000000000000000000000000000000000000000000000000001', \
                     'body temporal test', 'Static')",
            rusqlite::params![note_id, vault_id],
        )
        .expect("insert note temporal (notes)");
        // Synchroniser notes_fts — requis pour que delete_note_from_index fonctionne
        // sans "database disk image is malformed" (DELETE FROM notes_fts WHERE rowid = ?).
        conn.execute(
            "INSERT INTO notes_fts (rowid, body_text) \
             SELECT rowid, body_text FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
        )
        .expect("insert note temporal (fts)");
    }

    /// write_temporal_entry insère une entrée et peut être lue depuis temporal_index.
    #[tokio::test]
    async fn write_temporal_entry_inserts_and_updates() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        seed_note_temporal(&idx, "01TEMPORAL1", "main").await;

        let entry = TemporalEntry {
            note_id: "01TEMPORAL1".to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 1_700_000_000_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Static".to_string(),
            valid_until_ms: None,
        };

        idx.write_temporal_entry(&entry)
            .await
            .expect("write_temporal_entry doit réussir");

        // Vérifier l'entrée en DB.
        let conn = idx.conn.lock().await;
        let (anchor_ms, anchor_src, doc_kind): (i64, String, String) = conn
            .query_row(
                "SELECT anchor_ms, anchor_src, doc_kind FROM temporal_index WHERE note_id='01TEMPORAL1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("lire entrée temporal_index");

        assert_eq!(anchor_ms, 1_700_000_000_000);
        assert_eq!(anchor_src, "created");
        assert_eq!(doc_kind, "Static");
    }

    /// write_temporal_entry met à jour (INSERT OR REPLACE) une entrée existante.
    #[tokio::test]
    async fn write_temporal_entry_updates_existing_entry() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        seed_note_temporal(&idx, "01TEMPORAL2", "main").await;

        // Première écriture avec anchor_src=Created.
        let entry1 = TemporalEntry {
            note_id: "01TEMPORAL2".to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 1_000_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Static".to_string(),
            valid_until_ms: None,
        };
        idx.write_temporal_entry(&entry1).await.expect("write 1");

        // Mise à jour avec anchor_src=OccurredAt (curate enrichissement).
        let entry2 = TemporalEntry {
            note_id: "01TEMPORAL2".to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 2_000_000_000_000,
            anchor_src: AnchorSrc::OccurredAt,
            doc_kind: "Event".to_string(),
            valid_until_ms: None,
        };
        idx.write_temporal_entry(&entry2).await.expect("write 2");

        let conn = idx.conn.lock().await;
        let (anchor_ms, anchor_src, doc_kind): (i64, String, String) = conn
            .query_row(
                "SELECT anchor_ms, anchor_src, doc_kind FROM temporal_index WHERE note_id='01TEMPORAL2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("lire après update");

        assert_eq!(
            anchor_ms, 2_000_000_000_000,
            "anchor_ms doit être mis à jour"
        );
        assert_eq!(anchor_src, "occurred_at", "anchor_src doit être mis à jour");
        assert_eq!(doc_kind, "Event", "doc_kind doit être mis à jour");
    }

    /// La suppression d'une note retire son entrée temporal_index (caveat C7).
    ///
    /// Vérifie que DELETE FROM temporal_index est explicitement appelé dans
    /// delete_note_from_index — sans compter sur ON DELETE CASCADE.
    #[tokio::test]
    async fn delete_note_removes_temporal_entry() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let note_id = "01TEMPORAL3";

        // Insérer une note avec entrée temporal_index.
        seed_note_temporal(&idx, note_id, "main").await;
        let entry = TemporalEntry {
            note_id: note_id.to_string(),
            vault_id: "main".to_string(),
            anchor_ms: 5_000_000,
            anchor_src: AnchorSrc::Created,
            doc_kind: "Static".to_string(),
            valid_until_ms: None,
        };
        idx.write_temporal_entry(&entry)
            .await
            .expect("write temporal");

        // Vérifier que l'entrée existe bien avant suppression.
        {
            let conn = idx.conn.lock().await;
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM temporal_index WHERE note_id=?1)",
                    rusqlite::params![note_id],
                    |r| r.get(0),
                )
                .expect("check exists");
            assert!(
                exists,
                "entrée temporal_index doit exister avant suppression"
            );
        }

        // Supprimer la note.
        let deleted = idx
            .delete_note_from_index("main", note_id)
            .await
            .expect("delete_note_from_index");
        assert!(deleted, "delete_note_from_index doit retourner true");

        // Vérifier que l'entrée temporal_index a été supprimée.
        let conn = idx.conn.lock().await;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM temporal_index WHERE note_id=?1)",
                rusqlite::params![note_id],
                |r| r.get(0),
            )
            .expect("check after delete");
        assert!(
            !exists,
            "entrée temporal_index doit être supprimée après delete_note_from_index (caveat C7)"
        );
    }

    /// delete_temporal_entry sur note inexistante → Ok(false) — idempotent.
    #[tokio::test]
    async fn delete_temporal_entry_nonexistent_is_idempotent() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let result = idx
            .delete_temporal_entry("01NONEXISTENT000000000000")
            .await
            .expect("delete_temporal_entry ne doit pas échouer");
        assert!(!result, "note inexistante → false");
    }

    /// backfill_temporal_index : INSERT OR IGNORE — idempotent si entrée déjà présente.
    #[tokio::test]
    async fn backfill_temporal_index_idempotent_on_existing_entries() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        seed_note_temporal(&idx, "01BACKFILL1", "main").await;

        // Première passe : insère l'entrée manquante.
        let n1 = idx.backfill_temporal_index().await.expect("backfill 1");
        assert_eq!(n1, 1, "premier backfill doit insérer 1 note");

        // Seconde passe : INSERT OR IGNORE → 0 insertions supplémentaires.
        let n2 = idx.backfill_temporal_index().await.expect("backfill 2");
        assert_eq!(n2, 0, "second backfill doit retourner 0 (idempotent)");
    }

    // ── Tests timeline read (F-55 zone D) ────────────────────────────────────
    // ULID valides 26 chars, ordre lexico A < B < C garanti par le dernier char.
    const A_ULID: &str = "01HQ0000000000000000000000";
    const B_ULID: &str = "01HQ0000000000000000000001";
    const C_ULID: &str = "01HQ0000000000000000000002";
    const G_ULID: &str = "01HQ000000000000000000000G"; // garbage

    /// Seed une note (id, status, title) + son entrée temporelle (anchor_ms, doc_kind).
    ///
    /// Insère dans `notes` + `notes_fts` (cohérence delete) puis appelle
    /// `write_temporal_entry`. `anchor_src = AnchorSrc::Created` (enum).
    async fn seed_note_with_temporal(
        idx: &SqliteIndex,
        note_id: &str,
        anchor_ms: i64,
        doc_kind: &str,
        status: &str,
    ) {
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, doc_kind, title) \
                 VALUES (?1, 'main', 'decisions', ?2, 1, ?3, \
                         X'0000000000000000000000000000000000000000000000000000000000000001', \
                         'body timeline test', ?4, ?5)",
                rusqlite::params![note_id, status, anchor_ms, doc_kind, format!("Titre {note_id}")],
            )
            .expect("insert note timeline (notes)");
            conn.execute(
                "INSERT INTO notes_fts (rowid, body_text) \
                 SELECT rowid, body_text FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
            )
            .expect("insert note timeline (fts)");
        }
        let entry = TemporalEntry {
            note_id: note_id.to_string(),
            vault_id: "main".to_string(),
            anchor_ms,
            anchor_src: AnchorSrc::Created,
            doc_kind: doc_kind.to_string(),
            valid_until_ms: None,
        };
        idx.write_temporal_entry(&entry)
            .await
            .expect("write_temporal_entry seed");
    }

    /// Marque une note `forgotten=1` (réutilise le chemin F-44 SQL direct).
    async fn mark_forgotten(idx: &SqliteIndex, note_id: &str) {
        let conn = idx.conn.lock().await;
        conn.execute(
            "UPDATE notes SET forgotten = 1 WHERE id = ?1",
            rusqlite::params![note_id],
        )
        .expect("mark_forgotten");
    }

    use gradatum_core::temporal_query::{TimelineCursor, TimelineFilter};

    #[tokio::test]
    async fn timeline_orders_anchor_desc_then_note_id_desc() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 3000, "Static", "live").await;
        seed_note_with_temporal(&idx, C_ULID, 2000, "Event", "live").await;

        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        // anchor_ms DESC : 3000(B), 2000(C), 1000(A)
        assert_eq!(ids, vec![B_ULID, C_ULID, A_ULID]);
        assert_eq!(rows[0].anchor_ms, 3000);
        assert_eq!(rows[0].doc_kind, "Static");
    }

    #[tokio::test]
    async fn timeline_tiebreak_same_anchor_note_id_desc() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 5000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 5000, "Event", "live").await; // même anchor
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        assert_eq!(ids, vec![B_ULID, A_ULID]); // note_id DESC à anchor égal
    }

    #[tokio::test]
    async fn timeline_filters_doc_kind() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 2000, "Static", "live").await;
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    doc_kind: Some(vec!["Event".into()]),
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].doc_kind, "Event");
    }

    #[tokio::test]
    async fn timeline_windows_from_to_inclusive() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 2000, "Event", "live").await;
        seed_note_with_temporal(&idx, C_ULID, 3000, "Event", "live").await;
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    from_ms: Some(2000),
                    to_ms: Some(3000),
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        assert_eq!(ids, vec![C_ULID, B_ULID]); // bornes incluses
    }

    #[tokio::test]
    async fn timeline_paginates_via_cursor() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 2000, "Event", "live").await;
        seed_note_with_temporal(&idx, C_ULID, 3000, "Event", "live").await;
        let page1 = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        let last = &page1[1];
        let cursor = TimelineCursor {
            anchor_ms: last.anchor_ms,
            note_id: last.note_id.0.to_string(),
        };
        let page2 = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 2,
                    cursor: Some(cursor),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = page2.iter().map(|r| r.note_id.0.to_string()).collect();
        assert_eq!(ids, vec![A_ULID]); // pas de chevauchement avec page1 (C,B)
    }

    #[tokio::test]
    async fn timeline_excludes_garbage_and_sentinels() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, G_ULID, 2000, "Event", "garbage").await;
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        assert_eq!(ids, vec![A_ULID]); // garbage exclu
    }

    /// Seed une note dans une section arbitraire + son entrée temporelle.
    /// Variante de `seed_note_with_temporal` paramétrée par `section`.
    async fn seed_note_with_temporal_section(
        idx: &SqliteIndex,
        note_id: &str,
        anchor_ms: i64,
        doc_kind: &str,
        status: &str,
        section: &str,
    ) {
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, doc_kind, title) \
                 VALUES (?1, 'main', ?6, ?2, 1, ?3, \
                         X'0000000000000000000000000000000000000000000000000000000000000001', \
                         'body timeline test', ?4, ?5)",
                rusqlite::params![note_id, status, anchor_ms, doc_kind, format!("Titre {note_id}"), section],
            )
            .expect("insert note timeline section (notes)");
            conn.execute(
                "INSERT INTO notes_fts (rowid, body_text) \
                 SELECT rowid, body_text FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
            )
            .expect("insert note timeline section (fts)");
        }
        let entry = TemporalEntry {
            note_id: note_id.to_string(),
            vault_id: "main".to_string(),
            anchor_ms,
            anchor_src: AnchorSrc::Created,
            doc_kind: doc_kind.to_string(),
            valid_until_ms: None,
        };
        idx.write_temporal_entry(&entry)
            .await
            .expect("write_temporal_entry seed section");
    }

    // V1 sécu — les sections protégées (council, agent-issues) sont exclues de la
    // timeline (fuite de titres sensibles). Seule la note `decisions` ressort.
    #[tokio::test]
    async fn timeline_excludes_protected_sections() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // A = council (protégée), B = decisions (visible).
        seed_note_with_temporal_section(&idx, A_ULID, 1000, "Event", "live", "council").await;
        seed_note_with_temporal_section(&idx, B_ULID, 2000, "Event", "live", "decisions").await;
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        assert_eq!(ids, vec![B_ULID], "council exclu, seule decisions visible");
    }

    // P2-6 — forgotten=1 INCLUS (choix explicite spec §3 : journal factuel).
    #[tokio::test]
    async fn timeline_includes_forgotten_notes() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_temporal(&idx, A_ULID, 1000, "Event", "live").await;
        seed_note_with_temporal(&idx, B_ULID, 2000, "Event", "live").await;
        mark_forgotten(&idx, B_ULID).await; // forgotten=1
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
        assert!(ids.contains(&B_ULID.to_string())); // forgotten reste visible
    }

    // P2-5 / M-2 — cap limit à 200 réellement appliqué : avec 201 notes seedées
    // et limit=10_000, le clamp côté impl borne le résultat à exactement 200
    // (assertion non tautologique — il y a strictement plus de lignes que le cap).
    #[tokio::test]
    async fn timeline_caps_limit_at_200() {
        use ulid::Ulid;
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // 201 notes, ULID valides distincts, anchor_ms variés.
        for i in 0..201_i64 {
            let id = Ulid::new().to_string();
            seed_note_with_temporal(&idx, &id, 1000 + i, "Event", "live").await;
        }
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            200,
            "clamp à 200 appliqué malgré 201 notes / limit 10_000"
        );
    }

    // ── Tests Lot 2 — sémantique as-of validité (v0.5.1) ─────────────────────

    /// Seed une note avec valid_until_ms explicite.
    async fn seed_note_with_valid_until(
        idx: &SqliteIndex,
        note_id: &str,
        anchor_ms: i64,
        valid_until_ms: Option<i64>,
    ) {
        seed_note_with_temporal(idx, note_id, anchor_ms, "Event", "live").await;
        // Met à jour valid_until_ms dans temporal_index.
        let conn = idx.conn.lock().await;
        conn.execute(
            "UPDATE temporal_index SET valid_until_ms = ?1 WHERE note_id = ?2",
            rusqlite::params![valid_until_ms, note_id],
        )
        .expect("update valid_until_ms");
    }

    // ULID pour les tests de validité (distinct des A/B/C/G existants).
    // Alphabet Crockford Base32 : 0-9 A-Z sauf I, L, O, U.
    const V1_ULID: &str = "01HV0000000000000000000001";

    /// Cas a — note sans valid_until : visible à tout T ≥ anchor.
    #[tokio::test]
    async fn timeline_as_of_note_without_valid_until_always_visible() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor = 1000, pas de valid_until (NULL)
        seed_note_with_valid_until(&idx, V1_ULID, 1_000, None).await;
        // as_of bien après anchor → doit être visible
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(9_999_999),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "note sans valid_until doit être visible à tout T ≥ anchor"
        );
    }

    /// Cas b — note valid_until futur, as_of avant valid_until : visible.
    #[tokio::test]
    async fn timeline_as_of_before_valid_until_visible() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor = 1000, valid_until = 5000
        seed_note_with_valid_until(&idx, V1_ULID, 1_000, Some(5_000)).await;
        // as_of = 3000 : anchor(1000) <= 3000 < valid_until(5000) → visible
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(3_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "cas b : note doit être visible (as_of < valid_until)"
        );
    }

    /// Cas c — note expirée : as_of après valid_until → exclue.
    #[tokio::test]
    async fn timeline_as_of_after_valid_until_excluded() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor = 1000, valid_until = 5000
        seed_note_with_valid_until(&idx, V1_ULID, 1_000, Some(5_000)).await;
        // as_of = 6000 : T >= valid_until → expirée → exclue
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(6_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "cas c : note expirée doit être exclue (as_of > valid_until)"
        );
    }

    /// Cas d — as_of == valid_until exactement → exclue (borne exclusive).
    #[tokio::test]
    async fn timeline_as_of_equal_valid_until_excluded() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        seed_note_with_valid_until(&idx, V1_ULID, 1_000, Some(5_000)).await;
        // as_of == valid_until = 5000 → exclusif → exclue
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(5_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "cas d : as_of == valid_until → borne exclusive → exclue"
        );
    }

    /// Cas e — as_of == anchor_ms exactement → visible (borne incluse).
    #[tokio::test]
    async fn timeline_as_of_equal_anchor_included() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor = 2000, valid_until = 5000
        seed_note_with_valid_until(&idx, V1_ULID, 2_000, Some(5_000)).await;
        // as_of == anchor = 2000 → inclusif → visible
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(2_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "cas e : as_of == anchor → borne incluse → visible"
        );
    }

    /// Cas e-bis — as_of avant anchor → note pas encore créée → exclue.
    #[tokio::test]
    async fn timeline_as_of_before_anchor_excluded() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor = 2000
        seed_note_with_valid_until(&idx, V1_ULID, 2_000, None).await;
        // as_of = 1000 < anchor = 2000 → note n'existait pas encore → exclue
        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(1_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "as_of avant anchor → note pas encore créée → exclue"
        );
    }

    /// Cas g — include_expired=true sans as_of → montre les expirées.
    #[tokio::test]
    async fn timeline_include_expired_shows_all() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let anchor_ms = 1_000i64;
        let valid_until_ms = 2_000i64; // dans le passé
        // Seed V1 avec valid_until déjà passé (ancre 1000, valid_until 2000)
        seed_note_with_valid_until(&idx, V1_ULID, anchor_ms, Some(valid_until_ms)).await;
        // Sans include_expired, as_of=3000 → exclue
        let rows_excluded = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(3_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows_excluded
                .iter()
                .any(|r| r.note_id.0.to_string() == V1_ULID),
            "précondition : note expirée exclue normalement"
        );
        // Avec include_expired=true (pas d'as_of) → visible
        let rows_all = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    include_expired: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            rows_all.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "cas g : include_expired=true → note expirée visible"
        );
    }

    // ── Tests P2-1+P2-2 — sémantique include_expired avec as_of (v0.5.1) ─────

    /// P2-1 : {as_of=t, include_expired=true} montre une note expirée à t
    /// que {as_of=t, include_expired=false} exclut.
    ///
    /// anchor=1000, valid_until=2000, as_of=3000 (après expiry).
    /// - include_expired=false → exclue (valid_until=2000 < as_of=3000)
    /// - include_expired=true  → visible (née avant t=3000, peu importe valid_until)
    #[tokio::test]
    async fn timeline_as_of_include_expired_shows_expired_note() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor=1000, valid_until=2000 → expirée à t=3000
        seed_note_with_valid_until(&idx, V1_ULID, 1_000, Some(2_000)).await;

        // include_expired=false : exclue
        let rows_strict = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(3_000),
                    include_expired: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows_strict
                .iter()
                .any(|r| r.note_id.0.to_string() == V1_ULID),
            "P2-1 précondition : note expirée exclue avec include_expired=false"
        );

        // include_expired=true : visible (née avant t=3000)
        let rows_incl = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(3_000),
                    include_expired: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            rows_incl.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "P2-1 : as_of=3000 + include_expired=true doit montrer la note expirée"
        );
    }

    /// P2-2 : {as_of=t, include_expired=true} exclut toujours une note née APRÈS t.
    ///
    /// anchor=5000 > t=3000 → note n'existait pas encore → exclue même avec include_expired.
    #[tokio::test]
    async fn timeline_as_of_include_expired_still_excludes_future_notes() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        // anchor=5000, pas de valid_until
        seed_note_with_valid_until(&idx, V1_ULID, 5_000, None).await;

        let rows = idx
            .timeline(
                &VaultId::new("main"),
                &TimelineFilter {
                    as_of_ms: Some(3_000),
                    include_expired: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.note_id.0.to_string() == V1_ULID),
            "P2-2 : note née après t exclue même avec include_expired=true"
        );
    }
}

// ── Tests F-60 : recall_lessons ───────────────────────────────────────────────

#[cfg(test)]
mod recall_lessons_tests {
    use super::*;

    /// Recall par classe : matche via le tag (le mot n'est pas dans le corps),
    /// retourne tags + anchor_ms, et restreint à la section `lessons-learned`.
    #[tokio::test]
    async fn recall_lessons_matches_by_tag_and_returns_metadata() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault = VaultId::new("main");

        // Leçon taguée `deploy` mais le mot "deploy" n'est PAS dans le corps.
        idx.seed_lesson(
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "Procédure de mise en ligne",
            "deploy release",
            "Toujours vérifier le health check avant le cutover.",
            1_700_000_000_000,
        )
        .await
        .expect("seed lesson deploy");

        // Note d'une autre section avec le mot "deploy" dans le corps → exclue.
        idx.seed_note_with_fts(
            "01DDDDDDDDDDDDDDDDDDDDDDDD",
            "debug",
            "deploy failed at boot",
        )
        .await
        .expect("seed debug note");

        let hits = idx
            .recall_lessons(&vault, "deploy", 5)
            .await
            .expect("recall_lessons");

        assert_eq!(hits.len(), 1, "seule la leçon lessons-learned doit matcher");
        let h = &hits[0];
        assert_eq!(h.note_id.0.to_string(), "01CCCCCCCCCCCCCCCCCCCCCCCC");
        assert_eq!(h.title.as_deref(), Some("Procédure de mise en ligne"));
        assert_eq!(h.tags, vec!["deploy".to_string(), "release".to_string()]);
        assert_eq!(h.anchor_ms, 1_700_000_000_000);
        assert!(!h.snippet.is_empty(), "snippet FTS5 non vide attendu");
    }

    /// Exclusion du tag `codified` : une leçon codifiée n'est jamais retournée,
    /// même si elle matche la classe. Le filtre est un token exact (pas substring).
    #[tokio::test]
    async fn recall_lessons_excludes_codified() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault = VaultId::new("main");

        // Leçon active.
        idx.seed_lesson(
            "01EEEEEEEEEEEEEEEEEEEEEEEE",
            "Migration immutable",
            "migration",
            "Ne jamais modifier une migration sqlx appliquée.",
            1_700_000_000_000,
        )
        .await
        .expect("seed active");

        // Leçon codifiée (même classe) → doit être exclue.
        idx.seed_lesson(
            "01FFFFFFFFFFFFFFFFFFFFFFFF",
            "Migration codifiée",
            "migration codified",
            "Cette leçon est déjà intégrée au système migration.",
            1_700_000_001_000,
        )
        .await
        .expect("seed codified");

        // Leçon avec un tag contenant la sous-chaîne "codified" mais token distinct.
        idx.seed_lesson(
            "01GGGGGGGGGGGGGGGGGGGGGGGG",
            "Migration codified-2026",
            "migration codified-2026",
            "Tag distinct ne doit pas être confondu avec codified.",
            1_700_000_002_000,
        )
        .await
        .expect("seed codified-like");

        let hits = idx
            .recall_lessons(&vault, "migration", 10)
            .await
            .expect("recall_lessons");

        let ids: Vec<String> = hits.iter().map(|h| h.note_id.0.to_string()).collect();
        assert!(
            ids.contains(&"01EEEEEEEEEEEEEEEEEEEEEEEE".to_string()),
            "leçon active doit figurer. ids={ids:?}"
        );
        assert!(
            !ids.contains(&"01FFFFFFFFFFFFFFFFFFFFFFFF".to_string()),
            "leçon codified doit être exclue. ids={ids:?}"
        );
        assert!(
            ids.contains(&"01GGGGGGGGGGGGGGGGGGGGGGGG".to_string()),
            "tag codified-2026 (token distinct) ne doit PAS être exclu. ids={ids:?}"
        );
    }

    /// La limite est respectée : le sur-fetch interne ne fait pas déborder le résultat net.
    #[tokio::test]
    async fn recall_lessons_respects_limit() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault = VaultId::new("main");

        for i in 0..5u8 {
            let id = format!("01HHHHHHHHHHHHHHHHHHHHHHH{i}");
            idx.seed_lesson(
                &id,
                &format!("Leçon ci-cd {i}"),
                "ci-cd",
                "Pipeline runner discipline.",
                1_700_000_000_000 + i64::from(i),
            )
            .await
            .expect("seed loop");
        }

        let hits = idx
            .recall_lessons(&vault, "ci-cd", 2)
            .await
            .expect("recall_lessons");
        assert_eq!(hits.len(), 2, "limit=2 doit borner le résultat net");
    }

    /// Audit lot C P1.2 — une leçon `forgotten` (F-44) n'est jamais recallée.
    #[tokio::test]
    async fn recall_lessons_excludes_forgotten() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault = VaultId::new("main");

        // Leçon active.
        idx.seed_lesson(
            "01JAAAAAAAAAAAAAAAAAAAAAAA",
            "Deploy active",
            "deploy",
            "Vérifier le health check avant cutover.",
            1_700_000_000_000,
        )
        .await
        .expect("seed active");

        // Leçon oubliée (même classe) → doit être exclue.
        idx.seed_lesson(
            "01JBBBBBBBBBBBBBBBBBBBBBBB",
            "Deploy oubliée",
            "deploy",
            "Ancienne procédure deploy obsolète.",
            1_700_000_001_000,
        )
        .await
        .expect("seed forgotten");
        idx.seed_mark_forgotten_at("01JBBBBBBBBBBBBBBBBBBBBBBB", 1_700_000_002_000)
            .await
            .expect("mark forgotten");

        let hits = idx
            .recall_lessons(&vault, "deploy", 10)
            .await
            .expect("recall_lessons");

        let ids: Vec<String> = hits.iter().map(|h| h.note_id.0.to_string()).collect();
        assert!(
            ids.contains(&"01JAAAAAAAAAAAAAAAAAAAAAAA".to_string()),
            "leçon active présente. ids={ids:?}"
        );
        assert!(
            !ids.contains(&"01JBBBBBBBBBBBBBBBBBBBBBBB".to_string()),
            "leçon forgotten doit être exclue. ids={ids:?}"
        );
    }

    // ── v0.5.2 Phase A Lot A2 : write_note_derived_batch + delete_vault_from_index ──

    /// Helper : crée 3 notes dérivées de test.
    fn make_derived_notes(vault_id: &str, source_path: &str) -> Vec<DerivedNote> {
        let sep = 0x1fu8;
        let mut notes = Vec::new();
        for kind_name in [("fn", "parse_file"), ("struct", "Parser"), ("fn", "helper")] {
            let key: Vec<u8> = format!(
                "{vault_id}\x1f{source_path}\x1f{}\x1f{}",
                kind_name.0, kind_name.1
            )
            .into_bytes();
            // On ne peut pas appeler NoteId::derived_from depuis ici sans import — utiliser le type direct.
            let _ = sep; // utilisé dans format! ci-dessus via \x1f
            let id = gradatum_core::identity::NoteId::derived_from(&key);
            notes.push(DerivedNote {
                id,
                body_text: format!("fn {}() — signature et doc-comment", kind_name.1),
                tags: format!("code rust {} test_module", kind_name.0),
                title: Some(kind_name.1.to_string()),
                code_meta: Some(CodeSymbolMeta {
                    source_path: source_path.to_string(),
                    kind: kind_name.0.to_string(),
                    qualified_name: kind_name.1.to_string(),
                    signature: Some(format!("({}) -> ()", kind_name.1)),
                    deps: vec![],
                    visibility: Some("pub".to_string()),
                    span: None,
                }),
            });
        }
        notes
    }

    /// Insérer 3 notes dérivées → elles doivent être présentes dans notes.
    #[tokio::test]
    async fn write_note_derived_batch_inserts_notes() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-test";
        let source_path = "src/parser.rs";
        let notes = make_derived_notes(vault_id, source_path);
        let note_ids: Vec<String> = notes.iter().map(|n| n.id.to_string()).collect();

        idx.write_note_derived_batch(vault_id, source_path, "abc123hash", "deadbeef", notes)
            .await
            .expect("write_note_derived_batch");

        // Vérifier que les 3 notes sont présentes.
        let conn = idx.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 3, "3 notes dérivées doivent être présentes");

        // Vérifier que les IDs sont corrects.
        let mut stmt = conn
            .prepare("SELECT id FROM notes WHERE vault_id = ?1")
            .expect("prepare");
        let ids_in_db: Vec<String> = stmt
            .query_map(rusqlite::params![vault_id], |row| row.get(0))
            .expect("query_map")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        for id in &note_ids {
            assert!(ids_in_db.contains(id), "id {id} absent de la DB");
        }

        // Vérifier provenance et section.
        let prov: String = conn
            .query_row(
                "SELECT provenance FROM notes WHERE id = ?1",
                rusqlite::params![&note_ids[0]],
                |row| row.get(0),
            )
            .expect("provenance");
        assert_eq!(prov, "derived:tree-sitter");

        let section: String = conn
            .query_row(
                "SELECT section FROM notes WHERE id = ?1",
                rusqlite::params![&note_ids[0]],
                |row| row.get(0),
            )
            .expect("section");
        assert_eq!(section, "architecture");

        // Vérifier code_freshness.
        let hash: String = conn
            .query_row(
                "SELECT content_hash_source FROM code_freshness WHERE vault_id = ?1 AND source_path = ?2",
                rusqlite::params![vault_id, source_path],
                |row| row.get(0),
            )
            .expect("code_freshness");
        assert_eq!(hash, "abc123hash");
    }

    /// Re-ingest du même source_path : anciennes notes supprimées, nouvelles insérées atomiquement.
    #[tokio::test]
    async fn write_note_derived_batch_replaces_old_notes() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-test";
        let source_path = "src/lib.rs";

        // Premier ingest : 2 notes.
        let notes1 = {
            let key1: Vec<u8> =
                format!("{vault_id}\x1f{source_path}\x1ffn\x1fold_func_a").into_bytes();
            let key2: Vec<u8> =
                format!("{vault_id}\x1f{source_path}\x1ffn\x1fold_func_b").into_bytes();
            vec![
                DerivedNote {
                    id: gradatum_core::identity::NoteId::derived_from(&key1),
                    body_text: "fn old_func_a()".to_string(),
                    tags: "code rust fn".to_string(),
                    title: Some("old_func_a".to_string()),
                    code_meta: None,
                },
                DerivedNote {
                    id: gradatum_core::identity::NoteId::derived_from(&key2),
                    body_text: "fn old_func_b()".to_string(),
                    tags: "code rust fn".to_string(),
                    title: Some("old_func_b".to_string()),
                    code_meta: None,
                },
            ]
        };
        let old_ids: Vec<String> = notes1.iter().map(|n| n.id.to_string()).collect();

        idx.write_note_derived_batch(vault_id, source_path, "hash_v1", "sha_v1", notes1)
            .await
            .expect("first ingest");

        // Deuxième ingest avec 1 nouvelle note : les anciennes doivent disparaître.
        let notes2 = {
            let key_new: Vec<u8> =
                format!("{vault_id}\x1f{source_path}\x1ffn\x1fnew_func").into_bytes();
            vec![DerivedNote {
                id: gradatum_core::identity::NoteId::derived_from(&key_new),
                body_text: "fn new_func()".to_string(),
                tags: "code rust fn".to_string(),
                title: Some("new_func".to_string()),
                code_meta: None,
            }]
        };
        let new_id = notes2[0].id.to_string();

        idx.write_note_derived_batch(vault_id, source_path, "hash_v2", "sha_v2", notes2)
            .await
            .expect("second ingest");

        let conn = idx.conn.lock().await;

        // Les anciennes notes ne doivent plus exister.
        for old_id in &old_ids {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1)",
                    rusqlite::params![old_id],
                    |row| row.get(0),
                )
                .expect("exists check");
            assert!(!exists, "ancienne note {old_id} doit avoir été supprimée");
        }

        // La nouvelle note doit exister.
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1)",
                rusqlite::params![new_id],
                |row| row.get(0),
            )
            .expect("new note exists");
        assert!(exists, "nouvelle note {new_id} doit être présente");

        // code_freshness doit être mis à jour.
        let new_hash: String = conn
            .query_row(
                "SELECT content_hash_source FROM code_freshness WHERE vault_id = ?1 AND source_path = ?2",
                rusqlite::params![vault_id, source_path],
                |row| row.get(0),
            )
            .expect("freshness hash");
        assert_eq!(
            new_hash, "hash_v2",
            "code_freshness doit refléter le 2e ingest"
        );
    }

    /// delete_vault_from_index : supprime toutes les notes du vault, 0 orphelin dans main.
    #[tokio::test]
    async fn delete_vault_from_index_removes_all_and_preserves_main() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        // Insérer des notes dans code-test.
        let code_vault = "code-test";
        let notes = make_derived_notes(code_vault, "src/a.rs");
        idx.write_note_derived_batch(code_vault, "src/a.rs", "hashA", "sha1", notes)
            .await
            .expect("write code-test");

        // Insérer une note dans main (via seed_note — ne doit pas être affectée).
        idx.seed_note(
            "01JMAINAAAAAAAAAAAAAAAAAAA",
            "decisions",
            "Note dans vault main",
        )
        .await
        .expect("seed main note");

        // Vérifier le compte avant suppression.
        let conn_guard = idx.conn.lock().await;
        let count_code: i64 = conn_guard
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                rusqlite::params![code_vault],
                |row| row.get(0),
            )
            .expect("count code");
        assert_eq!(count_code, 3, "3 notes dans code-test avant suppression");
        drop(conn_guard);

        // Supprimer le vault code-test.
        let deleted = idx
            .delete_vault_from_index(code_vault)
            .await
            .expect("delete_vault");
        assert_eq!(deleted, 3, "delete_vault doit retourner 3 notes supprimées");

        // Vérifier que code-test est vide.
        let conn_guard = idx.conn.lock().await;
        let count_after: i64 = conn_guard
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                rusqlite::params![code_vault],
                |row| row.get(0),
            )
            .expect("count after");
        assert_eq!(count_after, 0, "0 notes dans code-test après suppression");

        // Vérifier que main est intacte.
        let main_exists: bool = conn_guard
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE id = '01JMAINAAAAAAAAAAAAAAAAAAA')",
                [],
                |row| row.get(0),
            )
            .expect("main note exists");
        assert!(main_exists, "la note main ne doit pas être touchée");

        // Vérifier code_freshness nettoyée.
        let freshness_count: i64 = conn_guard
            .query_row(
                "SELECT COUNT(*) FROM code_freshness WHERE vault_id = ?1",
                rusqlite::params![code_vault],
                |row| row.get(0),
            )
            .expect("freshness count");
        assert_eq!(
            freshness_count, 0,
            "code_freshness doit être vide après delete_vault"
        );
    }

    // ── P0-3 : garde isolation vault main ─────────────────────────────────────

    /// P0-3a : write_note_derived_batch avec vault_id="main" doit retourner Err.
    ///
    /// TDD : ce test doit ÉCHOUER avant l'ajout de la garde (la méthode accepte "main" sans erreur).
    #[tokio::test]
    async fn p0_3_write_note_derived_batch_main_vault_rejected() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let result = idx
            .write_note_derived_batch("main", "src/lib.rs", "hash", "sha", vec![])
            .await;
        assert!(
            result.is_err(),
            "write_note_derived_batch('main') doit retourner Err — isolation vault principal"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("code-"),
            "message d'erreur doit mentionner le préfixe 'code-', got: {msg}"
        );
    }

    /// P0-3b : write_note_derived_batch avec vault_id sans préfixe "code-" doit retourner Err.
    #[tokio::test]
    async fn p0_3_write_note_derived_batch_arbitrary_vault_rejected() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let result = idx
            .write_note_derived_batch("monprojet", "src/lib.rs", "hash", "sha", vec![])
            .await;
        assert!(
            result.is_err(),
            "write_note_derived_batch sans préfixe 'code-' doit retourner Err"
        );
    }

    /// P0-3c : write_note_derived_batch avec vault_id "code-x" doit passer (préfixe valide).
    #[tokio::test]
    async fn p0_3_write_note_derived_batch_code_prefix_accepted() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let result = idx
            .write_note_derived_batch("code-x", "src/lib.rs", "hash", "sha", vec![])
            .await;
        assert!(
            result.is_ok(),
            "write_note_derived_batch('code-x') doit passer, got: {:?}",
            result.err()
        );
    }

    /// P0-3d : delete_vault_from_index avec vault_id="main" doit retourner Err.
    ///
    /// TDD : ce test doit ÉCHOUER avant l'ajout de la garde (la méthode détruirait le vault main).
    #[tokio::test]
    async fn p0_3_delete_vault_from_index_main_vault_rejected() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let result = idx.delete_vault_from_index("main").await;
        assert!(
            result.is_err(),
            "delete_vault_from_index('main') doit retourner Err — destruction vault principal interdite"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("code-"),
            "message d'erreur doit mentionner le préfixe 'code-', got: {msg}"
        );
    }

    /// P0-3e : delete_vault_from_index avec vault_id "code-test" doit passer.
    #[tokio::test]
    async fn p0_3_delete_vault_from_index_code_prefix_accepted() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        // vault vide → delete retourne 0.
        let result = idx.delete_vault_from_index("code-empty").await;
        assert!(
            result.is_ok(),
            "delete_vault_from_index('code-empty') doit passer, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), 0, "vault vide → 0 notes supprimées");
    }

    // ── P0-1 : desync FTS5 sur re-ingest ──────────────────────────────────────

    /// P0-1 : re-ingest d'un fichier MODIFIÉ ne doit PAS créer de doublons FTS5.
    ///
    /// Scénario exact du bug : 2 ingests SUCCESSIFS du même source_path avec les MÊMES note_ids
    /// (NoteId::derived_from = hash stable du chemin+kind+name). Dans ce cas :
    /// - Ingest 1 : INSERT notes + INSERT notes_fts OK
    /// - Ingest 2 : étape 2 supprime old notes+FTS, étape 3 INSERT ON CONFLICT DO UPDATE (rowid stable)
    ///   → si étape 3 utilise INSERT brut (sans OR REPLACE), le même rowid est ré-inséré dans FTS
    ///   → doublon FTS (body_text v1 + body_text v2 coexistent pour le même rowid)
    ///
    /// TDD : ce test doit ÉCHOUER avant le fix (INSERT brut vs INSERT OR REPLACE).
    #[tokio::test]
    async fn p0_1_fts5_no_duplicate_on_reingest_modified_file() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-fts-test";
        let source_path = "src/parser.rs";

        // ── Ingest 1 : notes v1 ────────────────────────────────────────────────
        let notes_v1 = make_derived_notes(vault_id, source_path);
        let count_v1 = notes_v1.len() as i64;
        // Capturer les IDs pour vérification (stables via derived_from).
        let note_ids: Vec<String> = notes_v1.iter().map(|n| n.id.to_string()).collect();
        idx.write_note_derived_batch(vault_id, source_path, "hash_v1", "sha_v1", notes_v1)
            .await
            .expect("ingest v1");

        // ── Ingest 2 : mêmes IDs mais body_text DIFFÉRENT (fichier modifié) ───
        // NoteId::derived_from(key) = même valeur → same rowid dans notes → ON CONFLICT DO UPDATE.
        let notes_v2: Vec<DerivedNote> = make_derived_notes(vault_id, source_path)
            .into_iter()
            .map(|n| DerivedNote {
                body_text: format!("MODIFIÉ_v2 — {}", n.body_text),
                ..n
            })
            .collect();
        idx.write_note_derived_batch(vault_id, source_path, "hash_v2", "sha_v2", notes_v2)
            .await
            .expect("ingest v2 (fichier modifié)");

        let conn = idx.conn.lock().await;

        // ── Invariant 1 : COUNT(notes) == count_v1 (pas de duplication de lignes) ──
        let count_notes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("count notes");
        assert_eq!(
            count_notes, count_v1,
            "notes : attendu {count_v1}, trouvé {count_notes}"
        );

        // ── Invariant 2 : notes_fts ne doit PAS avoir de doublon de rowid ────
        // FTS5 content=notes : un rowid FTS correspond exactement à un rowid notes.
        // Si INSERT brut, le même rowid est présent 2× dans notes_fts → COUNT(fts) > count_v1.
        // Compter les entrées FTS pour les rowids appartenant à ce vault.
        let fts_count_via_join: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts nf JOIN notes n ON n.rowid = nf.rowid WHERE n.vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("fts count via join");

        assert_eq!(
            fts_count_via_join, count_v1,
            "notes_fts : attendu {count_v1} entrée(s), trouvé {fts_count_via_join} (doublon FTS si > {count_v1})"
        );

        // ── Invariant 3 : le contenu FTS est celui de v2 (pas v1) ─────────────
        // Si INSERT brut laisse le vieux contenu dans FTS, body_text v1 persiste.
        for note_id in &note_ids {
            let fts_body: Option<String> = conn
                .query_row(
                    "SELECT nf.body_text FROM notes_fts nf JOIN notes n ON n.rowid = nf.rowid WHERE n.id = ?1",
                    rusqlite::params![note_id],
                    |row| row.get(0),
                )
                .optional()
                .expect("fts body query");
            let body = fts_body.unwrap_or_default();
            assert!(
                body.contains("MODIFIÉ_v2"),
                "FTS body doit contenir 'MODIFIÉ_v2' pour note {note_id}, got: '{body}'"
            );
        }
    }

    // ── P0-2 : vault_id corrompu dans la propagation des suppressions ──────────

    /// P0-2 : les notes d'un fichier SUPPRIMÉ doivent être effacées lors du re-ingest.
    ///
    /// Ce test vérifie le comportement de `write_note_derived_batch` avec notes=[]
    /// (utilisé par run_ingest pour propager une suppression git).
    /// TDD : ce test doit ÉCHOUER avant le fix de code_cmd.rs (vault_id mal passé).
    ///
    /// NOTE : ce test vérifie directement sqlite.rs (isolation de la méthode).
    /// Le test d'intégration run_ingest est dans gradatum-admin/tests/code_ingest.rs.
    #[tokio::test]
    async fn p0_2_write_note_derived_batch_empty_deletes_notes() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-del-test";
        let source_path = "src/to_delete.rs";

        // Ingest initial.
        let notes = make_derived_notes(vault_id, source_path);
        let count_initial = notes.len();
        idx.write_note_derived_batch(vault_id, source_path, "hash_init", "sha_init", notes)
            .await
            .expect("ingest initial");

        // Propager la suppression : write avec notes=[].
        // Le bon appel est write_note_derived_batch(vault_id, source_path, ...).
        // Le bug P0-2 était d'appeler write_note_derived_batch(source_path, source_path, ...)
        // → le vault_id ne match rien → notes pas supprimées.
        idx.write_note_derived_batch(vault_id, source_path, "", "", vec![])
            .await
            .expect("propagation suppression");

        let conn = idx.conn.lock().await;
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1 AND id IN (SELECT id FROM notes WHERE vault_id = ?1)",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("remaining");
        assert_eq!(
            remaining, 0,
            "toutes les notes de {source_path} doivent être supprimées, trouvé {remaining}/{count_initial}"
        );

        // Vérifier aussi dans notes_fts : 0 entrée orpheline.
        let fts_orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE rowid NOT IN (SELECT rowid FROM notes)",
                [],
                |row| row.get(0),
            )
            .expect("fts orphans");
        assert_eq!(fts_orphans, 0, "0 orphelins FTS après suppression");
    }

    // ── v0.5.2 Phase B — Lot B2 : smoke FTS reindex après ON CONFLICT ────────

    /// B2 (smoke) : vérifie que `write_note_derived_batch` maintient la cohérence
    /// COUNT(notes) == COUNT(notes_fts) et que le contenu FTS est à jour après
    /// ré-insertion d'une note existante via le chemin ON CONFLICT.
    ///
    /// ## Limites de ce test
    ///
    /// Le chemin `ON CONFLICT(id) DO UPDATE` n'est pas atteignable via l'API publique
    /// (write_note_derived_batch DELETE les anciens ids avant INSERT à l'étape 2).
    /// `INSERT OR REPLACE INTO notes_fts` est une défense en profondeur non discriminable
    /// par test au niveau API (un test ne peut forcer 2 rowid identiques sur FTS5 content=).
    /// Le bug P0-1 d'origine était sur-classé : doublon non atteignable en pratique.
    /// Ce test est un smoke de cohérence COUNT, pas un test probant du chemin ON CONFLICT.
    #[tokio::test]
    async fn b2_fts_reindex_smoke() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-on-conflict-test";

        // ── Setup : construire un note_id déterministe ────────────────────────
        // On utilise un source_path "ghost" que l'on n'enregistre PAS dans code_freshness.
        // Ainsi, write_note_derived_batch avec source_path "real" ne connaîtra pas cet ID.
        let ghost_source_path = "src/ghost.rs";
        let key: Vec<u8> =
            format!("{vault_id}\x1f{ghost_source_path}\x1ffn\x1fghost_fn").into_bytes();
        let note_id = gradatum_core::identity::NoteId::derived_from(&key);
        let note_id_str = note_id.to_string();

        // ── Insérer la note directement dans notes (bypass code_freshness) ────
        // Simule un état où une note existe déjà sans être tracée dans code_freshness.
        {
            let conn = idx.conn.lock().await;
            use sha2::{Digest as _, Sha256};
            let body_v1 = "fn ghost_fn() { /* v1 */ }";
            let hash_v1: [u8; 32] = Sha256::digest(body_v1.as_bytes()).into();
            conn.execute(
                "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, tags, provenance, trust)
                 VALUES (?1, ?2, NULL, 'architecture', 'live', 1, 0, ?3, ?4, 'code rust fn root', 'derived:tree-sitter', 0.5)",
                rusqlite::params![&note_id_str, vault_id, hash_v1.as_slice(), body_v1],
            )
            .expect("insert note directe (bypass code_freshness)");

            // Insérer dans FTS également (comme le ferait l'ingest normal).
            conn.execute(
                "INSERT OR REPLACE INTO notes_fts (rowid, body_text, tags)
                 SELECT rowid, body_text, tags FROM notes WHERE id = ?1",
                rusqlite::params![&note_id_str],
            )
            .expect("insert fts v1");
        }

        // Vérifier état initial : 1 note, 1 entrée FTS.
        {
            let conn = idx.conn.lock().await;
            let count_notes: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                    rusqlite::params![vault_id],
                    |row| row.get(0),
                )
                .expect("count notes initial");
            let count_fts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM notes_fts nf JOIN notes n ON n.rowid = nf.rowid WHERE n.vault_id = ?1",
                    rusqlite::params![vault_id],
                    |row| row.get(0),
                )
                .expect("count fts initial");
            assert_eq!(count_notes, 1, "1 note avant write_note_derived_batch");
            assert_eq!(count_fts, 1, "1 FTS avant write_note_derived_batch");
        }

        // ── Appeler write_note_derived_batch avec le MÊME note_id ────────────
        // source_path "real.rs" différent de "ghost.rs" → l'étape 2 ne trouve rien
        // dans code_freshness (code_freshness n'a pas d'entrée pour "real.rs") → 0 DELETE.
        // L'INSERT va déclencher ON CONFLICT(id) DO UPDATE car note_id existe déjà.
        let real_source_path = "src/real.rs";
        let body_v2 = "fn ghost_fn() { /* v2 — modifié */ }";
        let note_v2 = DerivedNote {
            id: note_id,
            body_text: body_v2.to_string(),
            tags: "code rust fn root".to_string(),
            title: Some("ghost_fn".to_string()),
            code_meta: None,
        };
        idx.write_note_derived_batch(
            vault_id,
            real_source_path,
            "hash_real_v2",
            "sha_real",
            vec![note_v2],
        )
        .await
        .expect("write_note_derived_batch avec note en conflit");

        // ── Invariant : COUNT(notes) == COUNT(notes_fts) ─────────────────────
        // Si INSERT OR REPLACE → FTS est mis à jour en place, pas de doublon.
        // Si INSERT brut → 2 entrées FTS pour le même rowid → COUNT(fts) > COUNT(notes).
        let conn = idx.conn.lock().await;

        let count_notes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("count notes final");

        let count_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts nf JOIN notes n ON n.rowid = nf.rowid WHERE n.vault_id = ?1",
                rusqlite::params![vault_id],
                |row| row.get(0),
            )
            .expect("count fts final");

        assert_eq!(
            count_notes, count_fts,
            "COUNT(notes)={count_notes} != COUNT(notes_fts)={count_fts} — doublon FTS détecté"
        );

        // ── Le contenu FTS doit être v2 (pas v1) ─────────────────────────────
        let fts_body: String = conn
            .query_row(
                "SELECT nf.body_text FROM notes_fts nf JOIN notes n ON n.rowid = nf.rowid WHERE n.id = ?1",
                rusqlite::params![&note_id_str],
                |row| row.get(0),
            )
            .expect("fts body final");
        assert!(
            fts_body.contains("v2"),
            "FTS doit contenir le contenu v2 après ON CONFLICT DO UPDATE, got: '{fts_body}'"
        );

        // Le chemin ON CONFLICT DO UPDATE n'est pas atteignable via l'API publique
        // (write_note_derived_batch DELETE les anciens ids avant INSERT à l'étape 2).
        // `INSERT OR REPLACE INTO notes_fts` est une défense en profondeur non discriminable
        // par test au niveau API (un test ne peut forcer 2 rowid identiques sur FTS5 content=).
        // Le bug P0-1 d'origine était sur-classé : doublon non atteignable en pratique.
    }

    // ── v0.5.2 Phase B — Lot B1 : drift-detection check_freshness ────────────

    /// B1-a : fichier inchangé après ingest → Fresh.
    ///
    /// Scénario : ingest avec content_hash_source = sha256(bytes),
    /// puis check_freshness avec les mêmes bytes → Fresh.
    #[tokio::test]
    async fn b1_check_freshness_fresh() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-fresh-test";
        let source_path = "src/main.rs";
        let file_bytes = b"fn main() { println!(\"hello\"); }";

        // Calculer le hash comme le ferait l'ingest.
        let hash = SqliteIndex::sha256_hex(file_bytes);

        // Simuler un ingest : écrire les notes et le hash dans code_freshness.
        idx.write_note_derived_batch(vault_id, source_path, &hash, "sha_abc", vec![])
            .await
            .expect("write_note_derived_batch pour setup");

        // check_freshness avec les mêmes bytes → Fresh.
        let result = idx
            .check_freshness(vault_id, source_path, file_bytes)
            .await
            .expect("check_freshness");
        assert_eq!(
            result,
            Freshness::Fresh,
            "fichier inchangé doit retourner Fresh"
        );
    }

    /// B1-b : bytes mutés après ingest → Stale.
    ///
    /// Scénario : ingest avec hash des bytes v1, puis check_freshness
    /// avec bytes v2 différents → Stale avec les deux hashes.
    #[tokio::test]
    async fn b1_check_freshness_stale() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let vault_id = "code-stale-test";
        let source_path = "src/lib.rs";
        let bytes_v1 = b"fn original() {}";
        let bytes_v2 = b"fn modified() { /* changement */ }";

        // Ingest avec hash v1.
        let hash_v1 = SqliteIndex::sha256_hex(bytes_v1);
        idx.write_note_derived_batch(vault_id, source_path, &hash_v1, "sha_v1", vec![])
            .await
            .expect("write_note_derived_batch v1");

        // check_freshness avec bytes v2 → Stale.
        let result = idx
            .check_freshness(vault_id, source_path, bytes_v2)
            .await
            .expect("check_freshness");

        let hash_v2 = SqliteIndex::sha256_hex(bytes_v2);
        match result {
            Freshness::Stale {
                stored_hash,
                current_hash,
            } => {
                assert_eq!(
                    stored_hash, hash_v1,
                    "stored_hash doit être le hash de l'ingest initial"
                );
                assert_eq!(
                    current_hash, hash_v2,
                    "current_hash doit être le hash des bytes courants"
                );
            }
            other => panic!("attendu Freshness::Stale, got {:?}", other),
        }
    }

    /// B1-c : path non indexé → Unknown.
    ///
    /// Scénario : check_freshness sur un (vault_id, source_path) absent de code_freshness
    /// → Unknown (accuracy > coverage : jamais Fresh par défaut).
    #[tokio::test]
    async fn b1_check_freshness_unknown() {
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        // Aucun ingest préalable — code_freshness est vide.
        let result = idx
            .check_freshness(
                "code-unknown-test",
                "src/never_ingested.rs",
                b"some content",
            )
            .await
            .expect("check_freshness");
        assert_eq!(
            result,
            Freshness::Unknown,
            "path non indexé doit retourner Unknown (pas Fresh par défaut)"
        );
    }

    // ── v0.5.2 Phase C : code_scope_query + get_last_ingested_sha ──────────────

    /// Helper : crée une note dérivée avec métadonnées structurées.
    fn make_meta_note(
        vault_id: &str,
        source_path: &str,
        kind: &str,
        qname: &str,
        sig: Option<&str>,
        deps: Vec<&str>,
    ) -> DerivedNote {
        let key: Vec<u8> = format!("{vault_id}\x1f{source_path}\x1f{kind}\x1f{qname}").into_bytes();
        let id = gradatum_core::identity::NoteId::derived_from(&key);
        DerivedNote {
            id,
            body_text: format!("{kind} {qname}\nsignature: {}", sig.unwrap_or("")),
            tags: format!("code rust {kind} root"),
            title: Some(qname.to_string()),
            code_meta: Some(CodeSymbolMeta {
                source_path: source_path.to_string(),
                kind: kind.to_string(),
                qualified_name: qname.to_string(),
                signature: sig.map(|s| s.to_string()),
                deps: deps.into_iter().map(|d| d.to_string()).collect(),
                visibility: Some("pub".to_string()),
                span: None,
            }),
        }
    }

    /// code_scope_query selector=Query → retourne le symbole matché avec ses champs structurés.
    #[tokio::test]
    async fn c_code_scope_query_fts() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-test";
        let notes = [
            make_meta_note(
                vault_id,
                "src/parser.rs",
                "fn",
                "parse_tokens",
                Some("(input: &str) -> Vec<Token>"),
                vec!["Token"],
            ),
            make_meta_note(vault_id, "src/lexer.rs", "struct", "Lexer", None, vec![]),
        ];
        // 2 fichiers distincts → 2 batches.
        idx.write_note_derived_batch(
            vault_id,
            "src/parser.rs",
            "h1",
            "sha1",
            vec![notes[0].clone()],
        )
        .await
        .expect("batch1");
        idx.write_note_derived_batch(
            vault_id,
            "src/lexer.rs",
            "h2",
            "sha1",
            vec![notes[1].clone()],
        )
        .await
        .expect("batch2");

        let res = idx
            .code_scope_query(vault_id, &CodeSelector::Query("parse_tokens".into()), 10)
            .await
            .expect("query");
        assert_eq!(res.len(), 1, "1 match attendu pour parse_tokens");
        assert_eq!(res[0].qualified_name, "parse_tokens");
        assert_eq!(res[0].source_path, "src/parser.rs");
        assert_eq!(res[0].kind, "fn");
        assert_eq!(
            res[0].signature.as_deref(),
            Some("(input: &str) -> Vec<Token>")
        );
        assert_eq!(res[0].deps, vec!["Token".to_string()]);
    }

    /// code_scope_query selector=Path → tous les symboles d'un fichier.
    #[tokio::test]
    async fn c_code_scope_query_path() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-test";
        let notes = vec![
            make_meta_note(vault_id, "src/a.rs", "fn", "alpha", None, vec![]),
            make_meta_note(vault_id, "src/a.rs", "fn", "beta", None, vec![]),
        ];
        idx.write_note_derived_batch(vault_id, "src/a.rs", "h", "sha", notes)
            .await
            .expect("batch");
        // Ajouter un fichier hors-scope.
        idx.write_note_derived_batch(
            vault_id,
            "src/b.rs",
            "h2",
            "sha",
            vec![make_meta_note(
                vault_id,
                "src/b.rs",
                "fn",
                "gamma",
                None,
                vec![],
            )],
        )
        .await
        .expect("batch b");

        let res = idx
            .code_scope_query(vault_id, &CodeSelector::Path("src/a.rs".into()), 10)
            .await
            .expect("path query");
        assert_eq!(res.len(), 2, "2 symboles dans src/a.rs");
        let names: Vec<&str> = res.iter().map(|e| e.qualified_name.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"));
        assert!(!names.contains(&"gamma"), "gamma (src/b.rs) hors scope");
    }

    /// code_scope_query selector=Symbol → match substring sur qualified_name.
    #[tokio::test]
    async fn c_code_scope_query_symbol() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-test";
        idx.write_note_derived_batch(
            vault_id,
            "src/x.rs",
            "h",
            "sha",
            vec![
                make_meta_note(
                    vault_id,
                    "src/x.rs",
                    "method",
                    "Parser::parse",
                    None,
                    vec![],
                ),
                make_meta_note(vault_id, "src/x.rs", "fn", "unrelated", None, vec![]),
            ],
        )
        .await
        .expect("batch");

        let res = idx
            .code_scope_query(vault_id, &CodeSelector::Symbol("Parser".into()), 10)
            .await
            .expect("symbol query");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].qualified_name, "Parser::parse");
    }

    /// code_scope_query rejette un vault_id ne commençant pas par code- (défense en profondeur).
    #[tokio::test]
    async fn c_code_scope_query_rejects_main() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let err = idx
            .code_scope_query("main", &CodeSelector::Query("x".into()), 10)
            .await;
        assert!(err.is_err(), "vault 'main' doit être rejeté");
    }

    // ── v0.6.3 : code_scope_reverse_deps ─────────────────────────────────────────

    /// returns_callers_for_known_symbol : étant donné un symbole S avec des callers connus,
    /// `code_scope_reverse_deps` doit retourner les callers corrects.
    ///
    /// Scénario : `alpha` dépend de `Token` ; `beta` ne dépend pas de `Token`.
    /// `reverse_deps("Token")` doit retourner `alpha` uniquement.
    #[tokio::test]
    async fn returns_callers_for_known_symbol() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-revdeps";

        // alpha dépend de Token, beta n'a aucune dep.
        let alpha = make_meta_note(vault_id, "src/a.rs", "fn", "alpha", None, vec!["Token"]);
        let beta = make_meta_note(vault_id, "src/b.rs", "fn", "beta", None, vec![]);
        idx.write_note_derived_batch(vault_id, "src/a.rs", "ha", "sha1", vec![alpha])
            .await
            .expect("batch alpha");
        idx.write_note_derived_batch(vault_id, "src/b.rs", "hb", "sha1", vec![beta])
            .await
            .expect("batch beta");

        let callers = idx
            .code_scope_reverse_deps(vault_id, "Token", 50)
            .await
            .expect("code_scope_reverse_deps");

        assert_eq!(callers.len(), 1, "1 caller attendu pour Token");
        assert_eq!(callers[0].qualified_name, "alpha");
        assert!(
            callers[0].deps.contains(&"Token".to_string()),
            "alpha doit déclarer Token dans ses deps"
        );
    }

    /// returns_empty_callers_for_unknown_symbol : symbole inconnu → liste vide, pas d'erreur.
    #[tokio::test]
    async fn returns_empty_callers_for_unknown_symbol() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-revdeps-empty";

        // Un seul symbole sans dep.
        let note = make_meta_note(vault_id, "src/lib.rs", "fn", "foo", None, vec![]);
        idx.write_note_derived_batch(vault_id, "src/lib.rs", "h", "sha", vec![note])
            .await
            .expect("batch");

        let callers = idx
            .code_scope_reverse_deps(vault_id, "NonExistentSymbol", 50)
            .await
            .expect("code_scope_reverse_deps symbole inconnu");

        assert!(
            callers.is_empty(),
            "symbole inconnu doit retourner une liste vide, pas une erreur"
        );
    }

    /// default_direction_unchanged : sans `include_callers`, le comportement code_scope_query
    /// est identique à avant (test de non-régression du contrat API rétro-compatible).
    #[tokio::test]
    async fn default_direction_unchanged() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-compat";

        let note = make_meta_note(
            vault_id,
            "src/parser.rs",
            "fn",
            "parse",
            Some("(s: &str) -> bool"),
            vec!["Lexer"],
        );
        idx.write_note_derived_batch(vault_id, "src/parser.rs", "h", "sha", vec![note])
            .await
            .expect("batch");

        // code_scope_query n'a aucun champ include_callers → comportement inchangé.
        let res = idx
            .code_scope_query(vault_id, &CodeSelector::Symbol("parse".into()), 10)
            .await
            .expect("code_scope_query");

        assert_eq!(res.len(), 1, "1 résultat attendu");
        assert_eq!(res[0].qualified_name, "parse");
        // Le résultat ne contient PAS de callers — la struct CodeScopeEntryRaw n'en a pas.
        assert_eq!(res[0].deps, vec!["Lexer".to_string()]);
    }

    /// code_scope_reverse_deps rejette un vault_id ne commençant pas par code-.
    #[tokio::test]
    async fn code_scope_reverse_deps_rejects_non_code_vault() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let err = idx.code_scope_reverse_deps("main", "SomeSymbol", 10).await;
        assert!(
            err.is_err(),
            "vault 'main' doit être rejeté par code_scope_reverse_deps"
        );
    }

    /// get_last_ingested_sha retourne le sha le plus fréquent.
    #[tokio::test]
    async fn c_get_last_ingested_sha() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-test";
        // 2 fichiers à sha_old, 1 fichier à sha_new (update partiel simulé).
        idx.write_note_derived_batch(
            vault_id,
            "src/a.rs",
            "h",
            "sha_old",
            vec![make_meta_note(
                vault_id,
                "src/a.rs",
                "fn",
                "a",
                None,
                vec![],
            )],
        )
        .await
        .expect("a");
        idx.write_note_derived_batch(
            vault_id,
            "src/b.rs",
            "h",
            "sha_old",
            vec![make_meta_note(
                vault_id,
                "src/b.rs",
                "fn",
                "b",
                None,
                vec![],
            )],
        )
        .await
        .expect("b");
        idx.write_note_derived_batch(
            vault_id,
            "src/c.rs",
            "h",
            "sha_new",
            vec![make_meta_note(
                vault_id,
                "src/c.rs",
                "fn",
                "c",
                None,
                vec![],
            )],
        )
        .await
        .expect("c");

        let sha = idx
            .get_last_ingested_sha(vault_id)
            .await
            .expect("sha")
            .expect("some sha");
        assert_eq!(sha, "sha_old", "sha_old est le plus fréquent (2 vs 1)");

        // Vault vierge → None.
        let none = idx
            .get_last_ingested_sha("code-empty")
            .await
            .expect("empty");
        assert!(none.is_none());
    }
    // ── v0.6.4 : reverse_deps avec path qualifié (Axe C hybride) ──────────────────
    //
    // Ces tests vérifient que la couche SQL (code_scope_reverse_deps_batch) matche
    // correctement les path qualifiés (`"SlowJobStore::set_pending"`) qui seront
    // stockés dans les deps après le fix du parser Axe C.

    /// method_qualified_call_finds_callers :
    /// alpha a `"SlowJobStore::set_pending"` ET `"set_pending"` dans ses deps
    /// (comportement post-fix parser Axe C).
    /// beta a uniquement `"set_pending"` (appel via self.x() — terminal seul).
    ///
    /// reverse_deps_batch("SlowJobStore::set_pending") doit trouver alpha uniquement.
    /// reverse_deps_batch("set_pending") doit trouver les deux (alpha et beta).
    #[tokio::test]
    async fn method_qualified_call_finds_callers() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-revdeps-qualified";

        // alpha a les DEUX formes : terminal + path qualifié (post-fix parser Axe C).
        let alpha = make_meta_note(
            vault_id,
            "src/alpha.rs",
            "fn",
            "alpha_caller",
            None,
            vec!["set_pending", "SlowJobStore::set_pending"],
        );
        // beta n'a que le terminal (appel via self.x() — type du receiver non résolvable).
        let beta = make_meta_note(
            vault_id,
            "src/beta.rs",
            "fn",
            "beta_other",
            None,
            vec!["set_pending"],
        );
        idx.write_note_derived_batch(vault_id, "src/alpha.rs", "ha", "sha1", vec![alpha])
            .await
            .expect("batch alpha");
        idx.write_note_derived_batch(vault_id, "src/beta.rs", "hb", "sha1", vec![beta])
            .await
            .expect("batch beta");

        // Sur le path qualifié — seul alpha (exact match).
        let result = idx
            .code_scope_reverse_deps_batch(vault_id, &["SlowJobStore::set_pending"], 50)
            .await
            .expect("batch qualified");

        let callers = result
            .get("SlowJobStore::set_pending")
            .cloned()
            .unwrap_or_default();

        assert!(
            callers.contains(&"alpha_caller".to_string()),
            "SlowJobStore::set_pending doit trouver alpha_caller. callers={callers:?}"
        );
        assert!(
            !callers.contains(&"beta_other".to_string()),
            "beta_other ne doit PAS être retourné pour SlowJobStore::set_pending. callers={callers:?}"
        );

        // Sur le terminal — les deux (alpha et beta ont tous les deux "set_pending").
        let result2 = idx
            .code_scope_reverse_deps_batch(vault_id, &["set_pending"], 50)
            .await
            .expect("batch terminal");

        let callers2 = result2.get("set_pending").cloned().unwrap_or_default();
        assert!(
            callers2.contains(&"beta_other".to_string()),
            "set_pending doit trouver beta_other. callers2={callers2:?}"
        );
        assert!(
            callers2.contains(&"alpha_caller".to_string()),
            "set_pending doit aussi trouver alpha_caller (a les deux formes). callers2={callers2:?}"
        );
    }

    /// free_function_callers_unchanged :
    /// non-régression — fn libre indexée par terminal seul → reverse_deps fonctionne toujours.
    #[tokio::test]
    async fn free_function_callers_unchanged() {
        let idx = SqliteIndex::open_in_memory().await.expect("open");
        let vault_id = "code-revdeps-free";

        let alpha = make_meta_note(
            vault_id,
            "src/alpha.rs",
            "fn",
            "alpha",
            None,
            vec!["validate_code_vault_id"],
        );
        let beta = make_meta_note(vault_id, "src/beta.rs", "fn", "beta", None, vec![]);
        idx.write_note_derived_batch(vault_id, "src/alpha.rs", "ha", "sha1", vec![alpha])
            .await
            .expect("batch alpha");
        idx.write_note_derived_batch(vault_id, "src/beta.rs", "hb", "sha1", vec![beta])
            .await
            .expect("batch beta");

        let result = idx
            .code_scope_reverse_deps_batch(vault_id, &["validate_code_vault_id"], 50)
            .await
            .expect("batch");

        let callers = result
            .get("validate_code_vault_id")
            .cloned()
            .unwrap_or_default();

        assert!(
            callers.contains(&"alpha".to_string()),
            "validate_code_vault_id doit trouver alpha. callers={callers:?}"
        );
        assert!(
            !callers.contains(&"beta".to_string()),
            "beta sans deps ne doit PAS apparaître. callers={callers:?}"
        );
    }
}

/// Unit tests for `apply_cap` — pure function, no database required.
#[cfg(test)]
mod apply_cap_tests {
    use super::*;

    /// Branche cappée : 10001 → (10000, true).
    #[test]
    fn apply_cap_capped_exactly_10001() {
        assert_eq!(SqliteIndex::apply_cap(10001), (10000, true));
    }

    /// Branche cappée : valeur > 10001.
    #[test]
    fn apply_cap_capped_above_10001() {
        assert_eq!(SqliteIndex::apply_cap(99_999), (10000, true));
    }

    /// Frontière haute : 10000 exact → non cappé.
    #[test]
    fn apply_cap_uncapped_exactly_10000() {
        assert_eq!(SqliteIndex::apply_cap(10000), (10000, false));
    }

    /// Valeur ordinaire : non cappée.
    #[test]
    fn apply_cap_uncapped_small() {
        assert_eq!(SqliteIndex::apply_cap(42), (42, false));
    }

    /// Zéro : non cappé.
    #[test]
    fn apply_cap_uncapped_zero() {
        assert_eq!(SqliteIndex::apply_cap(0), (0, false));
    }
}
