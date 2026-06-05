//! Dispatcher de jobs : poll queue → curator cascade → vault write → audit.
//!
//! # Note Phase 2 v0.2.0
//!
//! Ce module est conservé pour compatibilité avec les tests d'intégration existants.
//! Le binaire v0.2.0 utilise le Monitor Apalis (`monitor.rs`) — Dispatcher non actif.
//!
//! ## Phase 2.0c T5 — implémentation complète
//!
//! `process_job` traite les 3 kinds de jobs :
//! - `curate`    : decode VaultWriteRequest → CuratorPipeline.process → Vault.write_note
//! - `classify`  : decode VaultClassifyRequest → read_note → CuratorProcess.process (cascade B3) → Vault.write_note
//! - `downgrade` : decode VaultDowngradeRequest → read_note → state machine → Vault.write_note
//!
//! ## Garanties
//!
//! - `run_once` est idempotent : si la queue est vide, retourne `Ok(false)`.
//! - Les erreurs de traitement sont loguées et passées à `Queue::fail` —
//!   jamais de crash silencieux.
//! - Le job est `complete`-é uniquement si `process_job` retourne `Ok(())`.
//! - AuditSink optionnel : si absent, les événements sont loggués mais non persistés.
//!

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::audit::http::{AuditSink, HttpAuditActor, HttpAuditEvent};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_curator::{CurateOutcome, CuratorProcess, Note as CuratorNote};
use gradatum_embed::Embedder;
use gradatum_index::SqliteIndex;
// Traits de storage — nécessaires pour résoudre insert_note_embedding sur Arc<SqliteIndex>.
use gradatum_core::VectorStore as _;
use gradatum_queue::{LeasedJob, NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tracing::instrument;
use ulid::Ulid;

/// Durée de lease par défaut pour un job dispatché.
///
/// 5 minutes : suffisant pour la cascade curator (novelty + routing + tags
/// + wikilinks + dedup) sur les modèles heuristiques et LLM légers.
const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(300);

/// Acteur système utilisé pour les événements d'audit émis par le worker.
///
/// Distinct des acteurs JWT du serveur HTTP — le worker agit en mode
/// batch non-interactif pour le compte du pipeline curator.
const WORKER_SYSTEM_KID: &str = "gradatum-worker";

// ── DTOs locaux (miroir de gradatum-server pour éviter la dépendance circulaire) ─

/// Requête `vault_write` décodée depuis le payload bincode de la queue.
///
/// Structure identique à `gradatum_server::api_v1::dto::VaultWriteRequest`.
/// Dupliquée ici pour éviter de créer une dépendance worker→server (circuit).
#[derive(Debug, Serialize, Deserialize)]
struct VaultWriteRequest {
    title: String,
    body: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    section_hint: Option<String>,
    #[serde(default = "default_main")]
    tenant_id: String,
}

/// Requête `vault_classify` décodée depuis le payload bincode de la queue.
#[derive(Debug, Serialize, Deserialize)]
struct VaultClassifyRequest {
    note_id: String,
    #[serde(default = "default_main")]
    tenant_id: String,
}

/// Requête `vault_downgrade` décodée depuis le payload bincode de la queue.
#[derive(Debug, Serialize, Deserialize)]
struct VaultDowngradeRequest {
    note_id: String,
    reason: String,
    #[serde(default)]
    replaced_by: Option<String>,
    #[serde(default = "default_main")]
    tenant_id: String,
}

fn default_main() -> String {
    "main".into()
}

// ── NoopAuditSink ─────────────────────────────────────────────────────────────

/// Sink d'audit no-op pour les tests et les modes sans persistance d'audit.
///
/// T7 câblera `JsonlFileSink` comme implémentation de production.
pub struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    /// Ne fait rien — les événements sont silencieusement ignorés.
    async fn record(&self, _event: HttpAuditEvent) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

/// Dispatcher de jobs pour le worker P2.0c.
///
/// Construit via le pattern builder :
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use gradatum_queue::SqliteQueue;
/// # use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
/// # async fn ex(queue: Arc<SqliteQueue>, vault: Arc<gradatum_vault::Vault>, curator: Arc<gradatum_curator::CuratorPipeline>) {
/// let dispatcher = Dispatcher::new(queue)
///     .with_vault(vault)
///     .with_curator(curator)
///     .with_audit(Arc::new(NoopAuditSink));
/// # }
/// ```
pub struct Dispatcher {
    queue: Arc<SqliteQueue>,
    vault: Option<Arc<Vault>>,
    /// Pipeline de curation injectable (trait objet) — Phase 4 alpha.15 Task 18.
    ///
    /// Accepte `CuratorPipeline` (production) ou un mock pour les tests.
    curator: Option<Arc<dyn CuratorProcess>>,
    audit: Option<Arc<dyn AuditSink>>,
    /// Index SQLite pour la persistance des embeddings (Phase 2.1.1).
    ///
    /// Optionnel — si absent, `embed_note` est silencieusement ignoré.
    index: Option<Arc<SqliteIndex>>,
    /// Backend d'embedding (Phase 2.1.1).
    ///
    /// Optionnel — si absent, `embed_note` est silencieusement ignoré.
    embedder: Option<Arc<dyn Embedder>>,
}

impl Dispatcher {
    /// Crée un nouveau dispatcher avec la queue donnée.
    pub fn new(queue: Arc<SqliteQueue>) -> Self {
        Self {
            queue,
            vault: None,
            curator: None,
            audit: None,
            index: None,
            embedder: None,
        }
    }

    /// Injecte le vault pour la persistance des notes.
    #[must_use]
    pub fn with_vault(mut self, vault: Arc<Vault>) -> Self {
        self.vault = Some(vault);
        self
    }

    /// Injecte la pipeline curator pour le traitement heuristique et LLM.
    ///
    /// Accepte tout type implémentant [`CuratorProcess`] (incluant `CuratorPipeline`
    /// pour la production et les mocks pour les tests — Phase 4 alpha.15 Task 18).
    #[must_use]
    pub fn with_curator(mut self, curator: Arc<dyn CuratorProcess>) -> Self {
        self.curator = Some(curator);
        self
    }

    /// Injecte le sink d'audit pour la traçabilité des opérations.
    ///
    /// Optionnel — si absent, les événements sont loggués sans persistance.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Injecte l'index SQLite pour la persistance des embeddings (Phase 2.1.1).
    ///
    /// Requis pour traiter les jobs `embed_note`. Sans lui, les jobs sont
    /// silencieusement ignorés (skip noop).
    #[must_use]
    pub fn with_index(mut self, index: Arc<SqliteIndex>) -> Self {
        self.index = Some(index);
        self
    }

    /// Injecte le backend d'embedding (Phase 2.1.1).
    ///
    /// Requis pour traiter les jobs `embed_note`. Sans lui, les jobs sont
    /// silencieusement ignorés (skip noop).
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Tente de traiter un job disponible.
    ///
    /// Retourne `Ok(true)` si un job a été traité (succès ou failure loguée),
    /// `Ok(false)` si la queue était vide (backoff à l'appelant).
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let leased = self
            .queue
            .lease(
                &["curate", "classify", "downgrade", "embed_note"],
                DEFAULT_LEASE_DURATION,
            )
            .await?;

        let Some(job) = leased else {
            return Ok(false);
        };

        match self.process_job(&job).await {
            Ok(()) => {
                self.queue.complete(job.id).await?;
            }
            Err(e) => {
                tracing::error!(
                    job_id = job.id,
                    kind = %job.kind,
                    error = %e,
                    "job échoué — enregistrement pour retry ou dead-letter"
                );
                self.queue.fail(job.id, &e.to_string()).await?;
            }
        }

        Ok(true)
    }

    /// Traite un job leasé — cascade complète curator + vault + audit.
    ///
    /// ## Kinds supportés
    ///
    /// - `curate`    : admission heuristique + persist note.
    /// - `classify`  : re-routing section d'une note existante.
    /// - `downgrade` : transition Live → Deprecated validée par la state machine.
    ///
    /// ## Erreurs
    ///
    /// Toute erreur est remontée à `run_once` qui la passe à `Queue::fail`.
    /// Jamais de panic — le job est retryable.
    #[instrument(skip(self), fields(job_id = job.id, kind = %job.kind))]
    async fn process_job(&self, job: &LeasedJob) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        let outcome: &str;

        // embed_note est traité avant d'accéder à vault/curator (optionnels pour ce kind).
        if job.kind.as_str() == "embed_note" {
            self.process_embed_note(job).await?;
            let duration_ms = start.elapsed().as_millis() as i64;
            self.emit_audit(job, "ok", duration_ms).await;
            tracing::info!(
                job_id = job.id,
                kind = %job.kind,
                outcome = "ok",
                duration_ms = duration_ms,
                "job traité"
            );
            return Ok(());
        }

        let vault = self
            .vault
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault non configuré — appeler with_vault"))?;
        let curator = self
            .curator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("curator non configuré — appeler with_curator"))?;

        match job.kind.as_str() {
            // ── curate : novelty + routing + tags → vault.write_note ──────────
            "curate" => {
                let req: VaultWriteRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultWriteRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    title = %req.title,
                    "job curate — lancement cascade curator"
                );

                // Construire le Note curator à partir de la requête
                let curator_note = CuratorNote {
                    id: ulid::Ulid::new().to_string(),
                    title: req.title.clone(),
                    body: req.body.clone(),
                    tags_hint: req.tags.clone(),
                    section_hint: req.section_hint.clone(),
                };

                let curate_outcome = curator.process(curator_note).await;

                match curate_outcome {
                    CurateOutcome::Admitted { decisions } => {
                        // Convertir section string → Section enum via serde
                        let section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let frontmatter = build_frontmatter(
                            &job.tenant_id,
                            section,
                            NoteStatus::Live,
                            &req,
                            &decisions.tags,
                        );

                        let note = vault
                            .write_note(frontmatter, req.body.clone())
                            .await
                            .context("vault.write_note curate")?;

                        outcome = "admitted";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note admise et persistée"
                        );

                        // ── B5 (alpha.13 Task 13) : wikilinks post-curate ─────
                        //
                        // Extraction `[[Titre]]` depuis le body de la requête, résolution via
                        // `idx.title_lookup` (filtre `status='live'`), persistance via
                        // `idx.upsert_link` (idempotent INSERT OR IGNORE).
                        //
                        // **Non-fatal** : un échec d'extraction, de title_lookup ou d'upsert
                        // ne remet jamais en cause la note déjà persistée — uniquement loggué.
                        //
                        // Caveat backlog C9 (council review B-P0-4, Phase 2.x.5) : la boucle
                        // `for target in &wikilinks` est série (N×N si N notes × N wikilinks).
                        // Cible 2.x.5 : batch `WHERE title IN (?, ?, ?)` ou tokio::join_all.
                        process_wikilinks_b5(self, &job.tenant_id, &note.id.to_string(), &req.body)
                            .await;

                        // Phase 2.1.1 — chaînage automatique : enqueue embed_note après curate write réussi.
                        // Best-effort : un échec d'enqueue n'invalide pas le curate (note déjà persistée).
                        // Le backfill (gradatum-admin backfill-embeddings) pourra re-tenter si nécessaire.
                        let embed_payload = serde_json::json!({
                            "note_id": note.id.to_string(),
                            "body_text": note.body.markdown,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %note.id,
                                error = %e,
                                "chaînage embed_note enqueue échoué — backfill pourra re-tenter"
                            );
                        }
                    }
                    CurateOutcome::Rejected { reason } => {
                        // Note rejetée — pas d'écriture dans le vault
                        outcome = "rejected";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note rejetée par le curator — aucune écriture"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        // Note en attente de revue manuelle — écriture avec statut Staging
                        let section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let frontmatter = build_frontmatter(
                            &job.tenant_id,
                            section,
                            NoteStatus::Staging,
                            &req,
                            &decisions.tags,
                        );

                        let note = vault
                            .write_note(frontmatter, req.body.clone())
                            .await
                            .context("vault.write_note curate pending")?;

                        outcome = "pending";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note mise en Staging (revue manuelle requise)"
                        );

                        // ── B5 (alpha.13 Task 13) : wikilinks post-curate (parité Pending) ──
                        //
                        // Même branchage que la branche Admitted — résolution L-P0-1
                        // rev2 §2.4 cas Pending. Un brouillon avec wikilinks doit voir ses
                        // liens persistés au même titre qu'une note admise.
                        process_wikilinks_b5(self, &job.tenant_id, &note.id.to_string(), &req.body)
                            .await;

                        // Phase 2.1.1 — chaînage automatique : enqueue embed_note après curate write réussi.
                        // Best-effort : un échec d'enqueue n'invalide pas le curate (note déjà persistée).
                        // Le backfill (gradatum-admin backfill-embeddings) pourra re-tenter si nécessaire.
                        let embed_payload = serde_json::json!({
                            "note_id": note.id.to_string(),
                            "body_text": note.body.markdown,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %note.id,
                                error = %e,
                                "chaînage embed_note enqueue échoué — backfill pourra re-tenter"
                            );
                        }
                    }
                }
            }

            // ── classify : re-router la section d'une note via cascade curator complète ──
            //
            // Phase 4 alpha.15 Task 18 — B3 : brancher la cascade curator complète
            // (heuristique + LLM si configuré) au lieu de `heuristic_route` seul.
            // Cohérence curate ↔ classify : même pipeline de décision.
            "classify" => {
                let req: VaultClassifyRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultClassifyRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    "job classify — cascade curator complète (B3 alpha.15)"
                );

                // Lire la note existante depuis le vault (inchangé)
                let note_ulid = Ulid::from_string(&req.note_id)
                    .map_err(|e| anyhow::anyhow!("ULID invalide {}: {e}", req.note_id))?;
                let note_id = NoteId(note_ulid);
                let existing = vault
                    .read_note(note_id)
                    .await
                    .context("read_note pour classify")?;

                // Construire le CuratorNote depuis la note existante.
                // section_hint = None : laisser le curator décider de la section canonique.
                // Le "titre" est reconstitué depuis le H1 du body si présent, sinon
                // depuis la section courante (proxy sémantique).
                let title_for_curator = gradatum_curator::extract_h1_title(&existing.body.markdown)
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| existing.frontmatter.section.as_str().to_string());

                let curator_note = CuratorNote {
                    id: req.note_id.clone(),
                    title: title_for_curator,
                    body: existing.body.markdown.clone(),
                    tags_hint: existing
                        .frontmatter
                        .tags
                        .iter()
                        .map(|t| t.as_str().to_string())
                        .collect(),
                    section_hint: None,
                };

                tracing::debug!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    "classify curator processing — cascade complète"
                );

                let curate_outcome = curator.process(curator_note).await;

                match curate_outcome {
                    CurateOutcome::Admitted { decisions } => {
                        let new_section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let mut updated_fm = existing.frontmatter.clone();
                        updated_fm.section = new_section;

                        // Union des tags curator avec les tags existants
                        // (Tag::new peut retourner Err pour des tags malformés — skip silencieux)
                        for tag_str in &decisions.tags {
                            if !updated_fm
                                .tags
                                .iter()
                                .any(|t| t.as_str() == tag_str.as_str())
                            {
                                if let Ok(t) = gradatum_core::tag::Tag::new(tag_str) {
                                    updated_fm.tags.push(t);
                                }
                            }
                        }

                        // NOTE v0.3.4 : si ce path classify est étendu (write_note ci-dessous),
                        // wirer upsert_note_title (cf apalis_handlers.rs:324) sinon notes.title
                        // ne sera pas peuplé pour les notes reclassifiées.
                        vault
                            .write_note(updated_fm, existing.body.markdown.clone())
                            .await
                            .context("vault.write_note classify admitted")?;

                        outcome = "reclassified";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note reclassifiée par cascade curator (Admitted)"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        let new_section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let mut updated_fm = existing.frontmatter.clone();
                        updated_fm.section = new_section;
                        updated_fm.status = NoteStatus::Staging;

                        // NOTE v0.3.4 : si ce path classify est étendu (write_note ci-dessous),
                        // wirer upsert_note_title (cf apalis_handlers.rs:324) sinon notes.title
                        // ne sera pas peuplé pour les notes mises en Staging.
                        vault
                            .write_note(updated_fm, existing.body.markdown.clone())
                            .await
                            .context("vault.write_note classify pending")?;

                        outcome = "classify_pending";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "note mise en Staging par classify (LLM incertain)"
                        );
                    }
                    CurateOutcome::Rejected { reason } => {
                        // Rejected = log warn + skip écriture — note inchangée dans le vault.
                        // Non-fatal : le job est considéré comme traité.
                        outcome = "classify_rejected";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "classify rejeté par le curator — note inchangée dans le vault"
                        );
                    }
                }
            }

            // ── downgrade : transition Live → Deprecated ──────────────────────
            "downgrade" => {
                let req: VaultDowngradeRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultDowngradeRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    reason = %req.reason,
                    "job downgrade — rétrogradation de la note"
                );

                // Lire la note existante
                let note_ulid = Ulid::from_string(&req.note_id)
                    .map_err(|e| anyhow::anyhow!("ULID invalide {}: {e}", req.note_id))?;
                let note_id = NoteId(note_ulid);
                let existing = vault
                    .read_note(note_id)
                    .await
                    .context("read_note pour downgrade")?;

                // Valider la state machine : seul Live peut passer à Deprecated
                if !existing
                    .frontmatter
                    .status
                    .can_transition_to(NoteStatus::Deprecated)
                {
                    anyhow::bail!(
                        "transition invalide {:?} → Deprecated pour la note {} — seul Live est autorisé",
                        existing.frontmatter.status,
                        req.note_id
                    );
                }

                // Réécrire avec statut Deprecated + raison
                let mut downgraded_fm = existing.frontmatter.clone();
                downgraded_fm.status = NoteStatus::Deprecated;
                downgraded_fm.status_reason = Some(req.reason.clone());
                downgraded_fm.status_changed = Some(Utc::now());

                vault
                    .write_note(downgraded_fm, existing.body.markdown.clone())
                    .await
                    .context("vault.write_note downgrade")?;

                outcome = "deprecated";
                tracing::info!(job_id = job.id, "note rétrogradée vers Deprecated");
            }

            other => {
                anyhow::bail!("kind de job inconnu : {other:?}");
            }
        }

        // ── Émission d'audit ──────────────────────────────────────────────────
        let duration_ms = start.elapsed().as_millis() as i64;
        self.emit_audit(job, outcome, duration_ms).await;

        tracing::info!(
            job_id = job.id,
            kind = %job.kind,
            outcome = outcome,
            duration_ms = duration_ms,
            "job traité"
        );

        Ok(())
    }

    /// Traite un job `embed_note` : calcule l'embedding du corps de la note
    /// et le persiste dans `note_embeddings` via l'index SQLite.
    ///
    /// ## Comportement skip silencieux
    ///
    /// - Embedder absent (`with_embedder` non appelé) → `Ok(())` sans insert.
    /// - Index absent (`with_index` non appelé) → `Ok(())` sans insert.
    /// - `body_text` vide → `Ok(())` sans calcul.
    ///
    /// ## Payload JSON
    ///
    /// ```json
    /// { "note_id": "<ULID>", "body_text": "<markdown>" }
    /// ```
    ///
    /// ## Troncature
    ///
    /// Le corps est tronqué à 2 048 caractères Unicode (≈ 8 KB UTF-8 worst-case)
    /// avant l'appel embedder pour éviter les dépassements de contexte modèle.
    async fn process_embed_note(&self, job: &LeasedJob) -> anyhow::Result<()> {
        let embedder = match &self.embedder {
            Some(e) => e,
            None => {
                tracing::info!(job_id = job.id, "embed_note skipped — embedder absent");
                return Ok(());
            }
        };
        let index = match &self.index {
            Some(i) => i,
            None => {
                tracing::info!(job_id = job.id, "embed_note skipped — index absent");
                return Ok(());
            }
        };

        let payload: serde_json::Value =
            serde_json::from_slice(&job.payload).context("embed_note: parse payload JSON")?;

        let note_id_str = payload
            .get("note_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("embed_note: payload manque 'note_id'"))?;

        let body_text = payload
            .get("body_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if body_text.is_empty() {
            tracing::info!(
                job_id = job.id,
                note_id = %note_id_str,
                "embed_note skipped — body vide"
            );
            return Ok(());
        }

        // Tronquer à 2 048 caractères Unicode (UTF-8-safe via char_indices).
        // Évite les dépassements de contexte modèle sans slice byte arbitraire.
        let truncated = if body_text.len() > 8192 {
            let end = body_text
                .char_indices()
                .nth(2048)
                .map(|(i, _)| i)
                .unwrap_or(body_text.len());
            &body_text[..end]
        } else {
            body_text
        };

        let vec = embedder
            .embed(truncated)
            .await
            .map_err(|e| anyhow::anyhow!("embed_note embed: {e}"))?;

        let note_ulid = Ulid::from_string(note_id_str)
            .map_err(|e| anyhow::anyhow!("embed_note: ULID invalide '{note_id_str}': {e}"))?;
        let note_id = NoteId(note_ulid);

        index
            .insert_note_embedding(&note_id, embedder.embedder_id(), embedder.dim(), &vec)
            .await
            .map_err(|e| anyhow::anyhow!("embed_note insert_note_embedding: {e}"))?;

        tracing::info!(
            job_id = job.id,
            note_id = %note_id_str,
            embedder_id = embedder.embedder_id(),
            dim = embedder.dim(),
            "embed_note done"
        );

        Ok(())
    }

    /// Émet un événement d'audit pour un job traité.
    ///
    /// Les erreurs d'audit sont loguées sans propager — le job est déjà traité.
    async fn emit_audit(&self, job: &LeasedJob, outcome: &str, duration_ms: i64) {
        if let Some(audit) = &self.audit {
            let event = HttpAuditEvent {
                ts: Utc::now(),
                event: format!("worker_{}", job.kind),
                actor: HttpAuditActor {
                    kid: WORKER_SYSTEM_KID.into(),
                    sub: "gradatum-worker".into(),
                    aud: "gradatum".into(),
                },
                tenant_id: job.tenant_id.clone(),
                locus: format!("{}/{}", job.tenant_id, job.kind),
                note_id: None,
                content_hash: None,
                outcome: outcome.into(),
                curator: Some(serde_json::json!({ "duration_ms": duration_ms })),
                request_id: format!("job-{}", job.id),
            };
            // Les erreurs d'audit sont loguées sans propager — le job est déjà traité.
            if let Err(e) = audit.record(event).await {
                tracing::warn!(
                    job_id = job.id,
                    error = %e,
                    "échec écriture audit — le job est quand même marqué complet"
                );
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Convertit une section kebab-case en `Section` enum via serde_json.
///
/// Retourne `None` si la chaîne n'est pas une section canonique valide.
/// L'appelant doit fournir un fallback (typiquement `Section::Reference`).
fn section_from_str(s: &str) -> Option<Section> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<Section>(&json_str).ok()
}

/// Construit un `Frontmatter` depuis une `VaultWriteRequest` et les décisions curator.
///
/// ## Invariants
///
/// - `vault_id` = tenant courant (mono-tenant Phase 1).
/// - `created` = `Utc::now()`.
/// - `tags` = union tags_hint + tags curator (dédupliqués).
/// - `author` = request.author si fourni.
fn build_frontmatter(
    tenant_id: &str,
    section: Section,
    status: NoteStatus,
    req: &VaultWriteRequest,
    curator_tags: &[String],
) -> Frontmatter {
    // Union tags request + tags curator (ordre : request en premier, curator ensuite)
    let mut all_tags: Vec<String> = req.tags.clone();
    for t in curator_tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }

    // Tags validés — les tags malformés sont ignorés silencieusement (défense en profondeur)
    let tags: SmallVec<[gradatum_core::tag::Tag; 4]> = all_tags
        .iter()
        .filter_map(|t| gradatum_core::tag::Tag::new(t.clone()).ok())
        .collect();

    let author = req
        .author
        .as_deref()
        .map(gradatum_core::author::AuthorRef::system);

    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(tenant_id),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: None,
        tags,
        author,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
    }
}

// ── B5 (alpha.13 Task 13) — wikilinks post-curate ────────────────────────────

/// Extrait les wikilinks `[[...]]` du body, les résout via `idx.title_lookup` et
/// les persiste dans `note_links` via `idx.upsert_link`.
///
/// **Non-fatal absolu** : un échec d'extraction, de title_lookup ou d'upsert_link
/// ne propage jamais d'erreur — uniquement loggué (`warn!`/`debug!`). La note est
/// déjà persistée à ce point ; un B5 défaillant ne doit JAMAIS retry-er le job.
///
/// **Idempotence** : `upsert_link` utilise `INSERT OR IGNORE` côté SQLite — un
/// doublon (même paire src/dst sur le même vault) est silencieusement ignoré.
///
/// **Comportement si `index` absent** : skip silencieux (cas worker démarré sans
/// `with_index` — ex. tests historiques avant alpha.13). Les wikilinks ne sont
/// pas extraits dans ce cas.
///
/// **Comportement si une note cible n'existe pas** (`title_lookup` retourne `None`) :
/// log `debug` et skip — le wikilink reste "en suspens", non persisté. Caveat
/// backlog C3 (Phase 3) : pas de résolution rétroactive lors de la création
/// ultérieure de la note cible.
async fn process_wikilinks_b5(
    dispatcher: &Dispatcher,
    tenant_id: &str,
    src_note_id: &str,
    body: &str,
) {
    let Some(idx) = dispatcher.index.as_ref() else {
        tracing::debug!(
            note_id = %src_note_id,
            "B5 skip: dispatcher sans index injecté (test historique ?)"
        );
        return;
    };

    let wikilinks = gradatum_curator::wikilinks::extract_wikilinks(body);
    if wikilinks.is_empty() {
        return;
    }

    // C9 résolue (Task 22 alpha.15) : title_lookup parallèles via tokio::task::JoinSet.
    //
    // Les N lookups sont spawné simultanément — seule la contention sur le mutex
    // SQLite interne à SqliteIndex les sérialise, sans délai inter-tasks.
    // Les `upsert_link` restent séquentiels : le write lock SQLite ne permet pas
    // de paralléliser les inserts sans contention notable (N ≤ 10 en pratique).
    // Pattern cohérent avec Task 20 (vault_trace JoinSet — commit f814fc6).
    let mut join_set = tokio::task::JoinSet::new();

    for target_title in &wikilinks {
        // Clone requis : JoinSet::spawn exige 'static — idx Arc<SqliteIndex> et
        // tenant_id String sont les deux seules allocations ajoutées (N ≤ 10).
        let idx_arc = Arc::clone(idx);
        let tenant = tenant_id.to_string();
        let title = target_title.clone();
        join_set.spawn(async move {
            let result = idx_arc.title_lookup(&tenant, &title).await;
            (title, result)
        });
    }

    // Collecte des résultats lookup dans l'ordre d'arrivée (ordre de complétion —
    // pas d'ordre garanti avec JoinSet). Les upsert_link restent séquentiels.
    let mut lookup_results = Vec::with_capacity(wikilinks.len());
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(pair) => lookup_results.push(pair),
            Err(e) => {
                // Panique dans une task lookup — non-fatal, log warn et skip.
                tracing::warn!(err = %e, "B5 title_lookup task panicked — wikilink ignoré");
            }
        }
    }

    for (target_title, lookup_result) in lookup_results {
        match lookup_result {
            Ok(Some(dst_id)) => {
                if let Err(e) = idx.upsert_link(tenant_id, src_note_id, &dst_id).await {
                    tracing::warn!(
                        err = %e,
                        src = %src_note_id,
                        dst = %dst_id,
                        "B5 upsert_link failed — non-fatal"
                    );
                } else {
                    tracing::debug!(
                        src = %src_note_id,
                        dst = %dst_id,
                        target = %target_title,
                        "B5 wikilink persisté"
                    );
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target = %target_title,
                    "B5 wikilink non résolu — note cible absente (caveat C3 Phase 3)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    target = %target_title,
                    "B5 title_lookup failed — wikilink ignoré (non-fatal)"
                );
            }
        }
    }
}
