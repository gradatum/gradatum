//! `TestInternalClient` — implémentation `InternalClient` wrappant `Vault` + `SqliteIndex`
//! pour les tests d'intégration des handlers (worker-flip v0.5.3).

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::VectorStore;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::VaultError;
use gradatum_vault::{Vault, write::WriteResult};
use smallvec::SmallVec;
use toml::Value as TomlValue;
use ulid::Ulid;

use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};

// ── Helpers de parse ──────────────────────────────────────────────────────────

fn parse_section(s: &str) -> Result<Section, InternalClientError> {
    // Délègue à Section::from_canonical_str (SSOT) : tout nouveau variant dans
    // l'enum est automatiquement accepté — plus de match hardcodé à maintenir.
    // Commentaire spec §16 B2 : le match exhaustif était nécessaire avant la fn
    // core ; désormais l'enum fait office de registre.
    Section::from_canonical_str(s).ok_or_else(|| InternalClientError::ServerError {
        status: 400,
        body: format!("section invalide : {s:?}"),
    })
}

fn parse_status(s: &str) -> Result<NoteStatus, InternalClientError> {
    match s {
        "draft" => Ok(NoteStatus::Draft),
        "live" | "Live" => Ok(NoteStatus::Live),
        "pending-review" | "PendingReview" | "Pending" => Ok(NoteStatus::PendingReview),
        "staging" | "Staging" => Ok(NoteStatus::Staging),
        "garbage" | "Garbage" => Ok(NoteStatus::Garbage),
        "archived" | "deprecated" | "Deprecated" => Ok(NoteStatus::Deprecated),
        _ => Err(InternalClientError::ServerError {
            status: 400,
            body: format!("statut invalide : {s:?}"),
        }),
    }
}

// Miroir exact de `parse_author` côté serveur (`internal/persist.rs`, Tâche 11 — R2).
// R2 refuse d'inventer une identité, pas de défaulter une métadonnée d'audit : sont refusés
// un `kind:` explicite mais inconnu et la chaîne vide/blanche ; un nom nu PORTE l'identité du
// credential (l'`id`, charset AgentId sans `:`) et est accepté avec un `kind` par défaut. Ce
// double doit suivre le serveur au comportement près, sous peine de divergence silencieuse.
fn parse_author(s: &str) -> Result<AuthorRef, GradatumError> {
    if s.trim().is_empty() {
        return Err(GradatumError::InvalidInput(
            "empty author — no identity resolved (R2)".to_string(),
        ));
    }
    match s.split_once(':') {
        Some((kind_str, id)) => {
            let kind = match kind_str {
                "human" => AuthorKind::Human,
                "main-agent" => AuthorKind::MainAgent,
                "sub-agent" => AuthorKind::SubAgent,
                "system" => AuthorKind::System,
                other => {
                    return Err(GradatumError::InvalidInput(format!(
                        "unknown author kind {other:?} — an explicit 'kind:id' must name a recognized kind (R2)"
                    )));
                }
            };
            Ok(AuthorRef {
                kind,
                id: id.to_string(),
                display_name: None,
            })
        }
        // Nom nu = identité résolue du credential (charset AgentId interdit `:`) ; `kind`
        // par défaut = métadonnée d'audit sans effet d'autorisation. Voir persist.rs pour
        // la justification complète.
        None => Ok(AuthorRef {
            kind: AuthorKind::MainAgent,
            id: s.to_string(),
            display_name: None,
        }),
    }
}

fn parse_tags(tags: &[String]) -> Result<SmallVec<[Tag; 4]>, InternalClientError> {
    tags.iter()
        .map(|t| {
            Tag::new(t.clone()).map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("tag invalide : {e}"),
            })
        })
        .collect()
}

