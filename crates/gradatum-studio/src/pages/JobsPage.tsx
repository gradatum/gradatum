/**
 * JobsPage — /jobs
 * GET /api/v1/jobs?order=desc&limit=50[&status=X][&created_after=T&created_before=T][&cursor=ULID]
 *
 * Contrats réels vérifiés LIVE F-16.3 (2026-06-11) :
 *   GET /api/v1/jobs : items[], next_cursor — params order/status/created_after/created_before/limit/cursor
 *     → order=desc envoyé systématiquement (tri serveur newest-first)
 *     → filtre ?status=DLQ|Failed|Done|Running|Pending fonctionnel
 *     → created_after / created_before : RFC3339 UTC, bornes EXCLUSIVES, url-encodées
 *     → cursor : ULID du dernier item de la page précédente
 *   GET /api/v1/dashboard : jobs_by_status (vrais totaux COUNT GROUP BY)
 *     → source unique pour les chips de résumé (évite divergence dashboard↔jobs)
 *   POST /api/v1/jobs (F-16 LIVE) :
 *     - Header Idempotency-Key obligatoire (400 sinon)
 *     - Curate : body { spec: { kind: { type: "Curate", data: { note_id } } } } → 202 — note_id manquant = 400
 *     - Purge : body { spec: { kind: { type: "Purge", data: { mode: "Lifecycle", dry_run } } } } → 202
 *       mode obligatoire (seul "Lifecycle" implémenté), dry_run défaut true côté serveur
 *     - CurateSpec.dry_run : INEXISTANT (retiré — Curate n'a pas de dry-run)
 *   Retry/replay DLQ : aucun endpoint API — admin CLI only (vérifié LIVE F-16.3)
 *     → bouton Retry désactivé avec tooltip explicite (pas de 404 silencieux)
 *
 * F-16.3 : bandeau E-13 RETIRÉ — triggers câblés sur contrats réels.
 * Re-curate par-note → NoteDetailPage (bouton contextuel, note_id vient de la note courante).
 * Purge (dry-run défaut) → JobsPage avec confirm dialog avant purge réel.
 *
 * Filtre jour — timezone locale → UTC :
 *   created_after = minuit local du jour sélectionné (exclusif)
 *   created_before = minuit local du lendemain (exclusif)
 *
 * Filtre statut vs filtre jour — MUTUELLEMENT EXCLUSIFS
 * chip-failed filtre status=DLQ
 * Pagination cursor : cursorStack pour navigation prev/next
 *
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 *   Couleurs dynamiques (chip/badge status) → CSS custom properties --chip-color / --job-status-*
 */

import { useEffect, useState, useCallback, useRef } from 'react';
import { Layout } from '../components/Layout';
import { Toast } from '../components/Toast';
import { ConfirmModal } from '../components/ConfirmModal';
import type { Job, FlatJob, JobsResponse, DashboardResponse } from '../types/api';
import { apiFetch } from '../hooks/useAuth';
import { triggerPurge } from '../lib/jobs';

type StatusFilter = 'all' | 'DLQ' | 'Failed' | 'Done' | 'Running' | 'Pending';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

