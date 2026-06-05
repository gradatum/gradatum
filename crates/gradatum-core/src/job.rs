//! Types primitifs pour le système de jobs (v0.2.0+ — ARCH-D15).
//!
//! Ce module définit les types canoniques L0 utilisés par toute la couche
//! job infrastructure. Il appartient à `gradatum-core` pour permettre aux
//! crates de niveaux supérieurs (`gradatum-queue`, `gradatum-worker`,
//! `gradatum-db-sqlite`) d'en dépendre sans cycle.
//!
//! # Couches architecturales
//!
//! ```text
//! gradatum-core (L0) — Job, JobRecord, QueueStore, QueueEvent, DryRunAware
//!     ↑
//! gradatum-db-sqlite (L2) — SqliteQueueStore impl QueueStore
//!     ↑
//! gradatum-queue (L3)     — GradatumQueue facade (Apalis backend)
//!     ↑
//! gradatum-worker (L4)    — handlers Apalis + orchestration
//! ```
//!
//! # Ordre bincode — IMMUABLE
//!
//! Les variants de [`Job`] sont encodés par position par `bincode`.
//! **Ne jamais réordonner les variants existants.** Ajouter uniquement en fin
//! de liste. Violation = corruption silencieuse des jobs stockés en base.
//!
//! # Références
//!
//! - v81 architecture globale §6 L2159-2779
//! - Decision ARCH-D15 : `docs/decisions/ARCH-D15-apalis-embedded.md`

#![allow(dead_code)] // Phase 1.1+ — types consommés progressivement

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// VaultScope — alias canonique (v81 §6)
// ─────────────────────────────────────────────────────────────────────────────

/// Scope d'un job ou d'une requête vault.
///
/// `VaultScope` est l'alias de type pour [`JobScope`] — les deux noms coexistent
/// pour maintenir la cohérence avec le code vault existant (`VaultScope`) et le
/// nouveau système de jobs (`JobScope`).
pub type VaultScope = JobScope;

// ─────────────────────────────────────────────────────────────────────────────
// Job enum — ordre bincode figé (v55)
// ─────────────────────────────────────────────────────────────────────────────

/// Type de job soumis à la queue.
///
/// # Ordre bincode — IMMUABLE
///
/// Les variants sont encodés par position (0-20). Ne jamais réordonner.
/// Ajouter uniquement en fin de liste.
///
/// Les commentaires de position `(N)` sont informatifs — bincode encode par
/// ordre de déclaration Rust, pas par valeur numérique explicite.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Job {
    // System jobs (0-12) — automatiques
    /// ReAct loop F-04 — position bincode 0.
    Agent,
    /// Step `[[pipelines]]` F-52 — position bincode 1.
    Pipeline,
    /// Web crawler F-20 — position bincode 2.
    Collect,
    /// Semantic/Learn/Peer/Rationale F-22 — position bincode 3.
    Distill,
    /// Sauvegarde vault — position bincode 4.
    Backup,
    /// Lifecycle + semantic forget F-32/F-44 — position bincode 5.
    Purge,
    /// FtsOnly/VectorsOnly/Full/MissingOnly F-15 — position bincode 6.
    ReIndex(ReIndexMode),
    /// Zone B compression F-30 — position bincode 7.
    Summarize,
    /// Memory Validation + healing F-43 — position bincode 8.
    Validate,
    /// Vault score + dédup F-51 — position bincode 9.
    Audit,
    /// Modèles mentaux F-49 — position bincode 10.
    Consolidate,
    /// Inbox/ classification F-42 — position bincode 11.
    Curate(CurateSpec),
    /// Semantic forget F-44 — position bincode 12.
    Forget,

    // Human jobs (13-16) — JobClass::Human requis
    /// Valide/rejette un lot de notes `needs-review` — position bincode 13.
    Review,
    /// Classifie manuellement une note `inbox/` non résolue — position bincode 14.
    Classify,
    /// Fusionne deux notes dupliquées (post `Job::Audit`) — position bincode 15.
    Merge,
    /// Enrichit les métadonnées d'un lot de notes — position bincode 16.
    Annotate,

    // Nouveaux variants v59 — ajoutés EN FIN (ordre bincode figé)
    /// Import predecessor v1.6.2 → Gradatum · `JobClass::Human` — position bincode 17.
    Migrate(MigrateSource),
    /// CSV/PDF/JSON depuis notes · `JobClass::Agent|Human` — position bincode 18.
    Export(ExportSource),
    /// Notification externe cascade · `JobClass::System` — position bincode 19.
    Notify(NotifySource),
    /// Ingestion document F-06 via queue · `JobClass::Agent|Human` — position bincode 20.
    Ingest(IngestSource),

    // Embed : ajouté EN FIN pour préserver l'ordre bincode des variants 0-20
    /// Génération d'embedding vectoriel (Phase 1.1) — position bincode 21.
    Embed(EmbedSpec),
}

// ─────────────────────────────────────────────────────────────────────────────
// ReIndexMode
// ─────────────────────────────────────────────────────────────────────────────

/// Mode de réindexation pour [`Job::ReIndex`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReIndexMode {
    /// Rebuild FTS5 + PageRank (défaut).
    FtsOnly,
    /// Recalcule tous les embeddings (après migration modèle).
    VectorsOnly,
    /// FtsOnly + VectorsOnly.
    Full,
    /// Embed uniquement les notes sans vecteur (nouvelles notes).
    ///
    /// Plus rapide que `VectorsOnly` sur grand vault actif.
    MissingOnly,
}