fn gradatum_err_to_client(e: GradatumError, note_id: &str) -> InternalClientError {
    let msg = format!("{e}");
    if msg.contains("not found") || msg.contains("introuvable") || msg.contains("NoteNotFound") {
        InternalClientError::NotFound {
            ulid: note_id.to_string(),
        }
    } else if msg.contains("conflict") || msg.contains("hash mismatch") {
        InternalClientError::Conflict {
            current_sha256_hex: None,
        }
    } else {
        InternalClientError::ServerError {
            status: 500,
            body: msg,
        }
    }
}

fn vault_err_to_client(e: VaultError, note_id: &str) -> InternalClientError {
    match e {
        VaultError::Conflict(_) => InternalClientError::Conflict {
            current_sha256_hex: None,
        },
        VaultError::Core(inner) => gradatum_err_to_client(inner, note_id),
        VaultError::Storage(msg) | VaultError::Markdown(msg) => {
            if msg.contains("not found") || msg.contains("introuvable") {
                InternalClientError::NotFound {
                    ulid: note_id.to_string(),
                }
            } else {
                InternalClientError::ServerError {
                    status: 500,
                    body: msg,
                }
            }
        }
    }
}

/// Rendu hexadécimal minuscule d'un hash brut (colonne `notes.content_hash`), pour les
/// lectures servies depuis l'index seul (aucun `.md` du vault demandé).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

fn section_to_str(section: Section) -> String {
    // Délègue à la SSOT `Section::as_str` (IMPORT > COPIER) : évite une landmine
    // de match exhaustif à chaque extension de section (spec §16 B2).
    section.as_str().to_string()
}

fn status_to_str(status: NoteStatus) -> String {
    match status {
        NoteStatus::Draft => "draft".to_string(),
        NoteStatus::Live => "live".to_string(),
        NoteStatus::PendingReview => "pending-review".to_string(),
        NoteStatus::Staging => "staging".to_string(),
        NoteStatus::Garbage => "garbage".to_string(),
        NoteStatus::Deprecated => "deprecated".to_string(),
    }
}

// ── TestInternalClient ────────────────────────────────────────────────────────

/// Client de test — délègue vers `Vault` + `SqliteIndex` in-memory.
pub struct TestInternalClient {
    pub vault: Arc<Vault>,
    pub index: Arc<SqliteIndex>,
}

impl TestInternalClient {
    pub fn new(vault: Arc<Vault>, index: Arc<SqliteIndex>) -> Self {
        Self { vault, index }
    }
}

