//! Méthodes de requêtes enrichies sur `SqliteIndex` — T3 P2.0c.
//!
//! Ces 7 méthodes couvrent les endpoints MCP read qui n'avaient pas d'impl
//! en alpha.3 : `vault_authors`, `vault_tags`, `vault_links`, `vault_graph`,
//! `vault_trace`, `vault_read`, `vault_context`.
//!
//! ## Adaptation au schéma réel
//!
//! Le plan P2.0c référençait une API `sqlx` et des colonnes (`title`, `tenant_id`,
//! `tags` JSON). Le schéma réel `0001_phase1.sql` utilise `rusqlite` +
//! `Arc<Mutex<Connection>>`. Les adaptations documentées :
//!
//! - `distinct_authors` : via colonnes `author_id` + `author_display_name`.
//! - `distinct_tags` : via `notes_fts.tags` (espace-séparé), split côté Rust.
//! - `backlinks` / `neighbors` / `trace_lineage` : table `note_links` (migration 0002).
//! - `title_lookup` : recherche `body_text` commençant par `# {title}` (Markdown H1).
//! - `get_note` : SELECT sur `notes` avec toutes les colonnes disponibles.
//!
//! Les sentinelles (`__sentinel__{vault_id}`) sont exclues de tous les résultats.

use gradatum_core::error::GradatumError;
// NoteRecord défini dans gradatum-core pour usage via trait Index (décision Q5DAG).
pub use gradatum_core::index::NoteRecord;
// Types migrés vers gradatum-core à l'Étape 0.2a — re-exportés pour compat consommateurs.
pub use gradatum_core::index_store::{AuthorRow, Lineage};

use crate::sqlite::SqliteIndex;

// rusqlite importé via `self.conn.lock().await` — pas besoin d'import direct.
// chrono utilisé pour `upsert_link`.
use chrono::Utc;

// ── Helpers privés ────────────────────────────────────────────────────────────

/// Échappe les wildcards SQLite `%`, `_` et `\` dans un pattern LIKE.
///
/// SQLite accepte une clause `ESCAPE '\\'` : le caractère `\` précédant un `%` ou `_`
/// les rend littéraux. Cette fonction préfixe chaque `%`, `_` et `\` par `\`.
///
/// À utiliser avec `ESCAPE '\\'` dans la requête SQL.
///
/// # Exemples
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

// ── Implémentation ────────────────────────────────────────────────────────────

impl SqliteIndex {
    /// Liste les auteurs distincts d'un vault avec leur nombre de notes.
    ///
    /// Exclut les sentinelles (`id LIKE '__sentinel__%'`).
    /// Retourne `name` = `author_display_name` si défini, sinon `author_id`.
    /// Notes sans auteur (`author_id IS NULL`) sont exclues.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
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

    /// Liste les tags distincts d'un vault avec leur fréquence.
    ///
    /// Les tags sont stockés espace-séparés dans `notes.tags` (migration 0003).
    /// Cette méthode charge tous les tags des notes actives et les agrège côté Rust.
    /// Exclut les sentinelles et les notes sans tags.
    ///
    /// Retourne `Vec<(tag, count)>` trié par fréquence décroissante.
    ///
    /// ## Implémentation
    ///
    /// La colonne `tags TEXT` dans `notes` est peuplée par `upsert_note` (migration 0003).
    /// L'agrégation est faite côté Rust (split sur espace) car SQLite n'a pas de
    /// fonction native pour splitter une chaîne espace-séparée en lignes.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
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

    /// Retourne les backlinks (notes qui lient vers `note_id`) pour un vault.
    ///
    /// Nécessite la table `note_links` (migration 0002). Retourne une liste
    /// d'identifiants ULID (`src_note_id`) qui pointent vers `note_id`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue ou si `note_links` est absent.
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

