//! Note lifecycle CRUD — creation, persistence, status updates.
//!
//! ## Operations
//!
//! - `write_note`: computes `ContentHash`, persists `.md` on disk, upserts the SQLite index.
//! - `write_note_with_id`: same but honours a pre-allocated ULID (stable wikilinks).
//! - `read_note`: reads from SQLite index + disk, with cache validation.
//! - `update_status`: validates the transition via `NoteStatus::can_transition_to`, updates
//!   the SQLite index, then persists the frontmatter on disk via `write_note_with_id`
//!   (copy-on-write — `.history/` snapshot if the hash differs).
//! - `delete_note`: removes the `.md` from storage + purges `.history/<id>/`.
//!
//! ## Invariants
//!
//! - `vault_id` in the frontmatter always equals `self.tenant_id` (forced if absent).
//! - `updated` is set to `Utc::now()` on every write.
//! - On-disk path: `<root>/<tenant>/<locus>/<id>.md` or `<root>/<tenant>/<id>.md`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::Utc;
use gradatum_cache::CacheKey;
use gradatum_core::config::HistoryConfig;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::history::sha256_for_history;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
// DocumentStore : write_note, get_content_hash, get_note, list_by_status (Étape 0.1).
use gradatum_core::DocumentStore as _;
use gradatum_core::note::{EffectiveNote, Note, NoteBody};
use gradatum_core::status::NoteStatus;
use gradatum_storage::Storage as _;

use crate::error::VaultError;
use crate::registry::Vault;

/// Prefix for copy-on-write history directories — excluded from the index, FTS, and drift detector.
///
/// Any path beginning with this prefix (relative to the tenant root) is treated as
/// a history artefact and must not be indexed, searched, or scanned for drift.
/// Convention: `<tenant_id>/.history/<note_id>/<timestamp>.md`.
///
/// Exposed publicly so consumers (untracked-file scan, drift detector filtering)
/// can exclude history paths without duplicating the constant.
pub const HISTORY_DIR_PREFIX: &str = ".history/";

/// Maximum number of tags per note (after union in `add_tags`).
///
/// Upper safety bound against unbounded frontmatter and FTS index growth
/// (DoS via repeated `add_tags`). When exceeded, `add_tags` returns a
/// `Validation` error (mapped to `409 Conflict` by the API handler) without
/// modifying the note.
pub const MAX_NOTE_TAGS: usize = 200;

impl Vault {
    /// Writes a note to the vault, generating a new ULID.
    ///
    /// Delegates to `write_note_inner` with `NoteId::new()`.
    ///
    /// ## Operations
    ///
    /// 1. Forces `vault_id = self.tenant_id` if absent from the frontmatter.
    /// 2. Sets `frontmatter.updated` to `Utc::now()`.
    /// 3. Computes `ContentHash::compute(&frontmatter, &body)`.
    /// 4. Generates a new `NoteId` (ULID) via `NoteId::new()`.
    /// 5. Serialises the note to Markdown via `gradatum-markdown::write`.
    /// 6. Persists the `.md` at `<root>/<tenant>/<locus?>/<id>.md` via `FileStorage`.
    /// 7. Upserts the SQLite index via `Index::upsert_note`.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::Markdown(...))` if serialisation fails.
    /// - `VaultError::Storage(...)` if the disk write fails.
    /// - `VaultError::Core(GradatumError::Storage(...))` if the SQLite upsert fails.
    pub async fn write_note(
        &self,
        frontmatter: Frontmatter,
        body: String,
    ) -> Result<Note, VaultError> {
        self.write_note_inner(frontmatter, body, NoteId::new())
            .await
    }

    /// Writes a note using a caller-supplied ULID.
    ///
    /// The worker passes the pre-allocated `note_id` at enqueue time so that
    /// the write-time id equals the stored id, guaranteeing stable wikilinks.
    ///
    /// Delegates to `write_note_inner` with the supplied `id`.
    pub async fn write_note_with_id(
        &self,
        frontmatter: Frontmatter,
        body: String,
        id: NoteId,
    ) -> Result<Note, VaultError> {
        self.write_note_inner(frontmatter, body, id).await
    }