#[async_trait]
impl InternalClient for TestInternalClient {
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;
        let section = parse_section(&req.section)?;
        let status = parse_status(&req.status)?;
        let author_ref = req
            .author
            .as_deref()
            .map(parse_author)
            .transpose()
            .map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("invalid author: {e}"),
            })?;
        let tags = parse_tags(&req.tags)?;

        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section,
            status,
            status_reason: None,
            status_changed: None,
            tags,
            author: author_ref,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: req.provenance.clone(),
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        // F-41: If expected_sha256 is present, use write_if_match (optimistic-lock).
        let written = if let Some(ref sha_hex) = req.expected_sha256 {
            // Parse hex → [u8; 32] using stdlib (no hex crate needed)
            let expected_bytes: Option<[u8; 32]> = (sha_hex.len() == 64)
                .then(|| {
                    let mut arr = [0u8; 32];
                    for (i, chunk) in sha_hex.as_bytes().chunks(2).enumerate() {
                        let s = std::str::from_utf8(chunk).ok()?;
                        arr[i] = u8::from_str_radix(s, 16).ok()?;
                    }
                    Some(arr)
                })
                .flatten();
            match self
                .vault
                .write_if_match(frontmatter, req.body.clone(), note_id, expected_bytes)
                .await
                .map_err(|e| vault_err_to_client(e, &req.note_id))?
            {
                WriteResult::Written { .. } => {
                    // Re-read to get the written note's id
                    self.vault
                        .read_note(note_id)
                        .await
                        .map_err(|e| vault_err_to_client(e, &req.note_id))?
                }
                WriteResult::Conflict { current_sha256 } => {
                    let sha_hex = gradatum_core::identity::ContentHash(current_sha256).hex();
                    return Err(InternalClientError::Conflict {
                        current_sha256_hex: Some(sha_hex),
                    });
                }
            }
        } else {
            self.vault
                .write_note_with_id(frontmatter, req.body.clone(), note_id)
                .await
                .map_err(|e| vault_err_to_client(e, &req.note_id))?
        };

        let _ = self
            .index
            .upsert_note_title(
                written.frontmatter.vault_id.as_str(),
                &written.id,
                &req.title,
            )
            .await;

        if let Some(temporal) = &req.temporal {
            let anchor_src = match temporal.anchor_src.as_str() {
                "occurred_at" | "OccurredAt" => AnchorSrc::OccurredAt,
                "event-date" | "EventDate" => AnchorSrc::EventDate,
                "valid_from" | "ValidFrom" => AnchorSrc::ValidFrom,
                _ => AnchorSrc::Created,
            };
            let entry = TemporalEntry {
                note_id: req.note_id.clone(),
                vault_id: "main".to_string(),
                anchor_ms: temporal.anchor_ms,
                anchor_src,
                doc_kind: temporal.doc_kind.clone(),
                valid_until_ms: temporal.valid_until_ms,
            };
            let _ = self.index.write_temporal_entry(&entry).await;
        }

        for link in &req.links {
            let _ = self.index.upsert_link("main", &link.src, &link.dst).await;
        }

        if let Some(trust) = req.trust {
            let _ = self
                .index
                .set_note_trust(written.frontmatter.vault_id.as_str(), &written.id, trust)
                .await;
        }

        Ok(PersistOkResponse {
            note_id: req.note_id.clone(),
            status: "ok".to_string(),
        })
    }

    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;

        let dim = req.vector.len();
        self.index
            .insert_note_embedding("main", &note_id, &req.embedder_id, req.dim, &req.vector)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })?;

        Ok(EmbeddingOkResponse {
            note_id: req.note_id.clone(),
            embedder_id: req.embedder_id.clone(),
            dim,
        })
    }

    async fn persist_forget(
        &self,
        req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;
        let section = parse_section(&req.section)?;

        let existing = self
            .vault
            .read_note(note_id)
            .await
            .map_err(|e| vault_err_to_client(e, &req.note_id))?;

        let mut new_fm = existing.frontmatter.clone();
        new_fm.section = section;
        new_fm.forgotten = Some(true);
        new_fm.forgotten_at = Some(Utc::now());
        new_fm.forgotten_by = req.forgotten_by.clone();

        self.vault
            .write_note_with_id(new_fm, req.body.clone(), note_id)
            .await
            .map_err(|e| vault_err_to_client(e, &req.note_id))?;

        let _ = self
            .index
            .mark_forgotten("main", &req.note_id, req.forgotten_by.as_deref())
            .await;

        Ok(PersistOkResponse {
            note_id: req.note_id.clone(),
            status: "ok".to_string(),
        })
    }

    /// Miroir de `POST /internal/v1/note/:ulid/forget-resync` : ré-affirme la marque
    /// d'index SANS toucher au frontmatter ni aux colonnes d'audit.
    async fn resync_forget_index(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<(), InternalClientError> {
        self.index
            .reassert_forgotten(vault_id, ulid)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("reassert_forgotten failed: {e}"),
            })
    }

    async fn persist_distill(
        &self,
        req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;

        // Try to read the existing note. If it does not exist (new synthesis note),
        // create it with a PendingReview frontmatter. This mirrors the server-side
        // behavior where the synthesis note is written via persist_distill on first call.
        let (mut new_fm, body_to_write) = match self.vault.read_note(note_id).await {
            Ok(existing) => {
                // Existing note — update in place (source marking path).
                let section = if req.section.is_empty() {
                    existing.frontmatter.section
                } else {
                    parse_section(&req.section)?
                };
                let mut fm = existing.frontmatter.clone();
                fm.section = section;
                let body = if req.body.is_empty() {
                    existing.body.markdown
                } else {
                    req.body.clone()
                };
                (fm, body)
            }
            Err(VaultError::Core(gradatum_core::error::GradatumError::NoteNotFound(_)))
            | Err(VaultError::Storage(_)) => {
                // New note (synthesis) — create with PendingReview.
                let section = if req.section.is_empty() {
                    gradatum_core::section::Section::Reference
                } else {
                    parse_section(&req.section)?
                };
                let mut extra = ExtraFields::empty();
                if !req.derived_from.is_empty() {
                    let derived_from_vals: Vec<TomlValue> = req
                        .derived_from
                        .iter()
                        .map(|id| TomlValue::String(id.clone()))
                        .collect();
                    let extra_map = extra
                        .0
                        .get_or_insert_with(|| Box::new(std::collections::BTreeMap::new()));
                    extra_map.insert(
                        "derived-from".to_string(),
                        TomlValue::Array(derived_from_vals),
                    );
                }
                let fm = Frontmatter {
                    schema_version: 1,
                    vault_id: VaultId::new("main"),
                    locus: None,
                    section,
                    status: NoteStatus::PendingReview,
                    status_reason: Some("distilled — en attente de revue".to_string()),
                    status_changed: None,
                    tags: smallvec::SmallVec::new(),
                    author: Some(gradatum_core::author::AuthorRef {
                        kind: gradatum_core::author::AuthorKind::System,
                        id: "vault-distiller".to_string(),
                        display_name: None,
                    }),
                    created: Utc::now(),
                    updated: None,
                    extra,
                    provenance: Some("distilled".to_string()),
                    forgotten: None,
                    forgotten_at: None,
                    forgotten_by: None,
                };
                (fm, req.body.clone())
            }
            Err(e) => return Err(vault_err_to_client(e, &req.note_id)),
        };

        if req.mark_processed {
            let extra_map = new_fm
                .extra
                .0
                .get_or_insert_with(|| Box::new(std::collections::BTreeMap::new()));
            extra_map.insert("processed".to_string(), TomlValue::Boolean(true));
            if let Some(ref into_ulid) = req.derived_into {
                extra_map.insert(
                    "derived-into".to_string(),
                    TomlValue::String(into_ulid.clone()),
                );
            }
        }

        let written = self
            .vault
            .write_note_with_id(new_fm, body_to_write, note_id)
            .await
            .map_err(|e| vault_err_to_client(e, &req.note_id))?;

        if !req.title.is_empty() {
            let _ = self
                .index
                .upsert_note_title(
                    written.frontmatter.vault_id.as_str(),
                    &written.id,
                    &req.title,
                )
                .await;
        }

        if let Some(trust) = req.trust {
            let _ = self
                .index
                .set_note_trust(written.frontmatter.vault_id.as_str(), &written.id, trust)
                .await;
        }

        Ok(PersistOkResponse {
            note_id: req.note_id.clone(),
            status: "ok".to_string(),
        })
    }

    async fn delete_note(&self, vault_id: &str, ulid: &str) -> Result<(), InternalClientError> {
        let note_id =
            Ulid::from_string(ulid)
                .map(NoteId)
                .map_err(|e| InternalClientError::ServerError {
                    status: 400,
                    body: format!("{e}"),
                })?;

        self.vault
            .delete_note(note_id)
            .await
            .map_err(|e| vault_err_to_client(e, ulid))?;

        let _ = self.index.delete_note_from_index(vault_id, ulid).await;
        let _ = self.index.delete_redirect_by_ulid(vault_id, ulid).await;

        Ok(())
    }

    /// Lecture **scopée par vault** (A2-bis) — miroir de
    /// `GET /internal/v1/note/:ulid?vault_id=…`.
    ///
    /// Le harnais n'a qu'UN vault physique : le `.md` ne peut donc pas, à lui seul,
    /// distinguer deux homonymes. Ce qui distingue les vaults ici, c'est l'INDEX, dont la
    /// clé est composite `(vault_id, id)` (migration 0032) — la **même source** que les
    /// listings (`search_fts_for_forget`, `list_notes_by_locus`, `list_notes_by_agent`).
    ///
    /// Règle :
    /// - l'index tranche l'**existence** et les métadonnées scopées (`section`, `status`) ;
    /// - le `.md` n'enrichit (corps, hash, tags, `forgotten`, `processed`) que s'il
    ///   appartient au vault demandé, sinon ses colonnes d'index font foi.
    ///
    /// Repli sur le `.md` seul **uniquement** quand l'index n'a pas de ligne ET que le
    /// vault demandé est le vault physique : c'est le comportement historique, préservé
    /// pour les tests qui écrivent hors index. Pour tout autre vault, l'absence de ligne
    /// est un `NotFound` — jamais un repli silencieux sur l'homonyme du vault physique,
    /// qui est exactement le trou que ce lot ferme.
    async fn get_note(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        let note_id =
            Ulid::from_string(ulid)
                .map(NoteId)
                .map_err(|e| InternalClientError::ServerError {
                    status: 400,
                    body: format!("{e}"),
                })?;

        let record = self.index.get_note(vault_id, ulid).await.map_err(|e| {
            InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            }
        })?;

        let is_physical_vault = self.vault.vault_id().as_str() == vault_id;
        let md = if is_physical_vault {
            self.vault.read_note(note_id).await.ok()
        } else {
            None
        };

        match (record, md) {
            // Ligne d'index + `.md` du même vault : métadonnées de l'index, corps du `.md`.
            (Some(rec), Some(note)) => Ok(NoteReadDto {
                note_id: ulid.to_string(),
                sha256_hex: note.content_hash.hex(),
                body: note.body.markdown,
                section: rec.section,
                status: rec.status,
                tags: note
                    .frontmatter
                    .tags
                    .iter()
                    .map(|t| t.as_str().to_string())
                    .collect(),
                forgotten: note.frontmatter.forgotten.unwrap_or(false),
                processed: note
                    .frontmatter
                    .extra
                    .get("processed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }),
            // Ligne d'index sans `.md` accessible dans ce vault : l'index fait foi seul.
            (Some(rec), None) => Ok(NoteReadDto {
                note_id: ulid.to_string(),
                sha256_hex: hex_lower(&rec.content_hash),
                body: rec.body_text,
                section: rec.section,
                status: rec.status,
                tags: rec
                    .tags_raw
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
                forgotten: false,
                processed: false,
            }),
            // Pas de ligne d'index mais un `.md` du vault physique : comportement
            // historique (tests qui écrivent le `.md` sans passer par l'index).
            (None, Some(note)) => Ok(NoteReadDto {
                note_id: ulid.to_string(),
                sha256_hex: note.content_hash.hex(),
                body: note.body.markdown,
                section: section_to_str(note.frontmatter.section),
                status: status_to_str(note.frontmatter.status),
                tags: note
                    .frontmatter
                    .tags
                    .iter()
                    .map(|t| t.as_str().to_string())
                    .collect(),
                forgotten: note.frontmatter.forgotten.unwrap_or(false),
                processed: note
                    .frontmatter
                    .extra
                    .get("processed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }),
            // Absente de CE vault.
            (None, None) => Err(InternalClientError::NotFound {
                ulid: ulid.to_string(),
            }),
        }
    }

    /// Re-check statut SCOPÉ (C4-1e W3) : délègue à l'INDEX réel
    /// (`get_note_status(vault_id, id)`, `WHERE vault_id = ?1 AND id = ?2`) — même
    /// source que `list_garbage`, contrairement à `get_note` qui lit le `.md` mono-vault.
    async fn get_note_status(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        self.index
            .get_note_status(vault_id, ulid)
            .await
            .map(|opt| opt.map(|s| s.to_string()))
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn get_note_embedding(
        &self,
        vault_id: &str,
        ulid: &str,
        embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        let note_id =
            Ulid::from_string(ulid)
                .map(NoteId)
                .map_err(|e| InternalClientError::ServerError {
                    status: 400,
                    body: format!("{e}"),
                })?;

        match self
            .index
            .get_note_embedding(vault_id, &note_id, embedder_id)
            .await
        {
            Ok(Some(vector)) => Ok(EmbeddingReadDto {
                note_id: ulid.to_string(),
                embedder_id: embedder_id.to_string(),
                dim: vector.len(),
                vector,
            }),
            Ok(None) => Err(InternalClientError::NotFound {
                ulid: ulid.to_string(),
            }),
            Err(e) => Err(InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            }),
        }
    }

    async fn get_trust(&self, vault_id: &str, ulid: &str) -> Result<f32, InternalClientError> {
        let note_id =
            Ulid::from_string(ulid)
                .map(NoteId)
                .map_err(|e| InternalClientError::ServerError {
                    status: 400,
                    body: format!("{e}"),
                })?;

        match self.index.get_trust(vault_id, &note_id).await {
            Ok(Some(trust)) => Ok(trust),
            Ok(None) => Err(InternalClientError::NotFound {
                ulid: ulid.to_string(),
            }),
            Err(e) => Err(InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            }),
        }
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        match self.index.title_lookup("main", title).await {
            Ok(result) => Ok(result.map(|id| id.to_string())),
            Err(e) => Err(InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            }),
        }
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        match self.index.id_lookup("main", note_id).await {
            Ok(result) => Ok(result),
            Err(e) => Err(InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            }),
        }
    }

    async fn list_notes_by_locus(
        &self,
        vault: &str,
        prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        // list_notes_by_locus_prefix returns Vec<(note_id_str, section_str)>
        self.index
            .list_notes_by_locus_prefix(vault, prefix)
            .await
            .map(|notes| {
                notes
                    .into_iter()
                    .map(|(id_str, sec_str)| NoteIdDto {
                        note_id: id_str,
                        section: sec_str,
                    })
                    .collect()
            })
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn list_by_status(
        &self,
        vault: &str,
        status_str: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let status = parse_status(status_str)?;
        let vault_id = VaultId::new(vault);
        // SqliteIndex::list_by_status takes (&VaultId, NoteStatus)
        self.index
            .list_by_status(&vault_id, status)
            .await
            .map(|ids| {
                ids.into_iter()
                    .map(|id| NoteIdDto {
                        note_id: id.to_string(),
                        section: String::new(),
                    })
                    .collect()
            })
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn list_garbage(
        &self,
        vault: &str,
        before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        // list_garbage_older_than(vault_id: &str, cutoff_ms: i64)
        // The cutoff_ms is already computed by the handler; grace_days is informational only.
        self.index
            .list_garbage_older_than(vault, before_ms)
            .await
            .map(|ids| {
                ids.into_iter()
                    .map(|id| NoteIdDto {
                        note_id: id.to_string(),
                        section: String::new(),
                    })
                    .collect()
            })
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn search_fts_for_forget(
        &self,
        vault: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        // search_fts_for_forget returns Vec<(note_id_str, section_str)>
        self.index
            .search_fts_for_forget(vault, query, limit)
            .await
            .map(|notes| {
                notes
                    .into_iter()
                    .map(|(id_str, sec_str)| NoteIdDto {
                        note_id: id_str,
                        section: sec_str,
                    })
                    .collect()
            })
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn list_notes_by_agent(
        &self,
        agent: &str,
        vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        // list_notes_by_agent returns Vec<(note_id_str, section_str)>
        self.index
            .list_notes_by_agent(agent, vaults)
            .await
            .map(|notes| {
                notes
                    .into_iter()
                    .map(|(id_str, sec_str)| NoteIdDto {
                        note_id: id_str,
                        section: sec_str,
                    })
                    .collect()
            })
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }
}
