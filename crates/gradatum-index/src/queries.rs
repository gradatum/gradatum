//! Enriched query methods on `SqliteIndex`.
//!
//! These methods cover the MCP read endpoints: `vault_authors`, `vault_tags`,
//! `vault_links`, `vault_graph`, `vault_trace`, `vault_read`, `vault_context`.
//!
//! ## Schema adaptation
//!
//! The actual `0001_phase1.sql` schema uses `rusqlite` + `Arc<Mutex<Connection>>`.
//! Adaptations from the original plan:
//!
//! - `distinct_authors`: via columns `author_id` + `author_display_name`.
//! - `distinct_tags`: via `notes.tags` (space-separated), split on the Rust side.
//! - `backlinks` / `neighbors` / `trace_lineage`: `note_links` table (migration 0002).
//! - `title_lookup`: matches `body_text` starting with `# {title}` (Markdown H1).
//! - `get_note`: SELECT on `notes` with all available columns.
//!
//! Sentinels (`__sentinel__{vault_id}`) are excluded from all results.

use gradatum_core::error::GradatumError;
// NoteRecord is defined in gradatum-core for use via the Index trait.
pub use gradatum_core::index::NoteRecord;
// Types migrated to gradatum-core — re-exported for consumer compatibility.
pub use gradatum_core::index_store::{AuthorRow, Lineage};

use crate::sqlite::SqliteIndex;

// rusqlite importé via `self.conn.lock().await` — pas besoin d'import direct.
// chrono utilisé pour `upsert_link`.
use chrono::Utc;

// ── Private helpers ───────────────────────────────────────────────────────────

