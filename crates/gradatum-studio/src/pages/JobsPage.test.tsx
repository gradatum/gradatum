/**
 * Tests JobsPage — shape réelle LIVE vérifiée 2026-06-11
 * Teste flattenJob + défensif sur champs absents + pas de crash sur items vide
 *
 * Note : tri newest-first est désormais géré par le serveur (order=desc).
 * Le tri client-side (ULID lexicographique inversé) a été supprimé au commit
 * feat(studio): jobs page — server desc sort + day filter + cursor pagination.
 */

import { describe, it, expect } from 'vitest';
import type { Job } from '../types/api';

// Import de la fonction de mapping (extraite pour testabilité)
// On la reteste via une copie inline pour ne pas coupler au composant React
function flattenJob(j: Job) {
  const kind = j.spec?.kind?.type ?? 'Unknown';
  const lc = j.lifecycle ?? {} as Job['lifecycle'];
  const status = (lc.status ?? 'Pending') as 'Pending' | 'Running' | 'Done' | 'Failed' | 'DLQ';
  const duration_ms = lc.result?.duration_ms ?? undefined;
  const lastError = j.retry?.last_error ?? null;
  return {
    id: j.id,
    kind,
    status,
    created_at: lc.created_at ?? '',
    started_at: lc.started_at ?? null,
    completed_at: lc.completed_at ?? null,
    duration_ms,
    lastError,
  };
}

// Shape réelle LIVE
const JOB_DLQ: Job = {
  id: '01KST5293VT9RBSJZBP3CZV262',
  spec: { kind: { type: 'Curate', data: { note_id: 'abc', tenant_id: 'main' } } },
  lifecycle: {
    status: 'DLQ',
    created_at: '2026-05-29T15:18:55.355147880Z',
    started_at: null,
    completed_at: '2026-06-02T19:23:03.093296647Z',
    result: null,
  },
  retry: {
    count: 4,
    max: 3,
    last_error: 'max_retries atteint (4 / 3)',
    errors: [{ at: '2026-05-29T23:44:25.296387106Z', message: 'erreur métier', attempt: 4 }],
  },
};

const JOB_DONE: Job = {
  id: '01KST9FXJ8BJQPDT3X5R98YY2S',
  spec: { kind: { type: 'Curate' } },
  lifecycle: {
    status: 'Done',
    created_at: '2026-05-29T16:36:16.584091604Z',
    started_at: null,
    completed_at: '2026-06-02T13:15:45.335672069Z',
    result: { success: true, duration_ms: 1466, cost_usd: null, result_note: null },
  },
};

describe('flattenJob — shape réelle LIVE', () => {
  it('extrait kind depuis spec.kind.type', () => {
    const flat = flattenJob(JOB_DLQ);
    expect(flat.kind).toBe('Curate');
  });

  it('extrait status depuis lifecycle.status', () => {
    expect(flattenJob(JOB_DLQ).status).toBe('DLQ');
    expect(flattenJob(JOB_DONE).status).toBe('Done');
  });

  it('extrait duration_ms depuis lifecycle.result.duration_ms', () => {
    expect(flattenJob(JOB_DONE).duration_ms).toBe(1466);
    expect(flattenJob(JOB_DLQ).duration_ms).toBeUndefined();
  });

  it('extrait lastError depuis retry.last_error', () => {
    expect(flattenJob(JOB_DLQ).lastError).toBe('max_retries atteint (4 / 3)');
    expect(flattenJob(JOB_DONE).lastError).toBeNull();
  });

  it('défensif : kind Unknown si spec absent', () => {
    const bad = { id: 'x', spec: { kind: { type: undefined as unknown as string } }, lifecycle: JOB_DONE.lifecycle } as unknown as Job;
    expect(flattenJob(bad).kind).toBe('Unknown');
  });

  it('défensif : status Pending si lifecycle.status absent', () => {
    const bad = { id: 'x', spec: JOB_DONE.spec, lifecycle: { ...JOB_DONE.lifecycle, status: undefined as unknown as 'Done' } } as unknown as Job;
    expect(flattenJob(bad).status).toBe('Pending');
  });

  it('ne crash pas sur job minimal (champs optionnels absents)', () => {
    const minimal = { id: 'min', spec: { kind: { type: 'Purge' } }, lifecycle: { status: 'Pending', created_at: '', started_at: null, completed_at: null } } as unknown as Job;
    expect(() => flattenJob(minimal)).not.toThrow();
    expect(flattenJob(minimal).lastError).toBeNull();
    expect(flattenJob(minimal).duration_ms).toBeUndefined();
  });
});

