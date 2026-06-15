/**
 * DashboardPage — /
 * GET /api/v1/dashboard
 * Source : s02-ux-normatif.md §5.1 + contrats s1-contrats-endpoints.md
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 *   Couleurs dynamiques (dot status) → CSS custom property --stat-dot-color / --alert-dot-color
 */

import { useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Layout } from '../components/Layout';
import type { DashboardResponse } from '../types/api';
import { apiFetch } from '../hooks/useAuth';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

function fmtWal(bytes: number | undefined): string {
  if (bytes === undefined) return 'n/a';
  if (bytes === 0) return 'n/a'; // 0 jamais affiché (normatif)
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function timeAgo(isoStr: string): string {
  const diff = Date.now() - new Date(isoStr).getTime();
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s} s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ago`;
  return `${Math.floor(m / 60)} h ago`;
}

// Dot color pour chaque status — CSS custom property settée via style attribute
const STATUS_DOTS: Record<string, string> = {
  live: '#15803d',
  staging: '#b54708',
  'pending-review': '#2a5db0',
  draft: '#8a857c',
  deprecated: '#77736a',
  downgraded: '#77736a',
  garbage: '#b42318',
};

const STATUS_LABELS: Record<string, string> = {
  live: 'Live',
  staging: 'Staging',
  'pending-review': 'Review',
  draft: 'Draft',
  deprecated: 'Deprecated',
  downgraded: 'Deprecated',
  garbage: 'Garbage',
};

export function DashboardPage() {
  const navigate = useNavigate();
  const [data, setData] = useState<DashboardResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    apiFetch('/api/v1/dashboard')
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as DashboardResponse;
        if (!cancelled) setData(json);
      })
      .catch(err => {
        if (!cancelled) setError(String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Stats cards : 5 statuts principaux dans l'ordre
  const STAT_STATUSES = ['live', 'pending-review', 'staging', 'draft', 'deprecated'];

  const alerts: Array<{ dot: string; title: string; sub: string; action: string; path: string }> = [];
  if (data) {
    const pending = data.jobs_by_status['DLQ'] ?? 0;
    if (pending > 0) {
      alerts.push({
        dot: '#b42318',
        title: `${pending} job(s) in DLQ`,
        sub: 'Dead letter queue — manual retry needed',
        action: 'View jobs',
        path: '/jobs',
      });
    }
    const walBytes = data.wal_size_bytes;
    if (walBytes !== undefined && walBytes > 16 * 1024 * 1024) {
      alerts.push({
        dot: '#b54708',
        title: `WAL size: ${fmtWal(walBytes)}`,
        sub: 'WAL exceeds 16 MB — checkpoint recommended',
        action: 'View vaults',
        path: '/admin/vaults',
      });
    }
    const reviewCount = (data.notes_by_status['pending-review'] ?? 0)
      + (data.notes_by_status['staging'] ?? 0);
    if (reviewCount > 0) {
      alerts.push({
        dot: '#2a5db0',
        title: `${reviewCount} note(s) awaiting review`,
        sub: 'Curator confidence below threshold',
        action: 'Review',
        path: '/review',
      });
    }
  }

  const reviewCount = (data?.notes_by_status['pending-review'] ?? 0)
    + (data?.notes_by_status['staging'] ?? 0);
  const pendingJobs = data?.jobs_by_status['Pending'] ?? 0;
  const runningJobs = data?.jobs_by_status['Running'] ?? 0;
  const failedJobs = data?.jobs_by_status['Failed'] ?? 0;

  // Couleur du statut last_job — ensemble fini
  const lastJobStatusColor = (status: string) => {
    if (status === 'Done') return 'var(--color-ok)';
    if (status === 'Failed') return 'var(--color-danger)';
    return 'var(--color-text-dim)';
  };

  return (
    <Layout
      title="Dashboard"
      subtitle="Overview of vault health and pending work"
      reviewCount={reviewCount}
    >
      {loading && <div className="loading-text">Loading…</div>}
      {error && (
        <div role="alert" className="error-banner">
          Failed to load dashboard: {error}
        </div>
      )}
      {data && (
        <div className="studio-page-content" data-testid="dashboard-content">
          {/* Stats cards */}
          <div className="dashboard-stats-grid">
            {STAT_STATUSES.map(statusKey => {
              let count = data.notes_by_status[statusKey] ?? 0;
              if (statusKey === 'deprecated') {
                count += data.notes_by_status['downgraded'] ?? 0;
              }
              const dotColor = STATUS_DOTS[statusKey] ?? '#8a857c';
              return (
                <button
                  key={statusKey}
                  onClick={() => navigate(`/notes?status=${statusKey}`)}
                  className="stat-card"
                  data-testid={`stat-card-${statusKey}`}
                >
                  <div className="stat-card-header">
                    {/* CSS custom property pour la couleur dynamique du dot */}
                    <span
                      className="stat-dot"
                      style={{ '--stat-dot-color': dotColor } as React.CSSProperties}
                      aria-hidden="true"
                    />
                    <span className="stat-label">
                      {STATUS_LABELS[statusKey] ?? statusKey}
                    </span>
                  </div>
                  <div className="stat-count tabular">{count}</div>
                  <div className="stat-unit">notes</div>
                </button>
              );
            })}
          </div>

          {/* Grille inférieure */}
          <div className="dashboard-lower-grid">
            {/* Active alerts */}
            <div className="card" style={{ overflow: 'hidden' }}>
              <div className="card-header-row">
                <span className="card-title">Active alerts</span>
                {alerts.length > 0 && (
                  <span className="alert-count-label">{alerts.length} open</span>
                )}
              </div>
              {alerts.length === 0 ? (
                <div className="alert-empty">No active alerts.</div>
              ) : (
                alerts.map((al, i) => (
                  <div key={i} className="alert-row">
                    {/* CSS custom property pour la couleur dynamique du dot */}
                    <span
                      className="alert-dot"
                      style={{ '--alert-dot-color': al.dot } as React.CSSProperties}
                      aria-hidden="true"
                    />
                    <div className="alert-body">
                      <div className="alert-title">{al.title}</div>
                      <div className="alert-sub">{al.sub}</div>
                    </div>
                    <button
                      onClick={() => navigate(al.path)}
                      className="link-accent"
                      style={{ fontSize: '12.5px' }}
                    >
                      {al.action} →
                    </button>
                  </div>
                ))
              )}
            </div>

            {/* Worker + Review inbox */}
            <div className="dashboard-right-col">
              {/* Worker */}
              <div className="card worker-card">
                <div className="card-title">Worker</div>
                <div className="worker-grid">
                  <span className="worker-label">Last job</span>
                  <span className="worker-value tabular">
                    {data.last_job
                      ? `${data.last_job.id.slice(0, 12)}… · ${timeAgo(data.last_job.created_at)} · `
                      : '—'}
                    {data.last_job && (
                      <span style={{ color: lastJobStatusColor(data.last_job.status) }}>
                        {data.last_job.status.toLowerCase()}
                      </span>
                    )}
                  </span>
                  <span className="worker-label">Queue</span>
                  <span className="worker-value tabular">
                    {pendingJobs} pending · {runningJobs} running
                    {failedJobs > 0 && (
                      <> · <span style={{ color: 'var(--color-danger)' }}>{failedJobs} failed</span></>
                    )}
                  </span>
                  <span className="worker-label">WAL</span>
                  <span className="worker-value tabular">{fmtWal(data.wal_size_bytes)}</span>
                </div>
                <button
                  onClick={() => navigate('/jobs')}
                  className="link-accent"
                  style={{ fontSize: '12.5px' }}
                  data-testid="open-jobs-link"
                >
                  Open jobs →
                </button>
              </div>

              {/* Review inbox */}
              <div className="card review-inbox-card">
                <div className="card-title">Review inbox</div>
                <div className="review-inbox-text">
                  <strong className="worker-value tabular" data-testid="review-count">
                    {reviewCount}
                  </strong>{' '}
                  {reviewCount === 1
                    ? 'note awaits human review.'
                    : 'notes await human review.'}
                </div>
                <button
                  onClick={() => navigate('/review')}
                  className="link-accent"
                  style={{ fontSize: '12.5px' }}
                  data-testid="open-review-link"
                >
                  Open review queue →
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
    </Layout>
  );
}