/// Extracts the H1 title from a Markdown body, aligned with the SQL predicate used
/// by `title_lookup` (`body_text LIKE '# %'`).
///
/// # Canonical H1 extraction rule
///
/// The **first line** must start **exactly** with `"# "` (hash + space,
/// no leading space before `#`). The remainder is trimmed and returned.
///
/// This rule mirrors the SQL predicate in `title_lookup`:
/// ```sql
/// body_text LIKE '# %'
/// ```
/// which matches only if the body *starts with* `# ` — no indentation,
/// at the absolute first position.
///
/// An H1 on line 2, indented, or with no space after `#` returns `None`
/// — intentional, to guarantee SQL ↔ runtime consistency:
/// `vault_read.title` can never return a title that `title_lookup` would not find.
///
/// # Examples
///
/// ```
/// use gradatum_index::extract_h1_title;
/// assert_eq!(extract_h1_title("# Titre"),             Some("Titre".to_owned()));
/// assert_eq!(extract_h1_title("# Titre\ncorps"),      Some("Titre".to_owned()));
/// assert_eq!(extract_h1_title("#   espaces   "),       Some("espaces".to_owned()));
/// assert_eq!(extract_h1_title("   # Indented"),         None);
/// assert_eq!(extract_h1_title("intro\n# Not at top"),  None);
/// assert_eq!(extract_h1_title("#nospace"),             None);
/// assert_eq!(extract_h1_title("# "),                  None);
/// assert_eq!(extract_h1_title(""),                    None);
/// ```
pub fn extract_h1_title(body: &str) -> Option<String> {
    body.lines()
        .next()
        .and_then(|l| l.strip_prefix("# "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Escapes the SQLite LIKE wildcards `%`, `_` and `\` in a pattern string.
///
/// SQLite accepts an `ESCAPE '\\'` clause: a `\` in front of `%` or `_` makes the
/// character literal. This function prefixes every `%`, `_` and `\` with `\`, and the
/// resulting pattern must be used together with `ESCAPE '\\'` in the query.
///
/// ```text
/// escape_like_pattern("User%")   → "User\\%"
/// escape_like_pattern("Note_1")  → "Note\\_1"
/// escape_like_pattern("a\\b")    → "a\\\\b"
/// escape_like_pattern("Normal")  → "Normal"
/// ```
fn escape_like_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

// ── Implementation ────────────────────────────────────────────────────────────

impl SqliteIndex {
    /// Lists distinct authors in a vault with their note count.
    ///
    /// Excludes sentinels (`id LIKE '__sentinel__%'`).
    /// Returns `name` = `author_display_name` if set, otherwise `author_id`.
    /// Notes without an author (`author_id IS NULL`) are excluded.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT
                     COALESCE(author_display_name, author_id) AS name,
                     COUNT(*) AS cnt
                 FROM notes
                 WHERE vault_id = ?1
                   AND author_id IS NOT NULL
                   AND id NOT LIKE '__sentinel__%'
                 GROUP BY author_id, author_display_name
                 ORDER BY cnt DESC, name ASC",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare distinct_authors : {e}")))?;

        let rows = stmt
            .query_map([vault_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| GradatumError::Storage(format!("query distinct_authors : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (name, cnt) =
                r.map_err(|e| GradatumError::Storage(format!("row distinct_authors : {e}")))?;
            out.push(AuthorRow {
                name,
                note_count: cnt as u64,
            });
        }
        Ok(out)
    }

    /// Lists distinct tags in a vault with their frequency.
    ///
    /// Tags are stored space-separated in `notes.tags` (migration 0003).
    /// This method loads all tags from live notes and aggregates them on the Rust side.
    /// Excludes sentinels and notes without tags.
    ///
    /// Returns `Vec<(tag, count)>` sorted by descending frequency.
    ///
    /// ## Implementation
    ///
    /// The `tags TEXT` column in `notes` is populated by `upsert_note` (migration 0003).
    /// Aggregation is done in Rust (space-split) because SQLite has no built-in function
    /// for splitting a space-separated string into rows.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    pub async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        let conn = self.conn.lock().await;

        // Lit les tags depuis notes.tags (migration 0003) — pas de JOIN FTS5.
        let mut stmt = conn
            .prepare(
                "SELECT tags
                 FROM notes
                 WHERE vault_id = ?1
                   AND id NOT LIKE '__sentinel__%'
                   AND tags IS NOT NULL
                   AND tags != ''",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare distinct_tags : {e}")))?;

        let rows = stmt
            .query_map([vault_id], |row| row.get::<_, String>(0))
            .map_err(|e| GradatumError::Storage(format!("query distinct_tags : {e}")))?;

        // Agrégation en mémoire : split espace, compter les occurrences.
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for r in rows {
            let tags_raw =
                r.map_err(|e| GradatumError::Storage(format!("row distinct_tags : {e}")))?;
            for tag in tags_raw.split_whitespace() {
                if !tag.is_empty() {
                    *counts.entry(tag.to_string()).or_insert(0) += 1;
                }
            }
        }

        let mut result: Vec<(String, u64)> = counts.into_iter().collect();
        // Tri : fréquence décroissante, puis alphabétique pour la stabilité.
        result.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(result)
    }

    /// Returns backlinks (notes that link to `note_id`) for a vault.
    ///
    /// Requires the `note_links` table (migration 0002). Returns a list
    /// of ULID identifiers (`src_note_id`) that point to `note_id`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails or `note_links` is absent.
    pub async fn backlinks(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Vec<String>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT src_note_id
                 FROM note_links
                 WHERE dst_note_id = ?1 AND vault_id = ?2
                 ORDER BY created_at DESC",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare backlinks : {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![note_id, vault_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query backlinks : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| GradatumError::Storage(format!("row backlinks : {e}")))?);
        }
        Ok(out)
    }

    /// Returns neighbors of a note up to `depth` levels (internal cap: 3).
    ///
    /// Uses a recursive BFS CTE on `note_links`. The source note is excluded
    /// from the result. `depth` is capped at 3 to prevent runaway traversal.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the CTE query fails.
    pub async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        // Cap interne à 3 niveaux — requis par la spec (prévient les traversées exponentielles).
        let depth_capped = depth.min(3) as i64;
        let conn = self.conn.lock().await;

        // CTE récursif BFS : part de `note_id`, suit les liens sortants niveau par niveau.
        // `UNION` (pas UNION ALL) évite les cycles : chaque id n'apparaît qu'une fois par CTE.
        let sql = format!(
            "WITH RECURSIVE bfs(id, lvl) AS (
                 SELECT ?1, 0
                 UNION
                 SELECT nl.dst_note_id, bfs.lvl + 1
                 FROM note_links nl
                 JOIN bfs ON nl.src_note_id = bfs.id
                 WHERE bfs.lvl < {depth_capped}
                   AND nl.vault_id = ?2
             )
             SELECT DISTINCT id FROM bfs WHERE id != ?1"
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("prepare neighbors : {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![note_id, vault_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query neighbors : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| GradatumError::Storage(format!("row neighbors : {e}")))?);
        }
        Ok(out)
    }

    /// Returns the lineage of a note: parents (backlinks) and children (outgoing links).
    ///
    /// Combines two queries on `note_links`:
    /// - `parents` = `src_note_id WHERE dst = note_id` (notes that point to this note)
    /// - `children` = `dst_note_id WHERE src = note_id` (notes this note points to)
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if either query fails.
    pub async fn trace_lineage(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Lineage, GradatumError> {
        let conn = self.conn.lock().await;

        // Parents : notes qui pointent vers note_id.
        let mut stmt_parents = conn
            .prepare(
                "SELECT src_note_id FROM note_links
                 WHERE dst_note_id = ?1 AND vault_id = ?2
                 ORDER BY created_at DESC",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare trace_lineage parents : {e}")))?;

        let parent_rows = stmt_parents
            .query_map(rusqlite::params![note_id, vault_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query parents : {e}")))?;

        let mut parents = Vec::new();
        for r in parent_rows {
            parents.push(r.map_err(|e| GradatumError::Storage(format!("row parents : {e}")))?);
        }

        // Enfants : notes vers lesquelles note_id pointe.
        let mut stmt_children = conn
            .prepare(
                "SELECT dst_note_id FROM note_links
                 WHERE src_note_id = ?1 AND vault_id = ?2
                 ORDER BY created_at DESC",
            )
            .map_err(|e| GradatumError::Storage(format!("prepare trace_lineage children : {e}")))?;

        let child_rows = stmt_children
            .query_map(rusqlite::params![note_id, vault_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GradatumError::Storage(format!("query children : {e}")))?;

        let mut children = Vec::new();
        for r in child_rows {
            children.push(r.map_err(|e| GradatumError::Storage(format!("row children : {e}")))?);
        }

        Ok(Lineage { parents, children })
    }

    /// Looks up a note by title — `title` column exact-match first, H1 fallback second.
    ///
    /// Returns the ULID of the first matching note, or `None`.
    ///
    /// ## Two-pass resolution
    ///
    /// 1. **Column exact-match**: queries `title = ?2` (no LIKE — no escaping needed).
    ///    The column is populated by `upsert_note_title` on every `persist_curated`.
    ///    If a note is found it is returned immediately.
    ///
    /// 2. **H1 fallback**: if no note matches via the column, falls back to
    ///    `body_text LIKE '# {title}\n%'` (backward-compatible corpus-wide).
    ///
    /// ## Collision policy
    ///
    /// If note A has `title='dup'` (column) and note B has the H1 `# dup` without a
    /// column entry, **the column wins**: note A is returned (pass 1 succeeds before pass 2).
    ///
    /// ## `status = 'live'` filter
    ///
    /// Notes with `status != 'live'` (downgraded, deprecated, etc.) are **excluded** from title
    /// resolution — an archived note is not addressable by title.
    ///    This filter applies to **both passes**.
    ///
    /// ## LIKE escaping (pass 2 only)
    ///
    /// SQLite LIKE wildcards `%`, `_`, and `\` in the title are escaped before
    /// interpolation — pass 1 exact-match does not use LIKE.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    pub async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;

        // ── Passe 1 : exact-match colonne `title` (priorité absolue) ──────────────
        //
        // Ne pas utiliser LIKE — l'exact-match sur colonne indexée n'a pas besoin d'escape.
        // Exclut sentinelles + notes non-live (même garde que passe 2).
        match conn.query_row(
            "SELECT id FROM notes
             WHERE vault_id = ?1 AND title = ?2
               AND id NOT LIKE '__sentinel__%'
               AND status = 'live'
             ORDER BY created DESC
             LIMIT 1",
            rusqlite::params![vault_id, title],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => return Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => {} // → passe 2
            Err(e) => {
                return Err(GradatumError::Storage(format!(
                    "title_lookup (colonne) : {e}"
                )));
            }
        }

        // ── Passe 2 : fallback LIKE H1 (backward-compat corpus) ───────────────────
        //
        // C1 alpha.15 : escape les wildcards SQLite avant interpolation.
        // Sans escape, un titre contenant `%` ou `_` produirait des faux positifs LIKE.
        let escaped = escape_like_pattern(title);

        // Pattern : `# {title}\n...` (H1 Markdown en première position).
        // `char(10)` = LF. Textes sans LF final sont aussi matchés via `body_text = ?3`.
        // ESCAPE '\\' : rend `\%` et `\_` littéraux dans le pattern lié.
        let pattern = format!("# {escaped}\n%");
        let pattern_no_lf = format!("# {escaped}");

        match conn.query_row(
            "SELECT id FROM notes
             WHERE vault_id = ?1
               AND id NOT LIKE '__sentinel__%'
               AND status = 'live'
               AND (body_text LIKE ?2 ESCAPE '\\' OR body_text = ?3)
             ORDER BY created DESC
             LIMIT 1",
            rusqlite::params![vault_id, pattern, pattern_no_lf],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("title_lookup (H1) : {e}"))),
        }
    }

    /// Checks that a note exists and is `live`, by its `note_id` (ULID string).
    ///
    /// Used for the ULID-first resolution of `[[section:ULID]]` wikilinks: when the link
    /// already carries a ULID, existence is all that needs checking — no H1 matching.
    ///
    /// Sentinels (`id LIKE '__sentinel__%'`) and notes whose status is not `'live'`
    /// (downgraded, archived, …) are excluded.
    ///
    /// ## Return value
    ///
    /// - `Ok(Some(id))` if the note exists and is `live`.
    /// - `Ok(None)` if the note is absent, downgraded, or a sentinel.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    pub async fn id_lookup(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;

        match conn.query_row(
            "SELECT id FROM notes
             WHERE vault_id = ?1
               AND id = ?2
               AND id NOT LIKE '__sentinel__%'
               AND status = 'live'",
            rusqlite::params![vault_id, note_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("id_lookup : {e}"))),
        }
    }

    /// Returns the full record for a note by its ULID.
    ///
    /// Returns `None` if the note does not exist or is a sentinel.
    /// Tags are read from the `notes.tags` column (migration 0003).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    /// Internal concrete method — called by `impl DocumentStore for SqliteIndex`.
    /// Renamed `_inner` to avoid collision with the `DocumentStore::get_note` trait method.
    pub(crate) async fn get_note_inner(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        let conn = self.conn.lock().await;

        // Lit depuis notes directement — tags depuis notes.tags (migration 0003).
        // Pas de JOIN FTS5 : notes_fts avec content=notes ne supporte pas les JOINs
        // pour récupérer des colonnes non-FTS de manière fiable.
        match conn.query_row(
            "SELECT
                 id,
                 vault_id,
                 section,
                 status,
                 body_text,
                 COALESCE(author_display_name, author_id) AS author,
                 tags,
                 content_hash,
                 created,
                 updated,
                 title,
                 locus
             FROM notes
             WHERE vault_id = ?1
               AND id = ?2
               AND id NOT LIKE '__sentinel__%'
             LIMIT 1",
            rusqlite::params![vault_id, note_id],
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
                    // D1.1 : locus pour permettre à read_note de résoudre le chemin
                    // physique après relocalisation (move_locus).
                    locus: row.get(11)?,
                })
            },
        ) {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_note : {e}"))),
        }
    }

    /// Inserts a wikilink between two notes (helper for tests and the worker).
    ///
    /// Idempotent via `INSERT OR IGNORE`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the insert fails.
    pub async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let now_ms = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT OR IGNORE INTO note_links (src_note_id, dst_note_id, vault_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![src_note_id, dst_note_id, vault_id, now_ms],
        )
        .map_err(|e| GradatumError::Storage(format!("upsert_link : {e}")))?;
        Ok(())
    }

    // ── In-degree backlinks ───────────────────────────────────────────────────

    /// Returns the in-degree backlink count for a note in a vault.
    ///
    /// Uses the `idx_note_links_dst` index → O(log N), no full scan.
    /// Returns 0 if the note does not exist or has no backlinks (no error).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    #[must_use = "the result must be propagated via ?"]
    pub async fn backlink_count(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<u64, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) \
                 FROM note_links \
                 WHERE dst_note_id = ?1 AND vault_id = ?2",
                rusqlite::params![note_id, vault_id],
                |row| row.get(0),
            )
            .map_err(|e| GradatumError::Storage(format!("backlink_count: {e}")))?;
        Ok(count.max(0) as u64)
    }

    /// Returns `(created_ms, in_degree)` for a note.
    ///
    /// Combines `notes.created` and `COUNT(note_links)` in 2 sequential queries,
    /// with the `MutexGuard` strictly scoped (dropped before the next `.await`).
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent
    /// - `GradatumError::Storage` if the SQLite query fails or `note_id` is not a valid ULID
    ///
    /// # Note
    ///
    /// `note_id` is passed as `&str` (consistent with `RrfHit.note_id: String` on the handler side).
    /// If the note is absent and a ULID parse is needed to build `NoteId`,
    /// the fallback returns a `Storage` error with an explicit message.
    pub async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        // 1ère requête : récupérer le timestamp de création (notes.created).
        let created_ms = {
            let conn = self.conn.lock().await;
            match conn.query_row(
                "SELECT created FROM notes WHERE id = ?1 AND vault_id = ?2",
                rusqlite::params![note_id, vault_id],
                |row| row.get::<_, i64>(0),
            ) {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // Construire NoteId via parse ULID. Si parse échoue → Storage typé.
                    return match note_id.parse::<ulid::Ulid>() {
                        Ok(u) => Err(GradatumError::NoteNotFound(
                            gradatum_core::identity::NoteId(u),
                        )),
                        Err(_) => Err(GradatumError::Storage(format!(
                            "get_note_created_and_indegree: note absent and non-ULID note_id: {note_id}"
                        ))),
                    };
                }
                Err(other) => {
                    return Err(GradatumError::Storage(format!(
                        "get_note_created_and_indegree.created: {other}"
                    )));
                }
            }
        }; // MutexGuard dropped ici — avant la 2e requête .await

        let in_degree = self.backlink_count(vault_id, note_id).await?;
        Ok((created_ms, in_degree))
    }

    // ── Batch enrichment for semantic-only hits ───────────────────────────────

    /// Fetches `title` and `section` in batch for a list of ULIDs.
    ///
    /// Returns a `HashMap<id, (title, section)>` for all requested `ids`.
    ///
    /// IDs are processed in chunks of at most 998 to respect the SQLite bound-variable
    /// limit (`SQLITE_LIMIT_VARIABLE_NUMBER = 999`): `vault_id` occupies 1 slot,
    /// leaving 998 slots per chunk. Larger batches use multiple sequential queries.
    ///
    /// Sentinels (`id LIKE '__sentinel__%'`) are excluded by the `AND id NOT LIKE` clause.
    ///
    /// # Preconditions
    ///
    /// If `ids` is empty, returns an empty `HashMap` immediately (zero SQLite queries).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if any query fails.
    pub async fn get_titles_sections(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // SQLite limite SQLITE_LIMIT_VARIABLE_NUMBER à 999.
        // vault_id occupe 1 slot → chaque lot peut contenir au plus 998 IDs.
        const CHUNK_SIZE: usize = 998;

        let mut out = std::collections::HashMap::with_capacity(ids.len());
        let conn = self.conn.lock().await;

        for chunk in ids.chunks(CHUNK_SIZE) {
            // Construire la liste de paramètres `?2, ?3, …` — vault_id occupe ?1.
            let placeholders: String = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "SELECT id, title, section \
                 FROM notes \
                 WHERE vault_id = ?1 \
                   AND id IN ({placeholders}) \
                   AND id NOT LIKE '__sentinel__%'"
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| GradatumError::Storage(format!("get_titles_sections prepare: {e}")))?;

            // Paramètres : [vault_id, id0, id1, …]
            let mut param_values: Vec<String> = Vec::with_capacity(chunk.len() + 1);
            param_values.push(vault_id.to_owned());
            param_values.extend_from_slice(chunk);

            let rows = stmt
                .query_map(rusqlite::params_from_iter(param_values.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| GradatumError::Storage(format!("get_titles_sections query: {e}")))?;

            for row in rows {
                let (id, title, section) = row
                    .map_err(|e| GradatumError::Storage(format!("get_titles_sections row: {e}")))?;
                out.insert(id, (title, section));
            }
        }

        Ok(out)
    }

    /// Reads the raw SQL status for a batch of notes (semantic filter).
    ///
    /// Returns `HashMap<note_id, status>` for present IDs. Returns the raw SQL value
    /// (kebab-case, e.g. `"live"`, `"downgraded"`) rather than `NoteStatus` to handle
    /// legacy `downgraded` values outside the enum. Same batch pattern as
    /// `get_titles_sections` (chunks of 998, `params_from_iter`).
    pub async fn get_statuses(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        const CHUNK_SIZE: usize = 998;

        let mut out = std::collections::HashMap::with_capacity(ids.len());
        let conn = self.conn.lock().await;

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders: String = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "SELECT id, status \
                 FROM notes \
                 WHERE vault_id = ?1 \
                   AND id IN ({placeholders}) \
                   AND id NOT LIKE '__sentinel__%'"
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| GradatumError::Storage(format!("get_statuses prepare: {e}")))?;

            let mut param_values: Vec<String> = Vec::with_capacity(chunk.len() + 1);
            param_values.push(vault_id.to_owned());
            param_values.extend_from_slice(chunk);

            let rows = stmt
                .query_map(rusqlite::params_from_iter(param_values.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| GradatumError::Storage(format!("get_statuses query: {e}")))?;

            for row in rows {
                let (id, status) =
                    row.map_err(|e| GradatumError::Storage(format!("get_statuses row: {e}")))?;
                out.insert(id, status);
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod backlinks_tests {
    use super::*;

    /// Helper interne crate — seed note minimale dans index in-memory.
    /// Ce helper est `pub(crate)` non exposé hors du crate (caveat L-P0-3).
    pub(crate) async fn seed_note_internal(
        idx: &SqliteIndex,
        vault_id: &str,
        note_id: &str,
        body: &str,
    ) {
        let conn = idx.conn.lock().await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, \
                                created, updated, content_hash, version, body_text) \
             VALUES (?1, ?2, ?3, ?4, 'live', 1, ?5, ?5, zeroblob(32), 1, ?6)",
            rusqlite::params![
                note_id,
                vault_id,
                format!("test/{note_id}"),
                "test",
                now_ms,
                body
            ],
        )
        .expect("seed_note_internal: insert failed");
    }

    // T12-1 : backlink_count — note sans backlink → 0
    #[tokio::test]
    async fn backlink_count_no_links_returns_zero() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let note_a = ulid::Ulid::generate().to_string();
        seed_note_internal(&idx, "main", &note_a, "# Note A").await;

        let count = idx.backlink_count("main", &note_a).await.unwrap();
        assert_eq!(count, 0, "note sans backlinks → count = 0");
    }

    // T12-2 : backlink_count — 2 backlinks corrects
    #[tokio::test]
    async fn backlink_count_returns_correct_in_degree() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let dst = ulid::Ulid::generate().to_string();
        let src1 = ulid::Ulid::generate().to_string();
        let src2 = ulid::Ulid::generate().to_string();
        seed_note_internal(&idx, "main", &dst, "# Cible").await;
        seed_note_internal(&idx, "main", &src1, "# Source 1").await;
        seed_note_internal(&idx, "main", &src2, "# Source 2").await;

        idx.upsert_link("main", &src1, &dst).await.unwrap();
        idx.upsert_link("main", &src2, &dst).await.unwrap();

        let count = idx.backlink_count("main", &dst).await.unwrap();
        assert_eq!(count, 2, "2 backlinks attendus, got {count}");
    }

    // T12-3 : backlink_count — isolation vault_id correcte
    //
    // PK = notes.id (uniquement) → on utilise des IDs distincts par vault.
    // La query backlink_count filtre sur (dst_note_id, vault_id) — un même
    // dst_note_id au sens textuel ne fuite pas entre vaults.
    #[tokio::test]
    async fn backlink_count_is_scoped_to_vault_id() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let dst_a = ulid::Ulid::generate().to_string();
        let dst_b = ulid::Ulid::generate().to_string();
        let src_a = ulid::Ulid::generate().to_string();
        seed_note_internal(&idx, "vault_a", &dst_a, "# Note X (vault A)").await;
        seed_note_internal(&idx, "vault_b", &dst_b, "# Note X (vault B)").await;
        seed_note_internal(&idx, "vault_a", &src_a, "# Source").await;

        // Lien dans vault_a SEULEMENT
        idx.upsert_link("vault_a", &src_a, &dst_a).await.unwrap();

        let count_a = idx.backlink_count("vault_a", &dst_a).await.unwrap();
        let count_b = idx.backlink_count("vault_b", &dst_b).await.unwrap();
        assert_eq!(count_a, 1, "vault_a : 1 backlink");
        assert_eq!(count_b, 0, "vault_b : 0 backlink — isolation vault OK");

        // Cas inverse : interroger vault_b avec dst_a ne doit RIEN trouver,
        // démontrant que les liens vault_a ne fuient pas dans vault_b.
        let cross = idx.backlink_count("vault_b", &dst_a).await.unwrap();
        assert_eq!(
            cross, 0,
            "interroger vault_b avec un dst_a (lié dans vault_a) → 0"
        );
    }

    // T12-4 : backlink_count — note inexistante → 0 (pas d'erreur)
    #[tokio::test]
    async fn backlink_count_nonexistent_note_returns_zero() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let nope = ulid::Ulid::generate().to_string();
        let count = idx.backlink_count("main", &nope).await.unwrap();
        assert_eq!(count, 0, "note inexistante → 0 backlinks sans erreur");
    }

    // T12-5 : get_note_created_and_indegree — note existante retourne (created_ms, in_degree)
    #[tokio::test]
    async fn get_note_created_and_indegree_returns_correct_values() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let note_y = ulid::Ulid::generate().to_string();
        let linker = ulid::Ulid::generate().to_string();
        seed_note_internal(&idx, "main", &note_y, "# Note Y").await;
        seed_note_internal(&idx, "main", &linker, "# Linker").await;
        idx.upsert_link("main", &linker, &note_y).await.unwrap();

        let (created_ms, in_degree) = idx
            .get_note_created_and_indegree("main", &note_y)
            .await
            .unwrap();

        assert!(
            (created_ms - now_ms).abs() < 1000,
            "created_ms ≈ now_ms (±1s), got delta={}ms",
            (created_ms - now_ms).abs()
        );
        assert_eq!(in_degree, 1, "1 backlink attendu, got {in_degree}");
    }

    // T12-6 : get_note_created_and_indegree — note inexistante → Err(NoteNotFound)
    #[tokio::test]
    async fn get_note_created_and_indegree_returns_not_found_on_missing() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let missing = ulid::Ulid::generate().to_string();

        let res = idx.get_note_created_and_indegree("main", &missing).await;

        assert!(
            matches!(res, Err(GradatumError::NoteNotFound(_))),
            "note inexistante (ULID valide) → Err(NoteNotFound), got {res:?}"
        );
    }

    // ── Tests get_titles_sections ─────────────────────────────────────────────

    /// Helper qui insère une note avec `title` et `section` explicites.
    async fn seed_note_with_title(
        idx: &SqliteIndex,
        vault_id: &str,
        note_id: &str,
        section: &str,
        title: Option<&str>,
    ) {
        let conn = idx.conn.lock().await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, \
                                created, updated, content_hash, version, body_text, title) \
             VALUES (?1, ?2, ?3, ?4, 'live', 1, ?5, ?5, zeroblob(32), 1, '', ?6)",
            rusqlite::params![
                note_id,
                vault_id,
                format!("{section}/{note_id}"),
                section,
                now_ms,
                title
            ],
        )
        .expect("seed_note_with_title: insert failed");
    }

    // T-gts-1 : get_titles_sections — retourne title+section pour les IDs seedés
    #[tokio::test]
    async fn get_titles_sections_returns_correct_mapping() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let id_a = ulid::Ulid::generate().to_string();
        let id_b = ulid::Ulid::generate().to_string();

        seed_note_with_title(&idx, "main", &id_a, "decisions", Some("Note A titre")).await;
        seed_note_with_title(&idx, "main", &id_b, "reference", None).await;

        let map = idx
            .get_titles_sections("main", &[id_a.clone(), id_b.clone()])
            .await
            .expect("get_titles_sections ne doit pas échouer");

        // id_a : title présent
        let (title_a, section_a) = map.get(&id_a).expect("id_a doit être dans la map");
        assert_eq!(
            title_a.as_deref(),
            Some("Note A titre"),
            "id_a : title attendu"
        );
        assert_eq!(section_a, "decisions", "id_a : section attendue");

        // id_b : title NULL
        let (title_b, section_b) = map.get(&id_b).expect("id_b doit être dans la map");
        assert!(title_b.is_none(), "id_b : title NULL attendu");
        assert_eq!(section_b, "reference", "id_b : section attendue");
    }

    // T-gts-2 : get_titles_sections — ids vides → HashMap vide (0 requête)
    #[tokio::test]
    async fn get_titles_sections_empty_ids_returns_empty_map() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let map = idx
            .get_titles_sections("main", &[])
            .await
            .expect("ids vides → Ok(HashMap::new())");
        assert!(map.is_empty(), "ids vides → map vide");
    }

    // T-gts-3 : get_titles_sections — id inexistant → absent de la map (pas d'erreur)
    #[tokio::test]
    async fn get_titles_sections_missing_id_absent_from_map() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let missing = ulid::Ulid::generate().to_string();
        let map = idx
            .get_titles_sections("main", std::slice::from_ref(&missing))
            .await
            .expect("id absent → Ok(map vide)");
        assert!(
            !map.contains_key(&missing),
            "id inexistant ne doit pas apparaître dans la map"
        );
    }

    // T-gts-4 : get_titles_sections — isolation vault_id stricte
    #[tokio::test]
    async fn get_titles_sections_scoped_to_vault_id() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let id_vault_a = ulid::Ulid::generate().to_string();
        let id_vault_b = ulid::Ulid::generate().to_string();

        seed_note_with_title(&idx, "vault_a", &id_vault_a, "decisions", Some("Note A")).await;
        seed_note_with_title(&idx, "vault_b", &id_vault_b, "decisions", Some("Note B")).await;

        // Interroger vault_a avec l'id de vault_b → absent
        let map = idx
            .get_titles_sections("vault_a", std::slice::from_ref(&id_vault_b))
            .await
            .expect("vault_a ne doit pas voir vault_b");
        assert!(
            !map.contains_key(&id_vault_b),
            "isolation vault_id : id_vault_b ne doit pas apparaître dans vault_a"
        );

        // Interroger vault_a avec son propre id → présent
        let map2 = idx
            .get_titles_sections("vault_a", std::slice::from_ref(&id_vault_a))
            .await
            .expect("vault_a doit trouver id_vault_a");
        assert!(
            map2.contains_key(&id_vault_a),
            "isolation vault_id : id_vault_a doit être dans vault_a"
        );
    }
}
