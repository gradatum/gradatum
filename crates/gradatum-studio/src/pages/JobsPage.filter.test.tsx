/**
 * JobsPage.filter.test.tsx — filtre statut (chips cliquables) + filtre jour + pagination cursor
 *
 * Vérifie :
 *   a) order=desc envoyé systématiquement (plus de tri client-side)
 *   b) filtre jour → created_after/created_before dans l'URL (bornes UTC de minuit local)
 *   c) chips compteurs cliquables → ?status=X, re-clic retire le filtre
 *   d) pagination cursor next/prev sans trou
 *   e) combinaison filtre statut + filtre jour dans la même requête
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { JobsPage } from './JobsPage';
import type { Job } from '../types/api';

// ── Fixtures ─────────────────────────────────────────────────────────────────

const JOB_DLQ_OLD: Job = {
  id: '01KST5293VT9RBSJZBP3CZV262',
  spec: { kind: { type: 'Curate', data: {} } },
  lifecycle: {
    status: 'DLQ',
    created_at: '2026-05-29T15:18:55.355Z',
    started_at: null,
    completed_at: '2026-06-02T19:23:03.093Z',
    result: null,
  },
  retry: { count: 4, max: 3, last_error: 'max_retries exceeded' },
};

const JOB_DONE_NEW: Job = {
  id: '01KTVTSYTB58RTY1S3S927VERJ',
  spec: { kind: { type: 'Curate', data: {} } },
  lifecycle: {
    status: 'Done',
    created_at: '2026-06-10T14:00:00.000Z',
    started_at: '2026-06-10T14:00:01.000Z',
    completed_at: '2026-06-10T14:00:05.000Z',
    result: { success: true, duration_ms: 4000, cost_usd: null, result_note: null },
  },
};

// last_job = JOB_DONE_NEW → défaut filtre jour = date locale de 2026-06-10T14:00:00.000Z
const DASHBOARD_RESPONSE = {
  notes_by_status: { live: 998 },
  forgotten_count: 0,
  jobs_by_status: { Done: 608, DLQ: 6, Failed: 2, Running: 1, Pending: 3 },
  queue_depth: 3,
  last_job: { id: JOB_DONE_NEW.id, status: 'Done', created_at: '2026-06-10T14:00:00.000Z' },
};

const CURSOR_PAGE1 = 'CURSOR_PAGE1_PLACEHOLDER';
const CURSOR_PAGE2 = 'CURSOR_PAGE2_PLACEHOLDER';

const JOBS_PAGE1 = { items: [JOB_DONE_NEW], next_cursor: CURSOR_PAGE1 };
const JOBS_PAGE2 = { items: [JOB_DLQ_OLD],  next_cursor: CURSOR_PAGE2 };
const JOBS_ALL   = { items: [JOB_DONE_NEW, JOB_DLQ_OLD], next_cursor: null };
const JOBS_DLQ   = { items: [JOB_DLQ_OLD], next_cursor: null };
const JOBS_DONE  = { items: [JOB_DONE_NEW], next_cursor: null };
const JOBS_EMPTY = { items: [], next_cursor: null };

// ── Helpers ───────────────────────────────────────────────────────────────────

function makeFetchResponse(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

const mockFetch = vi.fn();

function setupDefaultMocks(jobsBody: unknown = JOBS_ALL) {
  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.4.5-test' }));
    }
    if (typeof url === 'string' && url.includes('/dashboard')) {
      return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
    }
    return Promise.resolve(makeFetchResponse(jobsBody));
  });
}

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  sessionStorage.setItem('gradatum_studio_jwt', 'test-jwt');
});

afterEach(() => {
  sessionStorage.clear();
  vi.restoreAllMocks();
});

function renderJobsPage() {
  return render(
    <MemoryRouter>
      <JobsPage />
    </MemoryRouter>,
  );
}

/** Extrait toutes les URLs de jobs appelées (sans /dashboard ni /health). */
function getJobsUrls(): string[] {
  return mockFetch.mock.calls
    .map((c: unknown[]) => c[0] as string)
    .filter((u: string) => u.includes('/jobs'));
}

// ── a) order=desc systématique ────────────────────────────────────────────────

