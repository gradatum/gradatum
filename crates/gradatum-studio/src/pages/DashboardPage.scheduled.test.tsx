/**
 * DashboardPage.scheduled.test.tsx — TDD T7
 * Widget « Tâches planifiées » compact sur le Dashboard.
 * Mock : GET /api/v1/dashboard + GET /api/v1/system/scheduled
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { DashboardPage } from './DashboardPage';

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

const DASHBOARD_BODY = {
  notes_by_status: { live: 100 },
  forgotten_count: 0,
  jobs_by_status: {},
  queue_depth: 0,
};

// NOW_MS capturé ici — seuils assez larges pour ne pas être flaky
const NOW_MS = Date.now();

// 3 tâches : 1 ok, 1 error+errors_24h>0, 1 en retard
const SCHEDULED_3_TASKS = {
  tasks: [
    {
      name: 'telemetry-flush',
      last_run_ms: NOW_MS - 30_000,      // ok (30s < 60*3s=180s)
      last_outcome: 'ok',
      last_duration_ms: 12,
      last_error: null,
      run_count: 100,
      errors_24h: 0,
      interval_secs: 60,
    },
    {
      name: 'purge-event-log',
      last_run_ms: NOW_MS - 10_000,      // error récent
      last_outcome: 'error',
      last_duration_ms: 5,
      last_error: null,
      run_count: 50,
      errors_24h: 2,                     // errors_24h > 0
      interval_secs: 300,
    },
    {
      name: 'review-promote',
      last_run_ms: NOW_MS - 1_000_000,   // en retard (1000s > 300*3s=900s)
      last_outcome: 'ok',
      last_duration_ms: 5,
      last_error: null,
      run_count: 50,
      errors_24h: 0,
      interval_secs: 300,
    },
  ],
};

function setupFetchMock(scheduledBody: unknown) {
  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.7.5-test' }));
    }
    if (typeof url === 'string' && url.includes('/system/scheduled')) {
      return Promise.resolve(makeFetchResponse(scheduledBody));
    }
    return Promise.resolve(makeFetchResponse(DASHBOARD_BODY));
  });
}

function renderDashboard() {
  return render(
    <MemoryRouter>
      <DashboardPage />
    </MemoryRouter>,
  );
}

describe('DashboardPage — widget Scheduled Tasks (T7)', () => {
  it('affiche le widget scheduled-health-widget', async () => {
    setupFetchMock(SCHEDULED_3_TASKS);
    renderDashboard();
    const widget = await screen.findByTestId('scheduled-health-widget');
    expect(widget).toBeTruthy();
  });

  it('affiche le total de tâches (3)', async () => {
    setupFetchMock(SCHEDULED_3_TASKS);
    renderDashboard();
    const count = await screen.findByTestId('scheduled-total-count');
    expect(count.textContent?.trim()).toBe('3');
  });

  it('affiche le nombre de tâches avec errors_24h > 0 (1)', async () => {
    setupFetchMock(SCHEDULED_3_TASKS);
    renderDashboard();
    const count = await screen.findByTestId('scheduled-error-count');
    expect(count.textContent?.trim()).toBe('1');
  });

  it('affiche le nombre de tâches en retard (1)', async () => {
    setupFetchMock(SCHEDULED_3_TASKS);
    renderDashboard();
    const count = await screen.findByTestId('scheduled-late-count');
    expect(count.textContent?.trim()).toBe('1');
  });

  it('contient un lien vers /system', async () => {
    setupFetchMock(SCHEDULED_3_TASKS);
    renderDashboard();
    const link = await screen.findByTestId('system-link');
    expect(link).toBeTruthy();
  });

  it('ne crashe pas si /system/scheduled retourne une erreur 500 (dégradé non bloquant)', async () => {
    mockFetch.mockImplementation((url: string) => {
      if (typeof url === 'string' && url.includes('/health')) {
        return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.7.5-test' }));
      }
      if (typeof url === 'string' && url.includes('/system/scheduled')) {
        return Promise.resolve(makeFetchResponse({ error: 'oops' }, 500));
      }
      return Promise.resolve(makeFetchResponse(DASHBOARD_BODY));
    });
    renderDashboard();
    // Le dashboard doit continuer à s'afficher sans crash
    const content = await screen.findByTestId('dashboard-content');
    expect(content).toBeTruthy();
  });

  it('affiche 0 erreurs et 0 retards si toutes les tâches sont ok', async () => {
    const allOk = {
      tasks: [
        { name: 'task-a', last_run_ms: NOW_MS - 10_000, last_outcome: 'ok', last_duration_ms: 5, last_error: null, run_count: 10, errors_24h: 0, interval_secs: 60 },
        { name: 'task-b', last_run_ms: NOW_MS - 10_000, last_outcome: 'ok', last_duration_ms: 5, last_error: null, run_count: 10, errors_24h: 0, interval_secs: 60 },
      ],
    };
    setupFetchMock(allOk);
    renderDashboard();
    const errCount = await screen.findByTestId('scheduled-error-count');
    const lateCount = await screen.findByTestId('scheduled-late-count');
    expect(errCount.textContent?.trim()).toBe('0');
    expect(lateCount.textContent?.trim()).toBe('0');
  });
});