// ─────────────────────────────────────────────────────────────────────────────
// Source structs — variants actifs Phase 1.x
// ─────────────────────────────────────────────────────────────────────────────

/// Spécification d'un job de curation (`Job::Curate`).
///
/// Classification `inbox/`, scoring, mise à jour des métadonnées.
///
/// # Phase 1.2 — champs write optionnels
///
/// Les champs `title`, `body`, `author`, `tags`, `section_hint` sont portés
/// directement dans `CurateSpec` pour le path `vault_write → job_store`.
/// Ils sont optionnels (`#[serde(default)]`) — rétrocompatibles avec les
/// `JobRecord` existants (champ absent en JSON → `None`/`[]`).
///
/// Pour les jobs `Job::Curate` déclenchés par `vault_write` :
/// - `title` + `body` sont `Some` — portent le contenu à créer dans le vault.
///
/// Pour les jobs `Job::Curate` déclenchés par reclassification :
/// - `title` + `body` sont `None` — la note existe déjà dans le vault.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurateSpec {
    /// Identifiant ULID de la note à curer.
    pub note_id: Ulid,
    /// Identifiant du tenant propriétaire (défaut : `"main"`).
    #[serde(default = "default_tenant_main")]
    pub tenant_id: String,
    /// Titre de la note (présent pour vault_write, absent pour reclassification).
    #[serde(default)]
    pub title: Option<String>,
    /// Corps Markdown de la note (présent pour vault_write).
    #[serde(default)]
    pub body: Option<String>,
    /// Auteur de la note (optionnel).
    #[serde(default)]
    pub author: Option<String>,
    /// Tags initiaux (optionnel — le curator peut en ajouter d'autres).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Section suggérée (optionnel — le curator peut surclasser).
    #[serde(default)]
    pub section_hint: Option<String>,
}

fn default_tenant_main() -> String {
    "main".to_string()
}

/// Spécification d'un job d'embedding (`Job::Embed`).
///
/// Génération ou régénération du vecteur via `gradatum-embed::FallbackEmbedder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSpec {
    /// Identifiant ULID de la note à embedder.
    pub note_id: Ulid,
    /// Identifiant du tenant propriétaire (défaut : `"main"`).
    pub tenant_id: String,
    /// Forcer la régénération même si un vecteur existe déjà.
    pub force_regenerate: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Source structs — nouveaux variants v59
// ─────────────────────────────────────────────────────────────────────────────

/// Source pour [`Job::Migrate`] — import predecessor vault → Gradatum.
///
/// `JobClass::Human` uniquement — action irréversible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrateSource {
    /// Chemin du vault source.
    pub from_path: String,
    /// Mode de migration.
    pub mode: MigrateMode,
    /// Stratégie de résolution des conflits.
    pub conflict: ConflictStrategy,
    /// Simuler sans écrire — obligatoire en premier passage.
    pub dry_run: bool,
    /// Vault de destination.
    pub target: VaultScope,
}

/// Mode de migration pour [`MigrateSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MigrateMode {
    /// Mapping 10 sections → `CognitiveCategory` + `ContentSection` §16.
    PredecessorV1,
    /// Import depuis un autre vault Gradatum.
    GradatumVault,
    /// Import Markdown brut sans mapping.
    RawMarkdown,
}

/// Stratégie de résolution des conflits pour [`MigrateSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictStrategy {
    /// Écraser les notes existantes.
    Overwrite,
    /// Garder les existantes, ignorer les nouvelles.
    Skip,
    /// Suffixe `-imported` sur les conflits.
    Rename,
}

/// Source pour [`Job::Export`] — générer un fichier depuis des notes vault.
///
/// `JobClass::Agent | Human`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSource {
    /// Scope des notes à exporter.
    pub scope: VaultScope,
    /// Filtre FTS optionnel (ex : `"sections:decisions"`).
    pub filter: Option<String>,
    /// Format d'export.
    pub format: ExportFormat,
    /// Chemin OpenDAL destination (ex : `"exports/decisions-2026-05.pdf"`).
    pub target: String,
    /// Template Markdown pour le rendu.
    pub template: Option<String>,
}

/// Format d'export pour [`ExportSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportFormat {
    /// Une ligne par note — titre, locus, trust, date, sections.
    Csv,
    /// Rendu Markdown → PDF (pandoc ou similaire).
    Pdf,
    /// Sérialisation complète des `Document` + frontmatter.
    Json,
    /// Concaténation des notes en un seul `.md`.
    Markdown,
    /// Archive des fichiers `.md` bruts.
    Zip,
}

/// Source pour [`Job::Notify`] — notification externe en cascade.
///
/// `JobClass::System` — déclenché via `await_jobs` `OnDone`/`OnFailed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySource {
    /// Canal de notification.
    pub channel: NotifyChannel,
    /// Template du message avec variables (ex : `"Job {{job_kind}} terminé : {{notes_created}} notes"`).
    pub template: String,
    /// Job dont on notifie la complétion.
    pub job_ref: Option<Ulid>,
}

/// Canal de notification pour [`NotifySource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotifyChannel {
    /// Notification Telegram.
    Telegram {
        /// Identifiant du chat Telegram.
        chat_id: String,
    },
    /// Notification Slack via webhook.
    Slack {
        /// URL du webhook Slack.
        webhook_url: String,
    },
    /// Notification HTTP webhook générique.
    Webhook {
        /// URL du webhook.
        url: String,
        /// Méthode HTTP (`POST` recommandé).
        method: String,
    },
    /// Publication NATS.
    Nats {
        /// Subject NATS cible.
        subject: String,
    },
    /// Notification email.
    Email {
        /// Adresse email destinataire.
        to: String,
    },
}