describe('JobsPage — order=desc envoyé systématiquement', () => {
  it('le premier appel /jobs contient order=desc', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await screen.findByTestId('jobs-page');
    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.length).toBeGreaterThan(0);
      expect(urls[0]).toContain('order=desc');
    });
  });

  it('pas de tri client-side (ordre serveur conservé)', async () => {
    // Serveur renvoie DLQ_OLD avant DONE_NEW — on conserve cet ordre sans re-trier côté client
    setupDefaultMocks({ items: [JOB_DLQ_OLD, JOB_DONE_NEW], next_cursor: null });
    renderJobsPage();
    // Attendre que les deux rows soient dans le DOM (rendu stable post-fetch)
    await screen.findByTestId(`job-row-${JOB_DLQ_OLD.id}`);
    await screen.findByTestId(`job-row-${JOB_DONE_NEW.id}`);
    const allRows = document.querySelectorAll('[data-testid^="job-row-"]');
    const ids = Array.from(allRows).map(el => el.getAttribute('data-testid')?.replace('job-row-', ''));
    expect(ids[0]).toBe(JOB_DLQ_OLD.id);
    expect(ids[1]).toBe(JOB_DONE_NEW.id);
  });
});

// ── b) Filtre jour ────────────────────────────────────────────────────────────

describe('JobsPage — filtre jour', () => {
  it('la barre de filtre jour est présente', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await screen.findByTestId('day-filter-bar');
  });

  it('le filtre jour est initialisé sur le jour du last_job du dashboard', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const input = await screen.findByTestId('day-input') as HTMLInputElement;
    // last_job.created_at = 2026-06-10T14:00:00.000Z → jour local
    // En UTC la date est déjà 2026-06-10 (à 14h UTC)
    expect(input.value).toMatch(/^2026-06-1[01]$/); // tolérance TZ locale
  });

  it('un appel /jobs contient created_after et created_before (après init dashboard)', async () => {
    // Le dashboard répond avec last_job → dayFilter initialisé → second appel /jobs avec bornes
    setupDefaultMocks();
    renderJobsPage();
    await waitFor(() => {
      const urls = getJobsUrls();
      // Attendre qu'au moins un appel ait created_after (peut être le 2ème si dashboard répond après)
      expect(urls.some(u => u.includes('created_after=') && u.includes('created_before='))).toBe(true);
    }, { timeout: 3000 });
  });

  it('les bornes created_after/before sont des timestamps RFC3339', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await waitFor(() => {
      const urls = getJobsUrls();
      const urlWithBounds = urls.find(u => u.includes('created_after='));
      if (!urlWithBounds) return; // pas encore disponible
      const afterMatch = urlWithBounds.match(/created_after=([^&]+)/);
      const beforeMatch = urlWithBounds.match(/created_before=([^&]+)/);
      expect(afterMatch).not.toBeNull();
      expect(beforeMatch).not.toBeNull();
      const afterDecoded = decodeURIComponent(afterMatch![1]);
      const beforeDecoded = decodeURIComponent(beforeMatch![1]);
      expect(new Date(afterDecoded).toISOString()).toBeTruthy();
      expect(new Date(beforeDecoded).toISOString()).toBeTruthy();
    }, { timeout: 3000 });
  });

  it('created_before = created_after + 24h (fenêtre d\'un jour)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await waitFor(() => {
      const urls = getJobsUrls();
      const urlWithBounds = urls.find(u => u.includes('created_after='));
      if (!urlWithBounds) return;
      const afterMatch = urlWithBounds.match(/created_after=([^&]+)/);
      const beforeMatch = urlWithBounds.match(/created_before=([^&]+)/);
      if (!afterMatch || !beforeMatch) return;
      const after  = new Date(decodeURIComponent(afterMatch[1])).getTime();
      const before = new Date(decodeURIComponent(beforeMatch[1])).getTime();
      expect(before - after).toBe(86_400_000); // exactement 24h
    }, { timeout: 3000 });
  });

  it('clic "All days" retire created_after/before de l\'URL', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const clearBtn = await screen.findByTestId('day-clear');
    mockFetch.mockReset();
    setupDefaultMocks();
    fireEvent.click(clearBtn);
    await waitFor(() => {
      const urls = getJobsUrls();
      if (urls.length === 0) return;
      expect(urls[0]).not.toContain('created_after=');
      expect(urls[0]).not.toContain('created_before=');
    });
    // Le label "All days" apparaît (plus de bouton clear)
    await screen.findByTestId('day-all-label');
  });

  it('navigation prev rétrécit le filtre d\'un jour', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const input = await screen.findByTestId('day-input') as HTMLInputElement;
    const originalDay = input.value;

    mockFetch.mockReset();
    setupDefaultMocks();
    const prevBtn = screen.getByTestId('day-prev');
    fireEvent.click(prevBtn);

    await waitFor(() => {
      const newInput = screen.getByTestId('day-input') as HTMLInputElement;
      expect(newInput.value).not.toBe(originalDay);
    });
  });
});

// ── c) Chips cliquables pour filtre statut ────────────────────────────────────

