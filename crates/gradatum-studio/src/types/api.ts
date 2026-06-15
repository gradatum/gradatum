/**
 * Types API gradatum Studio — contrats JSON exacts des endpoints S1
 * Source : s1-contrats-endpoints.md (NORMATIF — ne pas modifier sans mise à jour contrats)
 */

// --- NoteStatus (6 états réels + forgotten overlay) ---

export type NoteStatus =
  | 'live'
  | 'staging'
  | 'pending-review'
  | 'draft'
  | 'deprecated'
  | 'garbage'
  | 'downgraded'; // legacy bucket — affiché DEPRECATED

// --- Score breakdown (opt-in via include_scores: true) ---

export interface ScoreBreakdown {
  rrf_score: number;
  recency_factor: number;
  pagerank_factor: number;
  in_degree: number;
  trust_raw: number;
  composite: number;
  bm25_rank?: number;
  sem_rank?: number;
  // rerank_score intentionnellement absent : NoopReranker, ligne omise (A1)
}

// --- vault_search (shape réelle vérifiée LIVE 2026-06-11) ---
//
// Hit brut reçu du backend :
//   { path: "section/ULID26", score, title, snippet, trust (legacy), scores? }
//   path doit être splitté pour obtenir section et ulid.
//
// Champs hors contrat → 422 deny_unknown_fields :
//   INTERDITS : offset, agent
//   Status filter : `status?: string` — accepté depuis commit feat(search):status-filter
//
// FlatSearchHit : shape normalisée après mapping path→{section, ulid}

/** Hit brut tel que retourné par le backend */
export interface RawSearchHit {
  path: string;                // "section/ULID26" — à splitter
  score: number;
  title: string | null;
  snippet: string;
  // trust: number — legacy, ignorer
  status?: string;             // optionnel — disponible après commit feat(search):status-filter
  scores?: ScoreBreakdown;
}

/** Hit normalisé pour l'affichage — produit par flattenHit() */
export interface SearchHit {
  ulid: string;                // extrait de path.split('/')[1]
  section: string;             // extrait de path.split('/')[0]
  path: string;                // conservé pour navigation /notes/:ulid
  score: number;
  title: string | null;
  snippet: string;
  status: NoteStatus;          // depuis hit.status ou fallback 'live'
  forgotten: boolean;          // toujours false — champ non fourni par vault_search
  scores?: ScoreBreakdown;
}

/** Champs acceptés par POST /api/v1/vault_search — deny_unknown_fields */
export interface SearchRequest {
  query: string;
  tenant_id?: string;
  section?: string;
  limit?: number;
  locus?: string;
  vault_id?: string;
  include_downgraded?: boolean;
  include_scores?: boolean;
  status?: string;             // optionnel — depuis commit feat(search):status-filter
  // INTERDITS : offset, agent → 422
}

export interface SearchResponse {
  items: RawSearchHit[];
  // Pas de total, pas d'elapsed_ms, pas d'algorithm côté backend actuel
}

// --- GET /api/v1/review ---

export interface ReviewItem {
  ulid: string;
  title: string | null;
  section: string;
  locus: string | null;
  status: 'pending-review' | 'staging';
  provenance: string | null;
  created_ms: number;
}

export interface ReviewResponse {
  items: ReviewItem[];
  next_cursor?: string;
  total: number;
}

// --- GET /api/v1/dashboard ---

export interface LastJob {
  id: string;
  status: string;
  created_at: string;
}

export interface DashboardResponse {
  notes_by_status: Record<string, number>;
  forgotten_count: number;
  jobs_by_status: Record<string, number>; // PascalCase côté backend
  queue_depth: number;
  wal_size_bytes?: number; // absent si non mesurable → "n/a"
  last_job?: LastJob;
}

// --- POST /api/v1/notes/{id}/move ---

export interface MoveRequest {
  locus: string;
}

// 204 No Content sur succès

// --- PATCH /api/v1/notes/{id} (status update) ---

export interface PatchNoteRequest {
  status?: NoteStatus;
}

// --- GET /api/v1/jobs (shape réelle vérifiée LIVE 2026-06-11) ---
// Structure imbriquée : spec.kind.type → kind, lifecycle.* → status/dates/result

export interface JobLifecycle {
  status: 'Pending' | 'Running' | 'Done' | 'Failed' | 'DLQ';
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  result?: {
    success: boolean;
    duration_ms?: number;
    cost_usd?: number | null;
    result_note?: string | null;
  } | null;
}

export interface JobRetry {
  count: number;
  max: number;
  last_error?: string | null;
  errors?: Array<{ at: string; message: string; attempt: number }>;
}

export interface JobSpec {
  kind: { type: string; data?: Record<string, unknown> };
}

export interface Job {
  id: string;
  spec: JobSpec;
  lifecycle: JobLifecycle;
  retry?: JobRetry;
}

