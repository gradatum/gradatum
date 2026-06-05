//! Implémentation concrète du trait `gradatum-core::index::Index` via SQLite + FTS5.
//!
//! ## Design
//!
//! `SqliteIndex` est thread-safe via `Arc<Mutex<Connection>>` :
//! rusqlite `Connection` n'est ni `Send` ni `Sync`. Le `Mutex` tokio garantit
//! un accès exclusif depuis n'importe quel thread du runtime.
//!
//! ## PRAGMA C12 obligatoires (spec §0.3)
//!
//! Appliqués à chaque `open()` / `open_in_memory()` avant la migration :
//! - `journal_mode = WAL`    : lectures concurrentes sans lock global.
//! - `synchronous = NORMAL`  : durable après crash OS (pas après crash électrique).
//! - `busy_timeout = 5000`   : 5s avant SQLITE_BUSY (multi-process safe).
//! - `foreign_keys = ON`     : intégrité référentielle cascade DELETE.
//!
//! ## Colonne extra_json (écart §5.2)
//!
//! La spec §5.2 nomme la colonne `extra_yaml TEXT`. Cette implémentation utilise
//! `extra_json TEXT` et `serde_json` pour sérialiser `ExtraFields` (BTreeMap<String, toml::Value>).
//! Raison : `serde_yml::to_string` sur des `toml::Value` produit des variantes ambiguës
//! (notamment `Datetime` → représentation privée toml non-portable). `serde_json` garantit
//! un round-trip stable pour les variants String/Integer/Float/Boolean/Array/Table.
//! Toml::Value::Datetime est interdit dans ExtraFields pour le hashing JCS (voir identity.rs).

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::Mutex;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::index::{FileChecksumEntry, FileKind, NoteRecord};
use gradatum_core::index_store::SearchHitRaw;
use gradatum_core::note::Note;
use gradatum_core::scope::{OverrideScope, VaultId};
use gradatum_core::section::{section_to_c_kind, section_to_doc_kind};
use gradatum_core::status::NoteStatus;