describe('JobsResponse — parsing défensif', () => {
  it('items: [] si clé jobs (ancienne shape) au lieu de items', () => {
    // Simule l'ancienne shape incorrecte : { jobs: [...] }
    const wrongShape = { jobs: [JOB_DLQ] } as unknown as { items: Job[] };
    const items = Array.isArray(wrongShape.items) ? wrongShape.items : [];
    expect(items).toHaveLength(0); // fallback vide, pas de crash
  });

  it('items normalement parsés avec vraie shape', () => {
    const correct = { items: [JOB_DLQ, JOB_DONE] };
    const items = Array.isArray(correct.items) ? correct.items : [];
    expect(items).toHaveLength(2);
    expect(items.map(flattenJob).map(j => j.status)).toEqual(['DLQ', 'Done']);
  });
});

// ── buildJobsUrl (logique utilitaire) ─────────────────────────────────────────

/**
 * Copie inline de buildJobsUrl pour tests unitaires purs.
 * À maintenir synchrone avec JobsPage.tsx si les paramètres changent.
 */
function buildJobsUrl(params: {
  statusFilter: string;
  dayFilter: string | null;
  cursor: string | null;
  limit?: number;
}): string {
  const { statusFilter, dayFilter, cursor, limit = 50 } = params;
  const qp = new URLSearchParams();
  qp.set('order', 'desc');
  qp.set('limit', String(limit));
  if (statusFilter !== 'all') qp.set('status', statusFilter);
  if (dayFilter) {
    const [y, m, d] = dayFilter.split('-').map(Number);
    const after  = new Date(y, m - 1, d, 0, 0, 0, 0);
    const before = new Date(y, m - 1, d + 1, 0, 0, 0, 0);
    qp.set('created_after', after.toISOString());
    qp.set('created_before', before.toISOString());
  }
  if (cursor) qp.set('cursor', cursor);
  return `/api/v1/jobs?${qp.toString()}`;
}

describe('buildJobsUrl — construction URL', () => {
  it('contient toujours order=desc', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: null, cursor: null });
    expect(url).toContain('order=desc');
  });

  it('contient limit=50 par défaut', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: null, cursor: null });
    expect(url).toContain('limit=50');
  });

  it('n\'ajoute pas status= si all', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: null, cursor: null });
    expect(url).not.toContain('status=');
  });

  it('ajoute status=Pending si filtre Pending', () => {
    const url = buildJobsUrl({ statusFilter: 'Pending', dayFilter: null, cursor: null });
    expect(url).toContain('status=Pending');
  });

  it('ajoute created_after et created_before si dayFilter présent', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: '2026-06-10', cursor: null });
    expect(url).toContain('created_after=');
    expect(url).toContain('created_before=');
  });

  it('created_before = created_after + 24h', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: '2026-06-10', cursor: null });
    const decoded = decodeURIComponent(url);
    const afterMatch = decoded.match(/created_after=([^&]+)/);
    const beforeMatch = decoded.match(/created_before=([^&]+)/);
    expect(afterMatch).not.toBeNull();
    expect(beforeMatch).not.toBeNull();
    const after  = new Date(afterMatch![1]).getTime();
    const before = new Date(beforeMatch![1]).getTime();
    expect(before - after).toBe(86_400_000);
  });

  it('ajoute cursor= si fourni', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: null, cursor: 'CURSOR123' });
    expect(url).toContain('cursor=CURSOR123');
  });

  it('combine status + day + cursor dans la même URL', () => {
    const url = buildJobsUrl({ statusFilter: 'Failed', dayFilter: '2026-06-11', cursor: 'CUR' });
    expect(url).toContain('status=Failed');
    expect(url).toContain('created_after=');
    expect(url).toContain('cursor=CUR');
    expect(url).toContain('order=desc');
  });

  it('n\'ajoute pas cursor= si null', () => {
    const url = buildJobsUrl({ statusFilter: 'all', dayFilter: null, cursor: null });
    expect(url).not.toContain('cursor=');
  });
});
