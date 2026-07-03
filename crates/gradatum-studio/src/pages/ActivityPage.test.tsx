/**
 * ActivityPage.test.tsx — TDD Slice 3 Task 4 (v0.7.5)
 * Tests : barre filtres, filtre action_type → re-query, expand ligne, Charger+, erreur/vide.
 * Pattern : mock useTraces (hook dep), MemoryRouter (Layout→Sidebar→router).
 */

import { render, screen, fireEvent, waitFor, within } from '@testing-library/react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MemoryRouter } from 'react-router-dom';
import ActivityPage from './ActivityPage';

// Mock useTraces — isole la page de son hook
vi.mock('../hooks/useTraces', () => ({ useTraces: vi.fn() }));
import { useTraces } from '../hooks/useTraces';

// Mock apiFetch pour Layout (useHealth utilise fetch natif, mais useAuth via apiFetch
// peut être appelé dans d'autres composants de Layout)
vi.mock('../hooks/useAuth', () => ({ apiFetch: vi.fn(), useUnauthorizedHandler: vi.fn() }));

// ── helpers ────────────────────────────────────────────────────────────────────

const mockEntry = (id: number, action_type = 'decision') => ({
  id,
  session_id: 'S1',
  agent_id: 'main',
  ts_ms: 1_700_000_000_000,
  action_type,
  target: '/tools/some_tool',
  intent: 'effectuer une action',
  outcome: 'success',
  ref: 'ref-xyz',
  created_at: 1_000,
});

const defaultUseTracesReturn = {
  entries: [mockEntry(1)],
  loading: false,
  error: null,
  hasMore: false,
  loadMore: vi.fn(),
  reload: vi.fn(),
};

beforeEach(() => {
  (useTraces as ReturnType<typeof vi.fn>).mockReturnValue({ ...defaultUseTracesReturn });
});

const wrap = (ui: React.ReactElement) =>
  render(<MemoryRouter>{ui}</MemoryRouter>);

// ── tests ──────────────────────────────────────────────────────────────────────

describe('ActivityPage', () => {
  it('renders filter bar and table row', async () => {
    wrap(<ActivityPage />);
    // Barre de filtres : sélecteur type
    expect(screen.getByLabelText(/type/i)).toBeInTheDocument();
    // Ligne de trace dans la table (scope via within pour éviter l'ambiguïté avec <option>)
    const table = await screen.findByRole('table');
    expect(within(table).getByText('decision')).toBeInTheDocument();
    expect(within(table).getByText('main')).toBeInTheDocument();
  });

  it('action_type filter change calls useTraces with updated action_type', async () => {
    wrap(<ActivityPage />);
    const sel = screen.getByLabelText(/type/i);
    fireEvent.change(sel, { target: { value: 'plan' } });
    await waitFor(() => {
      const calls = (useTraces as ReturnType<typeof vi.fn>).mock.calls;
      const lastFilters = calls[calls.length - 1][0];
      expect(lastFilters.action_type).toBe('plan');
    });
  });

  it('row click expands detail with intent', async () => {
    wrap(<ActivityPage />);
    // Scope via within(table) pour éviter l'ambiguïté avec <option value="decision">
    const table = await screen.findByRole('table');
    const badge = within(table).getByText('decision');
    fireEvent.click(badge.closest('tr')!);
    await waitFor(() =>
      expect(screen.getByText(/effectuer une action/i)).toBeInTheDocument(),
    );
  });

  it('second click on same row collapses the expand', async () => {
    wrap(<ActivityPage />);
    const table = await screen.findByRole('table');
    const badge = within(table).getByText('decision');
    fireEvent.click(badge.closest('tr')!);
    await waitFor(() =>
      expect(screen.getByText(/effectuer une action/i)).toBeInTheDocument(),
    );
    fireEvent.click(badge.closest('tr')!);
    await waitFor(() =>
      expect(screen.queryByText(/effectuer une action/i)).not.toBeInTheDocument(),
    );
  });

  it('shows Charger + when hasMore and calls loadMore on click', async () => {
    const loadMore = vi.fn();
    (useTraces as ReturnType<typeof vi.fn>).mockReturnValue({
      ...defaultUseTracesReturn,
      hasMore: true,
      loadMore,
    });
    wrap(<ActivityPage />);
    const btn = await screen.findByRole('button', { name: /charger/i });
    expect(btn).toBeInTheDocument();
    fireEvent.click(btn);
    expect(loadMore).toHaveBeenCalledOnce();
  });

  it('renders error alert BEFORE empty state (P2-B guard)', async () => {
    (useTraces as ReturnType<typeof vi.fn>).mockReturnValue({
      ...defaultUseTracesReturn,
      entries: [],
      error: 'erreur réseau',
    });
    wrap(<ActivityPage />);
    expect(screen.getByRole('alert')).toHaveTextContent(/erreur réseau/i);
    // L'état vide ne doit PAS s'afficher quand il y a une erreur
    expect(screen.queryByText(/aucune trace/i)).not.toBeInTheDocument();
  });

  it('renders empty state when entries is empty and no error', async () => {
    (useTraces as ReturnType<typeof vi.fn>).mockReturnValue({
      ...defaultUseTracesReturn,
      entries: [],
      error: null,
    });
    wrap(<ActivityPage />);
    expect(screen.getByText(/aucune trace/i)).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
});