    /// Retourne les voisins d'une note jusqu'à `depth` niveaux (max 3, cap interne).
    ///
    /// Utilise un CTE récursif BFS sur `note_links`. La note source est exclue
    /// du résultat. `depth` est plafonné à 3 pour éviter une traversée runaway.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête CTE échoue.
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

    /// Retourne la lignée d'une note : parents (backlinks) et enfants (liens sortants).
    ///
    /// Combine deux requêtes sur `note_links` :
    /// - `parents` = `src_note_id WHERE dst = note_id` (qui pointe vers cette note)
    /// - `children` = `dst_note_id WHERE src = note_id` (où cette note pointe)
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si l'une des requêtes échoue.
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

    /// Cherche une note par son titre Markdown (première ligne `# {title}`).
    ///
    /// Retourne l'identifiant ULID de la première note trouvée, ou `None`.
    ///
    /// ## Implémentation
    ///
    /// Pas de colonne `title` dans le schéma Phase 1. La recherche utilise
    /// `body_text LIKE '# ' || ?1 || char(10) || '%' ESCAPE '\\'` pour matcher la
    /// première ligne H1 Markdown. Limite à 1 résultat (premier trouvé par `created DESC`).
    ///
    /// Les wildcards SQLite `%`, `_` et `\` présents dans le titre sont échappés
    /// via [`escape_like_pattern`] avant l'interpolation — la requête LIKE est exacte.
    ///
    /// ## Filtre `status = 'live'` (Phase 2.x.4 alpha.13 — rev2 §2.1)
    ///
    /// Les notes `status != 'live'` (downgraded, etc.) sont **exclues** de la
    /// résolution titre — la résolution titre ignore par construction toute
    /// note non-live (cohérent avec la sémantique legacy vault v1.6.2 où une
    /// note archivée n'est pas adressable par titre).
    ///
    /// **Pré-vérif C7-bis (LIVE)** : la colonne `title` est quasi-vide (1/552 notes
    /// au 2026-05-10) — branche optimisée `WHERE title = ?` non viable LIVE. La
    /// requête conserve `body_text LIKE` pour la rétrocompatibilité du corpus
    /// existant.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    pub async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError> {
        let conn = self.conn.lock().await;

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
            Err(e) => Err(GradatumError::Storage(format!("title_lookup : {e}"))),
        }
    }

    /// Retourne le record complet d'une note par son identifiant ULID.
    ///
    /// Retourne `None` si la note n'existe pas ou est une sentinelle.
    /// Inclut les tags depuis `notes_fts.tags` (joint sur `rowid`).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    /// Méthode concrète interne — appelée par `impl DocumentStore for SqliteIndex`.
    /// Renommée `_inner` pour éviter la collision avec le trait method `DocumentStore::get_note`.
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
                 title
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
                })
            },
        ) {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("get_note : {e}"))),
        }
    }

    /// Insère un lien wikilink entre deux notes (helper pour les tests et le worker).
    ///
    /// Idempotent via `INSERT OR IGNORE`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si l'insertion échoue.
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

    // ── alpha.12 Task 12 — In-degree backlinks ────────────────────────────────

    /// Nombre de backlinks entrants (in-degree) pour une note dans un vault.
    ///
    /// Utilise l'index `idx_note_links_dst` → O(log N) — pas de full scan.
    /// Retourne 0 si la note n'existe pas ou n'a aucun backlink (pas d'erreur).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    #[must_use = "le résultat doit être propagé via ?"]
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

    /// Retourne `(created_ms, in_degree)` pour une note.
    ///
    /// Combine `notes.created` et `COUNT(note_links)` en 2 requêtes séquentielles,
    /// avec `MutexGuard` scopé strictement (drop avant `.await` suivant).
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si la note est absente
    /// - `GradatumError::Storage` si la requête SQLite échoue ou si `note_id` n'est pas un ULID valide
    ///
    /// # Note
    ///
    /// `note_id` est passé en `&str` (cohérent avec `RrfHit.note_id: String` côté handler).
    /// Si la note est absente et qu'un parsing ULID est requis pour construire `NoteId`,
    /// la fallback se fait via `Storage` avec un message explicite.
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
                            "get_note_created_and_indegree: note absente et note_id non-ULID: {note_id}"
                        ))),
                    };
                }
                Err(other) => {
                    return Err(GradatumError::Storage(format!(
                        "get_note_created_and_indegree.created: {other}"
                    )))
                }
            }
        }; // MutexGuard dropped ici — avant la 2e requête .await

        let in_degree = self.backlink_count(vault_id, note_id).await?;
        Ok((created_ms, in_degree))
    }

    // ── Enrichissement batch hits sémantique-only ─────────────────────────────

    /// Récupère `title` et `section` en batch pour une liste d'identifiants ULID.
    ///
    /// 1 seul `SELECT id, title, section FROM notes` avec clause `IN (…)` paramétrée
    /// via une liste d'`?` construite dynamiquement — évite le N+1 dans `vault_search`.
    ///
    /// Sentinelles (`id LIKE '__sentinel__%'`) exclues par la clause `AND id NOT LIKE`.
    ///
    /// # Préconditions
    ///
    /// Si `ids` est vide, retourne un `HashMap` vide immédiatement (zéro requête SQLite).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    pub async fn get_titles_sections(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Construire la liste de paramètres `?1, ?2, …` — 1 slot par id.
        // Les placeholders SQLite sont 1-indexés ; vault_id occupe le slot 1.
        let placeholders: String = (2..=ids.len() + 1)
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

        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("get_titles_sections prepare: {e}")))?;

        // Construire les paramètres dynamiques : [vault_id, id0, id1, …]
        // On matérialise en Vec<String> pour satisfaire ToSql via params_from_iter.
        let mut param_values: Vec<String> = Vec::with_capacity(ids.len() + 1);
        param_values.push(vault_id.to_owned());
        param_values.extend_from_slice(ids);

        let rows = stmt
            .query_map(rusqlite::params_from_iter(param_values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| GradatumError::Storage(format!("get_titles_sections query: {e}")))?;

        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for row in rows {
            let (id, title, section) =
                row.map_err(|e| GradatumError::Storage(format!("get_titles_sections row: {e}")))?;
            out.insert(id, (title, section));
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
        let note_a = ulid::Ulid::new().to_string();
        seed_note_internal(&idx, "main", &note_a, "# Note A").await;

        let count = idx.backlink_count("main", &note_a).await.unwrap();
        assert_eq!(count, 0, "note sans backlinks → count = 0");
    }

    // T12-2 : backlink_count — 2 backlinks corrects
    #[tokio::test]
    async fn backlink_count_returns_correct_in_degree() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let dst = ulid::Ulid::new().to_string();
        let src1 = ulid::Ulid::new().to_string();
        let src2 = ulid::Ulid::new().to_string();
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
        let dst_a = ulid::Ulid::new().to_string();
        let dst_b = ulid::Ulid::new().to_string();
        let src_a = ulid::Ulid::new().to_string();
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
        let nope = ulid::Ulid::new().to_string();
        let count = idx.backlink_count("main", &nope).await.unwrap();
        assert_eq!(count, 0, "note inexistante → 0 backlinks sans erreur");
    }

    // T12-5 : get_note_created_and_indegree — note existante retourne (created_ms, in_degree)
    #[tokio::test]
    async fn get_note_created_and_indegree_returns_correct_values() {
        let idx = SqliteIndex::open_in_memory().await.unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let note_y = ulid::Ulid::new().to_string();
        let linker = ulid::Ulid::new().to_string();
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
        let missing = ulid::Ulid::new().to_string();

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
        let id_a = ulid::Ulid::new().to_string();
        let id_b = ulid::Ulid::new().to_string();

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
        let missing = ulid::Ulid::new().to_string();
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
        let id_vault_a = ulid::Ulid::new().to_string();
        let id_vault_b = ulid::Ulid::new().to_string();

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