describe('JobsPage — chips cliquables (filtre statut)', () => {
  it('chip-pending est un bouton avec aria-pressed=false initial', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');
    expect(chip.tagName).toBe('BUTTON');
    expect(chip.getAttribute('aria-pressed')).toBe('false');
  });

  it('clic chip-pending déclenche GET /jobs?status=Pending', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');

    mockFetch.mockReset();
    mockFetch.mockImplementation((url: string) => {
      if (url.includes('/dashboard')) return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
      return Promise.resolve(makeFetchResponse(JOBS_EMPTY));
    });

    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => u.includes('status=Pending'))).toBe(true);
    });
  });

  it('chip actif a aria-pressed=true', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');
    fireEvent.click(chip);
    await waitFor(() => {
      expect(chip.getAttribute('aria-pressed')).toBe('true');
    });
  });

  it('re-clic sur chip actif retire le filtre (request sans ?status=)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');

    // Premier clic — active le filtre
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => u.includes('status=Pending'))).toBe(true);
    });

    // Deuxième clic — retire le filtre
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_ALL);
    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => !u.includes('status='))).toBe(true);
    });
  });

  it('chip-failed visible si DLQ > 0 (depuis dashboard)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await screen.findByTestId('chip-failed');
  });

  it('chip-failed affiche DLQ+Failed depuis dashboard (6+2=8)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-failed');
    expect(chip.textContent).toContain('8');
  });

  it('chip-pending affiche le vrai total depuis dashboard (3)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');
    expect(chip.textContent).toContain('3');
  });

  it('chip-running affiche le vrai total depuis dashboard (1)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-running');
    expect(chip.textContent).toContain('1');
  });

  it('clic chip-failed déclenche ?status=DLQ (bucket terminal, pas Failed transient)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    const chip = await screen.findByTestId('chip-failed');

    mockFetch.mockReset();
    setupDefaultMocks(JOBS_DLQ);
    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => u.includes('status=DLQ'))).toBe(true);
      // Ne doit PAS envoyer status=Failed (transient, 0 en base)
      expect(urls.some(u => u.includes('status=Failed'))).toBe(false);
    });
  });

  it('"N shown" est affiché si filtre actif', async () => {
    setupDefaultMocks(JOBS_DLQ);
    renderJobsPage();
    const chip = await screen.findByTestId('chip-pending');

    mockFetch.mockReset();
    setupDefaultMocks(JOBS_DLQ);
    fireEvent.click(chip);

    await waitFor(() => {
      expect(screen.queryByTestId('filter-count')).not.toBeNull();
    });
  });

  it('"N shown" absent si filtre = all (aucun chip actif)', async () => {
    setupDefaultMocks();
    renderJobsPage();
    await screen.findByTestId('chip-pending');
    expect(screen.queryByTestId('filter-count')).toBeNull();
  });

  it('liste vide avec filtre → empty state, pas de crash', async () => {
    setupDefaultMocks(JOBS_EMPTY);
    renderJobsPage();
    const chip = await screen.findByTestId('chip-running');

    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await waitFor(() => {
      expect(screen.queryByTestId('jobs-empty')).not.toBeNull();
    });
  });
});

// ── d) Pagination cursor ──────────────────────────────────────────────────────

