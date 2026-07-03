/**
 * useScheduledHealth — fetch GET /api/v1/system/scheduled
 * Hook partagé entre SystemPage (liste complète) et DashboardPage (widget compact).
 * Erreur non bloquante : état dégradé, pas de crash (tasks=[]).
 * T6+T7 — v0.7.5 Slice 1
 */

import { useEffect, useState } from 'react';
import { apiFetch } from './useAuth';
import type { ScheduledTask, ScheduledResponse } from '../types/api';

// ── Logique de badge exportée (réutilisée par SystemPage + tests purs) ────────

export type TaskBadge = 'ok' | 'error' | 'en retard' | 'jamais';

/**
 * Calcule le badge d'état d'une tâche.
 * Priorité : jamais > en retard > error > ok
 * Seuil retard : now - last_run_ms > interval_secs × 3 × 1000 (strict >)
 */
export function taskBadge(
  task: Pick<ScheduledTask, 'last_run_ms' | 'last_outcome' | 'interval_secs'>,
  nowMs: number,
): TaskBadge {
  if (task.last_run_ms === null) return 'jamais';
  if (nowMs - task.last_run_ms > task.interval_secs * 3 * 1000) return 'en retard';
  if (task.last_outcome === 'error') return 'error';
  return 'ok';
}

// ── État du hook ──────────────────────────────────────────────────────────────

export interface ScheduledHealthState {
  tasks: ScheduledTask[];
  loading: boolean;
  error: string | null;
}

export function useScheduledHealth(): ScheduledHealthState {
  const [state, setState] = useState<ScheduledHealthState>({
    tasks: [],
    loading: true,
    error: null,
  });

  useEffect(() => {
    let cancelled = false;
    setState(prev => ({ ...prev, loading: true, error: null }));
    apiFetch('/api/v1/system/scheduled')
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as ScheduledResponse;
        if (!cancelled) {
          setState({
            tasks: Array.isArray(json.tasks) ? json.tasks : [],
            loading: false,
            error: null,
          });
        }
      })
      .catch(err => {
        if (!cancelled) setState({ tasks: [], loading: false, error: String(err) });
      });
    return () => { cancelled = true; };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return state;
}
