/**
 * NoteDetailPage.recurate.test.tsx — bouton Re-curate (F-16.3)
 *
 * Contrat réel :
 * - POST /api/v1/vault_read { path: ulid } → note courante
 * - Bouton "Re-curate cette note" → POST /api/v1/jobs Curate { note_id: ulid courant }
 * - Feedback toast sur succès (job id) et erreur
 * - Idempotency-Key présent dans le POST
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { NoteDetailPage } from './NoteDetailPage';

const NOTE_ULID = '01KTST1WHWTMY4KYSD383ZF416';

// Shape VaultReadResponse réelle (vérifiée LIVE 2026-06-11)
const VAULT_READ_RESPONSE = {
  path: NOTE_ULID,
  content: '---\nauthor: test\ncreated: 1748000000000\nsection: retrospectives\nstatus: live\ntags: []\nupdated: 1748000000000\nvault_id: main\n---\n# Test Note\nContent here.',
  metadata: {
    author: 'test',
    created: 1748000000000,
    section: 'retrospectives',
    status: 'live',
    tags: [],
    updated: 1748000000000,
    vault_id: 'main',
  },
  size_bytes: 100,
  sha256: 'abc123',
};

// Réponse job 202
const JOB_CREATED = { id: '01KTWBBCMPHN1X55QZ0HFSMEQX', idempotent: false };

const mockFetch = vi.fn();

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  sessionStorage.setItem('gradatum_studio_jwt', 'test-jwt');
  Object.defineProperty(globalThis, 'crypto', {
    value: { randomUUID: () => 'ffffffff-test-uuid' },
    configurable: true,
  });

  mockFetch.mockImplementation((url: string, opts?: RequestInit) => {
    if (url === '/api/v1/vault_read') {
      return Promise.resolve({ ok: true, status: 200, json: async () => VAULT_READ_RESPONSE });
    }
    if (url === '/api/v1/jobs' && opts?.method === 'POST') {
      return Promise.resolve({ ok: true, status: 202, json: async () => JOB_CREATED });
    }
    return Promise.resolve({ ok: true, status: 200, json: async () => ({}) });
  });
});

afterEach(() => {
  sessionStorage.clear();
  vi.restoreAllMocks();
});

function renderNoteDetail() {
  return render(
    <MemoryRouter initialEntries={[`/notes/${NOTE_ULID}`]}>
      <Routes>
        <Route path="/notes/:id" element={<NoteDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

// ── Présence et état initial ─────────────────────────────────────────────────

describe('NoteDetailPage — bouton Re-curate (F-16.3)', () => {
  it('bouton recurate-btn présent sur la page', async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    expect(btn).toBeTruthy();
  });

  it('bouton recurate-btn actif par défaut (pas disabled)', async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    expect(btn.hasAttribute('disabled')).toBe(false);
  });

  it("bouton recurate-btn affiche 'Re-curate cette note'", async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    expect(btn.textContent).toContain('Re-curate');
  });
});

// ── Déclenchement POST Curate avec note_id courant ───────────────────────────

describe('NoteDetailPage — Re-curate déclenche POST correct', () => {
  it('POST /api/v1/jobs avec type=Curate et note_id=ulid courant', async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');

    fireEvent.click(btn);

    await waitFor(() => {
      const jobPosts = mockFetch.mock.calls.filter(
        c => (c as [string, RequestInit])[0] === '/api/v1/jobs' && (c as [string, RequestInit])[1]?.method === 'POST',
      );
      expect(jobPosts).toHaveLength(1);
    });

    const jobPost = mockFetch.mock.calls.find(
      c => (c as [string, RequestInit])[0] === '/api/v1/jobs',
    ) as [string, RequestInit];
    const body = JSON.parse(jobPost[1].body as string) as {
      spec: { kind: { type: string; data: { note_id: string } } };
    };
    expect(body.spec.kind.type).toBe('Curate');
    expect(body.spec.kind.data.note_id).toBe(NOTE_ULID);
  });

  it('header Idempotency-Key présent sur le POST Curate', async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    fireEvent.click(btn);

    await waitFor(() => {
      const jobPosts = mockFetch.mock.calls.filter(
        c => (c as [string, RequestInit])[0] === '/api/v1/jobs',
      );
      expect(jobPosts.length).toBeGreaterThan(0);
    });

    const jobPost = mockFetch.mock.calls.find(
      c => (c as [string, RequestInit])[0] === '/api/v1/jobs',
    ) as [string, RequestInit];
    const headers = jobPost[1].headers as Record<string, string>;
    expect(headers['Idempotency-Key']).toBeTruthy();
  });
});

// ── Feedback utilisateur ──────────────────────────────────────────────────────

describe('NoteDetailPage — feedback Re-curate', () => {
  it('toast succès affiché avec job id après Re-curate', async () => {
    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    fireEvent.click(btn);

    await waitFor(() => {
      expect(screen.queryByText(/Curate job queued/i) ?? screen.queryByText(/queued/i)).toBeTruthy();
    });
  });

  it("toast erreur affiché si POST échoue avec 400", async () => {
    mockFetch.mockImplementation((url: string, opts?: RequestInit) => {
      if (url === '/api/v1/vault_read') {
        return Promise.resolve({ ok: true, status: 200, json: async () => VAULT_READ_RESPONSE });
      }
      if (url === '/api/v1/jobs' && opts?.method === 'POST') {
        return Promise.resolve({
          ok: false,
          status: 400,
          json: async () => ({ error: "missing field `note_id`" }),
        });
      }
      return Promise.resolve({ ok: true, status: 200, json: async () => ({}) });
    });

    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    fireEvent.click(btn);

    await waitFor(() => {
      const errorText = screen.queryByText(/Error/i) ?? screen.queryByText(/note_id/i);
      expect(errorText).toBeTruthy();
    });
  });

  it('bouton devient disabled pendant le chargement', async () => {
    // POST lent — bouton doit être disabled pendant l'appel
    let resolvePost: (v: unknown) => void;
    const slowPost = new Promise(r => { resolvePost = r; });

    mockFetch.mockImplementation((url: string, opts?: RequestInit) => {
      if (url === '/api/v1/vault_read') {
        return Promise.resolve({ ok: true, status: 200, json: async () => VAULT_READ_RESPONSE });
      }
      if (url === '/api/v1/jobs' && opts?.method === 'POST') {
        return slowPost as Promise<Response>;
      }
      return Promise.resolve({ ok: true, status: 200, json: async () => ({}) });
    });

    renderNoteDetail();
    const btn = await screen.findByTestId('recurate-btn');
    fireEvent.click(btn);

    // Pendant l'appel async, le bouton doit être disabled
    await waitFor(() => {
      expect(screen.getByTestId('recurate-btn').hasAttribute('disabled')).toBe(true);
    });

    // Résoudre la promesse
    resolvePost!({ ok: true, status: 202, json: async () => JOB_CREATED });
  });
});