/// Source pour [`Job::Ingest`] — ingestion document F-06 via queue.
///
/// `JobClass::Agent | Human`. Remplace `gradatum-admin vault import --file`
/// pour les corpus larges (progress + retry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestSource {
    /// Source d'entrée à ingérer.
    pub source: IngestInputSource,
    /// Vault destination.
    pub vault: String,
    /// Locus destination (ex : `"rag/"`).
    pub locus: String,
    /// Stratégie d'ingestion.
    pub strategy: IngestStrategy,
    /// Simuler sans écrire — exceptions légitimes (opération potentiellement
    /// volumineuse, validation humaine recommandée).
    pub dry_run: bool,
}

/// Source d'entrée pour [`IngestSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestInputSource {
    /// Chemin OpenDAL.
    File {
        /// Chemin du fichier.
        path: String,
    },
    /// URL à fetcher.
    Url {
        /// URL à ingérer.
        url: String,
    },
    /// Batch d'URLs.
    Urls {
        /// Liste des URLs à ingérer.
        urls: Vec<String>,
    },
    /// Dossier de fichiers déjà sur disque.
    Locus {
        /// Chemin du dossier.
        path: String,
    },
}

/// Stratégie d'ingestion pour [`IngestSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestStrategy {
    /// Détecte automatiquement : `StructureGuided` ou `SlidingWindow`.
    Auto,
    /// Skeleton tree + structure-guided chunking.
    ForceStructured,
    /// Sliding window même si des headings sont présents.
    ForceSlidingWindow,
}

// ─────────────────────────────────────────────────────────────────────────────
// job_kind_str — helper de routing
// ─────────────────────────────────────────────────────────────────────────────

/// Retourne le nom du variant [`Job`] sous forme de chaîne statique.
///
/// Utilisé pour dénormaliser la colonne `kind` dans `gradatum_jobs` à l'enqueue,
/// et pour filtrer par `kind` dans [`QueueStore::dequeue_by_kind`].
///
/// # Exhaustivité
///
/// Ce match est exhaustif sans `_ =>` pour garantir qu'un nouveau variant de [`Job`]
/// provoque une erreur de compilation plutôt qu'un routage silencieusement incorrect.
///
/// # Correspondance JSON
///
/// La valeur retournée correspond à la clé `"type"` du payload sérialisé via
/// `#[serde(tag = "type", content = "data")]` (ex. `{"spec":{"kind":{"type":"Curate",...}}}`).
#[must_use]
pub fn job_kind_str(job: &Job) -> &'static str {
    match job {
        Job::Agent => "Agent",
        Job::Pipeline => "Pipeline",
        Job::Collect => "Collect",
        Job::Distill => "Distill",
        Job::Backup => "Backup",
        Job::Purge => "Purge",
        Job::ReIndex(_) => "ReIndex",
        Job::Summarize => "Summarize",
        Job::Validate => "Validate",
        Job::Audit => "Audit",
        Job::Consolidate => "Consolidate",
        Job::Curate(_) => "Curate",
        Job::Forget => "Forget",
        Job::Review => "Review",
        Job::Classify => "Classify",
        Job::Merge => "Merge",
        Job::Annotate => "Annotate",
        Job::Migrate(_) => "Migrate",
        Job::Export(_) => "Export",
        Job::Notify(_) => "Notify",
        Job::Ingest(_) => "Ingest",
        Job::Embed(_) => "Embed",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobSpec — ce que fait le job
// ─────────────────────────────────────────────────────────────────────────────

/// Spécification fonctionnelle d'un job.
///
/// Contient le type de travail, la classe de déclencheur, le mode d'exécution,
/// le scope et la priorité.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    /// Type fonctionnel + payload.
    pub kind: Job,
    /// Qui déclenche le job.
    pub class: JobClass,
    /// Comment s'exécute le job.
    pub mode: JobMode,
    /// Sur quoi s'applique le job.
    pub scope: JobScope,
    /// Priorité dans la queue.
    pub priority: JobPriority,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobClass
// ─────────────────────────────────────────────────────────────────────────────

/// Classe de déclencheur d'un job.
///
/// Détermine la priorité par défaut et le routage dans la queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobClass {
    /// Cron autonome — pas d'acteur humain.
    System,
    /// Déclenché/exécuté par un agent LLM.
    Agent,
    /// Action explicite CLI/studio.
    Human,
    /// Appel machine externe (MCP, tiers).
    Api,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobMode
// ─────────────────────────────────────────────────────────────────────────────

/// Mode d'exécution d'un job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JobMode {
    /// Traite N éléments, s'arrête (défaut).
    #[default]
    Batch,
    /// Traite en continu jusqu'à queue vide.
    Streaming,
    /// Requiert allers-retours avec un acteur.
    Interactive,
    /// Simule sans écrire.
    DryRun,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobScope
// ─────────────────────────────────────────────────────────────────────────────

/// Scope d'un job — sur quoi s'applique le travail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobScope {
    /// Tout le vault.
    VaultWide,
    /// Un locus spécifique (répertoire vault).
    Locus(String),
    /// Un ensemble de notes ciblées.
    Notes(Vec<Ulid>),
    /// Une session agent (contexte isolé).
    Session(Ulid),
}

// ─────────────────────────────────────────────────────────────────────────────
// JobPriority
// ─────────────────────────────────────────────────────────────────────────────