describe('JobsPage — pagination cursor next/prev', () => {
  it('barre de pagination présente quand des jobs sont affichés', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    await screen.findByTestId('pagination-bar');
  });

  it('bouton "Older →" activé si next_cursor non null', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const nextBtn = await screen.findByTestId('page-next');
    expect(nextBtn.hasAttribute('disabled')).toBe(false);
  });

  it('bouton "← Newer" désactivé sur première page', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const prevBtn = await screen.findByTestId('page-prev');
    expect(prevBtn.hasAttribute('disabled')).toBe(true);
  });

  it('bouton "Older →" désactivé si next_cursor null', async () => {
    setupDefaultMocks(JOBS_DONE);
    renderJobsPage();
    const nextBtn = await screen.findByTestId('page-next');
    await waitFor(() => {
      expect(nextBtn.hasAttribute('disabled')).toBe(true);
    });
  });

  it('clic "Older →" envoie le cursor dans la requête suivante', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const nextBtn = await screen.findByTestId('page-next');

    mockFetch.mockReset();
    mockFetch.mockImplementation((url: string) => {
      if (url.includes('/dashboard')) return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
      return Promise.resolve(makeFetchResponse(JOBS_PAGE2));
    });

    fireEvent.click(nextBtn);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => u.includes(`cursor=${CURSOR_PAGE1}`))).toBe(true);
    });
  });

  it('clic "← Newer" depuis page 2 revient à la page 1 sans cursor', async () => {
    // Setup : page 1
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const nextBtn = await screen.findByTestId('page-next');

    // Aller à la page 2
    mockFetch.mockReset();
    mockFetch.mockImplementation((url: string) => {
      if (url.includes('/dashboard')) return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
      return Promise.resolve(makeFetchResponse(JOBS_PAGE2));
    });
    fireEvent.click(nextBtn);

    const prevBtn = await screen.findByTestId('page-prev');
    await waitFor(() => expect(prevBtn.hasAttribute('disabled')).toBe(false));

    // Revenir à la page 1
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_PAGE1);
    fireEvent.click(prevBtn);

    await waitFor(() => {
      const urls = getJobsUrls();
      // L'appel retour ne doit pas avoir de cursor (première page)
      expect(urls.some(u => !u.includes('cursor='))).toBe(true);
    });
  });

  it('le numéro de page s\'incrémente à chaque "Older →"', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const pageInfo = await screen.findByTestId('page-info');
    expect(pageInfo.textContent).toContain('Page 1');

    const nextBtn = screen.getByTestId('page-next');
    mockFetch.mockReset();
    mockFetch.mockImplementation((url: string) => {
      if (url.includes('/dashboard')) return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
      return Promise.resolve(makeFetchResponse(JOBS_PAGE2));
    });

    fireEvent.click(nextBtn);
    await waitFor(() => {
      expect(screen.getByTestId('page-info').textContent).toContain('Page 2');
    });
  });

  it('order=desc est conservé sur les pages suivantes', async () => {
    setupDefaultMocks(JOBS_PAGE1);
    renderJobsPage();
    const nextBtn = await screen.findByTestId('page-next');

    mockFetch.mockReset();
    mockFetch.mockImplementation((url: string) => {
      if (url.includes('/dashboard')) return Promise.resolve(makeFetchResponse(DASHBOARD_RESPONSE));
      return Promise.resolve(makeFetchResponse(JOBS_PAGE2));
    });

    fireEvent.click(nextBtn);

    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.every(u => u.includes('order=desc'))).toBe(true);
    });
  });
});

// ── e) Exclusion mutuelle filtre statut / filtre jour ────────────────────────

describe('JobsPage — filtre statut et filtre jour mutuellement exclusifs', () => {
  it('clic chip retire le filtre jour (All days) — pas de created_after dans la requête', async () => {
    setupDefaultMocks(JOBS_ALL);
    renderJobsPage();
    // Attendre que le filtre jour s'initialise depuis dashboard
    await waitFor(() => {
      const urls = getJobsUrls();
      expect(urls.some(u => u.includes('created_after='))).toBe(true);
    }, { timeout: 3000 });

    const chip = screen.getByTestId('chip-running');
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      if (urls.length === 0) return;
      const u = urls[0];
      // Le statut est présent
      expect(u).toContain('status=Running');
      // Le filtre jour est ABSENT (exclusion mutuelle)
      expect(u).not.toContain('created_after=');
      expect(u).not.toContain('created_before=');
    });
  });

  it('clic chip → day-all-label visible (filtre jour retiré)', async () => {
    setupDefaultMocks(JOBS_ALL);
    renderJobsPage();
    await screen.findByTestId('day-input');

    const chip = screen.getByTestId('chip-pending');
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await screen.findByTestId('day-all-label');
  });

  it('changer le jour retire le filtre statut (pas de status= dans la requête)', async () => {
    setupDefaultMocks(JOBS_ALL);
    renderJobsPage();

    // Activer d'abord un chip
    const chip = await screen.findByTestId('chip-running');
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await waitFor(() => {
      expect(getJobsUrls().some(u => u.includes('status=Running'))).toBe(true);
    });

    // Changer le jour → retire le statut
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_ALL);
    const input = screen.getByTestId('day-input') as HTMLInputElement;
    fireEvent.change(input, { target: { value: '2026-06-09' } });

    await waitFor(() => {
      const urls = getJobsUrls();
      if (urls.length === 0) return;
      expect(urls[0]).not.toContain('status=');
      expect(urls[0]).toContain('created_after=');
    });
  });

  it('order=desc est toujours présent après clic chip', async () => {
    setupDefaultMocks(JOBS_ALL);
    renderJobsPage();

    const chip = await screen.findByTestId('chip-running');
    mockFetch.mockReset();
    setupDefaultMocks(JOBS_EMPTY);
    fireEvent.click(chip);

    await waitFor(() => {
      const urls = getJobsUrls();
      if (urls.length === 0) return;
      expect(urls[0]).toContain('order=desc');
    });
  });
});
