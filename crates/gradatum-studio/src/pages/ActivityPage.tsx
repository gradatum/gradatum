/**
 * ActivityPage — /activity
 * GET /api/v1/system/traces → table session_trace filtrée + expand ligne + auto-refresh
 * v0.7.5 Slice 3 — pattern plage/tick copié de SystemPage MetricsSection (Slice 2b).
 *
 * Garde P2-B (Auditeur) : afficher `error` AVANT le check `entries.length === 0`.
 */

import { Fragment, useEffect, useMemo, useState } from 'react';
import { Layout } from '../components/Layout';
import { useTraces } from '../hooks/useTraces';
import type { TraceEntry, TraceFilters } from '../types/api';

// ── constantes ─────────────────────────────────────────────────────────────────

const RANGES: { label: string; ms: number }[] = [
  { label: '1h',  ms: 3_600_000 },
  { label: '24h', ms: 86_400_000 },
  { label: '7j',  ms: 7 * 86_400_000 },
  { label: '14j', ms: 14 * 86_400_000 },
];

const REFRESH_MS = 60_000;

const ACTION_TYPES = ['plan', 'edit', 'tool-call', 'decision', 'verdict', 'deploy'];

// ── helpers ────────────────────────────────────────────────────────────────────

function truncate(s: string | null, max: number): string {
  if (!s) return '—';
  if (s.length <= max) return s;
  return `${s.slice(0, max)}…`;
}

// ── sous-composant : ligne expand ─────────────────────────────────────────────

function TraceExpandRow({ entry }: { entry: TraceEntry }) {
  return (
    <tr>
      <td colSpan={5} className="trace-expand-panel">
        {entry.intent && (
          <div>
            <strong>Intent :</strong> {entry.intent}
          </div>
        )}
        {entry.target && (
          <div>
            <strong>Target :</strong> {entry.target}
          </div>
        )}
        {entry.ref && (
          <div>
            <strong>Ref :</strong> {entry.ref}
          </div>
        )}
        {!entry.intent && !entry.target && !entry.ref && (
          <span className="job-date-cell">Aucun détail disponible</span>
        )}
      </td>
    </tr>
  );
}

// ── composant principal ────────────────────────────────────────────────────────

export default function ActivityPage() {
  const [actionType, setActionType] = useState('');
  const [agentId, setAgentId] = useState('');
  const [sessionId, setSessionId] = useState('');
  const [rangeMs, setRangeMs] = useState(86_400_000);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [tick, setTick] = useState(0);
  const [expandedId, setExpandedId] = useState<number | null>(null);

  // Auto-refresh : identique au pattern MetricsSection Slice 2b
  useEffect(() => {
    if (!autoRefresh) return;
    const t = setInterval(() => setTick(x => x + 1), REFRESH_MS);
    return () => clearInterval(t);
  }, [autoRefresh]);

  // Fenêtre glissante stable — Date.now() capturé ici, pas au render
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const { fromMs, toMs } = useMemo(() => {
    const now = Date.now();
    return { fromMs: now - rangeMs, toMs: now };
  }, [rangeMs, tick]);

  const filters: TraceFilters = useMemo(() => ({
    ...(actionType ? { action_type: actionType } : {}),
    ...(agentId ? { agent_id: agentId } : {}),
    ...(sessionId ? { session_id: sessionId } : {}),
    fromMs,
    toMs,
  }), [actionType, agentId, sessionId, fromMs, toMs]);

  const { entries, loading, error, hasMore, loadMore, reload } = useTraces(filters);

  const handleRowClick = (id: number) => {
    setExpandedId(prev => (prev === id ? null : id));
  };

  return (
    <Layout title="Activité" subtitle="Historique session_trace">
      <div className="studio-page-content" data-testid="activity-page">

        {/* ── barre de filtres ─────────────────────────────────────────────── */}
        <div className="metrics-controls activity-filter-bar">
          {/* Filtre type */}
          <label htmlFor="at-filter">Type</label>
          <select
            id="at-filter"
            aria-label="Type d'action"
            value={actionType}
            onChange={e => setActionType(e.target.value)}
          >
            <option value="">tous</option>
            {ACTION_TYPES.map(t => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>

          {/* Filtre agent */}
          <input
            type="text"
            aria-label="Agent ID"
            placeholder="Agent…"
            value={agentId}
            onChange={e => setAgentId(e.target.value)}
          />

          {/* Filtre session */}
          <input
            type="text"
            aria-label="Session ID"
            placeholder="Session…"
            value={sessionId}
            onChange={e => setSessionId(e.target.value)}
          />

          {/* Sélecteur de plage */}
          {RANGES.map(r => (
            <button
              key={r.label}
              className={rangeMs === r.ms ? 'active' : ''}
              onClick={() => setRangeMs(r.ms)}
            >
              {r.label}
            </button>
          ))}

          {/* Toggle auto-refresh */}
          <label>
            <input
              type="checkbox"
              checked={autoRefresh}
              onChange={e => setAutoRefresh(e.target.checked)}
            />{' '}
            auto 60s
          </label>

          {/* Rafraîchir manuel */}
          <button onClick={() => void reload()}>Rafraîchir</button>
        </div>

        {/* ── état chargement ──────────────────────────────────────────────── */}
        {loading && <div className="loading-text">Chargement…</div>}

        {/* ── état erreur — AVANT l'état vide (P2-B) ──────────────────────── */}
        {error && (
          <div role="alert" className="error-inline">
            {error}
          </div>
        )}

        {/* ── état vide — APRÈS l'erreur ───────────────────────────────────── */}
        {!error && !loading && entries.length === 0 && (
          <p className="card-empty-text">Aucune trace sur la plage sélectionnée.</p>
        )}

        {/* ── table traces ─────────────────────────────────────────────────── */}
        {!error && entries.length > 0 && (
          <div className="card" style={{ overflow: 'hidden' }}>
            <table className="activity-table">
              <thead>
                <tr>
                  <th>Date / Heure</th>
                  <th>Agent</th>
                  <th>Type</th>
                  <th>Target</th>
                  <th>Résultat</th>
                </tr>
              </thead>
              <tbody>
                {entries.map(e => (
                  <Fragment key={e.id}>
                    <tr
                      className="activity-row"
                      onClick={() => handleRowClick(e.id)}
                      aria-expanded={expandedId === e.id}
                    >
                      <td className="job-date-cell">
                        {new Date(e.ts_ms).toLocaleString('fr-FR')}
                      </td>
                      <td className="job-kind">{e.agent_id}</td>
                      <td>
                        <span className="job-status-badge">{e.action_type}</span>
                      </td>
                      <td className="job-date-cell" title={e.target ?? undefined}>
                        {truncate(e.target, 40)}
                      </td>
                      <td className="job-date-cell">{e.outcome ?? '—'}</td>
                    </tr>
                    {expandedId === e.id && <TraceExpandRow entry={e} />}
                  </Fragment>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {/* ── pagination keyset ────────────────────────────────────────────── */}
        {hasMore && (
          <div className="activity-load-more">
            <button onClick={() => void loadMore()}>Charger +</button>
          </div>
        )}

      </div>
    </Layout>
  );
}