/// Priorité d'un job dans la queue (v65).
///
/// Mapping par défaut :
/// - `Agent`  → `High`    (agent actif en conversation → réponse attendue)
/// - `Human`  → `High`    (action humaine explicite → réponse attendue)
/// - `Api`    → `Normal`  (appel machine → latence acceptable)
/// - `System` → `Low`     (tâche de fond cron → ne bloque pas les agents)
///
/// Câblé dans `GradatumQueue.dequeue()` via `ORDER BY priority DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JobPriority {
    /// Agent actif + Human — réponse attendue.
    High,
    /// Api — latence acceptable (défaut).
    #[default]
    Normal,
    /// System cron — tâche de fond, ne bloque pas les agents.
    Low,
    /// Schedulé dans le futur (Consolidate trimestriel).
    Deferred,
}

impl JobPriority {
    /// Valeur SQL pour `ORDER BY priority DESC` (High=3 passe avant Low=0).
    #[must_use]
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::High => 3,
            Self::Normal => 2,
            Self::Low => 1,
            Self::Deferred => 0,
        }
    }

    /// Priorité par défaut selon la classe du job.
    #[must_use]
    pub fn default_for(class: &JobClass) -> Self {
        match class {
            JobClass::Agent => Self::High,
            JobClass::Human => Self::High,
            JobClass::Api => Self::Normal,
            JobClass::System => Self::Low,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobScheduling — quand s'exécute le job
// ─────────────────────────────────────────────────────────────────────────────

/// Contraintes de scheduling d'un job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobScheduling {
    /// Source de déclenchement.
    pub trigger: TriggerSource,
    /// Date/heure de scheduling (UTC).
    pub scheduled_at: DateTime<Utc>,
    /// Chaînage déclaratif — `[]` = immédiat · `[x]` = chaîne · `[x,y]` = DAG.
    ///
    /// Sémantique : "déclenche-moi quand ces jobs sont terminés".
    /// Plus robuste que `not_before: DateTime` (fragile, dépend des durées).
    pub await_jobs: Vec<JobTrigger>,
    /// Deadline pour les jobs `Interactive` — timeout.
    pub deadline: Option<DateTime<Utc>>,
    /// Expression cron (ex : `"0 2 * * *"`).
    pub cron_expr: Option<String>,
}

/// Condition de déclenchement en cascade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTrigger {
    /// Identifiant du job attendu.
    pub job_id: Ulid,
    /// Condition de déclenchement.
    pub condition: TriggerCondition,
}

/// Condition sur l'état terminal d'un job attendu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// Uniquement si `Done` (succès).
    OnDone,
    /// `Done | Failed | DLQ` — quoi qu'il arrive.
    OnAnyTerminal,
    /// Uniquement si `Failed` (alerting).
    OnFailed,
}

/// Source de déclenchement d'un job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerSource {
    /// `[[worker.schedules]]` — tokio-cron-scheduler.
    Cron,
    /// `[[pipelines]]` step — pipeline_executor.
    Pipeline,
    /// `await_jobs` → `on_job_complete()` → `set_pending()`.
    Cascade,
    /// `WriteHook` ou `QaEvent` interceptor.
    OnEvent,
    /// `POST /api/v1/jobs/trigger` · admin CLI · `invoke_agent()`.
    Demand,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobLifecycle — où en est le job
// ─────────────────────────────────────────────────────────────────────────────

/// État courant du cycle de vie d'un job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLifecycle {
    /// Statut courant.
    pub status: JobStatus,
    /// Timestamp de création (UTC).
    pub created_at: DateTime<Utc>,
    /// Timestamp de démarrage (UTC) — `None` si pas encore démarré.
    pub started_at: Option<DateTime<Utc>>,
    /// Timestamp de fin (UTC) — `None` si pas encore terminé.
    pub completed_at: Option<DateTime<Utc>>,
    /// Expiration du lease SQLite — anti-doublon.
    pub lease_until: Option<DateTime<Utc>>,
    /// Résultat du job — `None` si pas encore terminé.
    pub result: Option<JobResult>,
}

/// Statut d'un job dans son cycle de vie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    /// Dans la queue, prêt à démarrer.
    Pending,
    /// Lease active — en cours d'exécution.
    Running,
    /// `await_jobs` non satisfaits.
    Waiting,
    /// Succès.
    Done,
    /// Erreur, retry possible.
    Failed,
    /// Dead-letter — `max_retries` atteint.
    DLQ,
    /// Deadline dépassée ou orphelin.
    Cancelled,
}

/// Résultat d'un job terminé.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// `true` si le job s'est terminé avec succès.
    pub success: bool,
    /// Durée d'exécution en millisecondes.
    pub duration_ms: u32,
    /// Coût LLM en USD — `None` si pas de LLM impliqué.
    pub cost_usd: Option<f32>,
    /// Note Gradatum résultat — point d'entrée unique pour l'agent (v57).
    ///
    /// `vault_read(result_note)` → frontmatter + chemins + wikilinks vers notes produites.
    /// Présent si `success=true`. Note d'erreur si `DLQ`.
    pub result_note: Option<Ulid>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobWorkspace — workspace physique OpenDAL
// ─────────────────────────────────────────────────────────────────────────────