/** Shape normalisée pour l'affichage — produite par flattenJob() */
export interface FlatJob {
  id: string;
  kind: string;
  status: 'Pending' | 'Running' | 'Done' | 'Failed' | 'DLQ';
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  duration_ms: number | undefined;
  lastError: string | null;
}

export interface JobsResponse {
  items: Job[];
  next_cursor?: string | null;
}

// --- POST /api/v1/jobs (shape réelle F-16.1, contrat gelé) ---
//
// Header obligatoire : Idempotency-Key (sinon 400)
// Auth : bearer JWT + ACL Write requis (401/403 sinon — fix authz F-16)
// Body : { spec: { kind: { type, data } }, lineage?: {...} }
//   `spec.kind` suit la représentation serde de gradatum_core::Job
//   (#[serde(tag = "type", content = "data")]). Kind/données invalides → 400.
//   L'ancienne shape `kind: "Curate"` (string legacy E-13) est désormais REJETÉE (400).
// Réponse 202 : { id: ULID, idempotent: false } (200 si Idempotency-Key réutilisée)
//
// F-16.1 : le JobKind réel est honoré (le note_id Curate fourni est respecté).
// Kinds déclenchables : Curate, Distill, Purge, Embed, Forget. Les autres → 400.
//
// Retry/replay DLQ : PAS d'endpoint API — admin CLI only. (vérifié LIVE F-16.3)

// --- Payloads spécifiques par JobKind ---

/** Payload Curate — note_id obligatoire (400 si absent). */
export interface CurateData {
  note_id: string;
}

/**
 * Payload Purge — contrat réel PurgeSpec vérifié LIVE F-16.3.
 *
 * `mode` est OBLIGATOIRE (seul `Lifecycle` implémenté en v0.4.8).
 * `dry_run` : défaut `true` côté serveur si absent — mais toujours fournir explicitement.
 * `grace_days` : défaut `30` côté serveur. `null` = pas de délai (dangereux, CLI expert).
 */
export interface PurgeData {
  mode: 'Lifecycle';
  dry_run: boolean;
  grace_days?: number | null;
}

/** Spécification d'un kind de job — serde tag=type / content=data. */
export interface JobKindSpec {
  /** Discriminant : 'Curate' | 'Distill' | 'Purge' | 'Embed' | 'Forget'. */
  type: 'Curate' | 'Distill' | 'Purge' | 'Embed' | 'Forget';
  /** Payload du kind (ex. { note_id } pour Curate) — optionnel selon le kind. */
  data?: CurateData | PurgeData | Record<string, unknown>;
}

export interface CreateJobRequest {
  spec: {
    kind: JobKindSpec;
  };
  lineage?: Record<string, unknown>;
}

export interface CreateJobResponse {
  id: string;        // ULID du job créé
  idempotent: boolean; // true si la même Idempotency-Key a déjà été utilisée
}

// --- Auth ---

export interface AuthExchangeResponse {
  token: string;
}

// --- vault_read (POST /api/v1/vault_read, shape réelle vérifiée LIVE 2026-06-11) ---
//
// Contrat réel : POST body { path: "<ULID>" } — PAS GET /{id}
// Champs acceptés : tenant_id, path, section — deny_unknown_fields
// Réponse : path, content (markdown complet), metadata, size_bytes, sha256
// PAS de title/body/locus/kind/forgotten/wikilinks/backlinks séparés.

export interface VaultReadMetadata {
  author: string;
  created: number;       // ms timestamp
  section: string;
  status: string;        // "live" | "staging" | etc.
  tags: string[];
  updated: number;       // ms timestamp
  vault_id: string;
}

export interface VaultReadResponse {
  path: string;          // ULID nu
  content: string;       // markdown complet (frontmatter en tête du texte)
  metadata: VaultReadMetadata;
  size_bytes: number;
  sha256: string;
}

export interface VaultReadRequest {
  path: string;          // ULID nu
  tenant_id?: string;
  section?: string;
  // deny_unknown_fields : tout autre champ → 422
}

// NoteDetail : shape normalisée produite par flattenVaultRead()
// pour rester compatible avec NoteDetailPage (actions PATCH/POST non affectées)
export interface NoteDetail {
  ulid: string;
  title: string | null;   // première ligne "# ..." si présente, sinon null
  body: string;           // content brut complet (frontmatter inclus)
  frontmatter: string;    // bloc --- ... --- extrait si présent, sinon ''
  section: string;        // depuis metadata.section
  locus: string | null;   // non fourni — null
  kind: string;           // non fourni — 'note'
  status: NoteStatus;     // depuis metadata.status (coercé)
  forgotten: boolean;     // non fourni — false
  agent: string | null;   // depuis metadata.author
  created_at: string;     // metadata.created ms → ISO string
  updated_at: string;     // metadata.updated ms → ISO string
  wikilinks?: string[];
  backlinks?: Array<{ ulid: string; title: string | null }>;
  history?: Array<{ sha: string; created_at: string; author: string }>;
  agent_runs?: Array<{ job_id: string; agent: string; created_at: string }>;
}
