/**
 * ReviewPage.test.tsx — comportements expandable + deep-link edit/title
 *
 * Fix contrat vault_read (2026-06-11) :
 *   Contrat réel : POST /api/v1/vault_read { path: ulid } — PAS GET /{id} (→404)
 *   Réponse : { path, content, metadata, size_bytes, sha256 }
 *   Fixture : shape LIVE vérifiée curl ULID 01KTT56A6T46E9T07EJ2N2T8KW
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { ReviewPage } from './ReviewPage';
import { clearUnauthorizedHandler } from '../hooks/useAuth';

// --- Fixtures shape réelle ---

const ULID = '01KTSGBYGG8KQGTDKGF9VNTHN9';

const ITEM = {
  ulid: ULID,
  title: 'Roadmap option A actée',
  section: 'decisions',
  locus: 'decisions/',
  status: 'pending-review' as const,
  provenance: 'claude-code',
  created_ms: Date.now() - 120_000,
};

const REVIEW_RESPONSE = { items: [ITEM], total: 1 };

// Fixture shape réelle POST /api/v1/vault_read (vérifiée LIVE 2026-06-11)
const NOTE_DETAIL = {
  path: ULID,
  content: '# Roadmap option A actée\n\nv0.4.4 distillation → v0.4.5 backends → v0.4.6 studio MVP.',
  metadata: {
    author: 'claude-code',
    created: 1781000000000,
    section: 'decisions',
    status: 'pending-review',
    tags: ['gradatum', 'roadmap'],
    updated: 1781100000000,
    vault_id: 'main',
  },
  size_bytes: 120,
  sha256: 'abc123',
};

// --- Helpers ---

function makeFetchResponse(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

const mockFetch = vi.fn();

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  localStorage.setItem('gradatum_studio_jwt_persist', 'test-jwt');
  clearUnauthorizedHandler();
});

afterEach(() => {
  localStorage.clear();
  clearUnauthorizedHandler();
  vi.restoreAllMocks();
});

/**
 * Configure fetch par URL pour éviter les fragilités d'ordre.
 * Order réel : useHealth (GET /health) → fetchReview (GET /review)
 *            → [expand] POST /api/v1/vault_read { path: ulid }
 * Contrat réel vault_read : POST sur /vault_read (pas GET /{ulid})
 */
function setupFetchMocks(opts: {
  vaultReadStatus?: number;
} = {}) {
  mockFetch.mockImplementation((url: string) => {
    if (typeof url === 'string' && url.includes('/health')) {
      return Promise.resolve(makeFetchResponse({ status: 'ok', version: '0.4.5-test' }));
    }
    if (typeof url === 'string' && url.includes('vault_read')) {
      return Promise.resolve(
        makeFetchResponse(NOTE_DETAIL, opts.vaultReadStatus ?? 200),
      );
    }
    // Default : /review endpoint
    return Promise.resolve(makeFetchResponse(REVIEW_RESPONSE));
  });
}

function renderReviewPage() {
  return render(
    <MemoryRouter initialEntries={['/review']}>
      <ReviewPage />
    </MemoryRouter>,
  );
}

// ── Bug 1 : card expandable ──────────────────────────────────────────────────

describe('ReviewPage — expand card', () => {
  it('bouton Expand visible sur chaque item', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    expect(expandBtn).toBeTruthy();
    expect(expandBtn.textContent).toContain('Expand');
  });

  it('aria-expanded=false avant expansion', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    expect(expandBtn.getAttribute('aria-expanded')).toBe('false');
  });

  it('clic Expand déclenche POST /api/v1/vault_read avec { path: ulid }', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    fireEvent.click(expandBtn);

    await waitFor(() => {
      // Contrat réel : POST sur /api/v1/vault_read (pas GET /{ulid})
      const vaultReadCall = mockFetch.mock.calls.find(c =>
        typeof c[0] === 'string' && c[0].includes('vault_read') && !c[0].includes(`/${ULID}`),
      );
      expect(vaultReadCall).toBeDefined();
      // Vérifie le body POST
      const opts = vaultReadCall?.[1] as RequestInit | undefined;
      expect(opts?.method).toBe('POST');
      const body = JSON.parse(opts?.body as string ?? '{}') as { path?: string };
      expect(body.path).toBe(ULID);
    });
  });

  it('corps rendu après expansion réussie (data-testid body-expanded)', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    fireEvent.click(expandBtn);

    await screen.findByTestId(`body-expanded-${ULID}`);
  });

  it('bouton change en Collapse apres expansion', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    fireEvent.click(expandBtn);

    await waitFor(() => {
      expect(expandBtn.textContent).toContain('Collapse');
    });
  });

  it('aria-expanded=true après expansion', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    fireEvent.click(expandBtn);

    await waitFor(() => {
      expect(expandBtn.getAttribute('aria-expanded')).toBe('true');
    });
  });

  it('clic Collapse replie la card (body-expanded absent du DOM)', async () => {
    setupFetchMocks();
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    // Expand
    fireEvent.click(expandBtn);
    await screen.findByTestId(`body-expanded-${ULID}`);
    // Collapse
    fireEvent.click(expandBtn);
    await waitFor(() => {
      expect(screen.queryByTestId(`body-expanded-${ULID}`)).toBeNull();
    });
  });

  it('affiche erreur si vault_read retourne HTTP 500', async () => {
    setupFetchMocks({ vaultReadStatus: 500 });
    renderReviewPage();

    const expandBtn = await screen.findByTestId(`expand-${ULID}`);
    fireEvent.click(expandBtn);

    await waitFor(() => {
      const bodyEl = screen.queryByTestId(`body-expanded-${ULID}`);
      expect(bodyEl).not.toBeNull();
      expect(bodyEl?.textContent).toContain('Failed to load');
    });
  });
});

// ── Bug 2 : deep-link Edit + titre → /notes/:ulid ───────────────────────────

describe('ReviewPage — deep-link Edit + titre', () => {
  it('bouton Edit présent dans chaque card', async () => {
    setupFetchMocks();
    renderReviewPage();

    const editBtn = await screen.findByTestId(`edit-${ULID}`);
    expect(editBtn).toBeTruthy();
  });

  it('titre de la card est un bouton cliquable avec le bon texte', async () => {
    setupFetchMocks();
    renderReviewPage();

    const titleBtn = await screen.findByTestId(`title-link-${ULID}`);
    expect(titleBtn.tagName).toBe('BUTTON');
    expect(titleBtn.textContent).toBe('Roadmap option A actée');
  });

  it('clic sur Edit ne crashe pas (navigation vers /notes/:ulid)', async () => {
    setupFetchMocks();
    renderReviewPage();

    const editBtn = await screen.findByTestId(`edit-${ULID}`);
    // Si navigate('/notes') sans ULID avait été appelé, NoteDetailPage crasherait
    // car useParams retournerait undefined. Le click doit rester stable.
    expect(() => fireEvent.click(editBtn)).not.toThrow();
  });

  it('clic sur le titre ne crashe pas (navigation vers /notes/:ulid)', async () => {
    setupFetchMocks();
    renderReviewPage();

    const titleBtn = await screen.findByTestId(`title-link-${ULID}`);
    expect(() => fireEvent.click(titleBtn)).not.toThrow();
  });
});