/// Workspace physique d'un job — structure OpenDAL (v57).
///
/// Tout passe par OpenDAL — même API quel que soit le backend (fs/s3/gcs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobWorkspace {
    /// Chemin d'entrée — ex : `"worker/2026-05-20/01J-XYZ/input/"`.
    pub input: String,
    /// Chemin de sortie — ex : `"worker/2026-05-20/01J-XYZ/output/"`.
    pub output: String,
    /// Chemin de métadonnées — ex : `"worker/2026-05-20/01J-XYZ/meta/"`.
    pub meta: String,
}

impl JobWorkspace {
    /// Construit le workspace depuis un [`JobRecord`].
    ///
    /// Format : `worker/{YYYY-MM-DD}/{job_id}/{input|output|meta}/`
    #[must_use]
    pub fn from_job(job: &JobRecord) -> Self {
        let date = job.lifecycle.created_at.format("%Y-%m-%d").to_string();
        let base = format!("worker/{}/{}", date, job.id);
        Self {
            input: format!("{}/input/", base),
            output: format!("{}/output/", base),
            meta: format!("{}/meta/", base),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobProgress — progress d'un job en cours
// ─────────────────────────────────────────────────────────────────────────────

/// Progress d'un job en cours (v58).
///
/// Stocké dans SQLite périodiquement.
/// `GET /api/v1/jobs/:id/status` → `{ status: "running", progress: { current: 47, total: 200 } }`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    /// Éléments traités.
    pub current: u32,
    /// Éléments total (si connu).
    pub total: u32,
    /// Description de l'étape courante.
    pub step: String,
    /// Estimation du temps restant en secondes.
    pub eta_secs: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobOutputFile + JobOutput
// ─────────────────────────────────────────────────────────────────────────────

/// Fichier produit par un job — stocké via OpenDAL dans `output/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutputFile {
    /// Nom du fichier (ex : `"export.csv"` | `"report.pdf"` | `"chart.png"`).
    pub name: String,
    /// MIME type (ex : `"text/csv"` | `"application/pdf"` | `"image/png"`).
    pub mime_type: String,
    /// Taille en bytes.
    pub size: u64,
    /// TTL en jours — `None` = défaut du locus (`worker/`=30j, `exports/`=90j).
    pub ttl_days: Option<u32>,
}

/// Sorties complètes produites par un job (v57).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutput {
    /// Notes Markdown créées dans le vault.
    pub notes_created: Vec<Ulid>,
    /// Notes modifiées (Validate/Heal).
    pub notes_modified: Vec<Ulid>,
    /// Binaires/CSV/images dans `output/`.
    pub files: Vec<JobOutputFile>,
    /// Contenu Markdown de la note résultat.
    ///
    /// Écrit dans `output/result.md`, copié dans `vault work/jobs/` pour `vault_read()`.
    pub result_note_md: String,
}

impl JobOutput {
    /// Retourner si `JobMode::DryRun` — aucune écriture effectuée.
    #[must_use]
    pub fn dry_run(would_affect: usize, description: &str) -> Self {
        Self {
            notes_created: vec![],
            notes_modified: vec![],
            files: vec![],
            result_note_md: format!(
                "## DRY-RUN — {description}\n\n\
                 **Simulation uniquement — aucune écriture effectuée.**\n\n\
                 Notes qui auraient été affectées : {would_affect}\n",
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobRetry — comment le job récupère
// ─────────────────────────────────────────────────────────────────────────────

/// Politique de retry d'un job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRetry {
    /// Tentatives effectuées.
    pub count: u32,
    /// Maximum de tentatives — `0` = pas de retry.
    pub max: u32,
    /// Stratégie de backoff.
    pub backoff: RetryBackoff,
    /// Dernière erreur enregistrée.
    pub last_error: Option<String>,
    /// Historique complet des erreurs.
    pub errors: Vec<JobError>,
}

impl Default for JobRetry {
    fn default() -> Self {
        Self {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        }
    }
}

/// Erreur individuelle enregistrée lors d'une tentative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    /// Timestamp de l'erreur (UTC).
    pub at: DateTime<Utc>,
    /// Message d'erreur.
    pub message: String,
    /// Numéro de la tentative.
    pub attempt: u32,
}

/// Stratégie de backoff entre les tentatives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RetryBackoff {
    /// N secondes fixes entre chaque tentative.
    Fixed(u64),
    /// Backoff exponentiel `base → 2×base → ... → max` secondes.
    Exponential {
        /// Délai de base en secondes.
        base: u64,
        /// Délai maximal en secondes.
        max: u64,
    },
}

impl RetryBackoff {
    /// Calcule la durée d'attente pour la tentative `attempt` (0-indexé).
    ///
    /// Pour `Fixed(n)` : toujours `n` secondes.
    /// Pour `Exponential { base, max }` : `min(base * 2^attempt, max)` secondes.
    #[must_use]
    pub fn duration_for(&self, attempt: u32) -> Duration {
        match self {
            Self::Fixed(secs) => Duration::from_secs(*secs),
            Self::Exponential { base, max } => {
                let secs = base.saturating_mul(1_u64 << attempt.min(62));
                Duration::from_secs(secs.min(*max))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobLineage — d'où vient le job
// ─────────────────────────────────────────────────────────────────────────────

/// Traçabilité de l'émetteur et du contexte déclencheur d'un job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLineage {
    /// `agent_id` | `user_id` | `cron_id` — `None` si non tracé.
    pub triggered_by: Option<String>,
    /// Job parent si job enfant (cascade, agent spawn).
    pub parent_job: Option<Ulid>,
    /// Pipeline si step d'un `[[pipelines]]`.
    pub pipeline_id: Option<Ulid>,
    /// Nom du step dans le pipeline.
    pub pipeline_step: Option<String>,
    /// Jobs créés par ce job (cascade sortante).
    pub children: Vec<Ulid>,
    /// Coût LLM cumulé en USD.
    pub cost_usd: Option<f32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobRecord — enveloppe complète 5 blocs
// ─────────────────────────────────────────────────────────────────────────────

/// Enveloppe complète d'un job structurée en 5 blocs orthogonaux.
///
/// `JobRecord` est le type canonique L0 circulant dans toute la couche job.
/// Sérialisé en JSON par le `QueueStore` pour persistance en SQLite.
///
/// # Les 5 blocs
///
/// 1. [`JobSpec`] — CE QUE fait le job
/// 2. [`JobScheduling`] — QUAND il s'exécute
/// 3. [`JobLifecycle`] — OÙ il en est
/// 4. [`JobRetry`] — COMMENT il récupère
/// 5. [`JobLineage`] — D'OÙ il vient / liens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Identifiant unique du job (ULID monotone, ordre FIFO implicite).
    pub id: Ulid,
    /// Bloc 1 — CE QUE fait le job.
    pub spec: JobSpec,
    /// Bloc 2 — QUAND il s'exécute.
    pub scheduling: JobScheduling,
    /// Bloc 3 — OÙ il en est.
    pub lifecycle: JobLifecycle,
    /// Bloc 4 — COMMENT il récupère.
    pub retry: JobRetry,
    /// Bloc 5 — D'OÙ il vient / liens.
    pub lineage: JobLineage,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobFilter — introspection F-16
// ─────────────────────────────────────────────────────────────────────────────

/// Filtre pour [`QueueStore::list`].
///
/// Phase 3 F-16 : ajout du champ `cursor` pour la pagination cursor-based.
/// `cursor` est le dernier `id` ULID retourné — la requête suivante retourne les jobs
/// avec `id > cursor` (ULID est monotone, donc équivaut à un ordre temporel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobFilter {
    /// Filtrer par classe de job.
    pub class: Option<JobClass>,
    /// Filtrer par statut.
    pub status: Option<JobStatus>,
    /// Filtrer par kind (nom du variant `Job`).
    pub kind: Option<String>,
    /// Filtrer les jobs créés après cette date.
    pub created_after: Option<DateTime<Utc>>,
    /// Nombre maximum de résultats (défaut : 50, max : 500).
    pub limit: usize,
    /// Cursor de pagination — dernier `id` ULID retourné (exclusif).
    ///
    /// `None` = début de la liste. `Some(ulid)` = jobs après ce ULID.
    /// Utiliser `next_cursor` de la réponse API précédente.
    pub cursor: Option<Ulid>,
}

impl Default for JobFilter {
    fn default() -> Self {
        Self {
            class: None,
            status: None,
            kind: None,
            created_after: None,
            limit: 50,
            cursor: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueEvent — événements publiés par le backend
// ─────────────────────────────────────────────────────────────────────────────

/// Événements publiés par le `QueueStore` via broadcast.
///
/// Consommés par :
/// - SSE endpoint `GET /api/v1/jobs/events`
/// - Cascade engine (`find_awaiting` + `set_pending`)
/// - Dashboard monitoring temps réel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueueEvent {
    /// Nouveau job inséré — `Pending` ou `Waiting`.
    JobInserted(Ulid),
    /// Job terminé — `Done` ou `DLQ`.
    JobCompleted(Ulid, JobStatus, JobResult),
    /// Job échoué — `Failed` + numéro de tentative.
    JobFailed(Ulid, u32),
    /// Job passé `Waiting → Pending` (cascade satisfaite).
    JobReady(Ulid),
    /// Job annulé — deadline ou orphelin.
    JobCancelled(Ulid),
}

// ─────────────────────────────────────────────────────────────────────────────
// GradatumJob — payload Apalis
// ─────────────────────────────────────────────────────────────────────────────

/// Payload Apalis wrappant un [`JobRecord`].
///
/// Sérialisé en JSON dans la colonne `job` de la table Apalis.
/// Le champ `priority` duplique `spec.priority.as_u8()` pour permettre
/// un `ORDER BY priority DESC` sans désérialisation du payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradatumJob {
    /// Enveloppe complète du job.
    pub record: JobRecord,
    /// Valeur de priorité dénormalisée pour le tri SQL (0-3).
    pub priority: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// DryRunAware trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait pour les jobs et handlers supportant le mode `DryRun` (v58).
///
/// Règle v62 : UN SEUL mécanisme, [`JobMode::DryRun`] dans [`JobSpec`].
/// Les `Source` structs ne portent PAS de champ `dry_run` sauf exceptions
/// légitimes (`MigrateSource.dry_run`, `IngestSource.dry_run`) pour les
/// opérations irréversibles nécessitant une validation humaine en premier.
///
/// Dans TOUS les handlers, la vérification est la première instruction :
///
/// ```rust,ignore
/// if job.spec.mode == JobMode::DryRun {
///     let count = ctx.vault.count(&src.scopes).await?;
///     return Ok(JobOutput::dry_run(count, "description"));
/// }
/// ```
pub trait DryRunAware {
    /// Retourne `true` si ce job est en mode DryRun.
    fn is_dry_run(&self) -> bool;

    /// Nombre de notes qui seraient affectées (estimation).
    ///
    /// Retourne `0` par défaut — surchargé par les implémentations qui peuvent
    /// calculer cette valeur sans effets de bord.
    fn notes_would_affect(&self) -> usize {
        0
    }
}

impl DryRunAware for JobRecord {
    fn is_dry_run(&self) -> bool {
        self.spec.mode == JobMode::DryRun
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobSource trait — factorisation v63
// ─────────────────────────────────────────────────────────────────────────────

/// Trait commun aux Source structs des variants `Job`.
///
/// Factorisation v63 — champs communs à 11 Source structs.
/// Rust ne supporte pas l'héritage de struct — trait préféré à embed.
pub trait JobSource {
    /// Scopes vault sur lesquels s'applique le job.
    fn scopes(&self) -> &[VaultScope];

    /// `true` si le job est en mode simulation (sans écriture).
    fn dry_run(&self) -> bool;

    /// Fenêtre temporelle optionnelle (notes créées/modifiées dans cette durée).
    fn window(&self) -> Option<Duration> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueStore trait — L0
// ─────────────────────────────────────────────────────────────────────────────

/// Trait de stockage de la queue de jobs — L0 `gradatum-core`.
///
/// Implémentations :
/// - `SqliteQueueStore` dans `gradatum-db-sqlite` (défaut embedded)
/// - `LibsqlQueueStore` dans `gradatum-db-sqlite` (remote opt-in F-25)
///
/// # Erreurs
///
/// Toutes les méthodes retournent `Result<_, QueueError>`.
/// Les implémentations ne doivent pas paniquer — propager les erreurs via `?`.
#[async_trait::async_trait]
pub trait QueueStore: Send + Sync {
    // ── Opérations de base ────────────────────────────────────────────────

    /// Insère un nouveau job dans la queue — retourne son `Ulid`.
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError>;

    /// Extrait le prochain job prêt à exécuter (lease atomique).
    ///
    /// Retourne `None` si la queue est vide ou si aucun job n'est prêt.
    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError>;

    /// Extrait le prochain job prêt à exécuter, filtré par `kind` (lease atomique).
    ///
    /// Garantit qu'un worker `curate` ne reçoit jamais un job `Embed` ou `ReIndex`,
    /// éliminant la race condition de routing (bug DLQ `UnexpectedVariant`).
    ///
    /// # Implémentation par défaut
    ///
    /// Fallback non filtré — les implémentations qui supportent le filtrage natif SQL
    /// (ex. [`SqliteQueueStore`]) doivent surcharger cette méthode avec `WHERE kind = ?`
    /// pour exploiter l'index `idx_jobs_status_kind`.
    ///
    /// # Paramètre
    ///
    /// `kind` : nom du variant `Job` tel que retourné par [`job_kind_str`] —
    /// ex. `"Curate"`, `"Embed"`, `"ReIndex"`.
    async fn dequeue_by_kind(&self, _kind: &str) -> Result<Option<JobRecord>, QueueError> {
        self.dequeue().await
    }

    /// Récupère un job par identifiant — `None` si inexistant.
    async fn get(&self, id: Ulid) -> Result<Option<JobRecord>, QueueError>;

    /// Marque un job comme `Done` avec son résultat.
    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError>;

    /// Marque un job comme `Failed` (retry possible selon policy).
    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError>;

    /// Annule un job (`Cancelled`).
    async fn cancel(&self, id: Ulid) -> Result<(), QueueError>;

    /// Envoie un job en dead-letter (`DLQ`) — max retries atteint.
    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError>;

    // ── Cascade — chaînage await_jobs ────────────────────────────────────

    /// Trouve les jobs en `Waiting` dont `await_jobs` contient `job_id`.
    async fn find_awaiting(&self, job_id: Ulid) -> Result<Vec<JobRecord>, QueueError>;

    /// Passe un job de `Waiting` à `Pending`.
    async fn set_pending(&self, id: Ulid) -> Result<(), QueueError>;

    // ── Sweep périodique (30s) ────────────────────────────────────────────

    /// Récupère les jobs dont le lease a expiré → remet en `Pending`.
    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError>;

    /// Annule les jobs dont la deadline est dépassée.
    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError>;

    /// Promeut les jobs schedulés en retry dont `scheduled_at <= now` → `Pending`.
    ///
    /// Garde v67 : si `retry.count >= retry.max` → `fail_dlq` au lieu de re-Pending.
    /// Évite la boucle infinie (`Failed → schedule → Failed → ...`).
    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError>;

    /// Schedule un job en retry à `at` (transition `Failed → Waiting`).
    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError>;

    // ── Introspection F-16 ────────────────────────────────────────────────

    /// Liste les jobs selon un filtre.
    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError>;

    // ── Événements ────────────────────────────────────────────────────────

    /// Souscrit au broadcast des [`QueueEvent`].
    ///
    /// Chaque appel retourne un nouveau `Receiver` indépendant.
    /// Les événements sont émis sans garantie de livraison si le consommateur
    /// est trop lent (channel broadcast avec capacité fixe).
    fn subscribe(&self) -> Receiver<QueueEvent>;
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueError — erreurs L0 (sans dépendances externes)
// ─────────────────────────────────────────────────────────────────────────────

/// Erreurs du `QueueStore` — sans dépendance vers `sqlx` ou autre driver.
///
/// Les implémentations (`SqliteQueueStore`, etc.) mappent leurs erreurs internes
/// vers ces variants via `map_err()`.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Erreur de stockage (driver SQLite, libsql, etc.).
    #[error("erreur de stockage : {0}")]
    Storage(String),

    /// Job introuvable par identifiant.
    #[error("job introuvable : {0}")]
    NotFound(Ulid),

    /// Erreur de sérialisation/désérialisation du payload JSON.
    #[error("erreur de sérialisation : {0}")]
    Serialization(String),

    /// Transition d'état invalide (ex : `Done → Running`).
    #[error("transition d'état invalide : {0}")]
    InvalidTransition(String),

    /// Opération annulée (timeout, shutdown).
    #[error("opération annulée : {0}")]
    Cancelled(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job_record(job: Job, class: JobClass) -> JobRecord {
        let now = Utc::now();
        JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: job,
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::default_for(&class),
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Pending,
                created_at: now,
                started_at: None,
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        }
    }

    #[test]
    fn job_priority_as_u8_ordering() {
        assert!(JobPriority::High.as_u8() > JobPriority::Normal.as_u8());
        assert!(JobPriority::Normal.as_u8() > JobPriority::Low.as_u8());
        assert!(JobPriority::Low.as_u8() > JobPriority::Deferred.as_u8());
    }

    #[test]
    fn job_priority_default_for_class() {
        assert_eq!(
            JobPriority::default_for(&JobClass::Agent),
            JobPriority::High
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::Human),
            JobPriority::High
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::Api),
            JobPriority::Normal
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::System),
            JobPriority::Low
        );
    }

    #[test]
    fn job_mode_default_is_batch() {
        assert_eq!(JobMode::default(), JobMode::Batch);
    }

    #[test]
    fn job_retry_default_values() {
        let r = JobRetry::default();
        assert_eq!(r.count, 0);
        assert_eq!(r.max, 3);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn retry_backoff_fixed_is_constant() {
        let b = RetryBackoff::Fixed(10);
        assert_eq!(b.duration_for(0), Duration::from_secs(10));
        assert_eq!(b.duration_for(5), Duration::from_secs(10));
    }

    #[test]
    fn retry_backoff_exponential_caps_at_max() {
        let b = RetryBackoff::Exponential { base: 5, max: 120 };
        assert_eq!(b.duration_for(0), Duration::from_secs(5));
        assert_eq!(b.duration_for(1), Duration::from_secs(10));
        assert_eq!(b.duration_for(10), Duration::from_secs(120)); // plafonné
    }

    #[test]
    fn job_record_serialize_roundtrip() {
        let record = make_job_record(
            Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobClass::Agent,
        );

        let json =
            serde_json::to_string(&record).expect("JobRecord doit être sérialisable en JSON");
        let back: JobRecord =
            serde_json::from_str(&json).expect("JobRecord doit être désérialisable depuis JSON");
        assert_eq!(record.id, back.id);
        assert_eq!(record.spec.priority.as_u8(), back.spec.priority.as_u8());
    }

    #[test]
    fn job_workspace_paths_format() {
        let record = make_job_record(Job::Consolidate, JobClass::System);
        let ws = JobWorkspace::from_job(&record);
        assert!(ws.input.ends_with("/input/"));
        assert!(ws.output.ends_with("/output/"));
        assert!(ws.meta.ends_with("/meta/"));
    }

    #[test]
    fn dry_run_job_record() {
        let record = {
            let now = Utc::now();
            JobRecord {
                id: Ulid::new(),
                spec: JobSpec {
                    kind: Job::Curate(CurateSpec {
                        note_id: Ulid::new(),
                        tenant_id: "main".to_string(),
                        ..Default::default()
                    }),
                    class: JobClass::Agent,
                    mode: JobMode::DryRun,
                    scope: JobScope::VaultWide,
                    priority: JobPriority::High,
                },
                scheduling: JobScheduling {
                    trigger: TriggerSource::Demand,
                    scheduled_at: now,
                    await_jobs: vec![],
                    deadline: None,
                    cron_expr: None,
                },
                lifecycle: JobLifecycle {
                    status: JobStatus::Pending,
                    created_at: now,
                    started_at: None,
                    completed_at: None,
                    lease_until: None,
                    result: None,
                },
                retry: JobRetry::default(),
                lineage: JobLineage {
                    triggered_by: None,
                    parent_job: None,
                    pipeline_id: None,
                    pipeline_step: None,
                    children: vec![],
                    cost_usd: None,
                },
            }
        };
        assert!(record.is_dry_run());
    }

    #[test]
    fn job_output_dry_run_format() {
        let out = JobOutput::dry_run(42, "test curate");
        assert!(out.notes_created.is_empty());
        assert!(out.result_note_md.contains("DRY-RUN"));
        assert!(out.result_note_md.contains("42"));
    }

    #[test]
    fn job_filter_default_limit() {
        let f = JobFilter::default();
        assert_eq!(f.limit, 50);
        assert!(f.class.is_none());
        assert!(f.status.is_none());
    }

    #[test]
    fn gradatum_job_priority_matches_spec() {
        let record = make_job_record(Job::Agent, JobClass::Human);
        let expected_priority = record.spec.priority.as_u8();
        let job = GradatumJob {
            priority: expected_priority,
            record,
        };
        assert_eq!(job.priority, 3); // Human → High → 3
    }

    #[test]
    fn vault_scope_is_alias_of_job_scope() {
        // VaultScope = JobScope — vérification que le type alias compile
        let vs: VaultScope = JobScope::VaultWide;
        let js: JobScope = vs;
        assert!(matches!(js, JobScope::VaultWide));
    }

    #[test]
    fn queue_event_variants_serialize() {
        let id = Ulid::new();
        let ev = QueueEvent::JobInserted(id);
        let json = serde_json::to_string(&ev).expect("QueueEvent doit être sérialisable");
        assert!(json.contains("JobInserted"));
    }
}
