/**
 * SystemPage.test.tsx — TDD T6
 * Tests : logique badge (purs) + rendu RTL
 * Contrat : GET /api/v1/system/scheduled → { tasks: ScheduledTask[] }
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { SystemPage } from './SystemPage';
import type { ScheduledTask } from '../types/api';

// ── Copie inline de taskBadge pour tests purs (pattern JobsPage.test.tsx) ─────
type TaskBadge = 'ok' | 'error' | 'en retard' | 'jamais';

function taskBadge(
  task: Pick<ScheduledTask, 'last_run_ms' | 'last_outcome' | 'interval_secs'>,
  nowMs: number,
): TaskBadge {
  if (task.last_run_ms === null) return 'jamais';
  if (nowMs - task.last_run_ms > task.interval_secs * 3 * 1000) return 'en retard';
  if (task.last_outcome === 'error') return 'error';
  return 'ok';
}

// ── Tests purs — logique de badge ────────────────────────────────────────────

describe('taskBadge — logique badge', () => {
  const NOW = 1_000_000_000;

  it('jamais si last_run_ms === null', () => {
    expect(taskBadge({ last_run_ms: null, last_outcome: null, interval_secs: 60 }, NOW)).toBe('jamais');
  });

  it('en retard si now - last_run_ms > interval_secs × 3 × 1000', () => {
    // interval=60s → seuil=180s → 200s passés → en retard
    expect(taskBadge({ last_run_ms: NOW - 200_000, last_outcome: 'ok', interval_secs: 60 }, NOW)).toBe('en retard');
  });

  it('ok si récent et last_outcome=ok', () => {
    // interval=60s → seuil=180s → 30s passés → ok
    expect(taskBadge({ last_run_ms: NOW - 30_000, last_outcome: 'ok', interval_secs: 60 }, NOW)).toBe('ok');
  });

  it('error si récent et last_outcome=error', () => {
    expect(taskBadge({ last_run_ms: NOW - 30_000, last_outcome: 'error', interval_secs: 60 }, NOW)).toBe('error');
  });

  it('en retard prend priorité sur error si trop vieux', () => {
    expect(taskBadge({ last_run_ms: NOW - 200_000, last_outcome: 'error', interval_secs: 60 }, NOW)).toBe('en retard');
  });

  it('limite exacte = seuil (strict >) → ok, seuil+1ms → en retard', () => {
    // seuil = 60 * 3 * 1000 = 180_000
    expect(taskBadge({ last_run_ms: NOW - 180_000, last_outcome: 'ok', interval_secs: 60 }, NOW)).toBe('ok');
    expect(taskBadge({ last_run_ms: NOW - 180_001, last_outcome: 'ok', interval_secs: 60 }, NOW)).toBe('en retard');
  });

  it('last_outcome null avec last_run_ms récent → ok', () => {
    expect(taskBadge({ last_run_ms: NOW - 30_000, last_outcome: null, interval_secs: 60 }, NOW)).toBe('ok');
  });
});

// ── Tests RTL — rendu ─────────────────────────────────────────────────────────

function makeFetchResponse(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

const mockFetch = vi.fn();

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  localStorage.setItem('gradatum_studio_jwt_persist', 'test-jwt');
});

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

// Fixtures 7 tâches couvrant tous les cas de badge
// NOW_MS capturé ici — les seuils sont assez larges pour absorber les ms de setup
const NOW_MS = Date.now();

const SEVEN_TASKS: ScheduledTask[] = [
  {
    name: 'telemetry-flush',
    last_run_ms: NOW_MS - 30_000,    // 30s passés, interval=60s → ok
    last_outcome: 'ok',
    last_duration_ms: 12,
    last_error: null,
    run_count: 1440,
    errors_24h: 0,
    interval_secs: 60,
  },
  {
    name: 'purge-event-log',
    last_run_ms: NOW_MS - 10_000,    // 10s passés, interval=300s → error (recent+error)
    last_outcome: 'error',
    last_duration_ms: 5,
    last_error: 'connection refused',
    run_count: 100,
    errors_24h: 3,
    interval_secs: 300,
  },
  {
    name: 'purge-session-trace',
    last_run_ms: NOW_MS - 10_000,
    last_outcome: 'ok',
    last_duration_ms: 5,
    last_error: null,
    run_count: 100,
    errors_24h: 0,
    interval_secs: 300,
  },
  {
    name: 'purge-read-usage',
    last_run_ms: NOW_MS - 1_000_000, // 1000s passés, interval=300s → en retard (>900s)
    last_outcome: 'ok',
    last_duration_ms: 5,
    last_error: null,
    run_count: 100,
    errors_24h: 0,
    interval_secs: 300,
  },
  {
    name: 'review-promote',
    last_run_ms: null,               // jamais exécuté
    last_outcome: null,
    last_duration_ms: null,
    last_error: null,
    run_count: 0,
    errors_24h: 0,
    interval_secs: 300,
  },
  {
    name: 'proactive-refresh',
    last_run_ms: NOW_MS - 10_000,
    last_outcome: 'ok',
    last_duration_ms: 5,
    last_error: null,
    run_count: 50,
    errors_24h: 0,
    interval_secs: 3600,
  },
  {
    name: 'active-recall-purge',
    last_run_ms: NOW_MS - 10_000,
    last_outcome: 'ok',
    last_duration_ms: 5,
    last_error: null,
    run_count: 50,
    errors_24h: 0,
    interval_secs: 3600,
  },
];

function setupFetchMock(scheduledBody: unknown) {
  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.7.5-test' }));
    }
    return Promise.resolve(makeFetchResponse(scheduledBody));
  });
}

function renderSystemPage() {
  return render(
    <MemoryRouter>
      <SystemPage />
    </MemoryRouter>,
  );
}

describe('SystemPage — rendu', () => {
  it('rend 7 lignes de tâches', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const rows = await screen.findAllByTestId(/^task-row-/);
    expect(rows).toHaveLength(7);
  });

  it('badge ok affiché pour telemetry-flush', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const badge = await screen.findByTestId('badge-telemetry-flush');
    expect(badge.textContent?.trim()).toBe('ok');
  });

  it('badge error affiché pour purge-event-log', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const badge = await screen.findByTestId('badge-purge-event-log');
    expect(badge.textContent?.trim()).toBe('error');
  });

  it('badge en retard affiché pour purge-read-usage', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const badge = await screen.findByTestId('badge-purge-read-usage');
    expect(badge.textContent?.trim()).toBe('en retard');
  });

  it('badge jamais affiché pour review-promote', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const badge = await screen.findByTestId('badge-review-promote');
    expect(badge.textContent?.trim()).toBe('jamais');
  });

  it('errors_24h affiché en rouge (classe errors-danger) si > 0', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const el = await screen.findByTestId('errors-24h-purge-event-log');
    expect(el.textContent?.trim()).toBe('3');
    expect(el.className).toContain('errors-danger');
  });

  it('errors_24h affiché normalement (sans classe errors-danger) si = 0', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const el = await screen.findByTestId('errors-24h-telemetry-flush');
    expect(el.textContent?.trim()).toBe('0');
    expect(el.className).not.toContain('errors-danger');
  });

  it('last_error affiché pour la tâche en erreur', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    const err = await screen.findByTestId('last-error-purge-event-log');
    expect(err.textContent).toContain('connection refused');
  });

  it('pas de last-error affiché pour les tâches sans erreur', async () => {
    setupFetchMock({ tasks: SEVEN_TASKS });
    renderSystemPage();
    await screen.findAllByTestId(/^task-row-/);
    expect(screen.queryByTestId('last-error-telemetry-flush')).toBeNull();
  });

  it('affiche un message d\'erreur si le fetch échoue', async () => {
    mockFetch.mockImplementation((url: string) => {
      if (typeof url === 'string' && url.includes('/health')) {
        return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.7.5-test' }));
      }
      return Promise.resolve(makeFetchResponse({ error: 'server error' }, 500));
    });
    renderSystemPage();
    const err = await screen.findByRole('alert');
    expect(err.textContent).toContain('HTTP 500');
  });

  it('liste vide si tasks absents dans la réponse', async () => {
    setupFetchMock({ tasks: [] });
    renderSystemPage();
    // Attendre que le loading finisse
    await screen.findByTestId('system-page');
    const rows = screen.queryAllByTestId(/^task-row-/);
    expect(rows).toHaveLength(0);
  });
});
