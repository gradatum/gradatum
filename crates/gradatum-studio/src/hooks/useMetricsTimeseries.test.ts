import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useMetricsTimeseries } from './useMetricsTimeseries';

vi.mock('./useAuth', () => ({
  apiFetch: vi.fn(),
}));
import { apiFetch } from './useAuth';

const okResp = (body: unknown) => ({ ok: true, status: 200, json: async () => body }) as Response;

beforeEach(() => {
  vi.useFakeTimers();
  (apiFetch as ReturnType<typeof vi.fn>).mockReset();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('useMetricsTimeseries', () => {
  it('fetches the CSV of keys with from/to and exposes the response', async () => {
    (apiFetch as ReturnType<typeof vi.fn>).mockResolvedValue(
      okResp({ from_ms: 0, to_ms: 100, bucket_secs: 60, series: [{ key: 'a', points: [] }] }),
    );
    const { result } = renderHook(() => useMetricsTimeseries(['a', 'b'], 0, 100));
    await vi.waitFor(() => expect(result.current.loading).toBe(false));
    const call = (apiFetch as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    expect(call).toContain('/api/v1/system/metrics/timeseries');
    expect(call).toContain('series=a%2Cb'); // CSV url-encoded, or 'series=a,b'
    expect(call).toContain('from_ms=0');
    expect(call).toContain('to_ms=100');
    expect(result.current.resp?.bucket_secs).toBe(60);
  });

  it('does not fetch when keys is empty', async () => {
    renderHook(() => useMetricsTimeseries([], 0, 100));
    expect((apiFetch as ReturnType<typeof vi.fn>).mock.calls.length).toBe(0);
  });
});