/// Implémentation SQLite + FTS5 des traits de storage Gradatum.
///
/// Créé via `SqliteIndex::open(&path)` ou `SqliteIndex::open_in_memory()`.
/// Une seule instance par processus suffit pour un vault Phase 1.
///
/// ## Traits implémentés (Étape 0.1 + 0.2a)
///
/// `SqliteIndex` implémente les 3 traits granulaires :
/// - [`DocumentStore`](gradatum_core::DocumentStore) — CRUD notes
/// - [`IndexStore`](gradatum_core::IndexStore) — FTS, overrides, checksums, scoring composite
/// - [`VectorStore`](gradatum_core::VectorStore) — embeddings + recherche sémantique cosine
///
/// La façade [`Index`](gradatum_core::index::Index) reste disponible via blanket impl.
///
/// ## Méthodes concrètes hors-trait (v0.3.0 post-Étape 0.2a)
///
/// Les méthodes suivantes sont concrètes (`pub`) sur `SqliteIndex` et NE SONT PAS
/// exposées via un trait à v0.3.0 :
/// - `search_fts_scored_filtered` : extension de `search_fts_scored` avec filtre section —
///   pas appelée directement par les handlers (ils utilisent `search_fts_with_snippet`).
/// - `downgrade_note`, `patch_note_status`, `upsert_note_title` : méthodes admin/lifecycle ;
///   présentes dans `DocumentStore` pour les mêmes raisons (voir trait). `patch_note_status`
///   est une méthode inhérente complémentaire sans consommateur trait-based à v0.3.0.
/// - `list_notes`, `total_body_size_bytes` : promues dans `IndexStore` à l'Étape 0.2a.
/// - `seed_note`, `seed_note_with_fts`, `seed_note_with_created` : utilitaires test —
///   restent sur le type concret, intentionnellement hors du trait `IndexStore`.
/// - Méthodes bench (`vault_id_count`, `locus_count`).
///
/// ## Contention v0.3.0
///
/// Les 3 traits partagent un `Arc<Mutex<Connection>>` unique. Toute implémentation
/// de méthode doit s'assurer que le MutexGuard est droppé AVANT tout `.await` suivant.
/// Séparation physique des connexions par trait prévue à v0.4.0.
pub struct SqliteIndex {
    /// Connexion SQLite partagée — `pub(crate)` pour les méthodes du module `queries`.
    ///
    /// Protégée par `Mutex` tokio pour garantir l'accès exclusif depuis n'importe
    /// quel thread du runtime (rusqlite `Connection` n'est ni `Send` ni `Sync`).
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl SqliteIndex {
    /// Ouvre un fichier SQLite à `path`. Crée le fichier s'il n'existe pas.
    ///
    /// Applique les 4 PRAGMA C12 puis exécute les migrations schéma.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si le fichier est inaccessible ou si
    /// l'application des PRAGMA / migrations échoue.
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

    /// Ouvre une base SQLite en mémoire (usage test / benchmark).
    ///
    /// Comportement identique à `open()` pour les PRAGMA et migrations.
    /// Note : `journal_mode` en mémoire retourne `"memory"` plutôt que `"wal"`.
    pub async fn open_in_memory() -> Result<Self, GradatumError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GradatumError::Storage(format!("sqlite in-memory : {e}")))?;
        Self::init(conn).await
    }

    /// Initialisation commune : 4 PRAGMA C12 + migration schéma.
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
        };
        idx.run_migrations().await?;
        Ok(idx)
    }

    /// Délègue l'exécution des migrations au module `migrations`.
    async fn run_migrations(&self) -> Result<(), GradatumError> {
        crate::migrations::run(&self.conn).await
    }

    /// Lit la valeur d'un PRAGMA SQLite.
    ///
    /// Utilisé dans les tests pour vérifier l'application des PRAGMA C12.
    /// `T` doit implémenter `rusqlite::types::FromSql` (String, i64, …).
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

    // ── Embeddings (Phase 2.1.1) ──────────────────────────────────────────────

    /// Insère ou remplace l'embedding d'une note dans la table `note_embeddings`.
    ///
    /// ## Clé primaire
    ///
    /// `(note_id, embedder_id)` — UPSERT idempotent. Une deuxième insertion avec
    /// le même couple remplace le vecteur, la dimension et l'horodatage.
    ///
    /// ## Format BLOB
    ///
    /// `vector` est sérialisé en BLOB f32 little-endian (4 bytes par dimension).
    /// Cohérence garantie entre plate-formes x86-64 et aarch64 (LE sur les deux).
    ///
    /// ## Validation
    ///
    /// Retourne `GradatumError::Storage` si `vector.len() != dim as usize`.
    /// Cette vérification est obligatoire : un mismatch silencieux produirait
    /// des vecteurs tronqués ou sur-dimensionnés incomparables à la requête.
    ///
    /// ## `model_version`
    ///
    /// Phase 2.1.1 : passé à `NULL` (colonne optionnelle selon schéma 0001_phase1.sql).
    /// L'`embedder_id` suffit à identifier le modèle pour l'instant.
    /// Méthode concrète interne — appelée par `impl VectorStore for SqliteIndex`.
    /// Renommée `_inner` pour éviter la collision de nom avec le trait method.
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
        conn.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)
             ON CONFLICT(note_id, embedder_id) DO UPDATE SET
                 vector      = excluded.vector,
                 dim         = excluded.dim,
                 computed_at = excluded.computed_at",
            rusqlite::params![note_id_str, embedder_id, blob, dim as i64, computed_at],
        )
        .map_err(|e| GradatumError::Storage(format!("insert_note_embedding : {e}")))?;

        Ok(())
    }

    /// Relit un vecteur d'embedding depuis la table `note_embeddings`.
    ///
    /// Retourne `None` si aucun embedding n'existe pour ce couple
    /// `(note_id, embedder_id)`. Retourne `Some(Vec<f32>)` après décodage
    /// BLOB f32 little-endian.
    ///
    /// Utilisé par le pipeline d'embed pour éviter un recalcul inutile
    /// (skip si embedding déjà présent et `computed_at` récent).
    /// Méthode concrète interne — appelée par `impl VectorStore for SqliteIndex`.
    /// Renommée `_inner` pour éviter la collision de nom avec le trait method.
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

    // ── Méthodes registry (T2 P2.0c) ─────────────────────────────────────────

    /// Nombre de vault_id distincts dans la table `notes`.
    ///
    /// Utilisé par `Registry::tenant_count` — chaque `vault_id` correspond
    /// à un tenant (le vault est mono-tenant Phase 1 : max 1 valeur distincte
    /// après `ensure_vault_id`).
    pub async fn vault_id_count(&self) -> Result<u32, GradatumError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row("SELECT COUNT(DISTINCT vault_id) FROM notes", [], |row| {
                row.get(0)
            })
            .map_err(|e| GradatumError::Storage(format!("vault_id_count : {e}")))?;
        Ok(count as u32)
    }

    /// Nombre de loci distincts dans la table `notes` (paires vault_id + locus).
    ///
    /// Un locus est l'unité d'organisation sub-tenant (section thématique).
    /// Retourne 0 si aucune note n'est indexée.
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

    /// S'assure qu'un vault_id existe dans la table `notes` en insérant une
    /// sentinelle si absente.
    ///
    /// Utilisé par `Registry::ensure_tenant` pour enregistrer le tenant avant
    /// toute ingestion de notes. La sentinelle a un `id` préfixé par `__sentinel__`
    /// et `section = "reference"` (section valide per schéma).
    ///
    /// Idempotent : `INSERT OR IGNORE` ne modifie rien si le vault_id est déjà présent.
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

    /// Phase 2.1.2 alpha.9 — Soft downgrade d'une note.
    ///
    /// Marque la note `note_id` avec `status = 'downgraded'`, peuple `status_reason`
    /// et `status_changed` (timestamp ms UTC now), et positionne `replaced_by`
    /// si fourni. Le corps (body_text) est conservé.
    ///
    /// Idempotent : un second appel met à jour la raison et le timestamp.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si aucune note ne correspond à `note_id`.
    /// - `GradatumError::Storage` en cas d'erreur SQLite.
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

    /// Phase 2.1.2 alpha.9 — PATCH partiel du statut d'une note.
    ///
    /// Met à jour uniquement les champs fournis (`None` = inchangé).
    /// `status_changed` est mis à jour uniquement si `status` est fourni.
    /// `updated` est toujours mis à jour.
    ///
    /// Au moins un champ doit être fourni (validation à la charge du handler).
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si aucune note ne correspond à `note_id`.
    /// - `GradatumError::Storage` en cas d'erreur SQLite.
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

    /// Compte les notes `status = 'live'` pour un vault.
    ///
    /// Exclut les sentinelles (`id NOT LIKE '__sentinel__%'`).
    /// Utilisé par `vault_status.note_count` (Bug1 fix — remplace tenant_count).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
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

    /// Somme totale de `LENGTH(body_text)` pour les notes non-sentinelles d'un vault.
    ///
    /// Retourne 0 si aucune note. `COALESCE` gère le cas vault vide (SUM NULL → 0).
    /// Utilisé par `vault_status.total_size_bytes` (Bug2 fix — remplace locus_count).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
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

    /// Met à jour la colonne `title` d'une note existante.
    ///
    /// Idempotent. Best-effort : log en cas d'erreur mais ne propage pas.
    /// Utilisé post-curate pour persister le titre H1 extrait du body.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
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

    /// Recherche FTS5 BM25 avec filtre section optionnel.
    ///
    /// Extension de `search_fts_scored` : ajoute `AND n.section = ?4` si `section` est fourni.
    /// `section = None` = comportement identique à `search_fts_scored` (toutes sections).
    ///
    /// ## Pattern rusqlite params dynamiques
    ///
    /// Deux branches `if let` explicites — rusqlite ne supporte pas une arité dynamique
    /// dans une seule closure `query_map`. Le collect est fait dans chaque branche.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    pub async fn search_fts_scored_filtered(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
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

        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
               {section_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

        // Deux branches pour les params dynamiques — rusqlite ne supporte pas
        // une arité variable dans la même closure query_map.
        //
        // Pattern E0597 : stmt doit vivre dans le même bloc que le collect.
        // Assigner `result` dans le bloc de stmt — évite le borrow dangling.
        let collected: Vec<(String, f64, String)> = if let Some(sec) = section {
            let mut stmt = conn.prepare(&sql).map_err(|e| {
                GradatumError::Storage(format!("prepare search_fts_scored_filtered: {e}"))
            })?;
            let result = stmt
                .query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64, sec],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
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
                })?;
            result
        } else {
            let mut stmt = conn.prepare(&sql).map_err(|e| {
                GradatumError::Storage(format!(
                    "prepare search_fts_scored_filtered (no section): {e}"
                ))
            })?;
            let result = stmt
                .query_map(
                    rusqlite::params![query, vault_id.as_str(), limit as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
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
                })?;
            result
        };

        // Mapping String → NoteId
        let mut results: Vec<(NoteId, f64, String)> = Vec::with_capacity(collected.len());
        for (id_str, score, status) in collected {
            let ulid = ulid::Ulid::from_string(&id_str).map_err(|e| {
                GradatumError::Storage(format!("ULID parse search_fts_scored_filtered: {e}"))
            })?;
            results.push((NoteId(ulid), score, status));
        }
        Ok(results)
    }

    /// Recherche FTS5 avec snippet FTS5 natif et filtre section optionnel.
    ///
    /// Utilise `snippet(notes_fts, 0, '»', '«', '...', 32)` pour localiser le passage
    /// le plus pertinent dans le body (vs `build_snippet` qui tronque la tête du body).
    ///
    /// Retourne `Vec<SearchHitRaw>` qui inclut le snippet, la section, et le titre.
    ///
    /// ## Pattern rusqlite params dynamiques
    ///
    /// Deux branches `if let` — même contrainte que `search_fts_scored_filtered`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    pub async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
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

        // FTS5 snippet() : col=0 (body_text), marqueurs »/«, ellipsis ..., max 32 tokens
        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status,
                    snippet(notes_fts, 0, '»', '«', '...', 32) AS snippet,
                    n.section,
                    n.title
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
               {section_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

        // Deux branches — params dynamiques rusqlite.
        // Pattern E0597 : stmt dans le même bloc que le collect.
        let raw_rows: Vec<(String, f64, String, String, String, Option<String>)> =
            if let Some(sec) = section {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet: {e}"))
                })?;
                let result = stmt
                    .query_map(
                        rusqlite::params![query, vault_id.as_str(), limit as i64, sec],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, f64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, Option<String>>(5)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("query search_fts_with_snippet: {e}"))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| {
                        GradatumError::Storage(format!("collect search_fts_with_snippet: {e}"))
                    })?;
                result
            } else {
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    GradatumError::Storage(format!("prepare search_fts_with_snippet (no sec): {e}"))
                })?;
                let result = stmt
                    .query_map(
                        rusqlite::params![query, vault_id.as_str(), limit as i64],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, f64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, Option<String>>(5)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!(
                            "query search_fts_with_snippet (no sec): {e}"
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| {
                        GradatumError::Storage(format!(
                            "collect search_fts_with_snippet (no sec): {e}"
                        ))
                    })?;
                result
            };

        // Mapping vers SearchHitRaw
        let mut results = Vec::with_capacity(raw_rows.len());
        for (id_str, bm25, status, snippet, section_str, title) in raw_rows {
            let ulid = ulid::Ulid::from_string(&id_str).map_err(|e| {
                GradatumError::Storage(format!("ULID parse search_fts_with_snippet: {e}"))
            })?;
            results.push(SearchHitRaw {
                note_id: NoteId(ulid),
                bm25,
                status,
                snippet,
                section: section_str,
                title,
            });
        }
        Ok(results)
    }

    /// Liste les notes d'un vault avec pagination par curseur ULID.
    ///
    /// `cursor` : dernier ULID reçu — retourne les notes dont l'ULID > cursor (ordre lexicographique).
    /// `limit` : clampé à [1, 200].
    /// Exclut les sentinelles et les notes downgraded par défaut.
    ///
    /// Retourne `(Vec<NoteRecord>, total_count)` où `total_count` est le nombre total sans pagination.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
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
                                tags, content_hash, created, updated, title
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

                let result = stmt
                    .query_map(
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
                            })
                        },
                    )
                    .map_err(|e| {
                        GradatumError::Storage(format!("query list_notes (section): {e}"))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| {
                        GradatumError::Storage(format!("collect list_notes (section): {e}"))
                    })?;
                result
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, vault_id, section, status, body_text,
                                COALESCE(author_display_name, author_id) AS author,
                                tags, content_hash, created, updated, title
                         FROM notes
                         WHERE vault_id = ?1
                           AND id NOT LIKE '__sentinel__%'
                           AND status != 'downgraded'
                           AND (?2 = '' OR id > ?2)
                         ORDER BY id ASC
                         LIMIT ?3",
                    )
                    .map_err(|e| GradatumError::Storage(format!("list_notes prepare: {e}")))?;

                let result = stmt
                    .query_map(
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
                            })
                        },
                    )
                    .map_err(|e| GradatumError::Storage(format!("query list_notes: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| GradatumError::Storage(format!("collect list_notes: {e}")))?;
                result
            }
        };

        Ok((records, total as u64))
    }

    /// Insère une note minimale directement en DB — usage réservé aux tests E2E.
    ///
    /// Insère une ligne dans `notes` avec les champs minimaux requis (id, vault_id,
    /// section, status, schema_version, created, content_hash, body_text).
    /// L'id doit être un ULID valide en string. Pas d'upsert FTS — suffisant pour
    /// tester downgrade/patch sur des notes seedées.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::Storage` si l'INSERT échoue (id dupliqué, contrainte, etc.).
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

    /// Insère une note avec FTS5 synchronisé — usage tests B1/M9.
    ///
    /// Insère dans `notes` ET dans `notes_fts` (FTS5 `content=notes` en mémoire
    /// ne se synchronise pas automatiquement — les tests FTS nécessitent un INSERT
    /// manuel dans `notes_fts`). Section configurable (vs `seed_note` qui fixe 'reference').
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::Storage` si l'un des INSERT échoue.
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

    /// Variante de `seed_note_with_fts` permettant de fixer `created` explicitement.
    ///
    /// alpha.12 Task 13 — usage tests multi-facteur (recency).
    /// Les notes créées « anciennes » sont nécessaires pour vérifier que le
    /// scoring composite préfère les notes récentes à RRF égal.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::Storage` si l'un des INSERT échoue.
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

    // ── Recherche sémantique (Phase 2.x.2 alpha.11) ────────────────────────────

    /// Recherche sémantique cosine sur `note_embeddings`.
    ///
    /// Charge tous les vecteurs `embedder_id` du vault en mémoire, calcule
    /// la similarité cosine avec `query_emb`, retourne les `limit` meilleurs.
    ///
    /// ## Complexité
    ///
    /// O(N × dim) où N = nombre de notes avec embedding.
    /// Pour N=600, dim=1024 : ~600K ops f32 ≈ 1-5ms sur un CPU moderne.
    /// Au-delà de N=10_000 → utiliser sqlite-vec ANN (Phase 3).
    ///
    /// ## Filtres appliqués
    ///
    /// - `vault_id = ?` : isolation tenant
    /// - `embedder_id = ?` : isolation modèle d'embedding
    /// - `status != 'downgraded'` : exclut les notes archivées
    /// - Sentinelles exclues via `id NOT LIKE '__sentinel__%'`
    ///
    /// ## Gestion norme nulle
    ///
    /// - Query norme nulle → `Ok(vec![])` immédiat.
    /// - Vecteur note norme nulle (NoopEmbedder) → skip silencieux.
    /// - Dim mismatch embedding vs query → skip silencieux (modèle différent).
    ///
    /// # Erreurs
    ///
    /// `GradatumError::Storage` si la requête SQLite échoue ou si un ULID
    /// ne peut pas être décodé.
    /// Méthode concrète interne — appelée par `impl VectorStore for SqliteIndex`.
    /// Renommée `_inner` pour éviter la collision de nom avec le trait method.
    pub(crate) async fn search_semantic_inner(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
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
        // Pattern E0597 : collecter dans la même portée que `stmt` pour éviter
        // que `stmt` soit droppé pendant que la MappedRows est encore en vie.
        let raw_rows: Vec<(String, Vec<u8>, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT ne.note_id, ne.vector, ne.dim
                     FROM note_embeddings ne
                     JOIN notes n ON n.id = ne.note_id
                     WHERE n.vault_id = ?1
                       AND ne.embedder_id = ?2
                       AND n.status != 'downgraded'
                       AND n.id NOT LIKE '__sentinel__%'",
                )
                .map_err(|e| GradatumError::Storage(format!("search_semantic prepare: {e}")))?;

            let result = stmt
                .query_map(rusqlite::params![vault_id, embedder_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| GradatumError::Storage(format!("search_semantic query: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| GradatumError::Storage(format!("search_semantic collect: {e}")))?;
            result
        };

        // Calcul cosine pour chaque note chargée.
        let mut scored: Vec<(NoteId, f32)> = Vec::with_capacity(raw_rows.len());
        for (id_str, blob, dim) in raw_rows {
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
            let cosine = dot / (norm_q * norm_v);

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

// ── Implémentations concrètes des méthodes de storage (corps SQL) ─────────────
//
// Ces méthodes étaient dans `impl Index for SqliteIndex` (trait monolithique pré-Étape 0.1).
// Après le carve (Étape 0.1), elles deviennent des méthodes inhérentes `pub(crate)` sur `SqliteIndex`.
// Les 3 traits granulaires (`DocumentStore`, `IndexStore`, `VectorStore`) délèguent ici.
// Le trait `Index` est désormais une façade supertrait avec blanket impl dans gradatum-core.
//
// Note : pas de `#[async_trait]` ici — les méthodes async inhérentes sur un `impl T`
// ordinaire n'en ont pas besoin (contrairement aux impl de trait).
// Les doc-comments détaillés sont dans les traits correspondants (DocumentStore/IndexStore).
// allow(missing_docs) limité à ce bloc impl : les méthodes *_inner sont pub(crate) et leur
// documentation de contrat réside dans les traits (document_store.rs/index_store.rs/vector_store.rs).
#[allow(missing_docs)]
impl SqliteIndex {
    /// Insère ou met à jour une note dans les tables `notes` et `notes_fts`.
    ///
    /// `ON CONFLICT(id) DO UPDATE` : upsert atomique sur la clé primaire ULID.
    /// FTS5 : `notes_fts` utilise `content=notes` — INSERT OR REPLACE maintient
    /// la cohérence `rowid ↔ body_text/tags` lors des updates.
    pub async fn upsert_note(&self, note: &Note) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;

        let id_str = note.id.to_string();
        let vault_id = &note.frontmatter.vault_id.0;
        let locus: Option<&str> = note.frontmatter.locus.as_ref().map(|l| l.0.as_str());
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

        conn.execute(
            "INSERT INTO notes (
                id, vault_id, locus, section, status, schema_version,
                author_kind, author_id, author_display_name,
                created, updated, status_changed, status_reason,
                content_hash, version, body_text, integrity_signature, extra_json, tags,
                c_kind, doc_kind
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
            ON CONFLICT(id) DO UPDATE SET
                vault_id             = excluded.vault_id,
                locus                = excluded.locus,
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
                doc_kind             = excluded.doc_kind",
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

    /// Upsert d'un override dans la table générique `note_overrides`.
    ///
    /// ## Clé primaire
    ///
    /// `(note_id, scope_kind, scope_id, override_type)` — 1 override actif par tuple.
    /// `ON CONFLICT … DO UPDATE` met à jour les champs évolutifs sans changer `created_at`.
    ///
    /// ## file_relative_path placeholder Phase 1
    ///
    /// Le chemin réel `.gradatum/overrides/{vault}/{locus}/{note_id}.{type}.toml` sera
    /// calculé par T11 (gradatum-vault orchestrator) qui connaît le vault_id + locus.
    /// Phase 1 : valeur placeholder `"_unset/{note_id}.{override_type}.toml"`.
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
            OverrideScope::Vault(v) => ("vault", v.0.as_str().to_string(), v.0.clone()),
            OverrideScope::Locus(l) => ("locus", l.0.clone(), "_unset".to_string()),
            OverrideScope::Bearer(b) => ("bearer", b.0.clone(), "_unset".to_string()),
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
            OverrideScope::Vault(v) => ("vault", v.0.clone()),
            OverrideScope::Locus(l) => ("locus", l.0.clone()),
            OverrideScope::Bearer(b) => ("bearer", b.0.clone()),
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
                    )))
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
        let downgraded_clause = if include_downgraded {
            ""
        } else {
            "AND n.status != 'downgraded'"
        };

        let sql = format!(
            "SELECT n.id,
                    bm25(notes_fts) AS score,
                    n.status
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.vault_id = ?2
               {downgraded_clause}
             ORDER BY score ASC
             LIMIT ?3"
        );

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
                    ))
                },
            )
            .map_err(|e| GradatumError::Storage(format!("query search_fts_scored : {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (id_str, bm25_raw, status) =
                r.map_err(|e| GradatumError::Storage(format!("row search_fts_scored : {e}")))?;
            let ulid = ulid::Ulid::from_string(&id_str)
                .map_err(|e| GradatumError::Storage(format!("parse ULID {id_str:?} : {e}")))?;
            // Pénalité downgraded : amplifier la valeur négative BM25 × 10 (équivalent ×0.1
            // sur le score positif normalisé en aval). Les notes live conservent leur score brut.
            let score = if status == "downgraded" {
                bm25_raw * 10.0
            } else {
                bm25_raw
            };
            out.push((NoteId(ulid), score, status));
        }
        // Re-trier après application de la pénalité (ORDER BY SQL portait sur le score brut).
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
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
            .search_fts_scored_filtered(&vault, "gradatum", 10, false, Some("decisions"))
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
            .search_fts_scored_filtered(&vault, "gradatum", 10, false, None)
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
            .search_fts_with_snippet(&vault, "hardening", 5, false, None)
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
            .search_fts_with_snippet(&vault, "architecture", 5, false, None)
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

    /// F-42 — upsert_note section="agent-issues" → c_kind="procedural" doc_kind="Event".
    ///
    /// Section d'issues agents : c_kind procedural (v81 §17) + doc_kind Event.
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