function flattenJob(j: Job): FlatJob {
  const kind = j.spec?.kind?.type ?? 'Unknown';
  const lc = j.lifecycle ?? {} as Job['lifecycle'];
  const status = (lc.status ?? 'Pending') as FlatJob['status'];
  const duration_ms = lc.result?.duration_ms ?? undefined;
  const lastError = j.retry?.last_error ?? null;
  return { id: j.id, kind, status, created_at: lc.created_at ?? '', started_at: lc.started_at ?? null, completed_at: lc.completed_at ?? null, duration_ms, lastError };
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1000)}s`;
}

function formatDate(iso: string): string {
  if (!iso) return '—';
  try {
    return new Date(iso).toLocaleString('fr-FR', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  } catch { return iso; }
}

function toDateInputValue(iso: string): string {
  if (!iso) return '';
  try {
    const d = new Date(iso);
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  } catch { return ''; }
}

function dayToQueryBounds(dateStr: string): { after: string; before: string } {
  const [y, m, d] = dateStr.split('-').map(Number);
  return {
    after:  new Date(y, m - 1, d, 0, 0, 0, 0).toISOString(),
    before: new Date(y, m - 1, d + 1, 0, 0, 0, 0).toISOString(),
  };
}

function shiftDay(dateStr: string, delta: 1 | -1): string {
  const [y, m, d] = dateStr.split('-').map(Number);
  const dt = new Date(y, m - 1, d + delta);
  return `${dt.getFullYear()}-${String(dt.getMonth() + 1).padStart(2, '0')}-${String(dt.getDate()).padStart(2, '0')}`;
}

// Palette de couleurs par statut — CSS custom properties
const STATUS_COLORS: Record<string, { color: string; bg: string; border: string }> = {
  Pending: { color: '#b54708', bg: '#fef0e6', border: '#f5d9c1' },
  Running: { color: '#1a56db', bg: '#ebf3ff', border: '#c6d9f7' },
  Done:    { color: '#15803d', bg: '#ecf8ef', border: '#bbf0d0' },
  Failed:  { color: '#b42318', bg: '#fff1f0', border: '#fecdca' },
  DLQ:     { color: '#7c3aed', bg: '#f3f0ff', border: '#ddd6fe' },
};

function buildJobsUrl(params: { statusFilter: StatusFilter; dayFilter: string | null; cursor: string | null; limit?: number }): string {
  const { statusFilter, dayFilter, cursor, limit = 50 } = params;
  const qp = new URLSearchParams();
  qp.set('order', 'desc');
  qp.set('limit', String(limit));
  if (statusFilter !== 'all') qp.set('status', statusFilter);
  if (dayFilter) {
    const { after, before } = dayToQueryBounds(dayFilter);
    qp.set('created_after', after);
    qp.set('created_before', before);
  }
  if (cursor) qp.set('cursor', cursor);
  return `/api/v1/jobs?${qp.toString()}`;
}

export function JobsPage() {
  const [jobs, setJobs] = useState<FlatJob[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');
  const [dayFilter, setDayFilter] = useState<string | null>(null);
  const [jobCounts, setJobCounts] = useState<Record<string, number>>({});
  const [currentCursor, setCurrentCursor] = useState<string | null>(null);
  const [cursorStack, setCursorStack] = useState<string[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [openLogs, setOpenLogs] = useState<Set<string>>(new Set());
  const autoOpened = useRef(false);

  // Purge — état du confirm dialog
  type PurgeIntent = { dryRun: boolean } | null;
  const [purgeIntent, setPurgeIntent] = useState<PurgeIntent>(null);
  const [purgeLoading, setPurgeLoading] = useState(false);

  useEffect(() => {
    let cancelled = false;
    apiFetch('/api/v1/dashboard')
      .then(async res => {
        if (!res.ok) return;
        const json = (await res.json()) as DashboardResponse;
        if (!cancelled) {
          setJobCounts(json.jobs_by_status ?? {});
          if (json.last_job?.created_at && dayFilter === null) {
            setDayFilter(toDateInputValue(json.last_job.created_at));
          }
        }
      })
      .catch(() => { /* silencieux — chips dégradées */ });
    return () => { cancelled = true; };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const fetchJobs = useCallback((filter: StatusFilter, day: string | null, cursor: string | null) => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    autoOpened.current = false;
    const url = buildJobsUrl({ statusFilter: filter, dayFilter: day, cursor });
    apiFetch(url)
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as JobsResponse;
        if (!cancelled) {
          const items = Array.isArray(json.items) ? json.items : [];
          setJobs(items.map(flattenJob));
          setNextCursor(json.next_cursor ?? null);
        }
      })
      .catch(err => { if (!cancelled) setError(String(err)); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    return fetchJobs(statusFilter, dayFilter, currentCursor);
  }, [fetchJobs, statusFilter, dayFilter, currentCursor]);

  useEffect(() => {
    if (loading || autoOpened.current) return;
    const failedJobs = jobs.filter(j => (j.status === 'Failed' || j.status === 'DLQ') && j.lastError);
    if (failedJobs.length > 0) {
      setOpenLogs(prev => {
        const next = new Set(prev);
        failedJobs.forEach(j => next.add(j.id));
        return next;
      });
      autoOpened.current = true;
    }
  }, [jobs, loading]);

  const toggleLog = (id: string) => {
    setOpenLogs(prev => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
  };

  const resetPagination = () => { setCurrentCursor(null); setCursorStack([]); setNextCursor(null); };

  const handleStatusChipClick = (status: StatusFilter) => {
    setStatusFilter(statusFilter === status ? 'all' : status);
    setDayFilter(null);
    resetPagination();
  };

  const handleDayChange = (newDay: string | null) => {
    setStatusFilter('all');
    setDayFilter(newDay);
    resetPagination();
  };

  const handleNextPage = () => {
    if (!nextCursor) return;
    setCursorStack(prev => [...prev, currentCursor ?? '']);
    setCurrentCursor(nextCursor);
    setOpenLogs(new Set());
  };

  const handlePrevPage = () => {
    if (cursorStack.length === 0) return;
    const stack = [...cursorStack];
    const prev = stack.pop() ?? null;
    setCursorStack(stack);
    setCurrentCursor(prev === '' ? null : prev);
    setOpenLogs(new Set());
  };

  const handlePurgeRequest = (dryRun: boolean) => {
    // Purge dry-run : pas de confirm (pas destructeur). Purge réel : confirm obligatoire.
    if (dryRun) {
      void executePurge(true);
    } else {
      setPurgeIntent({ dryRun: false });
    }
  };

  const executePurge = async (dryRun: boolean) => {
    setPurgeIntent(null);
    setPurgeLoading(true);
    const result = await triggerPurge(dryRun);
    setPurgeLoading(false);
    if (result.ok) {
      const label = dryRun ? 'dry-run' : 'real';
      setToast(`Purge (${label}) queued — id: ${result.id ?? '?'}`);
    } else {
      setToast(`Purge error: ${result.error ?? 'Unknown error'}`);
    }
  };

  const pending = jobCounts['Pending'] ?? 0;
  const running = jobCounts['Running'] ?? 0;
  const done    = jobCounts['Done'] ?? 0;
  const failed  = (jobCounts['Failed'] ?? 0) + (jobCounts['DLQ'] ?? 0);
  const isFirstPage = cursorStack.length === 0 && currentCursor === null;
  const pageNum = cursorStack.length + 1;

  return (
    <Layout title="Jobs" subtitle="Worker queue — background processing">
      <div className="studio-page-jobs" data-testid="jobs-page">
        {/* ── Barre de contrôles ── */}
        <div className="jobs-controls">
          {/* Chips compteurs cliquables */}
          <div className="jobs-chips-row">
            <StatusChip
              label="pending" count={pending}
              active={statusFilter === 'Pending'}
              colors={STATUS_COLORS['Pending']}
              onClick={() => handleStatusChipClick('Pending')}
              testId="chip-pending"
            />
            <StatusChip
              label="running" count={running}
              active={statusFilter === 'Running'}
              colors={STATUS_COLORS['Running']}
              onClick={() => handleStatusChipClick('Running')}
              testId="chip-running"
            />
            {(failed > 0 || statusFilter === 'DLQ') && (
              <StatusChip
                label="failed/DLQ" count={failed}
                active={statusFilter === 'DLQ'}
                colors={STATUS_COLORS['Failed']}
                onClick={() => handleStatusChipClick('DLQ')}
                testId="chip-failed"
              />
            )}
            {done > 0 && (
              <StatusChip
                label="done" count={done}
                active={statusFilter === 'Done'}
                colors={STATUS_COLORS['Done']}
                onClick={() => handleStatusChipClick('Done')}
                testId="chip-done"
              />
            )}
            {statusFilter !== 'all' && (
              <span className="jobs-filter-count" data-testid="filter-count">
                {jobs.length} shown
              </span>
            )}
          </div>

          {/* Triggers F-16 actifs */}
          <div className="jobs-actions-row">
            <button
              disabled
              className="btn-outline"
              style={{ fontSize: '12.5px', padding: '7px 14px', opacity: 0.45, cursor: 'not-allowed' }}
              title="Re-curate par note : utilisez le bouton « Re-curate cette note » sur la page de détail d'une note"
              data-testid="trigger-curate"
              aria-disabled="true"
            >
              Re-curate
            </button>
            <button
              onClick={() => handlePurgeRequest(true)}
              disabled={purgeLoading}
              className="btn-neutral"
              style={{ fontSize: '12.5px', padding: '7px 14px' }}
              title="Purge dry-run : liste les notes Garbage éligibles sans supprimer"
              data-testid="trigger-purge-dry"
            >
              {purgeLoading ? 'Running…' : 'Purge (dry-run)'}
            </button>
            <button
              onClick={() => handlePurgeRequest(false)}
              disabled={purgeLoading}
              className="btn-danger"
              style={{ fontSize: '12.5px', padding: '7px 14px' }}
              title="Purge réel : supprime définitivement les notes Garbage ayant dépassé la période de grâce. Une confirmation sera demandée."
              data-testid="trigger-purge"
            >
              Purge (réel)
            </button>
          </div>
        </div>

        {/* ── Filtre jour ── */}
        <div className="day-filter-bar" data-testid="day-filter-bar">
          <button
            onClick={() => dayFilter && handleDayChange(shiftDay(dayFilter, -1))}
            disabled={!dayFilter}
            className="btn-neutral"
            style={{ fontSize: '13px', padding: '3px 9px', minHeight: 'unset', lineHeight: 1 }}
            aria-label="Jour précédent" data-testid="day-prev"
          >
            ‹
          </button>

          <input
            type="date"
            value={dayFilter ?? ''}
            onChange={e => handleDayChange(e.target.value || null)}
            className={`day-input${!dayFilter ? ' all-days' : ''}`}
            aria-label="Filtre par jour" data-testid="day-input"
          />

          <button
            onClick={() => dayFilter && handleDayChange(shiftDay(dayFilter, 1))}
            disabled={!dayFilter}
            className="btn-neutral"
            style={{ fontSize: '13px', padding: '3px 9px', minHeight: 'unset', lineHeight: 1 }}
            aria-label="Jour suivant" data-testid="day-next"
          >
            ›
          </button>

          {dayFilter && (
            <button
              onClick={() => handleDayChange(null)}
              className="btn-neutral"
              style={{ fontSize: '11.5px', padding: '4px 10px', minHeight: 'unset' }}
              data-testid="day-clear"
            >
              All days
            </button>
          )}
          {!dayFilter && (
            <span className="day-all-label" data-testid="day-all-label">All days</span>
          )}
        </div>

        {loading && <div className="loading-text">Loading jobs…</div>}
        {error && <div role="alert" className="error-inline">{error}</div>}

        {!loading && jobs.length === 0 && !error && (
          <div
            className="card"
            style={{ padding: '48px', textAlign: 'center', color: 'var(--color-text-dim)', fontSize: '13px' }}
            data-testid="jobs-empty"
          >
            No jobs found for current filters.
          </div>
        )}

        {!loading && jobs.length > 0 && (
          <div className="card" style={{ overflow: 'hidden' }}>
            {/* En-tête */}
            <div className="jobs-table-header">
              <span>Kind</span>
              <span>ID</span>
              <span>Created</span>
              <span>Duration</span>
              <span>Status</span>
              <span style={{ textAlign: 'right' }}>Actions</span>
            </div>

            {jobs.map(job => {
              const st = STATUS_COLORS[job.status] ?? STATUS_COLORS['Pending'];
              const hasLog = Boolean(job.lastError);
              const logOpen = openLogs.has(job.id);

              return (
                <div key={job.id} className="jobs-row" data-testid={`job-row-${job.id}`}>
                  <div className="jobs-row-inner">
                    <div className="job-kind">{job.kind}</div>
                    <div className="job-id-cell" title={job.id}>{job.id}</div>
                    <div className="job-date-cell">{formatDate(job.created_at)}</div>
                    <div className="job-duration-cell">
                      {job.duration_ms !== undefined ? formatDuration(job.duration_ms) : '—'}
                    </div>
                    <div>
                      {/* CSS custom properties pour les couleurs dynamiques du statut */}
                      <span
                        className="job-status-badge"
                        style={{
                          '--job-status-color': st.color,
                          '--job-status-bg': st.bg,
                          '--job-status-border': st.border,
                        } as React.CSSProperties}
                        data-testid={`job-status-${job.id}`}
                      >
                        {job.status}
                      </span>
                    </div>
                    <div className="job-actions-cell">
                      {hasLog && (
                        <button
                          onClick={() => toggleLog(job.id)}
                          className="btn-neutral"
                          style={{ fontSize: '11.5px', padding: '4px 10px' }}
                          aria-expanded={logOpen}
                          data-testid={`log-toggle-${job.id}`}
                        >
                          {logOpen ? 'Hide' : 'Log'}
                        </button>
                      )}
                      {(job.status === 'Failed' || job.status === 'DLQ') && (
                        <button
                          disabled
                          className="btn-outline"
                          style={{ fontSize: '11.5px', padding: '4px 10px', opacity: 0.45, cursor: 'not-allowed' }}
                          title="Replay DLQ non disponible en API — admin CLI uniquement (`gradatum-cli jobs replay <id>`)"
                          data-testid={`retry-${job.id}`}
                          aria-disabled="true"
                        >
                          Retry
                        </button>
                      )}
                    </div>
                  </div>

                  {logOpen && job.lastError && (
                    <div
                      className="job-log-panel"
                      role="log"
                      aria-label={`Log du job ${job.id}`}
                      data-testid={`log-panel-${job.id}`}
                    >
                      {job.lastError}
                    </div>
                  )}
                </div>
              );
            })}

            {/* ── Pagination ── */}
            <div className="jobs-pagination" data-testid="pagination-bar">
              <button
                onClick={handlePrevPage}
                disabled={isFirstPage}
                className="btn-neutral"
                style={{ fontSize: '12px', padding: '5px 12px', opacity: isFirstPage ? 0.4 : 1, cursor: isFirstPage ? 'not-allowed' : 'pointer' }}
                aria-label="Page précédente — jobs plus récents"
                data-testid="page-prev"
              >
                ← Newer
              </button>

              <span data-testid="page-info">
                Page {pageNum}{jobs.length > 0 && ` · ${jobs.length} jobs`}
              </span>

              <button
                onClick={handleNextPage}
                disabled={!nextCursor}
                className="btn-neutral"
                style={{ fontSize: '12px', padding: '5px 12px', opacity: !nextCursor ? 0.4 : 1, cursor: !nextCursor ? 'not-allowed' : 'pointer' }}
                aria-label="Page suivante — jobs plus anciens"
                data-testid="page-next"
              >
                Older →
              </button>
            </div>
          </div>
        )}
      </div>

      {toast && <Toast message={toast} onDismiss={() => setToast(null)} />}

      {purgeIntent !== null && (
        <ConfirmModal
          title="Confirmer la purge définitive"
          message="Cette action supprimera définitivement toutes les notes en état Garbage ayant dépassé la période de grâce (30 jours par défaut). L'opération est irréversible. Exécuter quand même ?"
          confirmLabel="Purger définitivement"
          onConfirm={() => { void executePurge(false); }}
          onCancel={() => setPurgeIntent(null)}
        />
      )}
    </Layout>
  );
}

// ── Composant interne StatusChip ──────────────────────────────────────────────

interface StatusChipProps {
  label: string;
  count: number;
  active: boolean;
  colors: { color: string; bg: string; border: string };
  onClick: () => void;
  testId: string;
}

function StatusChip({ label, count, active, colors, onClick, testId }: StatusChipProps) {
  return (
    <button
      onClick={onClick}
      className="jobs-chip"
      style={{
        background: active ? colors.bg : '#fff',
        border: active ? `2px solid ${colors.color}` : `1px solid ${colors.border}`,
        padding: active ? '4px 11px' : '5px 12px',
        color: active ? colors.color : '#44423c',
        fontWeight: active ? 600 : 400,
        '--chip-color': colors.color,
      } as React.CSSProperties}
      aria-pressed={active}
      data-testid={testId}
    >
      <span className="jobs-chip-dot" aria-hidden="true" />
      {count} {label}
    </button>
  );
}
