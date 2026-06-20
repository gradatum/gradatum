/**
 * useHealth — fetch GET /health (public, sans auth)
 * Retourne la version réelle du serveur pour l'affichage dans la sidebar.
 * Erreur non bloquante : version reste undefined.
 */

import { useEffect, useState } from 'react';

interface HealthResponse {
  status: string;
  version: string;
}

export interface HealthState {
  version: string | undefined;
  healthy: boolean;
}

export function useHealth(): HealthState {
  const [state, setState] = useState<HealthState>({ version: undefined, healthy: true });

  useEffect(() => {
    let cancelled = false;
    fetch('/health')
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const data = (await res.json()) as HealthResponse;
        if (!cancelled) {
          setState({ version: data.version ?? undefined, healthy: data.status === 'ok' });
        }
      })
      .catch(() => {
        // Erreur non bloquante — service peut être down temporairement
        if (!cancelled) {
          setState({ version: undefined, healthy: false });
        }
      });
    return () => { cancelled = true; };
  }, []);

  return state;
}