    /// Common note-write logic — `id` is supplied by the caller.
    ///
    /// Called by `write_note` (with `NoteId::new()`), `write_note_with_id`
    /// (with a pre-allocated ULID), and `write_if_match` (optimistic-lock).
    /// The only functional difference between the two direct callers is the origin of `id`.
    /// `write_if_match` adds the hash check upstream.
    ///
    /// `pub(crate)`: accessible from `crate::write` and future `crate::history`.
    pub(crate) async fn write_note_inner(
        &self,
        mut frontmatter: Frontmatter,
        body: String,
        id: NoteId,
    ) -> Result<Note, VaultError> {
        // ── Phase 4 — Read-before-write ─────────────────────────────────────────
        // Tenter de lire la version existante AVANT d'écrire.
        // `existing` est `None` pour une création, `Some(note)` pour une mise à jour.
        // Réutilisé par :
        //   - F-41 `write_if_match` (check optimistic-lock via content_hash)
        //   - F-40 Copy-on-Write (snapshot .history/ si sha256_for_history diffère)
        // NoteNotFound n'est pas une erreur — la note est simplement nouvelle.
        let existing: Option<Note> = match self.read_note(id).await {
            Ok(note) => Some(note),
            Err(VaultError::Core(gradatum_core::error::GradatumError::NoteNotFound(_))) => None,
            Err(other) => return Err(other),
        };

        // Invariant : vault_id doit correspondre au tenant courant
        if frontmatter.vault_id.as_str().is_empty() {
            frontmatter.vault_id = self.tenant_id.clone();
        }

        // Mise à jour du timestamp de modification
        frontmatter.updated = Some(Utc::now());

        let body_obj = NoteBody { markdown: body };

        // Calcul du ContentHash JCS (§2.2) — déterministe cross-langage
        let content_hash = ContentHash::compute(&frontmatter, &body_obj.markdown);

        let note = Note {
            id,
            frontmatter,
            body: body_obj,
            version: NoteVersion::initial(),
            content_hash,
            integrity_signature: None,
        };

        // Chemin relatif on-disk : <tenant>/<locus?>/<id>.md
        let md_path = note_md_relative_path(&note);

        // ── F-40 Copy-on-Write ───────────────────────────────────────────────────
        // Si une version précédente existe ET que le contenu sémantique a changé
        // (sha256_for_history diffère), copier le .md courant dans .history/ AVANT
        // d'écraser. Le snapshot est lu depuis le storage (cohérence OpenDAL) et écrit
        // dans `<tenant>/.history/<id>/<timestamp_ms>.md`.
        //
        // Chemins .history/ JAMAIS passés à upsert_note → exclus de l'index SQLite,
        // FTS5 et drift scanner par construction. Le préfixe HISTORY_DIR_PREFIX est
        // documenté pour la Phase B (walk filesystem untracked, T12+) afin d'éviter
        // qu'elle indexe les snapshots.
        if let Some(ref prev) = existing {
            // Comparer les hashes sémantiques (excluant updated, processed, etc.).
            if sha256_for_history(prev) != sha256_for_history(&note) {
                // Timestamp de la version précédente — utilisé comme nom de fichier histoire.
                // Priorité : updated (timestamp de dernière écriture connue), sinon created,
                // sinon timestamp ULID de l'id (garanti non-nul, sortable).
                let ts_ms = prev
                    .frontmatter
                    .updated
                    .or(Some(prev.frontmatter.created))
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or_else(|| prev.id.timestamp_ms() as i64);

                // Chemin snapshot : <tenant>/.history/<id>/<ts_ms>.md
                // Le tenant dans le chemin provient du frontmatter existant (cohérent avec md_path).
                // Note : .history/ est sous le tenant, PAS sous un locus — toujours à la racine
                // tenant pour rester adressable même si le locus change lors d'un renommage.
                let tenant = prev.frontmatter.vault_id.as_str();
                let id_str = prev.id.to_string();
                let history_dir = format!("{}/.history/{}/", tenant, id_str);
                let snapshot_path = format!("{}{}.md", history_dir, ts_ms);

                // Lire le contenu actuel depuis le storage (source de vérité on-disk).
                // Le chemin courant est reconstruit depuis `prev` (avant la mise à jour
                // du frontmatter `updated` effectuée quelques lignes plus haut).
                let current_md_path = note_md_relative_path(prev);
                match self.storage.read(&current_md_path).await {
                    Ok(current_bytes) => {
                        // Créer le répertoire .history/<id>/ si nécessaire (idempotent).
                        if let Err(e) = self.storage.create_dir(&history_dir).await {
                            tracing::warn!(
                                id = %id_str,
                                history_dir = %history_dir,
                                err = %e,
                                "F-40 CoW : impossible de créer .history/ — snapshot ignoré"
                            );
                        } else {
                            // Écrire le snapshot — échec non fatal (on continue l'écriture principale).
                            if let Err(e) = self.storage.write(&snapshot_path, &current_bytes).await
                            {
                                tracing::warn!(
                                    id = %id_str,
                                    snapshot_path = %snapshot_path,
                                    err = %e,
                                    "F-40 CoW : écriture snapshot .history/ échouée — snapshot ignoré"
                                );
                            } else {
                                tracing::debug!(
                                    id = %id_str,
                                    snapshot_path = %snapshot_path,
                                    "F-40 CoW : snapshot .history/ créé"
                                );
                                // ── D1 — Rétention bornée (F-32A) ────────────────────────
                                // Après chaque écriture CoW réussie, appliquer la politique
                                // de rétention configurée (max_versions + ttl_days).
                                // Suppression non fatale : un échec est loggué sans bloquer.
                                // `.max(0)` sature à 0 avant le cast → ANSSI R11 safe.
                                let now_ms = Utc::now().timestamp_millis().max(0) as u64;
                                self.trim_history_to_max(id, &id_str, tenant, now_ms).await;
                            }
                        }
                    }
                    Err(gradatum_storage::StorageError::NotFound(_)) => {
                        // Fichier source absent — incohérence index/disque, on log mais on continue.
                        tracing::warn!(
                            id = %id_str,
                            current_md_path = %current_md_path,
                            "F-40 CoW : fichier source introuvable pour snapshot .history/ — skip"
                        );
                    }
                    Err(e) => {
                        // Erreur I/O inattendue — non fatale, on continue l'écriture principale.
                        tracing::warn!(
                            id = %id_str,
                            err = %e,
                            "F-40 CoW : erreur lecture fichier source — snapshot ignoré"
                        );
                    }
                }
            }
        }

        // Sérialisation Markdown (§5.1)
        let md_content = gradatum_markdown::write(&note)
            .map_err(|e| GradatumError::Markdown(format!("sérialisation md: {e}")))?;

        // Persistance sur disque via OpenDAL FileStorage
        self.storage
            .write(&md_path, md_content.as_bytes())
            .await
            .map_err(|e| VaultError::Storage(format!("write md {md_path}: {e}")))?;

        // Upsert dans l'index SQLite (FTS5 + note_overrides + file_checksums)
        // Étape 0.1 : upsert_note est devenu write_note via DocumentStore trait.
        self.index.write_note(&note).await?;

        Ok(note)
    }

