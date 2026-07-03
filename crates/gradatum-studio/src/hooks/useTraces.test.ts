/**
 * useTraces.test.ts — TDD Slice 3 Task 3 (v0.7.5)
 * Tests : page 1, loadMore append, reload reset, filtre → re-fetch, unmount guard.
 * Pattern mock : vi.mock('./useAuth') — identique à useMetricsTimeseries.test.ts.
 */

import { renderHook, act, waitFor } from '@testing-library/react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { useTraces } from './useTraces';

vi.mock('./useAuth', () => ({ apiFetch: vi.fn() }));
import { apiFetch } from './useAuth';

// ── helpers ────────────────────────────────────────────────────────────────────

const makeResp = (traces: object[], next_cursor: string | null): Response =>
  ({ ok: true, status: 200, json: async () => ({ traces, next_cursor }) }) as Response;

const trace = (id: number) => ({
  id,
  session_id: 'S1',
  agent_id: 'main',
  ts_ms: 1_000,
  action_type: 'decision',
  target: null,
  intent: 'do it',
  outcome: 'success',
  ref: null,
  created_at: 1_000,
});

beforeEach(() => {
  (apiFetch as ReturnType<typeof vi.fn>).mockReset();
});

// ── tests ──────────────────────────────────────────────────────────────────────

describe('useTraces', () => {
  it('fetches page 1 and replaces entries', async () => {
    (apiFetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      makeResp([trace(2), trace(1)], '100_1'),
    );
    const { result } = renderHook(() => useTraces({}));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.entries).toHaveLength(2);
    expect(result.current.hasMore).toBe(true);
    const url = String((apiFetch as ReturnType<typeof vi.fn>).mock.calls[0][0]);
    expect(url).not.toContain('cursor=');
  });

  it('loadMore appends via next_cursor without re-fetching page 1', async () => {
    (apiFetch as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(makeResp([trace(2), trace(1)], '100_1'))
      .mockResolvedValueOnce(makeResp([trace(0)], null));

    const { result } = renderHook(() => useTraces({}));
    await waitFor(() => expect(result.current.entries).toHaveLength(2));
    expect(result.current.hasMore).toBe(true);

    await act(async () => { await result.current.loadMore(); });
    await waitFor(() => expect(result.current.entries).toHaveLength(3));
    expect(result.current.hasMore).toBe(false);

    const secondUrl = String((apiFetch as ReturnType<typeof vi.fn>).mock.calls[1][0]);
    expect(secondUrl).toContain('cursor=100_1');
  });

  it('reload re-fetches page 1 and resets entries', async () => {
    const page1 = makeResp([trace(2), trace(1)], '100_1');
    const page2 = makeResp([trace(0)], null);
    (apiFetch as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(page1)
      .mockResolvedValueOnce(page2)
      .mockResolvedValueOnce(page1);

    const { result } = renderHook(() => useTraces({}));
    await waitFor(() => expect(result.current.entries).toHaveLength(2));

    await act(async () => { await result.current.loadMore(); });
    await waitFor(() => expect(result.current.entries).toHaveLength(3));

    await act(async () => { await result.current.reload(); });
    await waitFor(() => expect(result.current.entries).toHaveLength(2));
    expect(result.current.hasMore).toBe(true);
    const reloadUrl = String((apiFetch as ReturnType<typeof vi.fn>).mock.calls[2][0]);
    expect(reloadUrl).not.toContain('cursor=');
  });

  it('filter change re-fetches page 1 without cursor', async () => {
    (apiFetch as ReturnType<typeof vi.fn>).mockResolvedValue(makeResp([trace(1)], null));

    const { result, rerender } = renderHook(
      ({ at }: { at?: string }) => useTraces({ action_type: at }),
      { initialProps: { at: undefined as string | undefined } },
    );
    await waitFor(() => expect(result.current.loading).toBe(false));
    const callsBefore = (apiFetch as ReturnType<typeof vi.fn>).mock.calls.length;

    rerender({ at: 'plan' });
    await waitFor(() => {
      const calls = (apiFetch as ReturnType<typeof vi.fn>).mock.calls;
      expect(calls.length).toBeGreaterThan(callsBefore);
      const lastUrl = String(calls[calls.length - 1][0]);
      expect(lastUrl).toContain('action_type=plan');
      expect(lastUrl).not.toContain('cursor=');
    });
  });

  it('no setState after unmount (mounted guard)', async () => {
    let resolve!: (r: Response) => void;
    const pending = new Promise<Response>(r => { resolve = r; });
    (apiFetch as ReturnType<typeof vi.fn>).mockReturnValue(pending);

    const { unmount } = renderHook(() => useTraces({}));
    unmount();

    // Résolution après unmount — ne doit pas lever de warning React
    resolve(makeResp([], null));
    await new Promise(r => setTimeout(r, 0));
  });
});
