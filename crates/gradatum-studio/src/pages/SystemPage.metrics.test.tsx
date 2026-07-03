/**
 * SystemPage.metrics.test.tsx — TDD Task 4 (v0.7.5 Slice 2b)
 * Tests : section Métriques rendue depuis catalog mocké + re-fetch sur sélection plage.
 * Contrats mocqués : /scheduled, /metrics/catalog, /metrics/timeseries.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { SystemPage } from './SystemPage';

vi.mock('uplot', () => ({ default: class { destroy() {} setData() {} setSize() {} } }));
vi.mock('../hooks/useAuth', () => ({ apiFetch: vi.fn() }));
import { apiFetch } from '../hooks/useAuth';

const route = (url: string): Response => {
  if (url.includes('/scheduled'))
    return { ok: true, status: 200, json: async () => ({ tasks: [] }) } as Response;
  if (url.includes('/metrics/catalog'))
    return {
      ok: true,
      status: 200,
      json: async () => ({
        series: [
          { key: 'mcp_tool_calls.a', group: 'usage', kind: 'counter', unit: 'calls', instrumented: true },
        ],
      }),
    } as Response;
  if (url.includes('/metrics/timeseries'))
    return {
      ok: true,
      status: 200,
      json: async () => ({
        from_ms: 0,
        to_ms: 1,
        bucket_secs: 60,
        series: [{ key: 'mcp_tool_calls.a', points: [] }],
      }),
    } as Response;
  return { ok: true, status: 200, json: async () => ({}) } as Response;
};

beforeEach(() => {
  (apiFetch as ReturnType<typeof vi.fn>).mockReset();
  (apiFetch as ReturnType<typeof vi.fn>).mockImplementation((u: string) =>
    Promise.resolve(route(u)),
  );
});

describe('SystemPage — section Métriques', () => {
  it('renders the usage group block from the catalog', async () => {
    render(<MemoryRouter><SystemPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByText(/métriques/i)).toBeTruthy());
    await waitFor(() => expect(screen.getByText(/usage/i)).toBeTruthy());
  });

  it('re-requests timeseries with a new from_ms when a range button is clicked', async () => {
    render(<MemoryRouter><SystemPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByText('7j')).toBeTruthy());
    const callsBefore = (apiFetch as ReturnType<typeof vi.fn>).mock.calls
      .filter(c => String(c[0]).includes('/metrics/timeseries')).length;
    fireEvent.click(screen.getByText('7j'));
    await waitFor(() => {
      const tsCalls = (apiFetch as ReturnType<typeof vi.fn>).mock.calls
        .map(c => String(c[0]))
        .filter(u => u.includes('/metrics/timeseries'));
      expect(tsCalls.length).toBeGreaterThan(callsBefore);
    });
  });
});
