/**
 * NoteDetailPage — /notes/:id
 * vault_read : POST /api/v1/vault_read { path: ulid }
 * Contrat réel vérifiée LIVE 2026-06-11 — PAS GET /{id} (→404)
 * Actions : PATCH /api/v1/notes/{ulid} + POST /api/v1/notes/{ulid}/move
 *           + POST /api/v1/jobs (Curate) — Re-curate cette note (F-16.3)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useEffect, useState, useCallback } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { Layout } from '../components/Layout';
import { StatusBadge } from '../components/StatusBadge';
import { MarkdownBody } from '../components/MarkdownBody';
import { Toast } from '../components/Toast';
import type { NoteDetail, NoteStatus } from '../types/api';
import { flattenVaultRead, parseVaultReadResponse } from '../lib/vaultRead';
import { apiFetch } from '../hooks/useAuth';
import { triggerCurate } from '../lib/jobs';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

const NOTE_STATUSES: NoteStatus[] = [
  'live', 'staging', 'pending-review', 'draft', 'deprecated', 'garbage',
];

function timeAgo(isoStr: string): string {
  const diff = Date.now() - new Date(isoStr).getTime();
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}min ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function NoteDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();

  const [note, setNote] = useState<NoteDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const [targetStatus, setTargetStatus] = useState<NoteStatus>('live');
  const [moveLocus, setMoveLocus] = useState('');
  const [actionLoading, setActionLoading] = useState(false);
  const [recurateLoading, setRecurateLoading] = useState(false);

  const fetchNote = useCallback(() => {
    if (!id) return;
    let cancelled = false;
    setLoading(true);
    setError(null);

    apiFetch('/api/v1/vault_read', {
      method: 'POST',
      body: JSON.stringify({ path: id }),
    })
      .then(async res => {
        if (res.status === 404) { navigate('/notes'); return; }
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const data = await res.json() as unknown;
        const raw = parseVaultReadResponse(data);
        if (!raw) throw new Error('Invalid vault_read response');
        const n = flattenVaultRead(raw);
        if (!cancelled) {
          setNote(n);
          setTargetStatus(n.status);
          setMoveLocus(n.locus ?? '');
        }
      })
      .catch(err => {
        if (!cancelled) setError(String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  }, [id, navigate]);

  useEffect(() => {
    return fetchNote();
  }, [fetchNote]);

  const applyStatus = async () => {
    if (!note) return;
    setActionLoading(true);
    const res = await apiFetch(`/api/v1/notes/${note.ulid}`, {
      method: 'PATCH',
      body: JSON.stringify({ status: targetStatus }),
    });
    setActionLoading(false);
    if (res.ok) {
      setToast(`Status updated to ${targetStatus}`);
      fetchNote();
    } else {
      setToast(`Error: HTTP ${res.status}`);
    }
  };

  const applyMove = async () => {
    if (!note || !moveLocus.trim()) return;
    setActionLoading(true);
    const res = await apiFetch(`/api/v1/notes/${note.ulid}/move`, {
      method: 'POST',
      body: JSON.stringify({ locus: moveLocus.trim() }),
    });
    setActionLoading(false);
    if (res.status === 204 || res.ok) {
      setToast(`Moved to ${moveLocus.trim()}`);
      fetchNote();
    } else if (res.status === 400) {
      setToast('Invalid locus path');
    } else if (res.status === 409) {
      setToast('Conflict: note at target locus already exists');
    } else {
      setToast(`Error: HTTP ${res.status}`);
    }
  };

  const downgradeNote = async () => {
    if (!note) return;
    setActionLoading(true);
    const res = await apiFetch('/api/v1/vault_downgrade', {
      method: 'POST',
      body: JSON.stringify({ note_id: note.ulid, reason: 'Manual downgrade via studio' }),
    });
    setActionLoading(false);
    if (res.ok) {
      setToast('Note downgraded');
      fetchNote();
    } else {
      setToast(`Error: HTTP ${res.status}`);
    }
  };

  const handleRecurate = async () => {
    if (!note) return;
    setRecurateLoading(true);
    const result = await triggerCurate(note.ulid);
    setRecurateLoading(false);
    if (result.ok) {
      setToast(`Curate job queued — id: ${result.id ?? '?'}`);
    } else {
      setToast(`Error: ${result.error ?? 'Unknown error'}`);
    }
  };

  if (loading) {
    return (
      <Layout title="Note" subtitle="Loading…">
        <div className="loading-text">Loading…</div>
      </Layout>
    );
  }

  if (error || !note) {
    return (
      <Layout title="Note" subtitle="Error">
        <div role="alert" className="error-inline">
          {error ?? 'Note not found'}
        </div>
      </Layout>
    );
  }

  return (
    <Layout
      title={note.title ?? '(untitled)'}
      subtitle={`${note.section} · ${note.locus ?? ''}`}
    >
      <div className="studio-page-note-detail" data-testid="note-detail-page">
        {/* Lien retour */}
        <button
          onClick={() => navigate('/notes')}
          className="back-link"
          data-testid="back-to-notes"
        >
          ← Notes
        </button>

        <div className="note-detail-layout">
          {/* Panneau gauche — contenu */}
          <div className="note-detail-left">
            {/* En-tête + corps */}
            <div className="card note-content-card">
              <div className="note-header-row">
                <h2 className="note-title-h2">{note.title ?? '(untitled)'}</h2>
                <StatusBadge status={note.status} forgotten={note.forgotten} />
              </div>

              <div className="note-meta-row">
                {note.locus && <span>{note.locus}</span>}
                <span>§ {note.section}</span>
                <span>kind: {note.kind}</span>
                {note.agent && <span>{note.agent} · {timeAgo(note.updated_at)}</span>}
              </div>

              <div className="note-divider" />

              <MarkdownBody content={note.body} />

              {/* Wikilinks */}
              {note.wikilinks && note.wikilinks.length > 0 && (
                <div className="wikilinks-row">
                  {note.wikilinks.map(link => (
                    <button
                      key={link}
                      onClick={() => navigate(`/search?q=${encodeURIComponent(link)}`)}
                      className="wikilink-btn"
                      data-testid={`wikilink-${link}`}
                    >
                      [[{link}]]
                    </button>
                  ))}
                </div>
              )}
            </div>

            {/* Frontmatter */}
            {note.frontmatter && (
              <div className="card" style={{ overflow: 'hidden' }}>
                <div className="frontmatter-header">Frontmatter</div>
                <div className="frontmatter-body" data-testid="frontmatter">
                  {note.frontmatter}
                </div>
              </div>
            )}
          </div>

          {/* Panneau droit — actions */}
          <div className="note-detail-right">
            {/* Actions */}
            <div className="card actions-card">
              <div className="actions-title">Actions</div>
              <div className="actions-fields">
                {/* Status */}
                <div className="action-row">
                  <label htmlFor="status-select" className="action-label">Status</label>
                  <select
                    id="status-select"
                    value={targetStatus}
                    onChange={e => setTargetStatus(e.target.value as NoteStatus)}
                    className="action-select"
                    data-testid="status-select"
                  >
                    {NOTE_STATUSES.map(s => (
                      <option key={s} value={s}>{s}</option>
                    ))}
                  </select>
                  <button
                    onClick={applyStatus}
                    disabled={actionLoading || targetStatus === note.status}
                    className="btn-primary"
                    style={{ padding: '6px 12px', fontSize: '12px' }}
                    data-testid="apply-status"
                  >
                    Set
                  </button>
                </div>

                {/* Move to locus */}
                <div className="action-row">
                  <label htmlFor="move-input" className="action-label">Move to</label>
                  <input
                    id="move-input"
                    type="text"
                    value={moveLocus}
                    onChange={e => setMoveLocus(e.target.value)}
                    placeholder="locus/path"
                    className="action-input"
                    data-testid="move-input"
                  />
                  <button
                    onClick={applyMove}
                    disabled={actionLoading || !moveLocus.trim() || moveLocus === note.locus}
                    className="btn-primary"
                    style={{ padding: '6px 12px', fontSize: '12px' }}
                    data-testid="apply-move"
                  >
                    Go
                  </button>
                </div>

                {/* Downgrade (soft-downgrade F-39, retire du FTS) */}
                {note.status !== 'downgraded' && note.status !== 'garbage' && (
                  <button
                    onClick={downgradeNote}
                    disabled={actionLoading}
                    className="downgrade-btn"
                    data-testid="downgrade-note"
                  >
                    Downgrade
                  </button>
                )}
              </div>

              {/* Séparateur */}
              <div style={{ height: '1px', background: 'var(--color-border-input)', margin: '8px 0' }} />

              {/* Re-curate */}
              <button
                onClick={handleRecurate}
                disabled={recurateLoading || actionLoading}
                className="btn-outline"
                style={{ width: '100%', fontSize: '12.5px', padding: '7px 14px' }}
                aria-label={`Re-curate la note ${note.ulid}`}
                data-testid="recurate-btn"
              >
                {recurateLoading ? 'Queuing…' : 'Re-curate cette note'}
              </button>
            </div>

            {/* Content history */}
            <div className="card history-card">
              <div className="card-title">Content history</div>
              {note.history && note.history.length > 0 ? (
                note.history.map(h => (
                  <div key={h.sha} className="history-row" data-testid={`history-${h.sha}`}>
                    <span className="history-sha">{h.sha.slice(0, 8)}</span>
                    <span>{timeAgo(h.created_at)}</span>
                    <span className="history-author">{h.author}</span>
                  </div>
                ))
              ) : (
                <span className="card-empty-text">No content history.</span>
              )}
            </div>

            {/* Backlinks */}
            <div className="card backlinks-card">
              <div className="card-title">Backlinks</div>
              {note.backlinks && note.backlinks.length > 0 ? (
                note.backlinks.map(b => (
                  <button
                    key={b.ulid}
                    onClick={() => navigate(`/notes/${b.ulid}`)}
                    className="link-accent"
                    style={{ fontSize: '12.5px', lineHeight: 1.5, textAlign: 'left' }}
                    data-testid={`backlink-${b.ulid}`}
                  >
                    ← {b.title ?? b.ulid}
                  </button>
                ))
              ) : (
                <span className="card-empty-text">No backlinks yet.</span>
              )}
            </div>

            {/* Agent runs */}
            <div className="card agent-runs-card">
              <div className="card-title">Agent runs</div>
              {note.agent_runs && note.agent_runs.length > 0 ? (
                note.agent_runs.map((r, i) => (
                  <div key={i} className="agent-run-row">
                    <span className="agent-run-id">{r.job_id}</span>
                    <span>{r.agent}</span>
                    <span className="agent-run-time">{timeAgo(r.created_at)}</span>
                  </div>
                ))
              ) : (
                <span className="card-empty-text">No agent runs yet.</span>
              )}
            </div>
          </div>
        </div>
      </div>

      {toast && <Toast message={toast} onDismiss={() => setToast(null)} />}
    </Layout>
  );
}