    /// Reads a note by ULID identifier.
    ///
    /// ## Algorithm
    ///
    /// 1. **Cache hit**: checks presence in `EffectiveNoteCache` and validates the checksum
    ///    via `index.get_content_hash` (guards against stale concurrent cache entries).
    ///    Valid → returns the note immediately, increments `cache_hits`.
    ///    Stale → invalidates the entry, falls through to cache miss.
    /// 2. **Cache miss**: `index.get_note(vault_id, id)` → `NoteRecord`.
    ///    Reads the `.md` from disk via `storage.read(path)` to obtain the full Markdown.
    ///    Parses via `gradatum_markdown::parse` → `ParsedNote` → complete `Note`.
    ///    Inserts into cache for subsequent calls.
    ///
    /// ## Disk path (locus-aware)
    ///
    /// Resolution order:
    /// 1. `<vault_id>/<locus>/<id>.md` — if the index carries a `locus` (physical
    ///    relocation via `move_locus`). An explicit locus takes precedence over the
    ///    legacy section-as-locus layout.
    /// 2. `<vault_id>/<id>.md` — note at the tenant root (no locus).
    /// 3. `<vault_id>/<section>/<id>.md` — section as locus (legacy layout).
    ///
    /// Before locus-aware resolution was introduced, `read_note` ignored the index `locus`
    /// and could only resolve (2) and (3) — a note relocated via `move_locus` (which
    /// rewrites the `.md` to `<vault_id>/<locus>/<id>.md`) became unfindable.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::NoteNotFound)` if absent from the index.
    /// - `VaultError::Storage(...)` if the `.md` file is not found on disk.
    /// - `VaultError::Markdown(...)` if parsing fails.
    pub async fn read_note(&self, id: NoteId) -> Result<Note, VaultError> {
        let vault_id = self.tenant_id.as_str();
        let id_str = id.to_string();

        // ── 1. Cache hit path ─────────────────────────────────────────────────
        // Clé composite : (NoteId, scope_hash=0 pour read_note sans scope override).
        let cache_key: CacheKey = (id, 0u64);
        let index_for_validator = Arc::clone(&self.index);
        let id_for_validator = id;

        let cached = self
            .cache
            .get(cache_key, move |note_id| async move {
                // Validator : lit le hash courant depuis SQLite.
                // None = note absente de l'index → stale entry.
                index_for_validator
                    .get_content_hash(note_id)
                    .await?
                    .ok_or(GradatumError::NoteNotFound(id_for_validator))
            })
            .await
            .map_err(VaultError::Core)?;

        if let Some(effective) = cached {
            // Cache hit valide — reconstruire Note depuis EffectiveNote.
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(effective_note_to_note(&effective, id));
        }

        // ── 2. Cache miss path ────────────────────────────────────────────────
        // Vérifier que la note existe dans l'index.
        let record = self
            .index
            .get_note(vault_id, &id_str)
            .await
            .map_err(VaultError::Core)?
            .ok_or(VaultError::Core(GradatumError::NoteNotFound(id)))?;

        // Construire le chemin disque (D1.1 — locus-aware).
        // Ordre : locus explicite (relocalisation physique) → racine tenant → section.
        let path_with_locus = record
            .locus
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|locus| format!("{}/{}/{}.md", vault_id, locus, id_str));
        let path_no_locus = format!("{}/{}.md", vault_id, id_str);
        let path_with_section = format!("{}/{}/{}.md", vault_id, record.section, id_str);

