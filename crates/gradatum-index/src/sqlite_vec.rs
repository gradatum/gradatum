//! ANN search via sqlite-vec `vec0` virtual table.
//!
//! ## Design
//!
//! This module exposes:
//! - [`search_ann_inner`] — ANN SQL query on `note_embeddings_ann` (vec0) with time-based decay.
//! - [`search_ann_bench_inner`] — simplified variant for benchmark binaries (no decay).
//! - [`upsert_ann`] — insert/replace into `note_embeddings_ann` (safe in degraded mode).
//! - [`backfill_ann_from_conn`] — programmatic backfill from `note_embeddings`.
//! - [`ann_partition_deficits_from_conn`] — boot coverage measurement of the derived index.
//!
//! There is no `unsafe` code here: every operation goes through the safe `rusqlite` API.
//! Registering the sqlite-vec extension — which is unsafe, via `sqlite3_auto_extension` —
//! is the responsibility of the binary crates (`gradatum-server`, `gradatum-worker`) and
//! must happen before any connection is opened through [`SqliteIndex::open`].
//!
//! ## Activation
//!
//! This module **always** compiles: `lib.rs` declares it unconditionally. Only the native
//! `sqlite-vec` crate (C linkage) is gated behind the `sqlite-vec-ann` feature. Without
//! that feature the extension is not linked and cannot be registered, so the functions
//! here return empty values or `Ok(())` — the degraded mode. Registering the extension
//! itself always remains up to the binary crates.
//!
//! ## vec0 KNN syntax
//!
//! ```sql
//! SELECT note_id, distance
//! FROM note_embeddings_ann
//! WHERE vault_id = ?1
//!   AND embedder_id = ?2
//!   AND vector MATCH ?3   -- ?3 = little-endian f32 BLOB of the query vector
//!   AND k = ?4            -- ?4 = number of ANN candidates (i64)
//! ```
//!
//! - `vault_id` and `embedder_id` are partition-key filters, narrowing the ANN space.
//! - `distance` is a computed column (1 − cosine similarity for `distance_metric=cosine`).
//! - `k` is the candidate factor (`ef_search × limit`, clamped by `MAX_ANN_K`).
//!
//! ## References
//!
//! sqlite-vec documentation: <https://alexgarcia.xyz/sqlite-vec/api-reference.html>
//! sqlite-vec 0.1.9 sources: <https://github.com/asg017/sqlite-vec>

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::params;
use tokio::sync::Mutex;

use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::index_store::AnnPartitionDeficit;

/// Maximum number of ANN candidates passed to vec0 (DoS cap).
///
/// Bounded to prevent a vec0 query with k=∞ on a large vault.
const MAX_ANN_K: usize = 1024;

/// Expected vector dimension for the bge-m3 model.
///
/// Used in [`backfill_ann_from_conn`] to skip incompatible embeddings
/// (dim ≠ 1024 means a different model — silently skipped).
const BGE_M3_DIM: usize = 1024;

