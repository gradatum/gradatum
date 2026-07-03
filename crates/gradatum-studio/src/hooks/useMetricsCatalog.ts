/**
 * useMetricsCatalog — fetch GET /api/v1/system/metrics/catalog (v0.7.5 Slice 2b)
 * Pattern calqué sur useScheduledHealth: cancelled flag, apiFetch, error non bloquante.
 */

import { useEffect, useState } from 'react';
import { apiFetch } from './useAuth';
import type { CatalogEntry, CatalogResponse } from '../types/api';

export interface MetricsCatalogState {
  catalog: CatalogEntry[];
  loading: boolean;
  error: string | null;
}

export function useMetricsCatalog(): MetricsCatalogState {
  const [state, setState] = useState<MetricsCatalogState>({ catalog: [], loading: true, error: null });

  useEffect(() => {
    let cancelled = false;
    apiFetch('/api/v1/system/metrics/catalog')
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as CatalogResponse;
        if (!cancelled) setState({ catalog: Array.isArray(json.series) ? json.series : [], loading: false, error: null });
      })
      .catch(err => { if (!cancelled) setState({ catalog: [], loading: false, error: String(err) }); });
    return () => { cancelled = true; };
  }, []);

  return state;
}
