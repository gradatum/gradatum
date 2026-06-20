/**
 * DashboardPage.reviewCount.test.tsx (2026-06-11)
 *
 * Vérifie que le widget "Review inbox" = pending-review + staging
 * (aligne sur GET /api/v1/review qui retourne les deux statuts).
 *
 * Fixture réelle LIVE : staging=17, pending-review absent → inbox doit afficher 17.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { DashboardPage } from './DashboardPage';

// --- Helpers ---

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

function renderDashboard() {
  return render(
    <MemoryRouter>
      <DashboardPage />
    </MemoryRouter>,
  );
}

// Fixture réelle LIVE 2026-06-11 :
// notes_by_status: { live:998, downgraded:79, staging:17, garbage:4 }
// (pas de 'pending-review' → absent)
const DASHBOARD_STAGING_ONLY = {
  notes_by_status: { live: 998, downgraded: 79, staging: 17, garbage: 4 },
  forgotten_count: 0,
  jobs_by_status: { Done: 608, DLQ: 6 },
  queue_depth: 0,
};

const DASHBOARD_BOTH = {
  notes_by_status: { live: 500, staging: 10, 'pending-review': 5 },
  forgotten_count: 0,
  jobs_by_status: {},
  queue_depth: 0,
};

const DASHBOARD_NONE = {
  notes_by_status: { live: 200 },
  forgotten_count: 0,
  jobs_by_status: {},
  queue_depth: 0,
};

function setupFetchMock(dashboardBody: unknown) {
  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.4.5-test' }));
    }
    return Promise.resolve(makeFetchResponse(dashboardBody));
  });
}

// ── Calcul reviewCount ────────────────────────────────────────────────────────

describe('DashboardPage — Review inbox = pending-review + staging', () => {
  it('fixture LIVE : staging=17, pending-review absent → inbox affiche 17', async () => {
    setupFetchMock(DASHBOARD_STAGING_ONLY);
    renderDashboard();
    // Attendre le rendu du widget Review inbox
    const el = await screen.findByTestId('review-count');
    expect(el.textContent?.trim()).toBe('17');
  });

  it('les deux présents (staging=10 + pending-review=5) → inbox = 15', async () => {
    setupFetchMock(DASHBOARD_BOTH);
    renderDashboard();
    const el = await screen.findByTestId('review-count');
    expect(el.textContent?.trim()).toBe('15');
  });

  it('aucun des deux → inbox = 0', async () => {
    setupFetchMock(DASHBOARD_NONE);
    renderDashboard();
    const el = await screen.findByTestId('review-count');
    expect(el.textContent?.trim()).toBe('0');
  });
});
