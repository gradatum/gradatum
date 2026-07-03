/**
 * SystemPage — /system
 * GET /api/v1/system/scheduled    → santé des 7 tâches récurrentes
 * GET /api/v1/system/metrics/*    → section Métriques (uPlot charts, v0.7.5 Slice 2b)
 * T6 — v0.7.5 Slice 1 | T8 — polish UX | Task 4 — MetricsSection
 */

import { useEffect, useMemo, useState } from 'react';
import { Layout } from '../components/Layout';
import { useScheduledHealth, taskBadge, type TaskBadge } from '../hooks/useScheduledHealth';
import { useMetricsCatalog } from '../hooks/useMetricsCatalog';
import { useMetricsTimeseries } from '../hooks/useMetricsTimeseries';
import { groupCatalog, buildChartData, type ChartSpec } from '../lib/metricsTransform';
import type { TimeseriesPoint } from '../types/api';
import { MetricChart } from '../components/MetricChart';

// ── Formatage ─────────────────────────────────────────────────────────────────

function formatDurationMs(ms: number | null): string {
  if (ms === null) return '—';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function timeAgoMs(ms: number | null): string {
  if (ms === null) return '—';
  const diff = Date.now() - ms;
  const s = Math.floor(diff / 1000);
  if (s < 60) return `il y a ${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `il y a ${m}min`;
  return `il y a ${Math.floor(m / 60)}h`;
}

// ── Métriques — constantes ────────────────────────────────────────────────────

const RANGES: { label: string; ms: number }[] = [
  { label: '1h',  ms: 3_600_000 },
  { label: '24h', ms: 86_400_000 },
  { label: '7j',  ms: 7 * 86_400_000 },
  { label: '14j', ms: 14 * 86_400_000 },
];
const GROUP_ORDER = ['usage', 'context', 'server', 'write'];
const REFRESH_MS = 60_000;

// ── ChartBlock — fetch timeseries + rendu pour un ChartSpec ──────────────────
// P2-B (Auditeur) : affiche `error` en état distinct AVANT le check xs.length===0.
// P2-A : pas de refreshMs — la fenêtre glissante est pilotée par le parent.

function ChartBlock({
  spec,
  fromMs,
  toMs,
}: {
  spec: ChartSpec;
  fromMs: number;
  toMs: number;
}) {
  const keys = useMemo(() => spec.lines.flatMap(l => l.keys), [spec]);
  const { resp, error } = useMetricsTimeseries(keys, fromMs, toMs);
  const byKey = useMemo(() => {
    const m = new Map<string, TimeseriesPoint[]>();
    for (const s of resp?.series ?? []) m.set(s.key, s.points);
    return m;
  }, [resp]);
  const data = useMemo(() => buildChartData(spec, byKey), [spec, byKey]);

  if (error)
    return (
      <div className="metric-chart-empty error">
        {spec.title} — erreur : {error}
      </div>
    );
  if (data.xs.length === 0)
    return (
      <div className="metric-chart-empty">
        {spec.title} — aucune donnée sur la plage
      </div>
    );
  return <MetricChart spec={spec} data={data} bucketSecs={resp?.bucket_secs ?? 60} />;
}

// ── MetricsSection — catalog + range selector + groupes accordéon ─────────────
// P2-A (Auditeur) : tick parent pilote le refresh → fenêtre glissante réelle.

function MetricsSection() {
  const { catalog, loading, error } = useMetricsCatalog();
  const [rangeMs, setRangeMs] = useState(86_400_000); // défaut 24h
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => setTick(t => t + 1), REFRESH_MS);
    return () => clearInterval(id);
  }, [autoRefresh]);

  // Fenêtre glissante stable par tick — Date.now() capturé ici, pas au render.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const { fromMs, toMs } = useMemo(() => {
    const now = Date.now();
    return { fromMs: now - rangeMs, toMs: now };
  }, [rangeMs, tick]);

  const specs = useMemo(() => groupCatalog(catalog), [catalog]);
  const byGroup = useMemo(() => {
    const m = new Map<string, ChartSpec[]>();
    for (const s of specs) {
      if (!m.has(s.group)) m.set(s.group, []);
      m.get(s.group)!.push(s);
    }
    return m;
  }, [specs]);

  if (loading)
    return (
      <section className="metrics-section">
        <h2>Métriques</h2>
        <p>Chargement…</p>
      </section>
    );
  if (error)
    return (
      <section className="metrics-section">
        <h2>Métriques</h2>
        <p className="error-inline">Erreur : {error}</p>
      </section>
    );

  return (
    <section className="metrics-section">
      <h2>Métriques</h2>
      <div className="metrics-controls">
        {RANGES.map(r => (
          <button
            key={r.label}
            className={rangeMs === r.ms ? 'active' : ''}
            onClick={() => setRangeMs(r.ms)}
          >
            {r.label}
          </button>
        ))}
        <label>
          <input
            type="checkbox"
            checked={autoRefresh}
            onChange={e => setAutoRefresh(e.target.checked)}
          />{' '}
          auto 60s
        </label>
      </div>
      {GROUP_ORDER.filter(g => byGroup.has(g)).map(g => (
        <details key={g} open>
          <summary>{g}</summary>
          <div className="metrics-grid">
            {byGroup.get(g)!.map(spec => (
              <ChartBlock key={spec.title} spec={spec} fromMs={fromMs} toMs={toMs} />
            ))}
          </div>
        </details>
      ))}
    </section>
  );
}

// ── Couleurs badge — via CSS custom properties du design system ───────────────
// Utilise les tokens de tokens.css pour la cohérence et la pérennité.
// Contraste WCAG AA vérifié :
//   ok    : var(--color-ok)       #15803d sur #ecf8ef → ~5.4:1 ✅
//   error : var(--color-danger)   #b42318 sur #fdf1f0 → ~6.6:1 ✅
//   warn  : var(--color-warn)     #b54708 sur #fef0e6 → ~5.5:1 ✅
//   jamais: var(--color-text-muted) #66635b sur #f7f7f5 → ~5.0:1 ✅

const BADGE_COLORS: Record<TaskBadge, { color: string; bg: string; border: string }> = {
  ok:          { color: 'var(--color-ok)',         bg: 'var(--color-ok-bg)',     border: '#a7d5b5' },
  error:       { color: 'var(--color-danger)',     bg: 'var(--color-danger-bg)', border: 'var(--color-danger-bd)' },
  'en retard': { color: 'var(--color-warn)',       bg: 'var(--color-warn-bg)',   border: 'var(--color-warn-bd)' },
  jamais:      { color: 'var(--color-text-muted)', bg: 'var(--color-bg)',        border: 'var(--color-border-soft)' },
};

// ── Composant ─────────────────────────────────────────────────────────────────

export function SystemPage() {
  const { tasks, loading, error } = useScheduledHealth();

  return (
    <Layout title="Système" subtitle="Santé des tâches récurrentes">
      <div className="studio-page-content" data-testid="system-page">
        {loading && <div className="loading-text">Chargement…</div>}
        {error && (
          <div role="alert" className="error-inline">
            {error}
          </div>
        )}

        {!loading && !error && (
          <div className="card" style={{ overflow: 'hidden' }}>
            {/* Header colonnes — aligné sur system-task-row-inner */}
            <div className="system-task-header">
              <span>Tâche</span>
              <span>Dernière exécution</span>
              <span>Err. 24h</span>
              <span>Statut</span>
            </div>

            {tasks.map(task => {
              const badge = taskBadge(task, Date.now());
              const bc = BADGE_COLORS[badge];

              return (
                <div
                  key={task.name}
                  className="jobs-row"
                  data-testid={`task-row-${task.name}`}
                >
                  {/* Grille 4 colonnes */}
                  <div className="system-task-row-inner">
                    {/* Col 1 — nom de la tâche */}
                    <div className="job-kind">{task.name}</div>

                    {/* Col 2 — méta (date · durée · nb runs) */}
                    <div className="job-date-cell">
                      {timeAgoMs(task.last_run_ms)}
                      {task.last_duration_ms !== null && ` · ${formatDurationMs(task.last_duration_ms)}`}
                      {` · ${task.run_count} runs`}
                    </div>

                    {/* Col 3 — erreurs 24h */}
                    <span
                      className={`system-task-errors-col tabular${task.errors_24h > 0 ? ' errors-danger' : ''}`}
                      data-testid={`errors-24h-${task.name}`}
                      title={`Erreurs sur les dernières 24h : ${task.errors_24h}`}
                    >
                      {task.errors_24h}
                    </span>

                    {/* Col 4 — badge statut */}
                    <span
                      className="job-status-badge"
                      style={{
                        '--job-status-color': bc.color,
                        '--job-status-bg': bc.bg,
                        '--job-status-border': bc.border,
                      } as React.CSSProperties}
                      data-testid={`badge-${task.name}`}
                    >
                      {badge}
                    </span>
                  </div>

                  {/* Accordéon last_error — <details> natif (keyboard + screen reader ready) */}
                  {task.last_error && (
                    <details
                      className="task-error-accordion"
                      data-testid={`last-error-${task.name}`}
                    >
                      <summary className="task-error-summary">
                        Voir l'erreur
                      </summary>
                      <div className="job-log-panel">
                        {task.last_error}
                      </div>
                    </details>
                  )}
                </div>
              );
            })}
          </div>
        )}

        <MetricsSection />
      </div>
    </Layout>
  );
}
