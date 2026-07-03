/**
 * useTraces — fetch paginé GET /api/v1/system/traces (v0.7.5 Slice 3)
 * Pattern : apiFetch (auth JWT), reqId guard (réponses obsolètes), mounted ref,
 *           cursor opaque keyset, append vs replace selon `append` flag.
 * P2-B : error rendu AVANT empty dans les consommateurs (ex. ActivityPage).
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { TraceEntry, TracesResponse, TraceFilters } from '../types/api';
import { apiFetch } from './useAuth';

function buildQuery(f: TraceFilters, cursor: string | null): string {
  const p = new URLSearchParams();
  if (f.action_type) p.set('action_type', f.action_type);
  if (f.agent_id) p.set('agent_id', f.agent_id);
  if (f.session_id) p.set('session_id', f.session_id);
  if (f.fromMs != null) p.set('from_ms', String(f.fromMs));
  if (f.toMs != null) p.set('to_ms', String(f.toMs));
  if (cursor) p.set('cursor', cursor);
  return p.toString();
}

export interface UseTracesResult {
  entries: TraceEntry[];
  loading: boolean;
  error: string | null;
  hasMore: boolean;
  loadMore: () => Promise<void>;
  reload: () => Promise<void>;
}

export function useTraces(filters: TraceFilters): UseTracesResult {
  const [entries, setEntries] = useState<TraceEntry[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // reqId guard : ignore les réponses obsolètes (course filtre/pagination)
  const reqId = useRef(0);
  // mounted guard : évite setState après unmount
  const mounted = useRef(true);
  useEffect(() => () => { mounted.current = false; }, []);

  const fetchPage = useCallback(async (append: boolean, cur: string | null) => {
    const myReq = ++reqId.current;
    setLoading(true);
    setError(null);
    try {
      const qs = buildQuery(filters, append ? cur : null);
      const res = await apiFetch(`/api/v1/system/traces${qs ? `?${qs}` : ''}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = (await res.json()) as TracesResponse;
      if (!mounted.current || myReq !== reqId.current) return;
      setEntries(prev => (append ? [...prev, ...data.traces] : data.traces));
      setCursor(data.next_cursor);
    } catch (e) {
      if (mounted.current && myReq === reqId.current) {
        setError(e instanceof Error ? e.message : 'erreur réseau');
      }
    } finally {
      if (mounted.current && myReq === reqId.current) setLoading(false);
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters.action_type, filters.agent_id, filters.session_id, filters.fromMs, filters.toMs]);

  // Page 1 au montage + à tout changement de filtre effectif
  useEffect(() => { void fetchPage(false, null); }, [fetchPage]);

  const loadMore = useCallback(async () => {
    if (cursor) await fetchPage(true, cursor);
  }, [cursor, fetchPage]);

  const reload = useCallback(async () => {
    await fetchPage(false, null);
  }, [fetchPage]);

  return { entries, loading, error, hasMore: cursor !== null, loadMore, reload };
}
