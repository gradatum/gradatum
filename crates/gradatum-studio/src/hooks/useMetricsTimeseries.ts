/**
 * useMetricsTimeseries — fetch GET /api/v1/system/metrics/timeseries (v0.7.5 Slice 2b)
 * CORRECTION P2-A : signature SANS refreshMs param et SANS setInterval interne.
 * Re-fetch est piloté par le parent (Task 4) via changement fromMs/toMs.
 * Pattern calqué sur useScheduledHealth: cancelled flag, apiFetch, error non bloquante.
 */

import { useEffect, useState } from 'react';
import { apiFetch } from './useAuth';
import type { TimeseriesResponse } from '../types/api';

export interface MetricsTimeseriesState {
  resp: TimeseriesResponse | null;
  loading: boolean;
  error: string | null;
}

export function useMetricsTimeseries(
  keys: string[],
  fromMs: number,
  toMs: number,
): MetricsTimeseriesState {
  const [state, setState] = useState<MetricsTimeseriesState>({
    resp: null,
    loading: keys.length > 0,
    error: null,
  });
  const keysCsv = keys.join(',');

  useEffect(() => {
    if (keys.length === 0) {
      setState({ resp: null, loading: false, error: null });
      return;
    }
    let cancelled = false;
    setState(prev => ({ ...prev, loading: true, error: null }));
    const qs = `series=${encodeURIComponent(keysCsv)}&from_ms=${fromMs}&to_ms=${toMs}`;
    apiFetch(`/api/v1/system/metrics/timeseries?${qs}`)
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as TimeseriesResponse;
        if (!cancelled) setState({ resp: json, loading: false, error: null });
      })
      .catch(err => { if (!cancelled) setState({ resp: null, loading: false, error: String(err) }); });
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keysCsv, fromMs, toMs]);

  return state;
}