        // Premier chemin existant l'emporte. `path_with_locus` n'est tenté que s'il
        // est présent dans l'index ET diffère de la section (évite un exists() inutile).
        let md_bytes =
            if let Some(ref locus_path) = path_with_locus {
                if self.storage.exists(locus_path).await.unwrap_or(false) {
                    self.storage
                        .read(locus_path)
                        .await
                        .map_err(|e| VaultError::Storage(format!("read .md {locus_path}: {e}")))?
                } else if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
                    self.storage.read(&path_no_locus).await.map_err(|e| {
                        VaultError::Storage(format!("read .md {path_no_locus}: {e}"))
                    })?
                } else {
                    self.storage.read(&path_with_section).await.map_err(|e| {
                        VaultError::Storage(format!("read .md {path_with_section}: {e}"))
                    })?
                }
            } else if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
                self.storage
                    .read(&path_no_locus)
                    .await
                    .map_err(|e| VaultError::Storage(format!("read .md {path_no_locus}: {e}")))?
            } else {
                self.storage.read(&path_with_section).await.map_err(|e| {
                    VaultError::Storage(format!("read .md {path_with_section}: {e}"))
                })?
            };

        let md_str = String::from_utf8(md_bytes)
            .map_err(|e| VaultError::Storage(format!("UTF-8 decode .md {id_str}: {e}")))?;

        // Parse le Markdown complet pour reconstruire la Note.
        let parsed =
            gradatum_markdown::parse(&md_str).map_err(|e| VaultError::Markdown(e.to_string()))?;

        // Reconstruire la version depuis `record.version` si disponible (défaut : 1).
        let note = Note {
            id,
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            version: NoteVersion::initial(),
            content_hash: parsed.content_hash,
            integrity_signature: None,
        };

        // Insérer dans le cache pour les appels suivants.
        let effective = Arc::new(note_to_effective_note(&note));
        self.cache
            .insert(cache_key, effective, note.content_hash)
            .await;

        Ok(note)
    }

    /// Updates a note's status with state-machine validation.
    ///
    /// ## State machine
    ///
    /// Only transitions defined in `NoteStatus::can_transition_to` are allowed.
    /// Any other transition returns
    /// `VaultError::Core(GradatumError::InvalidStatusTransition { from, to })`.
    ///
    /// If `target == current` (same status), returns `Ok(())` without writing
    /// (idempotent — avoids spurious conflicts on curator replays).
    ///
    /// ## Legacy `downgraded`
    ///
    /// The `status='downgraded'` value is written **directly in SQLite** by
    /// `vault_downgrade` (stable-wikilinks downgrade mechanism). It lies outside
    /// the `NoteStatus` enum and therefore outside this state machine.
    /// The state machine ignores this case: if a note's `.md` frontmatter contains
    /// a `downgraded` status, `read_note` fails at TOML parse → `NoteNotFound`
    /// (the `.md` `status` field cannot be `downgraded` because `vault_downgrade`
    /// only writes SQLite, not the `.md`). This distinct mechanism is documented in
    /// CLAUDE-GRADATUM.md.
    ///
    /// ## CoW
    ///
    /// The update goes through `write_note_with_id` (normal path) — each
    /// transition is recorded in `.history/` via copy-on-write.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::NoteNotFound)` if the note is absent.
    /// - `VaultError::Core(GradatumError::InvalidStatusTransition { from, to })`
    ///   if the transition is not allowed by the state machine.
    /// - `VaultError::Storage` / `VaultError::Markdown` on I/O or parse error.
    pub async fn update_status(
        &self,
        id: NoteId,
        target: NoteStatus,
        reason: Option<String>,
    ) -> Result<(), VaultError> {
        // Lire la version courante pour obtenir le statut actuel.
        // Propage NoteNotFound si la note est absente.
        let note = self.read_note(id).await?;
        let current = note.frontmatter.status;

        // Idempotence : target == current → no-op silencieux.
        // Évite les 409 lors de rejeux ou retries du curator.
        if current == target {
            return Ok(());
        }

        // Valider la transition via la state machine.
        if !current.can_transition_to(target) {
            return Err(VaultError::Core(GradatumError::InvalidStatusTransition {
                from: current,
                to: target,
            }));
        }

        // Construire le frontmatter mis à jour avec le nouveau statut.
        let mut new_fm = note.frontmatter.clone();
        new_fm.status = target;
        new_fm.status_reason = reason;
        new_fm.status_changed = Some(Utc::now());

        // Écrire via write_note_with_id (chemin normal → CoW trace la transition).
        // L'id est préservé (wikilinks stables).
        self.write_note_with_id(new_fm, note.body.markdown, id)
            .await?;

        Ok(())
    }

    /// Physically moves the `.md` of a note to a new locus.
    ///
    /// Replaces the former index-only mutation with a **coherent** relocation:
    /// the `.md` is rewritten to `<tenant>/<new_locus>/<id>.md`, the index is updated,
    /// and the old orphan `.md` is deleted. After the call, `read_note` returns the
    /// **new** locus (stale-locus issue eliminated).
    ///
    /// ## Algorithm (deterministic order)
    ///
    /// 1. Reads the current note via `read_note` (propagates `NoteNotFound`).
    ///    Resolves the current physical path.
    /// 2. **Idempotent**: if the current frontmatter locus == `new_locus`, silent no-op
    ///    (no write, no spurious `.history/` snapshot).
    /// 3. Rewrites the `.md` via `write_note_with_id` with `frontmatter.locus = new_locus`:
    ///    - `content_hash` changes (locus is part of the JCS hash) → the upsert
    ///      `ON CONFLICT` branch applies `excluded.locus` = new locus;
    ///    - copy-on-write snapshots the **old** `.md` (read from the old path via
    ///      `note_md_relative_path(prev)`) into `<tenant>/.history/<id>/`;
    ///    - the new `.md` is written to the **new** path.
    /// 4. Deletes the old orphan `.md` (if different from the new path) — non-fatal.
    ///
    /// ## CoW / history
    ///
    /// History is **preserved**: `.history/<id>/` is under the tenant root (NOT
    /// under the locus), so a locus change does not move the history. The pre-move
    /// snapshot is added normally by the copy-on-write path.
    ///
    /// ## Preconditions
    ///
    /// `new_locus` is assumed **already validated** by the caller (`LocusId::parse`).
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::NoteNotFound)` if the note is absent.
    /// - `VaultError::Storage` / `VaultError::Markdown` on I/O or parse error.
    pub async fn move_locus(
        &self,
        id: NoteId,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), VaultError> {
        // 1. Lire la note courante (propage NoteNotFound si absente).
        let note = self.read_note(id).await?;

        // 2. Idempotence : locus inchangé → no-op (pas de write, pas de CoW parasite).
        let current_locus = note.frontmatter.locus.as_ref().map(|l| l.as_str());
        if current_locus == Some(new_locus.as_str()) {
            return Ok(());
        }

        // Chemins candidats de l'ANCIEN .md (avant écriture) — résolus comme read_note :
        // locus explicite courant, racine tenant, section-as-locus. On déterminera celui
        // qui existe réellement pour le supprimer après l'écriture du nouveau .md.
        let vault_id = self.tenant_id.as_str();
        let id_str = id.to_string();
        let old_path_candidates: Vec<String> = {
            let mut v = Vec::with_capacity(3);
            if let Some(loc) = current_locus.filter(|l| !l.is_empty()) {
                v.push(format!("{}/{}/{}.md", vault_id, loc, id_str));
            }
            v.push(format!("{}/{}.md", vault_id, id_str));
            v.push(format!(
                "{}/{}/{}.md",
                vault_id,
                note.frontmatter.section.as_str(),
                id_str
            ));
            v
        };

        // D1.3 (P1 audit) — Capturer le statut index-level AVANT le re-upsert.
        //
        // Anti-résurrection : `downgrade_note` / `patch_note_status` / decay posent un
        // statut (`downgraded`, `pending-review`…) + `status_reason` / `status_changed` /
        // `replaced_by` en mutation index-ONLY, SANS réécrire le `.md` (frontmatter reste
        // `live`). Le re-upsert ci-dessous applique `status = excluded.status` (= statut du
        // frontmatter stale) INCONDITIONNELLEMENT — ce qui ressusciterait silencieusement
        // une note downgradée (réapparition en search, écrasement reason/changed,
        // état incohérent `live + replaced_by`). On capture l'état index brut pour le
        // restituer après le write. Un move ne change JAMAIS la sémantique de statut :
        // pour une note non divergente (frontmatter == index), la restitution est un no-op.
        let vault_id_owned = vault_id.to_string();
        let status_snapshot = self
            .index
            .get_index_status_snapshot(&vault_id_owned, &id_str)
            .await?;

        // 3. Réécrire le .md avec le nouveau locus dans le frontmatter.
        // write_note_with_id gère : CoW snapshot de l'ancien .md, écriture au nouveau
        // chemin, re-upsert index (hash modifié → excluded.locus appliqué).
        let mut new_fm = note.frontmatter.clone();
        new_fm.locus = Some(new_locus.clone());
        let written = self
            .write_note_with_id(new_fm, note.body.markdown, id)
            .await?;

        // D1.3 — Restituer le statut index-level capturé (annule l'écrasement par le
        // frontmatter stale). Cohérent avec la garde locus/trust de l'upsert : un move
        // ne touche que le locus, jamais le statut/reason/changed/replaced_by.
        if let Some(snapshot) = status_snapshot {
            self.index
                .restore_index_status_fields(&vault_id_owned, &id_str, &snapshot)
                .await?;
        }

        // Nouveau chemin physique réel (après écriture).
        let new_path = note_md_relative_path(&written);

        // 4. Supprimer l'ancien .md orphelin — non fatal.
        // On supprime le premier candidat qui existe ET diffère du nouveau chemin.
        for old_path in &old_path_candidates {
            if *old_path == new_path {
                continue;
            }
            if self.storage.exists(old_path).await.unwrap_or(false) {
                if let Err(e) = self.storage.delete(old_path).await {
                    tracing::warn!(
                        id = %id_str,
                        old_path = %old_path,
                        err = %e,
                        "D1.1 move_locus : échec suppression ancien .md orphelin — non fatal"
                    );
                }
                // Un seul .md physique attendu ; on s'arrête au premier supprimé.
                break;
            }
        }

        Ok(())
    }

    /// Adds tags to a note — additive only, case-insensitive union semantics.
    ///
    /// ## Algorithm
    ///
    /// 1. Reads the current note (`read_note` — propagates `NoteNotFound`).
    /// 2. Computes the union of existing tags + `new_tags`, deduplicating
    ///    **case-insensitively**: a new tag whose lowercase form already exists
    ///    (among existing tags or other new tags) is ignored. The case of the
    ///    **existing** tag is preserved; only genuinely new tags are appended.
    /// 3. If the effective set is unchanged (all tags were already present) →
    ///    **no-op**: no write, no spurious `.history/` snapshot (strict idempotence).
    /// 4. Otherwise, writes via `write_note_with_id` (CoW + FTS reindex via `upsert_note`).
    ///
    /// Each tag is re-validated via `Tag::new` (parse-don't-validate at the storage boundary) —
    /// returns `VaultError::Core(GradatumError::Validation)` if a tag is malformed.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(NoteNotFound)` if the note is absent.
    /// - `VaultError::Core(Validation)` if a tag is malformed.
    /// - `VaultError::Storage` / `VaultError::Markdown` on I/O error.
    pub async fn add_tags(&self, id: NoteId, new_tags: &[String]) -> Result<(), VaultError> {
        use gradatum_core::tag::Tag;

        // Lire la version courante (propage NoteNotFound si absente).
        let note = self.read_note(id).await?;

        // Ensemble des formes lowercase déjà présentes (dédup case-insensitive).
        let mut seen_lower: std::collections::HashSet<String> = note
            .frontmatter
            .tags
            .iter()
            .map(|t| t.as_str().to_ascii_lowercase())
            .collect();

        let mut new_fm = note.frontmatter.clone();
        let mut changed = false;

        for raw in new_tags {
            // Re-validation à la frontière storage : format lowercase-with-dash, 1–64 chars.
            // Tag::new retourne ValidationError::InvalidTag → propagé en GradatumError::Validation.
            let tag = Tag::new(raw.clone())
                .map_err(|e| VaultError::Core(GradatumError::Validation(e)))?;
            let lower = tag.as_str().to_ascii_lowercase();
            // UNION case-insensitive : ignorer si déjà présent (existant ou nouveau dup).
            if seen_lower.insert(lower) {
                new_fm.tags.push(tag);
                changed = true;
            }
        }

        // Idempotence stricte : rien de nouveau → pas de write, pas de version CoW parasite.
        if !changed {
            return Ok(());
        }

        // cap du nombre TOTAL de tags après union. Borne une
        // croissance non maîtrisée (DoS index/frontmatter via add_tags répétés).
        // Vérifié APRÈS la dédup/union : seul l'ensemble effectif compte. Échec →
        // Validation (mappé en 409 Conflict côté handler — l'état n'est pas modifié).
        if new_fm.tags.len() > MAX_NOTE_TAGS {
            return Err(VaultError::Core(GradatumError::Validation(
                gradatum_core::error::ValidationError::InvalidInput(format!(
                    "nombre total de tags ({}) dépasse le maximum autorisé ({MAX_NOTE_TAGS})",
                    new_fm.tags.len()
                )),
            )));
        }

        // Écrire via write_note_with_id (CoW trace + réindex FTS via upsert_note).
        // L'id est préservé (wikilinks stables). `updated` est rafraîchi par write_note_with_id.
        self.write_note_with_id(new_fm, note.body.markdown, id)
            .await?;

        Ok(())
    }

    /// Deletes a note and purges its `.history/<id>/` directory.
    ///
    /// ## Operations (in order)
    ///
    /// 1. Locates the note in the SQLite index (propagates `NoteNotFound` if absent).
    /// 2. Resolves the `.md` path on disk using the same **3-way resolution** as `read_note`:
    ///    `path_with_locus` (`<vault>/<locus>/<id>.md`, only if `record.locus` is set) →
    ///    `path_no_locus` (`<vault>/<id>.md`, legacy root layout) →
    ///    `path_with_section` (`<vault>/<section>/<id>.md`, legacy section layout).
    ///    The **first path that exists on disk** is deleted.
    /// 3. Deletes the `.md` file via `storage.delete`.
    /// 4. Purges the `.history/<id>/` directory (non-fatal).
    ///
    /// ## Error behaviour
    ///
    /// - Note absent (index): `Err(VaultError::Core(NoteNotFound))`.
    /// - `.md` not found on any candidate path: `Err(VaultError::Storage(...))`.
    /// - `.md` deletion failure: `Err(VaultError::Storage(...))` — fatal.
    /// - `.history/` purge failure: logged as `warn!`, **non-fatal** — the note is already deleted.
    ///
    /// ## Note
    ///
    /// This function does not de-index the note (no `delete_note` in `DocumentStore`).
    /// Removal of the SQLite entry is deferred to the next drift detector pass.
    /// Callers looking up the note via `read_note` will receive an I/O error
    /// (`.md` absent) until the SQLite entry is purged.
    pub async fn delete_note(&self, id: NoteId) -> Result<(), VaultError> {
        let vault_id = self.tenant_id.as_str();
        let id_str = id.to_string();

        // Localiser le .md depuis l'index (nécessaire pour connaître la section/locus).
        let record = self
            .index
            .get_note(vault_id, &id_str)
            .await
            .map_err(VaultError::Core)?
            .ok_or(VaultError::Core(GradatumError::NoteNotFound(id)))?;

        // Résolution 3-voies du chemin disque — symétrique avec `read_note`.
        // Ordre : locus explicite → racine tenant (legacy) → section (legacy section-as-locus).
        let path_with_locus = record
            .locus
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|locus| format!("{}/{}/{}.md", vault_id, locus, id_str));
        let path_no_locus = format!("{}/{}.md", vault_id, id_str);
        let path_with_section = format!("{}/{}/{}.md", vault_id, record.section, id_str);

        // Sélectionner le PREMIER chemin existant pour la suppression.
        let md_path = if let Some(ref locus_path) = path_with_locus {
            if self.storage.exists(locus_path).await.unwrap_or(false) {
                locus_path.clone()
            } else if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
                path_no_locus
            } else {
                // path_with_section — tenter la suppression directe (erreur si absent).
                path_with_section
            }
        } else if self.storage.exists(&path_no_locus).await.unwrap_or(false) {
            path_no_locus
        } else {
            path_with_section
        };

        // Supprimer le fichier .md — fatal si échoue.
        self.storage
            .delete(&md_path)
            .await
            .map_err(|e| VaultError::Storage(format!("delete md {md_path}: {e}")))?;

        // Purger .history/<id>/ — non fatal.
        self.purge_history_dir(id, &id_str, vault_id).await;

        Ok(())
    }

    /// Lists the archived versions of a note in `.history/`.
    ///
    /// Returns the timestamps (Unix milliseconds) of available snapshots,
    /// sorted in ascending order (oldest first).
    ///
    /// Each snapshot is a file at `<tenant>/.history/<id>/<ts_ms>.md`.
    /// The list is built from the filesystem storage (OpenDAL) — NOT from
    /// the SQLite index (snapshots are not indexed by construction).
    ///
    /// ## Returns
    ///
    /// Empty `Vec<i64>` if no history exists (note created but never modified,
    /// or copy-on-write not yet triggered).
    ///
    /// ## Errors
    ///
    /// - `VaultError::Storage` if the `.history/` directory listing fails for any
    ///   reason other than "directory not found" (`NotFound` → empty list, not an error).
    pub async fn history_versions(&self, id: NoteId) -> Result<Vec<i64>, VaultError> {
        let tenant = self.tenant_id.as_str();
        let id_str = id.to_string();
        let history_dir = format!("{}/.history/{}/", tenant, id_str);

        let entries = match self.storage.list(&history_dir).await {
            Ok(entries) => entries,
            Err(gradatum_storage::StorageError::NotFound(_)) => {
                // Répertoire absent = pas d'historique → liste vide, pas une erreur.
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(VaultError::Storage(format!(
                    "history_versions list {history_dir}: {e}"
                )));
            }
        };

        // Extraire les timestamps depuis les noms de fichiers `<ts_ms>.md`.
        let mut timestamps: Vec<i64> = entries
            .iter()
            .filter(|e| !e.is_dir && e.path.ends_with(".md"))
            .filter_map(|e| {
                // Extraire le basename (dernier segment du chemin).
                let basename = e.path.rsplit('/').next()?;
                // Retirer l'extension `.md` et parser comme i64.
                basename.strip_suffix(".md")?.parse::<i64>().ok()
            })
            .collect();

        // Trier du plus ancien au plus récent (timestamps croissants).
        timestamps.sort_unstable();
        Ok(timestamps)
    }

    /// Reads the content of a historical snapshot of a note.
    ///
    /// ## Parameters
    ///
    /// - `id`: note identifier.
    /// - `ts_ms`: snapshot timestamp in Unix milliseconds (from `history_versions`).
    ///
    /// ## Errors
    ///
    /// - `VaultError::Storage` if the snapshot file is not found or cannot be read.
    /// - `VaultError::Markdown` if parsing the snapshot fails.
    pub async fn history_get(&self, id: NoteId, ts_ms: i64) -> Result<Note, VaultError> {
        let tenant = self.tenant_id.as_str();
        let id_str = id.to_string();
        let snapshot_path = format!("{}/.history/{}/{}.md", tenant, id_str, ts_ms);

        let bytes =
            self.storage.read(&snapshot_path).await.map_err(|e| {
                VaultError::Storage(format!("history_get read {snapshot_path}: {e}"))
            })?;

        let md_str = String::from_utf8(bytes).map_err(|e| {
            VaultError::Storage(format!("history_get UTF-8 decode {snapshot_path}: {e}"))
        })?;

        let parsed =
            gradatum_markdown::parse(&md_str).map_err(|e| VaultError::Markdown(e.to_string()))?;

        // Reconstituer la Note depuis le snapshot parsé.
        // L'id est celui de la note courante (le snapshot est une version ancienne).
        let note = Note {
            id,
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            version: NoteVersion::initial(),
            content_hash: parsed.content_hash,
            integrity_signature: None,
        };

        Ok(note)
    }

    // ── Helpers rétention + purge ─────────────────────────────────────────────

    /// Applies the retention policy after a successful copy-on-write.
    ///
    /// Delegates to [`Self::apply_history_trim`] passing `now_ms = Utc::now()`.
    /// Split into two methods to allow clock injection in tests.
    ///
    /// Non-fatal: any listing or individual deletion failure is logged as `warn!`
    /// without interrupting the main write.
    async fn trim_history_to_max(&self, id: NoteId, id_str: &str, tenant: &str, now_ms: u64) {
        self.apply_history_trim(id, id_str, tenant, &self.config.history, now_ms)
            .await;
    }

    /// Applies the configurable retention policy to `.history/` snapshots.
    ///
    /// ## Algorithm (deterministic order)
    ///
    /// 1. **TTL first** (if `cfg.ttl_days = Some(n)`): delete snapshots whose
    ///    `ts_ms` timestamp is earlier than `now_ms - n * 24 * 3600 * 1000`.
    /// 2. **Count cap next** (always): if the number of remaining snapshots exceeds
    ///    `cfg.max_versions`, delete the oldest ones (smallest timestamps).
    ///
    /// TTL-before-count ordering guarantees that snapshots retained after TTL are always
    /// the `max_versions` most recent. The result is deterministic and idempotent.
    ///
    /// ## Parameters
    ///
    /// - `id` / `id_str` / `tenant`: note identifier and tenant, used to build
    ///   OpenDAL snapshot paths.
    /// - `cfg`: retention configuration (`max_versions` + `ttl_days`).
    /// - `now_ms`: current timestamp in Unix milliseconds. Pass `Utc::now().timestamp_millis() as u64`
    ///   in production, or a simulated value in tests to control TTL.
    ///
    /// ## Error behaviour
    ///
    /// Non-fatal — any listing or individual deletion failure is logged as `warn!`
    /// without propagating an error.
    pub async fn apply_history_trim(
        &self,
        id: NoteId,
        id_str: &str,
        tenant: &str,
        cfg: &HistoryConfig,
        now_ms: u64,
    ) {
        // Récupérer la liste actuelle des snapshots (triée croissant par timestamp).
        let mut versions = match self.history_versions(id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    id = %id_str,
                    err = %e,
                    "D1/F-32A rétention : impossible de lister .history/ pour bornage"
                );
                return;
            }
        };

        // ── Étape 1 : TTL — supprimer les snapshots expirés ────────────────────
        if let Some(ttl_days) = cfg.ttl_days {
            // Seuil d'expiration en millisecondes : now_ms - ttl_days * 86_400_000.
            // On sature à 0 pour éviter un overflow si ttl_days est grand.
            let ttl_ms = u64::from(ttl_days).saturating_mul(24 * 3600 * 1000);
            let cutoff_ms = now_ms.saturating_sub(ttl_ms);

            // Collecter les timestamps expirés avant de muter `versions`.
            let expired: Vec<i64> = versions
                .iter()
                .copied()
                .filter(|&ts| {
                    // Un snapshot est expiré si son timestamp est antérieur au cutoff.
                    // Timestamp négatif ou invalide (< 0) → u64::try_from échoue →
                    // traité comme expiré (valeur 0 < cutoff_ms sauf si cutoff=0).
                    // Sémantique ANSSI R11 : cast explicite avec comportement documenté.
                    u64::try_from(ts).unwrap_or(0) < cutoff_ms
                })
                .collect();

            if !expired.is_empty() {
                tracing::debug!(
                    id = %id_str,
                    expired = expired.len(),
                    ttl_days = ttl_days,
                    "D1/F-32A TTL : suppression snapshots expirés"
                );
                for ts_ms in &expired {
                    let snapshot_path = format!("{}/.history/{}/{}.md", tenant, id_str, ts_ms);
                    if let Err(e) = self.storage.delete(&snapshot_path).await {
                        tracing::warn!(
                            id = %id_str,
                            snapshot_path = %snapshot_path,
                            err = %e,
                            "D1/F-32A TTL : échec suppression snapshot expiré — non fatal"
                        );
                    }
                }
                // Retirer les timestamps supprimés de la liste pour l'étape 2.
                versions.retain(|ts| !expired.contains(ts));
            }
        }

        // ── Étape 2 : cap count — limiter au max_versions restant ──────────────
        // max_versions = 0 est interprété comme 1 (garder au moins 1 snapshot).
        let effective_max = cfg.max_versions.max(1);

        if versions.len() <= effective_max {
            return;
        }

        let to_delete_count = versions.len() - effective_max;
        let to_delete = &versions[..to_delete_count];

        tracing::debug!(
            id = %id_str,
            total = versions.len(),
            deleting = to_delete_count,
            max = effective_max,
            "D1/F-32A cap count : suppression snapshots excédentaires"
        );

        for &ts_ms in to_delete {
            let snapshot_path = format!("{}/.history/{}/{}.md", tenant, id_str, ts_ms);
            if let Err(e) = self.storage.delete(&snapshot_path).await {
                tracing::warn!(
                    id = %id_str,
                    snapshot_path = %snapshot_path,
                    err = %e,
                    "D1/F-32A cap count : échec suppression snapshot — non fatal"
                );
            }
        }
    }

    /// Recursively deletes the `.history/<id>/` directory.
    ///
    /// Called during note deletion (`delete_note`) to avoid orphaned disk artefacts.
    /// Non-fatal — any failure is logged as `warn!`.
    ///
    /// ## Algorithm
    ///
    /// 1. Lists all files under `<tenant>/.history/<id>/`.
    /// 2. Deletes each file via `storage.delete()`.
    /// 3. Attempts to delete the now-empty directory itself (may fail if non-empty
    ///    or if the backend does not support directory deletion — non-fatal).
    async fn purge_history_dir(&self, _id: NoteId, id_str: &str, tenant: &str) {
        let history_dir = format!("{}/.history/{}/", tenant, id_str);

        // Lister tous les entrées sous .history/<id>/.
        let entries = match self.storage.list(&history_dir).await {
            Ok(e) => e,
            Err(gradatum_storage::StorageError::NotFound(_)) => {
                // Pas de .history/ — rien à purger.
                return;
            }
            Err(e) => {
                tracing::warn!(
                    id = %id_str,
                    history_dir = %history_dir,
                    err = %e,
                    "D1 purge : impossible de lister .history/ — non fatal"
                );
                return;
            }
        };

        // Supprimer chaque fichier (pas les répertoires — OpenDAL delete ne supporte
        // pas les répertoires non vides ; on supprime les feuilles d'abord).
        for entry in entries.iter().filter(|e| !e.is_dir) {
            if let Err(e) = self.storage.delete(&entry.path).await {
                tracing::warn!(
                    id = %id_str,
                    path = %entry.path,
                    err = %e,
                    "D1 purge : échec suppression fichier .history/ — non fatal"
                );
            }
        }

        // Tenter de supprimer le répertoire maintenant vide.
        // Certains backends (S3) ne nécessitent pas cette étape ; pour Fs, le répertoire
        // reste si non vide — on ignore l'erreur.
        if let Err(e) = self.storage.delete(&history_dir).await {
            tracing::debug!(
                id = %id_str,
                history_dir = %history_dir,
                err = %e,
                "D1 purge : suppression répertoire .history/ — ignorée (peut rester vide)"
            );
        }
    }
}

