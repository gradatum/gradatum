/**
 * JobsPage.trigger.test.tsx — triggers F-16.3 LIVE (contrats réels)
 *
 * F-16.3 : les triggers sont câblés sur les contrats réels vérifiés LIVE.
 * - Purge dry-run → POST /api/v1/jobs { kind: Purge, dry_run: true, mode: Lifecycle } → 202
 * - Purge réel → confirm dialog obligatoire → confirm → POST 202
 * - Purge réel → confirm dialog → cancel → aucun POST
 * - Re-curate → désactivé sur JobsPage (contextuel NoteDetailPage uniquement)
 * - Retry → désactivé (aucun endpoint replay API — admin CLI only)
 * - Bandeau E-13 → RETIRÉ (triggers câblés)
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { JobsPage } from './JobsPage';
import type { Job } from '../types/api';

// --- Fixtures (shape réelle LIVE) ---

const JOB_DLQ: Job = {
  id: '01KST5293VT9RBSJZBP3CZV262',
  spec: { kind: { type: 'Curate', data: { note_id: 'abc' } } },
  lifecycle: {
    status: 'DLQ',
    created_at: '2026-05-29T15:18:55.355Z',
    started_at: null,
    completed_at: '2026-06-02T19:23:03.093Z',
    result: null,
  },
  retry: { count: 4, max: 3, last_error: 'max_retries exceeded' },
};

const JOB_DONE: Job = {
  id: '01KST9FXJ8BJQPDT3X5R98YY2S',
  spec: { kind: { type: 'Purge', data: { mode: 'Lifecycle', dry_run: true } } },
  lifecycle: {
    status: 'Done',
    created_at: '2026-06-11T10:00:00Z',
    started_at: null,
    completed_at: '2026-06-11T10:01:00Z',
    result: { success: true, duration_ms: 1200, cost_usd: null, result_note: null },
  },
};

const JOBS_RESPONSE = { items: [JOB_DLQ, JOB_DONE], next_cursor: null };

// Réponse 202 conforme contrat réel
const JOB_CREATED_RESPONSE = { id: '01KTWBBCMPHN1X55QZ0HFSMEQX', idempotent: false };

// --- Helpers ---

function makeFetchResponse(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

const mockFetch = vi.fn();

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  localStorage.setItem('gradatum_studio_jwt_persist', 'test-jwt');

  // crypto.randomUUID requis par jobs.ts
  if (!globalThis.crypto?.randomUUID) {
    Object.defineProperty(globalThis, 'crypto', {
      value: { randomUUID: () => 'test-idempotency-key' },
      configurable: true,
    });
  }

  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.4.8-test' }));
    }
    if (typeof url === 'string' && url.includes('/dashboard')) {
      return Promise.resolve(makeFetchResponse({
        notes_by_status: {},
        forgotten_count: 0,
        jobs_by_status: { Pending: 0, Done: 2, Failed: 1, DLQ: 1 },
        queue_depth: 0,
      }));
    }
    if (typeof url === 'string' && url.includes('/jobs') && !url.includes('/jobs/')) {
      // POST /api/v1/jobs → 202
      const method = (mockFetch.mock.calls.at(-1) as [string, RequestInit | undefined])?.[1]?.method;
      if (method === 'POST') {
        return Promise.resolve(makeFetchResponse(JOB_CREATED_RESPONSE, 202));
      }
      return Promise.resolve(makeFetchResponse(JOBS_RESPONSE));
    }
    return Promise.resolve(makeFetchResponse(JOBS_RESPONSE));
  });
});

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

function renderJobsPage() {
  return render(
    <MemoryRouter>
      <JobsPage />
    </MemoryRouter>,
  );
}

// ── Bandeau E-13 RETIRÉ ───────────────────────────────────────────────────────

describe('JobsPage — bandeau E-13 retiré (F-16.3)', () => {
  it('bandeau e13-banner absent', async () => {
    renderJobsPage();
    // Attendre le rendu
    await screen.findByTestId('jobs-page');
    expect(screen.queryByTestId('e13-banner')).toBeNull();
  });
});

// ── Re-curate désactivé sur JobsPage (contextuel NoteDetailPage) ─────────────

describe('JobsPage — Re-curate désactivé (contextuel NoteDetailPage)', () => {
  it('trigger-curate est disabled (pas de note_id context-less)', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId('trigger-curate');
    expect(btn.hasAttribute('disabled') || btn.getAttribute('aria-disabled') === 'true').toBe(true);
  });

  it('trigger-curate tooltip mentionne NoteDetailPage', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId('trigger-curate');
    const title = btn.getAttribute('title') ?? '';
    expect(title.toLowerCase()).toContain('note');
  });

  it('clic sur trigger-curate ne déclenche aucun POST', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId('trigger-curate');
    const callsBefore = mockFetch.mock.calls.length;
    fireEvent.click(btn);
    await new Promise(r => setTimeout(r, 50));
    const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    expect(postCalls).toHaveLength(0);
  });
});

// ── Purge dry-run — déclenche directement sans confirm dialog ─────────────────

describe('JobsPage — Purge dry-run (F-16.3)', () => {
  it('bouton trigger-purge-dry est actif', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId('trigger-purge-dry');
    expect(btn.hasAttribute('disabled')).toBe(false);
    expect(btn.getAttribute('aria-disabled')).not.toBe('true');
  });

  it('clic Purge dry-run envoie POST avec mode:Lifecycle dry_run:true', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');

    const callsBefore = mockFetch.mock.calls.length;
    const btn = await screen.findByTestId('trigger-purge-dry');
    fireEvent.click(btn);

    await waitFor(() => {
      const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
      expect(postCalls).toHaveLength(1);
    });

    const postCalls = mockFetch.mock.calls.filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    const lastPost = postCalls.at(-1) as [string, RequestInit];
    expect(lastPost[0]).toBe('/api/v1/jobs');
    const body = JSON.parse(lastPost[1].body as string) as { spec: { kind: { type: string; data: { dry_run: boolean; mode: string } } } };
    expect(body.spec.kind.type).toBe('Purge');
    expect(body.spec.kind.data.dry_run).toBe(true);
    expect(body.spec.kind.data.mode).toBe('Lifecycle');
  });

  it('clic Purge dry-run NE montre PAS de confirm dialog', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge-dry');
    fireEvent.click(btn);
    // confirm-modal ne doit pas apparaître
    await new Promise(r => setTimeout(r, 50));
    expect(screen.queryByTestId('confirm-modal')).toBeNull();
  });

  it('toast de succès affiché après Purge dry-run', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge-dry');
    fireEvent.click(btn);
    await waitFor(() => {
      expect(screen.queryByText(/Purge.*dry-run.*queued/i) || screen.queryByText(/queued/i)).toBeTruthy();
    });
  });
});

// ── Purge réel — confirm dialog obligatoire ───────────────────────────────────

describe('JobsPage — Purge réel avec confirm dialog (F-16.3)', () => {
  it('bouton trigger-purge (réel) est actif', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId('trigger-purge');
    expect(btn.hasAttribute('disabled')).toBe(false);
  });

  it('clic trigger-purge affiche le confirm dialog (pas de POST direct)', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');

    const callsBefore = mockFetch.mock.calls.length;
    const btn = await screen.findByTestId('trigger-purge');
    fireEvent.click(btn);

    // Dialog doit apparaître
    const modal = await screen.findByTestId('confirm-modal');
    expect(modal).toBeTruthy();

    // Aucun POST déclenché à ce stade
    const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    expect(postCalls).toHaveLength(0);
  });

  it('confirm dialog — Cancel → aucun POST', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge');
    fireEvent.click(btn);
    await screen.findByTestId('confirm-modal');

    const callsBefore = mockFetch.mock.calls.length;
    const cancelBtn = screen.getByTestId('modal-cancel');
    fireEvent.click(cancelBtn);

    await new Promise(r => setTimeout(r, 50));
    const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    expect(postCalls).toHaveLength(0);
    // Dialog fermé
    expect(screen.queryByTestId('confirm-modal')).toBeNull();
  });

  it('confirm dialog — Confirmer → POST avec dry_run:false', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge');
    fireEvent.click(btn);
    await screen.findByTestId('confirm-modal');

    const callsBefore = mockFetch.mock.calls.length;
    const confirmBtn = screen.getByTestId('modal-confirm');
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
      expect(postCalls).toHaveLength(1);
    });

    const postCalls = mockFetch.mock.calls.filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    const lastPost = postCalls.at(-1) as [string, RequestInit];
    const body = JSON.parse(lastPost[1].body as string) as { spec: { kind: { type: string; data: { dry_run: boolean; mode: string } } } };
    expect(body.spec.kind.type).toBe('Purge');
    expect(body.spec.kind.data.dry_run).toBe(false);
    expect(body.spec.kind.data.mode).toBe('Lifecycle');
  });

  it('confirm dialog — Escape → aucun POST', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge');
    fireEvent.click(btn);
    await screen.findByTestId('confirm-modal');

    const callsBefore = mockFetch.mock.calls.length;
    fireEvent.keyDown(document, { key: 'Escape' });

    await new Promise(r => setTimeout(r, 50));
    const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    expect(postCalls).toHaveLength(0);
  });
});

// ── Retry désactivé — admin CLI only ─────────────────────────────────────────

describe('JobsPage — Retry désactivé (aucun endpoint API replay)', () => {
  it('bouton Retry visible sur job DLQ', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId(`retry-${JOB_DLQ.id}`);
    expect(btn).toBeTruthy();
  });

  it('Retry est disabled', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId(`retry-${JOB_DLQ.id}`);
    expect(btn.hasAttribute('disabled') || btn.getAttribute('aria-disabled') === 'true').toBe(true);
  });

  it('tooltip Retry mentionne admin CLI', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId(`retry-${JOB_DLQ.id}`);
    const title = btn.getAttribute('title') ?? '';
    expect(title.toLowerCase()).toContain('cli');
  });

  it('clic sur Retry ne déclenche aucun POST', async () => {
    renderJobsPage();
    const btn = await screen.findByTestId(`retry-${JOB_DLQ.id}`);
    const callsBefore = mockFetch.mock.calls.length;
    fireEvent.click(btn);
    await new Promise(r => setTimeout(r, 50));
    const postCalls = mockFetch.mock.calls.slice(callsBefore).filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    expect(postCalls).toHaveLength(0);
  });
});

// ── Header Idempotency-Key présent sur tout POST ──────────────────────────────

describe('JobsPage — Header Idempotency-Key (contrat réel)', () => {
  it('POST Purge dry-run porte le header Idempotency-Key', async () => {
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    const btn = await screen.findByTestId('trigger-purge-dry');
    fireEvent.click(btn);

    await waitFor(() => {
      const postCalls = mockFetch.mock.calls.filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
      expect(postCalls.length).toBeGreaterThan(0);
    });

    const postCalls = mockFetch.mock.calls.filter(c => (c as [string, RequestInit])[1]?.method === 'POST');
    const lastPost = postCalls.at(-1) as [string, RequestInit];
    const headers = lastPost[1].headers as Record<string, string>;
    expect(headers['Idempotency-Key']).toBeTruthy();
  });
});
