/**
 * jobs.ts — helpers pour POST /api/v1/jobs
 *
 * Contrat réel vérifié LIVE F-16.3 :
 * - Header Idempotency-Key obligatoire (sinon 400)
 * - Body { spec: { kind: { type, data } } }
 * - Curate : data.note_id obligatoire (sinon 400)
 * - Purge : data.mode obligatoire, seul "Lifecycle" implémenté, dry_run défaut true serveur
 * - Retry/replay : aucun endpoint API (admin CLI only)
 * - 202 { id, idempotent } sur succès
 * - 400 si spec invalide, 401 si pas de JWT, 403 si ACL insuffisant
 */

import { apiFetch } from '../hooks/useAuth';
import type { CreateJobRequest, CreateJobResponse, CurateData, PurgeData } from '../types/api';

/** Génère un Idempotency-Key unique basé sur crypto.randomUUID(). */
function newIdempotencyKey(): string {
  return crypto.randomUUID();
}

export interface JobCreationResult {
  ok: boolean;
  id?: string;
  idempotent?: boolean;
  status?: number;
  error?: string;
}

/**
 * POST /api/v1/jobs
 * Wrapper central — ajoute l'Idempotency-Key automatiquement.
 * Utilise apiFetch (intercepteur JWT centralisé D3.3).
 */
async function postJob(req: CreateJobRequest): Promise<JobCreationResult> {
  let res: Response;
  try {
    res = await apiFetch('/api/v1/jobs', {
      method: 'POST',
      headers: {
        'Idempotency-Key': newIdempotencyKey(),
      },
      body: JSON.stringify(req),
    });
  } catch (_err) {
    return { ok: false, error: 'Network error' };
  }

  if (res.ok) {
    const data = (await res.json()) as CreateJobResponse;
    return { ok: true, id: data.id, idempotent: data.idempotent };
  }

  let errorMsg = `HTTP ${res.status}`;
  try {
    const errBody = (await res.json()) as { error?: string };
    if (errBody.error) errorMsg = errBody.error;
  } catch { /* réponse non-JSON */ }

  return { ok: false, status: res.status, error: errorMsg };
}

/** Déclenche un job Curate pour la note `noteId`. */
export function triggerCurate(noteId: string): Promise<JobCreationResult> {
  const data: CurateData = { note_id: noteId };
  return postJob({ spec: { kind: { type: 'Curate', data } } });
}

/**
 * Déclenche un job Purge (Lifecycle).
 * @param dryRun - true = simulation seule (défaut prudent). false = suppression réelle.
 * @param graceDays - ancienneté minimale Garbage avant suppression (défaut 30).
 */
export function triggerPurge(dryRun: boolean, graceDays?: number): Promise<JobCreationResult> {
  const data: PurgeData = {
    mode: 'Lifecycle',
    dry_run: dryRun,
    ...(graceDays !== undefined ? { grace_days: graceDays } : {}),
  };
  return postJob({ spec: { kind: { type: 'Purge', data } } });
}