// ── Helpers de conversion cache ───────────────────────────────────────────────

/// Converts an `EffectiveNote` (from cache) into a complete `Note`.
///
/// `EffectiveNote` is structurally identical to `Note` (no overrides applied).
/// Reconstructs the `Note` from its fields.
fn effective_note_to_note(effective: &EffectiveNote, id: NoteId) -> Note {
    Note {
        id,
        frontmatter: effective.frontmatter.clone(),
        body: effective.body.clone(),
        version: effective.version,
        content_hash: effective.content_hash,
        integrity_signature: None,
    }
}

/// Converts a `Note` into an `EffectiveNote` for cache insertion.
///
/// Direct projection — no overrides applied.
fn note_to_effective_note(note: &Note) -> EffectiveNote {
    EffectiveNote {
        id: note.id,
        frontmatter: note.frontmatter.clone(),
        body: note.body.clone(),
        version: note.version,
        content_hash: note.content_hash,
    }
}

/// Builds the on-disk relative path of a note.
///
/// Format: `<tenant>/<locus>/<id>.md`, or `<tenant>/<id>.md` if no locus.
/// The path is always relative to the vault root (passed as-is to `Storage::write`).
fn note_md_relative_path(note: &Note) -> String {
    let tenant = note.frontmatter.vault_id.as_str();
    let id_str = note.id.to_string();
    match note.frontmatter.locus.as_ref() {
        Some(locus) => format!("{}/{}/{}.md", tenant, locus.as_str(), id_str),
        None => format!("{}/{}.md", tenant, id_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;

    fn build_minimal_frontmatter() -> Frontmatter {
        Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Decisions,
            status: NoteStatus::Draft,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: Default::default(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        }
    }

    #[test]
    fn note_md_relative_path_no_locus() {
        let fm = build_minimal_frontmatter();
        let body = NoteBody {
            markdown: "test".into(),
        };
        let hash = ContentHash::compute(&fm, "test");
        let id = NoteId::new();
        let note = Note {
            id,
            frontmatter: fm,
            body,
            version: NoteVersion::initial(),
            content_hash: hash,
            integrity_signature: None,
        };
        let path = note_md_relative_path(&note);
        assert!(path.starts_with("main/"));
        assert!(path.ends_with(".md"));
    }

    #[test]
    fn note_md_relative_path_with_locus() {
        use gradatum_core::scope::LocusId;
        let mut fm = build_minimal_frontmatter();
        fm.locus = Some(LocusId::new("my-locus"));
        let body = NoteBody {
            markdown: "test".into(),
        };
        let hash = ContentHash::compute(&fm, "test");
        let id = NoteId::new();
        let note = Note {
            id,
            frontmatter: fm,
            body,
            version: NoteVersion::initial(),
            content_hash: hash,
            integrity_signature: None,
        };
        let path = note_md_relative_path(&note);
        assert!(path.starts_with("main/my-locus/"));
        assert!(path.ends_with(".md"));
    }
}