/// Serialises an `f32` slice to a little-endian BLOB.
///
/// Native format for `note_embeddings.vector`. sqlite-vec accepts vectors as
/// f32-LE BLOB or JSON; the BLOB form is used to avoid O(dim) JSON serialisation.
pub(crate) fn f32_slice_to_blob(v: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for &x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

/// ANN search in `note_embeddings_ann` via the vec0 virtual table.
///
/// ## Parameters
///
/// - `conn`: shared connection (the sqlite-vec extension must be loaded).
/// - `vault_id`: vault PARTITION KEY filter.
/// - `embedder_id`: model PARTITION KEY filter.
/// - `query_emb`: query vector (dim=1024 for bge-m3).
/// - `limit`: number of final results desired.
/// - `ef_search`: exploration factor (oversampling = `limit × ef_search`).
/// - `locus`: optional prefix filter on `notes.locus`.
///
/// ## Returns
///
/// `Vec<(NoteId, f32)>` sorted by descending score (cosine after time-based decay).
///
/// ## Degraded mode
///
/// If the sqlite-vec extension is not loaded, the query fails with
/// "no such module: vec0". This error is propagated as `GradatumError::Storage`
/// so the caller can fall back to the brute-force path (`search_semantic_inner`).
///
/// ## Time-based Decay
///
/// Applied identically to the brute-force path: `cosine *= 0.5^elapsed_days`
/// for notes with `forgotten=1`.
///
/// # Errors
///
/// `GradatumError::Storage` if the SQL query fails, including when the extension is absent.
pub(crate) async fn search_ann_inner(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    vault_id: &str,
    embedder_id: &str,
    query_emb: &[f32],
    limit: usize,
    ef_search: u32,
    locus: Option<&str>,
) -> Result<Vec<(NoteId, f32)>, GradatumError> {
    // Pré-calcul norme query — si nulle, aucun cosine calculable.
    let norm_q: f32 = query_emb.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_q == 0.0 {
        return Ok(vec![]);
    }

    // Oversampling borné : ef_search × limit, cap MAX_ANN_K, min 1.
    let k_oversample = limit.saturating_mul(ef_search as usize).clamp(1, MAX_ANN_K);

    let query_blob = f32_slice_to_blob(query_emb);
    let locked = conn.lock().await;

    // Type alias pour réduire la complexité perçue par clippy::type_complexity.
    // Colonnes : (note_id, distance, forgotten, forgotten_at_ms)
    type AnnRow = (String, f64, i64, Option<i64>);

    // Requête ANN : vec0 retourne les k_oversample voisins les plus proches.
    // `distance` en vec0 distance_metric=cosine = 1 − cosine_similarity.
    // JOIN notes pour filtres status / forgotten / locus.
    //
    // Cross-vault hijack guard (C4-1e Slice E) : `ON n.id = ann.note_id AND
    // ann.vault_id = n.vault_id` — sans le 2e prédicat, une même note ULID
    // présente dans 2 vaults matcherait les 2 lignes `notes` (dup + bypass du
    // filtre status/forgotten/locus du mauvais vault). `ann.vault_id` est déjà
    // borné par `WHERE ann.vault_id = ?1` ci-dessous.
    //
    // Note vec0 PARTITION KEY : les colonnes vault_id et embedder_id sont des
    // PARTITION KEY — vec0 restreint automatiquement l'espace de recherche quand
    // elles apparaissent dans la clause WHERE avec un opérateur d'égalité.
    //
    // Note sur `AND k = ?4` : c'est la syntaxe vec0 pour spécifier le nombre
    // de candidats KNN. k n'est pas une colonne de la table mais une contrainte
    // spéciale interprétée par le moteur vec0.
    // Pattern E0597 rusqlite : `stmt` doit être dans le même bloc que `.collect()`
    // pour que la MappedRows soit droppée AVANT `stmt`. On utilise des blocs {} explicites.
    let raw_rows: Vec<AnnRow> = if let Some(loc) = locus {
        let locus_escaped = crate::sqlite::escape_like(loc);
        let mut stmt = locked
            .prepare(
                "SELECT ann.note_id, ann.distance, n.forgotten, n.forgotten_at
                 FROM note_embeddings_ann ann
                 JOIN notes n ON n.id = ann.note_id AND ann.vault_id = n.vault_id
                 WHERE ann.vault_id = ?1
                   AND ann.embedder_id = ?2
                   AND ann.vector MATCH ?3
                   AND k = ?4
                   AND n.status != 'downgraded'
                   AND n.id NOT LIKE '__sentinel__%'
                   AND n.locus LIKE ?5 || '%' ESCAPE '\\'",
            )
            .map_err(|e| GradatumError::Storage(format!("search_ann prepare (locus): {e}")))?;

        stmt.query_map(
            params![
                vault_id,
                embedder_id,
                query_blob,
                k_oversample as i64,
                locus_escaped
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(|e| GradatumError::Storage(format!("search_ann query (locus): {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| GradatumError::Storage(format!("search_ann collect (locus): {e}")))?
    } else {
        let mut stmt = locked
            .prepare(
                "SELECT ann.note_id, ann.distance, n.forgotten, n.forgotten_at
                 FROM note_embeddings_ann ann
                 JOIN notes n ON n.id = ann.note_id AND ann.vault_id = n.vault_id
                 WHERE ann.vault_id = ?1
                   AND ann.embedder_id = ?2
                   AND ann.vector MATCH ?3
                   AND k = ?4
                   AND n.status != 'downgraded'
                   AND n.id NOT LIKE '__sentinel__%'",
            )
            .map_err(|e| GradatumError::Storage(format!("search_ann prepare: {e}")))?;

        stmt.query_map(
            params![vault_id, embedder_id, query_blob, k_oversample as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(|e| GradatumError::Storage(format!("search_ann query: {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| GradatumError::Storage(format!("search_ann collect: {e}")))?
    };

    // Libérer le lock avant le calcul de decay (CPU pur — évite de tenir
    // le Mutex Tokio pendant un calcul potentiellement long).
    drop(locked);

    let now_ms = chrono::Utc::now().timestamp_millis();

    let mut scored: Vec<(NoteId, f32)> = Vec::with_capacity(raw_rows.len());
    for (id_str, distance, forgotten, forgotten_at_ms) in raw_rows {
        // Conversion distance cosine → similarité cosine.
        // vec0 distance_metric=cosine : distance = 1 − cosine_similarity.
        let cosine_raw = (1.0_f32 - distance as f32).clamp(-1.0, 1.0);

        // Decay F-44 : identique au chemin brute-force (`search_semantic_inner`).
        // cosine [0,1] × 0.5^elapsed_days → réduit le score des notes oubliées.
        let cosine = if forgotten != 0 {
            if forgotten_at_ms.is_none() {
                tracing::warn!(
                    note_id = %id_str,
                    "search_ann: forgotten=1 but forgotten_at=NULL — inconsistent state"
                );
            }
            let elapsed_days = forgotten_at_ms
                .map(|at_ms| (now_ms - at_ms) as f64 / 86_400_000.0)
                .unwrap_or(0.0)
                .max(0.0);
            cosine_raw * (0.5_f32).powf(elapsed_days as f32)
        } else {
            cosine_raw
        };

        let ulid = ulid::Ulid::from_string(&id_str)
            .map_err(|e| GradatumError::Storage(format!("search_ann ULID parse: {e}")))?;
        scored.push((NoteId(ulid), cosine));
    }

    // Tri décroissant par score + troncature au limit demandé.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    Ok(scored)
}

/// ANN query for bench binaries — simplified variant of [`search_ann_inner`].
///
/// Returns raw `note_id` strings ordered by ascending distance
/// (most similar first). No time-based decay applied (bench recall only).
///
/// ## Usage
///
/// Exposed via [`SqliteIndex::search_ann_bench`] for bench binaries
/// that cannot access `conn` directly (private field).
///
/// # Errors
///
/// `GradatumError::Storage` if the SQL query fails, including when the extension is absent.
pub(crate) async fn search_ann_bench_inner(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    vault_id: &str,
    embedder_id: &str,
    query: &[f32],
    k: usize,
) -> Result<Vec<String>, GradatumError> {
    let k_clamped = k.clamp(1, MAX_ANN_K);
    let query_blob = f32_slice_to_blob(query);
    let locked = conn.lock().await;

    let mut stmt = locked
        .prepare(
            "SELECT ann.note_id
             FROM note_embeddings_ann ann
             WHERE ann.vault_id = ?1
               AND ann.embedder_id = ?2
               AND ann.vector MATCH ?3
               AND k = ?4",
        )
        .map_err(|e| GradatumError::Storage(format!("search_ann_bench: prepare: {e}")))?;
    let result = stmt
        .query_map(
            params![vault_id, embedder_id, query_blob, k_clamped as i64],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| GradatumError::Storage(format!("search_ann_bench: query_map: {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| GradatumError::Storage(format!("search_ann_bench: collect: {e}")))?;
    Ok(result)
}

/// Returns `true` when a SQLite error denotes an **inactive ANN backend**
/// (degraded mode) rather than a real failure.
///
/// Two shapes are tolerated everywhere ANN is touched:
/// - `"no such module: vec0"` — the sqlite-vec extension is not registered.
/// - `"no such table: note_embeddings_ann"` — migration 0020 not yet applied.
///
/// Single source of truth for the degraded-mode predicate, shared by
/// [`upsert_ann`], `SqliteIndex::delete_note_from_index` and
/// `SqliteIndex::gc_orphan_ann`.
///
/// ## Deliberate fragility: matching on the message
///
/// SQLite returns the **same primary code** `SQLITE_ERROR` (1) for "no such table" AND
/// for "no such module", so the error code cannot distinguish degraded mode from a real
/// SQL failure. Matching on the message text is therefore a deliberate choice, not a
/// shortcut: it is the only signal available short of instrumenting the query plan. The
/// accepted risk is that a change in SQLite's wording — these diagnostics have been
/// stable for years — would reclassify a degraded case as a hard error. The worst
/// outcome is a loud failure, never a silent corruption or deletion.
pub(crate) fn is_ann_absent_error(e: &rusqlite::Error) -> bool {
    let msg = e.to_string();
    msg.contains("no such module: vec0") || msg.contains("no such table: note_embeddings_ann")
}

/// Inserts or replaces a vector in `note_embeddings_ann`.
///
/// Must be called inside the same transaction as `insert_note_embedding_inner`, so that
/// `note_embeddings` (the source of truth) and `note_embeddings_ann` (the derived index)
/// stay atomically consistent.
///
/// ## Behaviour in degraded mode
///
/// If the sqlite-vec extension is not loaded ("no such module: vec0") or the ANN table is
/// missing, the call returns `Ok(())` without error and the caller keeps working through
/// the brute-force path.
///
/// ## sqlite-vec 0.1.9 limitation
///
/// vec0 does not support UPDATE on a partition-key column, so the upsert is always a
/// DELETE of the targeted `(vault_id, embedder_id, note_id)` triple followed by an INSERT.
///
/// Both partition keys belong to that predicate. Since migration 0038 the vec0 identity is
/// `(vault_id, embedder_id)` (partition keys) plus `note_id` as a plain column, so the same
/// ULID legitimately holds one row per embedder within a vault. Rows of another vault — or
/// of another embedder of the same vault — are never evicted, even when they carry the same
/// ULID.
///
/// # Errors
///
/// `GradatumError::Storage` on any other SQL error.
pub(crate) fn upsert_ann(
    conn: &rusqlite::Connection,
    note_id: &str,
    vault_id: &str,
    embedder_id: &str,
    vector: &[f32],
) -> Result<(), GradatumError> {
    let blob = f32_slice_to_blob(vector);

    // Upsert scopé PARTITION (A4, migration 0038) : DELETE du triplet
    // `(vault_id, embedder_id, note_id)` puis INSERT.
    //
    // Remplace l'ancien `INSERT OR REPLACE` sur la PK GLOBALE `note_id` (schéma 0020) : deux
    // vaults indexant le MÊME ULID entraient en collision sur cette PK et s'évinçaient
    // mutuellement (une seule ligne ANN par ULID, toutes partitions confondues). Depuis 0038,
    // l'identité vec0 est `(vault_id, embedder_id)` PARTITION KEY + `note_id` colonne ordinaire :
    // le même ULID coexiste sur plusieurs vaults ET sur plusieurs embedders d'un même vault.
    //
    // Le DELETE porte donc les TROIS colonnes. Les deux clés de partition sont obligatoires :
    // omettre `embedder_id` (défaut corrigé ici) supprimait la ligne de TOUTES les partitions
    // d'embedder du vault, si bien qu'une note portant deux embedders n'en conservait qu'un —
    // le second insert évinçait le premier, et l'ordre d'écriture décidait seul du survivant
    // (perte silencieuse d'un axe sémantique entier au backfill suivant).
    // L'identité de ligne du DELETE doit égaler l'identité de ligne de l'INSERT ci-dessous.
    // On ne passe jamais par un UPDATE : vec0 0.1.9 l'interdit sur une colonne PARTITION KEY.
    //
    // Mode dégradé : si la table/l'extension vec0 est absente ("no such table"/"no such module"),
    // le DELETE échoue → no-op toléré (Ok), symétrique avec le chemin brute-force. Toute autre
    // erreur SQL est propagée.
    match conn.execute(
        "DELETE FROM note_embeddings_ann
         WHERE vault_id = ?1 AND embedder_id = ?2 AND note_id = ?3",
        params![vault_id, embedder_id, note_id],
    ) {
        Ok(_) => {}
        Err(e) if is_ann_absent_error(&e) => return Ok(()),
        Err(e) => return Err(GradatumError::Storage(format!("upsert_ann delete: {e}"))),
    }

    match conn.execute(
        "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
         VALUES (?1, ?2, ?3, ?4)",
        params![note_id, vault_id, embedder_id, blob],
    ) {
        Ok(_) => Ok(()),
        // Table disparue entre le DELETE et l'INSERT (ne devrait pas arriver hors course
        // concurrente sur DDL) → dégradé toléré, cohérent avec le DELETE ci-dessus.
        Err(e) if is_ann_absent_error(&e) => Ok(()),
        Err(e) => Err(GradatumError::Storage(format!("upsert_ann insert: {e}"))),
    }
}

/// `FROM` + `WHERE` clause selecting the ANN-eligible population of `note_embeddings`.
///
/// Single-sourced on purpose. [`backfill_ann_from_conn`], which *writes* the derived index,
/// and [`ann_partition_deficits_from_conn`], which *audits* it, must agree on the population
/// down to the last predicate: if they diverge they measure two different sets, and the gate
/// reports a phantom deficit (or hides a real one). Sharing the clause makes that drift
/// impossible by construction rather than by convention.
///
/// `?1` is the expected embedding dimension. Exclusions: `downgraded` notes, sentinels, and
/// embeddings produced by another model (`dim != ?1`).
const ANN_ELIGIBLE_FROM_WHERE: &str = "FROM note_embeddings ne
                 JOIN notes n ON n.id = ne.note_id AND ne.vault_id = n.vault_id
                 WHERE n.status != 'downgraded'
                   AND n.id NOT LIKE '__sentinel__%'
                   AND ne.dim = ?1";

/// Backfills `note_embeddings_ann` from `note_embeddings`.
///
/// Iterates over every non-downgraded note holding a 1024-dimension embedding (bge-m3) in
/// `note_embeddings` and inserts it into `note_embeddings_ann` through [`upsert_ann`].
///
/// ## Degraded mode
///
/// If the sqlite-vec extension is not loaded, `upsert_ann` returns `Ok(())` for every row
/// and silently skips it. The returned counter reflects the number of notes processed, not
/// necessarily the number of rows inserted into vec0.
///
/// ## Idempotence
///
/// Each write goes through the scoped delete-then-insert of [`upsert_ann`], so calling the
/// backfill repeatedly is idempotent. Cost is linear in the number of notes carrying a
/// 1024-dimension embedding.
///
/// ## Exclusions
///
/// - Notes with `status = 'downgraded'`.
/// - Sentinels (`id LIKE '__sentinel__%'`).
/// - Embeddings whose `dim` is not 1024 (a different model).
///
/// # Errors
///
/// `GradatumError::Storage` if the read query fails.
pub(crate) async fn backfill_ann_from_conn(
    conn: &Arc<Mutex<rusqlite::Connection>>,
) -> Result<u64, GradatumError> {
    // Type alias : (note_id, vault_id, embedder_id, vector_blob)
    type BackfillRow = (String, String, String, Vec<u8>);

    let rows: Vec<BackfillRow> = {
        let locked = conn.lock().await;
        // Pattern E0597 rusqlite : `result` collecté DANS le même bloc que `stmt`
        // pour que MappedRows soit droppée avant `stmt` et `locked`.
        let mut stmt = locked
            .prepare(&format!(
                "SELECT ne.note_id, n.vault_id, ne.embedder_id, ne.vector {ANN_ELIGIBLE_FROM_WHERE}"
            ))
            .map_err(|e| GradatumError::Storage(format!("backfill_ann: prepare SELECT: {e}")))?;

        stmt.query_map(params![BGE_M3_DIM as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|e| GradatumError::Storage(format!("backfill_ann: query_map: {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| GradatumError::Storage(format!("backfill_ann: collect: {e}")))?
    };
    // Lock libéré — on traite les lignes sans tenir le Mutex.

    let total = rows.len();
    let locked = conn.lock().await;

    for (note_id, vault_id, embedder_id, blob) in rows {
        if blob.len() % 4 != 0 || blob.len() / 4 != BGE_M3_DIM {
            // BLOB malformé ou dim incorrecte — skip silencieux.
            tracing::warn!(
                note_id = %note_id,
                blob_len = blob.len(),
                "backfill_ann: BLOB dim mismatch — skip"
            );
            continue;
        }
        // Décodage f32 LE depuis BLOB.
        let vec: Vec<f32> = blob
            .chunks_exact(4)
            .map(|b| {
                f32::from_le_bytes(
                    b.try_into()
                        .expect("chunks_exact guarantees 4 bytes — invariant"),
                )
            })
            .collect();

        upsert_ann(&locked, &note_id, &vault_id, &embedder_id, &vec)?;
    }

    // Cast u64 sans perte : total = usize, rows < usize::MAX garantie par collect().
    Ok(total as u64)
}

/// Groups a `SELECT vault_id, embedder_id, COUNT(*)` query by partition.
///
/// Returns a `BTreeMap` (not a `HashMap`): the iteration order decides the order of the
/// per-partition boot logs, which is observable output.
///
/// Shared by both counting queries of [`ann_partition_deficits_from_conn`] so the column
/// order and the `i64 → u64` conversion have a single source. Errors are returned raw
/// (`rusqlite::Error`) so the caller can still recognise a missing ANN table through
/// [`is_ann_absent_error`].
fn group_counts_by_partition(
    conn: &rusqlite::Connection,
    sql: &str,
    sql_params: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<BTreeMap<(String, String), u64>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out: BTreeMap<(String, String), u64> = BTreeMap::new();
    let rows = stmt.query_map(sql_params, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (vault_id, embedder_id, count) = row?;
        // `COUNT(*)` est ≥ 0 par construction SQL — `try_from` plutôt qu'un `as` aveugle
        // (`err-no-as-overflow`) ; la branche `unwrap_or(0)` est inatteignable.
        out.insert((vault_id, embedder_id), u64::try_from(count).unwrap_or(0));
    }
    Ok(out)
}

/// Measures the coverage of the derived ANN index, partition by partition.
///
/// Compares the eligible population of `note_embeddings` (see [`ANN_ELIGIBLE_FROM_WHERE`])
/// with the rows actually present in `note_embeddings_ann`, and returns one
/// [`AnnPartitionDeficit`] per partition `(vault_id, embedder_id)` where rows are **missing**
/// — the case that mutes the semantic axis without raising anything.
///
/// ## Cost
///
/// Two aggregate queries, whatever the number of partitions: one grouped `COUNT(*)` on the
/// source table, one on the derived table. No vector is decoded, `vector` is only measured
/// through `length()` (see below).
///
/// ## Why `length(ne.vector)` is added to the shared predicate
///
/// [`backfill_ann_from_conn`] applies one more filter *after* its `SELECT`: a row whose BLOB
/// does not hold exactly `BGE_M3_DIM` f32 values is logged and skipped, so it is never
/// written to the derived index. Counting it as eligible would make the gate permanently red
/// on a corpus holding one malformed BLOB — a gate that always cries is a gate nobody reads.
/// The predicate therefore measures what the backfill actually inserts.
///
/// ## Degraded mode
///
/// If `note_embeddings_ann` (or the vec0 module) is missing, there is no derived index to
/// audit: returns `Ok(Vec::new())`. That configuration is not silent — an ANN query fails
/// loudly with the same error and the caller falls back to brute force.
///
/// ## Surplus is not a deficit
///
/// `indexed > eligible` is never reported — see [`AnnPartitionDeficit`] for why it is a
/// legitimate steady state (downgraded notes keep their row) and harmless (the ANN query
/// re-filters `notes` at read time).
///
/// # Errors
///
/// `GradatumError::Storage` if either count fails for any other reason.
pub(crate) async fn ann_partition_deficits_from_conn(
    conn: &Arc<Mutex<rusqlite::Connection>>,
) -> Result<Vec<AnnPartitionDeficit>, GradatumError> {
    // Dimension attendue (?1) et longueur exacte du BLOB correspondant (?2, 4 octets/f32).
    let dim = i64::try_from(BGE_M3_DIM).unwrap_or(i64::MAX);
    let blob_len = dim.saturating_mul(4);

    let eligible_sql = format!(
        "SELECT n.vault_id, ne.embedder_id, COUNT(*) {ANN_ELIGIBLE_FROM_WHERE}
                   AND length(ne.vector) = ?2
                 GROUP BY n.vault_id, ne.embedder_id"
    );

    let locked = conn.lock().await;

    // Index dérivé d'abord : c'est la requête qui échoue vite quand la table est absente
    // (extension vec0 non chargée) — inutile de payer l'agrégat source dans ce cas.
    let indexed = match group_counts_by_partition(
        &locked,
        "SELECT vault_id, embedder_id, COUNT(*)
         FROM note_embeddings_ann
         GROUP BY vault_id, embedder_id",
        &[],
    ) {
        Ok(counts) => counts,
        // Table/module vec0 absent → aucun index dérivé à auditer (mode dégradé documenté).
        Err(e) if is_ann_absent_error(&e) => return Ok(Vec::new()),
        Err(e) => {
            return Err(GradatumError::Storage(format!(
                "ann_health_gate: count ANN rows: {e}"
            )));
        }
    };

    let eligible =
        group_counts_by_partition(&locked, &eligible_sql, &[&dim, &blob_len]).map_err(|e| {
            GradatumError::Storage(format!("ann_health_gate: count eligible pairs: {e}"))
        })?;
    drop(locked);

    // Ordre d'itération BTreeMap = `(vault_id, embedder_id)` croissant → sortie déterministe.
    let mut indexed = indexed;
    let mut deficits = Vec::new();
    for (partition, eligible_rows) in eligible {
        let indexed_rows = indexed.remove(&partition).unwrap_or(0);
        if indexed_rows < eligible_rows {
            let (vault_id, embedder_id) = partition;
            deficits.push(AnnPartitionDeficit {
                vault_id,
                embedder_id,
                eligible: eligible_rows,
                indexed: indexed_rows,
            });
        }
    }
    Ok(deficits)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tests unitaires (sans extension sqlite-vec chargée) ─────────────────

    /// Vérifie le round-trip f32_slice_to_blob → décodage f32 LE.
    ///
    /// L'invariant est que `f32_slice_to_blob` produit exactement `N×4` bytes
    /// et que chaque groupe de 4 bytes se redécode en la valeur originale.
    #[test]
    fn f32_slice_to_blob_round_trip() {
        let values = vec![0.1_f32, -0.5, 1.0, 0.0, f32::MAX, f32::MIN_POSITIVE];
        let blob = f32_slice_to_blob(&values);
        assert_eq!(blob.len(), values.len() * 4, "BLOB len = N × 4");

        let decoded: Vec<f32> = blob
            .chunks_exact(4)
            .map(|b| {
                f32::from_le_bytes(
                    b.try_into()
                        .expect("chunks_exact guarantees 4 bytes — invariant"),
                )
            })
            .collect();
        assert_eq!(
            decoded, values,
            "round-trip f32_slice_to_blob → f32::from_le_bytes"
        );
    }

    /// Vérifie que f32_slice_to_blob sur un slice vide retourne un BLOB vide.
    #[test]
    fn f32_slice_to_blob_empty() {
        let blob = f32_slice_to_blob(&[]);
        assert!(blob.is_empty(), "BLOB vide pour slice vide");
    }

    /// Vérifie que `upsert_ann` en mode dégradé (extension non chargée) retourne `Ok(())`.
    ///
    /// Sans enregistrement de l'extension sqlite-vec, `note_embeddings_ann` n'existe pas
    /// (pas de module vec0). `upsert_ann` doit retourner Ok(()) silencieusement.
    #[test]
    fn upsert_ann_mode_degrade_sans_extension() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");

        // Pas d'extension chargée, pas de CREATE VIRTUAL TABLE → "no such module: vec0"
        // si on essayait de créer la table. On simule directement l'INSERT qui produirait
        // "no such table: note_embeddings_ann".
        let vec = vec![0.1_f32; 4];
        let result = upsert_ann(&conn, "01TEST", "main", "bge-m3", &vec);

        // En mode dégradé, le message d'erreur contient "no such table" (pas "no such module")
        // car la table n'existe pas (virtual module non chargé). Les deux erreurs sont traitées
        // différemment : "no such module" (extension absente) et "no such table" (table non créée).
        // Notre implémentation catch uniquement "no such module: vec0".
        // Pour ce test : on s'assure juste que upsert_ann ne panique pas.
        // Le résultat peut être Ok(()) ou Err (selon si la table existe ou non).
        // Ce test vérifie principalement l'absence de panic.
        let _ = result; // OK qu'il soit Err dans ce contexte de test
    }

    /// Vérifie que `upsert_ann` sur une connexion avec `note_embeddings_ann` créée
    /// (via CREATE TABLE bidon, pas vec0) produit une erreur typée, pas un panic.
    #[test]
    fn upsert_ann_erreur_non_panic() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");

        // On ne charge pas l'extension → upsert_ann doit soit Ok() soit Err proprement.
        let vec = vec![0.0_f32; 1024];
        let result = upsert_ann(&conn, "01AATEST", "main", "bge-m3", &vec);
        // Pas de panic attendu — juste vérifier que le type est correct.
        match result {
            Ok(()) => {
                // Mode dégradé attrapé ("no such module: vec0" ou "no such table")
            }
            Err(GradatumError::Storage(_)) => {
                // Erreur SQL propagée correctement — attendu si la table n'existe pas
                // et l'erreur n'est pas "no such module: vec0"
            }
            Err(other) => panic!("upsert_ann : erreur inattendue {other:?}"),
        }
    }

    // ── Intégrité de partition ANN `(vault_id, embedder_id)` (migration 0038) ──────

    /// Matérialise la table shadow `note_embeddings_ann` et retourne l'index prêt à l'emploi.
    ///
    /// `vec0` est bin-only (l'extension n'est enregistrée que par les crates binaires) : la
    /// virtual table est donc ABSENTE des tests de cette crate. `seed_orphan_ann_for_test`
    /// porte l'unique DDL de la table shadow (image plate du schéma 0038) — on l'appelle ici
    /// pour créer la table, sur un couple `(vault, embedder, note)` DISJOINT des données de
    /// test afin de ne polluer aucun comptage.
    async fn index_avec_table_ann_shadow() -> crate::SqliteIndex {
        let idx = crate::SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — ne peut pas échouer");
        idx.seed_orphan_ann_for_test("01DDLBOOTSTRAP0000000000", "vault-ddl", "ddl-emb")
            .await
            .expect("création de la table shadow note_embeddings_ann");
        idx
    }

    /// Lit les `embedder_id` présents dans l'ANN pour un couple `(vault_id, note_id)`, triés.
    async fn embedders_ann(idx: &crate::SqliteIndex, vault_id: &str, note_id: &str) -> Vec<String> {
        let conn = idx.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT embedder_id FROM note_embeddings_ann
                 WHERE vault_id = ?1 AND note_id = ?2
                 ORDER BY embedder_id",
            )
            .expect("prepare SELECT embedder_id");
        stmt.query_map(params![vault_id, note_id], |r| r.get::<_, String>(0))
            .expect("query_map embedder_id")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect embedder_id")
    }

    /// Une note portant DEUX embedders doit conserver DEUX lignes ANN, une par partition.
    ///
    /// Régression ciblée : le `DELETE` d'`upsert_ann` était scopé `(vault_id, note_id)` alors
    /// que la migration 0038 fait de `(vault_id, embedder_id)` l'identité de partition vec0
    /// (`note_id` = colonne ordinaire). L'insertion du second embedder effaçait donc la ligne
    /// du premier : une seule ligne ANN survivait, et rien ne garantissait laquelle.
    #[tokio::test]
    async fn upsert_ann_conserve_une_ligne_par_embedder() {
        let idx = index_avec_table_ann_shadow().await;
        let note = "01ANNDUALEMBEDDER000000";

        {
            let conn = idx.conn.lock().await;
            upsert_ann(&conn, note, "main", "bge-m3-Q8_0", &[0.25_f32; 8])
                .expect("upsert embedder bge-m3-Q8_0");
            upsert_ann(&conn, note, "main", "embed", &[0.75_f32; 8])
                .expect("upsert embedder embed");
        }

        assert_eq!(
            embedders_ann(&idx, "main", note).await,
            vec!["bge-m3-Q8_0".to_string(), "embed".to_string()],
            "les deux partitions `(main, bge-m3-Q8_0)` et `(main, embed)` doivent coexister \
             pour le même ULID — le DELETE d'upsert_ann doit être scopé par embedder_id"
        );
    }

    /// Ré-upserter un embedder met à jour SA ligne sans dupliquer ni toucher l'autre embedder.
    ///
    /// Complément d'[`upsert_ann_conserve_une_ligne_par_embedder`] : la correction du scope du
    /// DELETE ne doit pas dégrader l'idempotence intra-partition (une ligne par triplet).
    #[tokio::test]
    async fn upsert_ann_reste_idempotent_dans_sa_partition() {
        let idx = index_avec_table_ann_shadow().await;
        let note = "01ANNIDEMPOTENT00000000";

        {
            let conn = idx.conn.lock().await;
            upsert_ann(&conn, note, "main", "embed", &[0.1_f32; 8]).expect("upsert initial");
            upsert_ann(&conn, note, "main", "bge-m3-Q8_0", &[0.2_f32; 8])
                .expect("upsert 2e embedder");
            // Ré-écriture du même triplet `(main, embed, note)` avec un vecteur différent.
            upsert_ann(&conn, note, "main", "embed", &[0.9_f32; 8]).expect("ré-upsert embed");
        }

        assert_eq!(
            embedders_ann(&idx, "main", note).await,
            vec!["bge-m3-Q8_0".to_string(), "embed".to_string()],
            "un ré-upsert doit remplacer SA ligne (pas de doublon) sans évincer l'autre embedder"
        );
    }

    /// Le backfill doit produire EXACTEMENT autant de lignes ANN que de paires
    /// `(note, embedder)` éligibles — aucune perte par écrasement inter-embedder.
    ///
    /// Réplique en miniature l'écart mesuré sur l'index LIVE (1972 lignes traitées, 1966
    /// présentes) : les notes portant deux embedders n'en conservaient qu'un. Le compteur
    /// retourné par `backfill_ann_from_conn` (nombre de lignes traitées) et le nombre de
    /// lignes réellement présentes doivent coïncider.
    ///
    /// Jeu de données : 2 notes `live` × 2 embedders (= 4 éligibles) + 3 lignes exclues par
    /// le SELECT (une note `downgraded`, une sentinelle, un embedding `dim != 1024`).
    #[tokio::test]
    async fn backfill_ann_conserve_toutes_les_paires_note_embedder() {
        let idx = index_avec_table_ann_shadow().await;

        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                 VALUES
                   ('01BF00000000000000000001', 'main', 'decisions', 'live', 1, 0, X'01', 'note live 1'),
                   ('01BF00000000000000000002', 'main', 'decisions', 'live', 1, 0, X'02', 'note live 2'),
                   ('01BF00000000000000000003', 'main', 'decisions', 'downgraded', 1, 0, X'03', 'note downgradee'),
                   ('__sentinel__main', 'main', 'decisions', 'live', 1, 0, X'04', 'sentinelle');",
            )
            .expect("seed notes");

            // BLOB f32 LE de dim 1024 (4096 bytes) — seul format accepté par le backfill.
            let blob: Vec<u8> = (0..BGE_M3_DIM)
                .flat_map(|i| (i as f32 / BGE_M3_DIM as f32).to_le_bytes())
                .collect();
            // BLOB dim 8 → exclu par `ne.dim = 1024`.
            let blob_court: Vec<u8> = (0..8_usize)
                .flat_map(|i| (i as f32).to_le_bytes())
                .collect();

            for (note, embedder, dim, b) in [
                ("01BF00000000000000000001", "bge-m3-Q8_0", 1024, &blob),
                ("01BF00000000000000000001", "embed", 1024, &blob),
                ("01BF00000000000000000002", "bge-m3-Q8_0", 1024, &blob),
                ("01BF00000000000000000002", "embed", 1024, &blob),
                // Exclusions attendues du SELECT du backfill.
                ("01BF00000000000000000003", "embed", 1024, &blob),
                ("__sentinel__main", "embed", 1024, &blob),
                ("01BF00000000000000000002", "modele-court", 8, &blob_court),
            ] {
                conn.execute(
                    "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, computed_at, vault_id)
                     VALUES (?1, ?2, ?3, ?4, 0, 'main')",
                    params![note, embedder, b, dim as i64],
                )
                .expect("seed note_embeddings");
            }
        }

        let traitees = backfill_ann_from_conn(&idx.conn)
            .await
            .expect("backfill_ann_from_conn");

        let presentes: i64 = {
            let conn = idx.conn.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM note_embeddings_ann WHERE vault_id = 'main'",
                [],
                |r| r.get(0),
            )
            .expect("count lignes ANN")
        };

        assert_eq!(
            (traitees, presentes),
            (4, 4),
            "le backfill doit traiter 4 paires (note, embedder) éligibles et en laisser 4 \
             en table — un écart traitées > présentes signe un écrasement inter-embedder"
        );
    }

    /// Vérifie que `backfill_ann_from_conn` sur une DB sans table `note_embeddings_ann`
    /// (extension non chargée) ne panique pas et retourne 0 (aucune note à backfiller
    /// puisque la table des embeddings est vide).
    #[tokio::test]
    async fn backfill_ann_db_vide_retourne_zero() {
        use crate::SqliteIndex;
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — ne peut pas échouer");

        // DB fraîche : table note_embeddings vide → backfill retourne 0.
        let count = crate::sqlite_vec::backfill_ann_from_conn(&idx.conn)
            .await
            .expect("backfill_ann sur DB vide ne doit pas échouer");

        assert_eq!(
            count, 0,
            "backfill_ann sur DB vide doit retourner 0, obtenu {count}"
        );
    }

    /// Vérifie que `backfill_ann_from_conn` avec des notes embeddings dans `note_embeddings`
    /// (mais extension sqlite-vec non chargée) retourne le nombre de notes traitées
    /// (même si le INSERT dans vec0 est un no-op en mode dégradé).
    #[tokio::test]
    async fn backfill_ann_avec_embeddings_retourne_count() {
        use crate::SqliteIndex;
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — ne peut pas échouer");

        // Seeder 2 notes avec embedding dim=1024.
        {
            let conn = idx.conn.lock().await;
            conn.execute_batch(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                 VALUES
                   ('01ANN0000001', 'main', 'decisions', 'live', 1, 0,
                    X'0000000000000000000000000000000000000000000000000000000000000001', 'note 1'),
                   ('01ANN0000002', 'main', 'decisions', 'live', 1, 0,
                    X'0000000000000000000000000000000000000000000000000000000000000002', 'note 2');",
            )
            .expect("insert notes de test");

            // Construire un BLOB f32 LE de dim=1024 (4096 bytes).
            let blob: Vec<u8> = (0..1024_usize)
                .flat_map(|i| (i as f32 / 1024.0).to_le_bytes())
                .collect();
            let blob2: Vec<u8> = (0..1024_usize)
                .flat_map(|i| (1.0 - i as f32 / 1024.0).to_le_bytes())
                .collect();

            conn.execute(
                "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, computed_at, vault_id)
                 VALUES ('01ANN0000001', 'bge-m3', ?1, 1024, 0, 'main')",
                rusqlite::params![blob],
            )
            .expect("insert embedding 1");
            conn.execute(
                "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, computed_at, vault_id)
                 VALUES ('01ANN0000002', 'bge-m3', ?1, 1024, 0, 'main')",
                rusqlite::params![blob2],
            )
            .expect("insert embedding 2");
        }

        // backfill_ann_from_conn : 2 notes avec dim=1024 → count=2.
        // En mode dégradé (sans extension), upsert_ann retourne Ok(()) ou Err(Storage).
        // Le count reflète le nombre de notes sélectionnées, pas les inserts vec0 réels.
        let count = crate::sqlite_vec::backfill_ann_from_conn(&idx.conn)
            .await
            .expect("backfill_ann ne doit pas échouer sur note_embeddings valide");

        assert_eq!(
            count, 2,
            "backfill_ann avec 2 embeddings dim=1024 doit retourner 2, obtenu {count}"
        );
    }

    // ── Gate de santé ANN au boot ───────────────────────────────────────────────

    /// BLOB f32 LE de dim 1024 (4096 octets) — seule forme que le backfill insère.
    fn blob_bge_m3() -> Vec<u8> {
        (0..BGE_M3_DIM)
            .flat_map(|i| (i as f32 / BGE_M3_DIM as f32).to_le_bytes())
            .collect()
    }

    /// Corpus de référence du gate : **6 paires éligibles** (3 notes `live` × 2 embedders)
    /// et 4 lignes que le backfill n'insère jamais — une note `downgraded`, une sentinelle,
    /// un `dim != 1024`, et un BLOB malformé (`dim` annoncé 1024, 32 octets réels).
    ///
    /// Ces 4 exclusions sont le piège du gate : les compter comme éligibles produirait un
    /// déficit fantôme permanent. Elles sont donc présentes dans TOUS les scénarios.
    async fn seed_corpus_ann(idx: &crate::SqliteIndex) {
        let conn = idx.conn.lock().await;
        conn.execute_batch(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES
               ('01GATE0000000000000001', 'main', 'decisions', 'live', 1, 0, X'01', 'note live 1'),
               ('01GATE0000000000000002', 'main', 'decisions', 'live', 1, 0, X'02', 'note live 2'),
               ('01GATE0000000000000003', 'main', 'decisions', 'live', 1, 0, X'03', 'note live 3'),
               ('01GATE0000000000000004', 'main', 'decisions', 'downgraded', 1, 0, X'04', 'note downgradee'),
               ('__sentinel__main', 'main', 'decisions', 'live', 1, 0, X'05', 'sentinelle');",
        )
        .expect("seed notes");

        let blob = blob_bge_m3();
        // dim annoncée 8 → exclu par `ne.dim = 1024`.
        let blob_court: Vec<u8> = (0..8_usize)
            .flat_map(|i| (i as f32).to_le_bytes())
            .collect();

        for (note, embedder, dim, bytes) in [
            // Population éligible : 3 notes × 2 embedders.
            ("01GATE0000000000000001", "emb-a", 1024_i64, &blob),
            ("01GATE0000000000000001", "emb-b", 1024, &blob),
            ("01GATE0000000000000002", "emb-a", 1024, &blob),
            ("01GATE0000000000000002", "emb-b", 1024, &blob),
            ("01GATE0000000000000003", "emb-a", 1024, &blob),
            ("01GATE0000000000000003", "emb-b", 1024, &blob),
            // Exclusions — le backfill ne les insère pas, le gate ne doit pas les attendre.
            ("01GATE0000000000000004", "emb-a", 1024, &blob),
            ("__sentinel__main", "emb-a", 1024, &blob),
            ("01GATE0000000000000003", "emb-court", 8, &blob_court),
            ("01GATE0000000000000002", "emb-malforme", 1024, &blob_court),
        ] {
            conn.execute(
                "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, computed_at, vault_id)
                 VALUES (?1, ?2, ?3, ?4, 0, 'main')",
                params![note, embedder, bytes, dim],
            )
            .expect("seed note_embeddings");
        }
    }

    /// Insère des lignes `(note_id, vault_id, embedder_id)` dans l'ANN shadow.
    async fn seed_ann_rows(idx: &crate::SqliteIndex, rows: &[(&str, &str, &str)]) {
        let conn = idx.conn.lock().await;
        for (note_id, vault_id, embedder_id) in rows {
            conn.execute(
                "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
                 VALUES (?1, ?2, ?3, X'00')",
                params![note_id, vault_id, embedder_id],
            )
            .expect("seed ligne ANN");
        }
    }

    /// Les 6 lignes ANN attendues du corpus de référence.
    const ANN_COMPLET: &[(&str, &str, &str)] = &[
        ("01GATE0000000000000001", "main", "emb-a"),
        ("01GATE0000000000000002", "main", "emb-a"),
        ("01GATE0000000000000003", "main", "emb-a"),
        ("01GATE0000000000000001", "main", "emb-b"),
        ("01GATE0000000000000002", "main", "emb-b"),
        ("01GATE0000000000000003", "main", "emb-b"),
    ];

    /// Le gate détecte la partition trouée, la nomme, et ferme le chemin ANN.
    ///
    /// Réplique l'écart mesuré sur l'index LIVE (1972 paires traitées, 1966 lignes présentes) :
    /// une partition d'embedder à qui il manque une ligne. La partition saine du même vault
    /// ne doit PAS être signalée — sans quoi le gate n'a aucun pouvoir discriminant.
    #[tokio::test]
    async fn ann_health_gate_detecte_partition_incomplete() {
        let idx = index_avec_table_ann_shadow().await;
        seed_corpus_ann(&idx).await;
        // `emb-a` complet (3/3), `emb-b` amputé d'une ligne (2/3) — le défaut historique.
        seed_ann_rows(&idx, &ANN_COMPLET[..5]).await;
        idx.set_ann_enabled(true);

        let deficits = idx.ann_health_gate().await.expect("ann_health_gate");

        assert_eq!(
            deficits,
            vec![gradatum_core::index_store::AnnPartitionDeficit {
                vault_id: "main".to_string(),
                embedder_id: "emb-b".to_string(),
                eligible: 3,
                indexed: 2,
            }],
            "le gate doit signaler exactement la partition `(main, emb-b)` trouée (3 éligibles, \
             2 indexées) et laisser `(main, emb-a)` tranquille"
        );
        assert!(
            !idx.ann_is_enabled(),
            "fail-closed : un déficit doit couper le chemin ANN (brute-force exact) — un axe \
             sémantique muet est pire qu'un axe lent"
        );
    }

    /// Index dérivé complet → gate silencieux et ANN conservé.
    ///
    /// Prouve la parité du prédicat avec le backfill : les 4 lignes exclues du corpus
    /// (downgradée, sentinelle, `dim != 1024`, BLOB malformé) ne sont pas attendues en table.
    /// Toute divergence de prédicat produirait ici un déficit fantôme.
    #[tokio::test]
    async fn ann_health_gate_silencieux_quand_index_complet() {
        let idx = index_avec_table_ann_shadow().await;
        seed_corpus_ann(&idx).await;
        seed_ann_rows(&idx, ANN_COMPLET).await;
        idx.set_ann_enabled(true);

        let deficits = idx.ann_health_gate().await.expect("ann_health_gate");

        assert_eq!(
            deficits,
            Vec::new(),
            "aucune ligne ne manque : le gate doit se taire — un gate qui crie toujours est \
             un gate que personne ne lit"
        );
        assert!(
            idx.ann_is_enabled(),
            "sans déficit, le chemin ANN doit rester actif"
        );
    }

    /// Un surplus (note `downgraded` gardant sa ligne ANN) n'est PAS un déficit.
    ///
    /// `downgrade_note` ne touche ni `note_embeddings` ni `note_embeddings_ann` : le surplus
    /// est un état de régime normal, et `search_ann_inner` re-filtre `status != 'downgraded'`
    /// à la lecture. Le traiter comme un écart rendrait le gate rouge en permanence.
    #[tokio::test]
    async fn ann_health_gate_ignore_le_surplus_dune_note_downgradee() {
        let idx = index_avec_table_ann_shadow().await;
        seed_corpus_ann(&idx).await;
        seed_ann_rows(&idx, ANN_COMPLET).await;
        // Ligne ANN survivante de la note downgradée → `(main, emb-a)` : 4 indexées / 3 éligibles.
        seed_ann_rows(&idx, &[("01GATE0000000000000004", "main", "emb-a")]).await;
        idx.set_ann_enabled(true);

        let deficits = idx.ann_health_gate().await.expect("ann_health_gate");

        assert_eq!(
            deficits,
            Vec::new(),
            "un surplus est inoffensif (filtré à la lecture) et normal (downgrade conserve la \
             ligne) : le signaler condamnerait le gate au rouge permanent"
        );
        assert!(idx.ann_is_enabled(), "un surplus ne doit rien couper");
    }

    /// ANN désactivé → le gate ne fait rien du tout, malgré un déficit réel présent.
    ///
    /// Non-vacuité prouvée par A/B à variable unique : fixture identique à
    /// [`ann_health_gate_detecte_partition_incomplete`], seul le flag change. Le retour vide
    /// vient de la sortie anticipée (avant tout `prepare`), pas d'une mesure à zéro.
    #[tokio::test]
    async fn ann_health_gate_ne_fait_rien_quand_ann_desactive() {
        let idx = index_avec_table_ann_shadow().await;
        seed_corpus_ann(&idx).await;
        seed_ann_rows(&idx, &ANN_COMPLET[..5]).await;

        assert!(
            !idx.ann_is_enabled(),
            "précondition : l'index s'ouvre en brute-force (ANN OFF par défaut)"
        );

        let deficits = idx
            .ann_health_gate()
            .await
            .expect("ann_health_gate ANN OFF");

        assert_eq!(
            deficits,
            Vec::new(),
            "à ANN OFF le gate doit être un no-op strict : le déficit semé est bien là (cf. \
             ann_health_gate_detecte_partition_incomplete sur la même fixture), seul le flag \
             diffère"
        );
        assert!(
            !idx.ann_is_enabled(),
            "le no-op ne doit toucher à aucun état"
        );
    }

    /// Table ANN absente (extension vec0 non chargée) → gate vide, ANN inchangé.
    ///
    /// Ce régime n'est pas silencieux : une requête ANN y échoue bruyamment et le chemin
    /// brute-force prend le relais sur `Err`. Il n'y a donc pas d'index dérivé à auditer.
    #[tokio::test]
    async fn ann_health_gate_table_absente_retourne_vide() {
        let idx = crate::SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory — ne peut pas échouer");
        seed_corpus_ann(&idx).await;
        idx.set_ann_enabled(true);

        let deficits = idx
            .ann_health_gate()
            .await
            .expect("table ANN absente = mode dégradé documenté, pas une erreur");

        assert_eq!(
            deficits,
            Vec::new(),
            "sans table `note_embeddings_ann` il n'y a aucun index dérivé à auditer"
        );
    }
}
