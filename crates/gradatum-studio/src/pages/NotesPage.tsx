/**
 * NotesPage — /notes
 * vault_search, shape réelle vérifiée LIVE 2026-06-11
 *   Request : { query, limit, section?, status? } — PAS offset, PAS agent
 *   Response : { items: RawSearchHit[] }
 *   Pagination : client-side sur limit=200 (pas de curseur serveur disponible)
 *   Total : items.length, affiché "200+" si == limit (honnête)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useEffect, useState, useCallback } from 'react';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Layout } from '../components/Layout';
import { getBadgeStyle } from '../components/StatusBadge';
import { Toast } from '../components/Toast';
import type { SearchHit, NoteStatus, MoveRequest } from '../types/api';
import { parseSearchResponse } from '../lib/searchHit';
import { apiFetch } from '../hooks/useAuth';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

const PAGE_SIZE = 20;
const FETCH_LIMIT = 200;

const SECTIONS = [
  'decisions', 'architecture', 'debug', 'experiments',
  'lessons-learned', 'agent-issues', 'reference', 'council',
  'retrospectives', 'reasoning',
];

export function NotesPage() {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();

  const [allNotes, setAllNotes] = useState<SearchHit[]>([]);
  const [page, setPage] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saturated, setSaturated] = useState(false);

  const [query, setQuery] = useState('');
  const [filterStatus, setFilterStatus] = useState<string>(searchParams.get('status') ?? 'all');
  const [filterSection, setFilterSection] = useState('all');

  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [moveTarget, setMoveTarget] = useState('knowledge/');
  const [toast, setToast] = useState<string | null>(null);

  const fetchNotes = useCallback(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    const body: Record<string, unknown> = {
      query: query || '*',
      limit: FETCH_LIMIT,
    };
    if (filterStatus !== 'all') body['status'] = filterStatus;
    if (filterSection !== 'all') body['section'] = filterSection;

    apiFetch('/api/v1/vault_search', {
      method: 'POST',
      body: JSON.stringify(body),
    })
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const data = await res.json() as unknown;
        if (!cancelled) {
          const hits = parseSearchResponse(data);
          setAllNotes(hits);
          setSaturated(hits.length >= FETCH_LIMIT);
        }
      })
      .catch(err => {
        if (!cancelled) setError(String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  }, [query, filterStatus, filterSection]);

  useEffect(() => {
    return fetchNotes();
  }, [fetchNotes]);

  useEffect(() => { setPage(0); }, [query, filterStatus, filterSection]);

  const totalPages = Math.ceil(allNotes.length / PAGE_SIZE);
  const notes = allNotes.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
  const totalLabel = saturated ? `${allNotes.length}+` : String(allNotes.length);

  const toggleSelect = (ulid: string, e: React.MouseEvent) => {
    e.stopPropagation();
    setSelected(prev => {
      const next = new Set(prev);
      next.has(ulid) ? next.delete(ulid) : next.add(ulid);
      return next;
    });
  };

  const applyMove = async () => {
    if (!moveTarget.trim() || selected.size === 0) return;
    const ids = Array.from(selected);
    let ok = 0;
    for (const id of ids) {
      const body: MoveRequest = { locus: moveTarget.trim() };
      const res = await apiFetch(`/api/v1/notes/${id}/move`, {
        method: 'POST',
        body: JSON.stringify(body),
      });
      if (res.ok) ok++;
    }
    setSelected(new Set());
    setToast(`Moved ${ok}/${ids.length} note(s) to ${moveTarget}`);
    fetchNotes();
  };

  return (
    <Layout title="Notes" subtitle={`${totalLabel} notes`}>
      <div className="studio-page-notes" data-testid="notes-page">
        {/* Barre de filtres */}
        <div className="filter-bar">
          <div className="filter-search-wrap">
            <svg width="14" height="14" viewBox="0 0 15 15" fill="none" aria-hidden="true">
              <circle cx="6.5" cy="6.5" r="4.75" stroke="#8a857c" strokeWidth="1.5" />
              <path d="M10 10L13.5 13.5" stroke="#8a857c" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
            <input
              type="search"
              value={query}
              onChange={e => setQuery(e.target.value)}
              placeholder="Filter notes by text…"
              aria-label="Filter notes by text"
              className="filter-search-input"
              data-testid="notes-search"
            />
          </div>

          <select
            value={filterStatus}
            onChange={e => setFilterStatus(e.target.value)}
            aria-label="Filter by status"
            className="filter-select"
            data-testid="filter-status"
          >
            <option value="all">state: all</option>
            <option value="live">live</option>
            <option value="staging">staging</option>
            <option value="pending-review">pending-review</option>
            <option value="draft">draft</option>
            <option value="deprecated">deprecated</option>
            <option value="garbage">garbage</option>
          </select>

          <select
            value={filterSection}
            onChange={e => setFilterSection(e.target.value)}
            aria-label="Filter by section"
            className="filter-select"
          >
            <option value="all">§ all sections</option>
            {SECTIONS.map(s => (
              <option key={s} value={s}>{s}</option>
            ))}
          </select>
        </div>

        {/* Zone de sélection multiple */}
        {selected.size > 0 && (
          <div className="bulk-actions-bar" data-testid="bulk-actions">
            <span className="bulk-selected-count">{selected.size} selected</span>
            <span className="bulk-separator" />
            <span className="bulk-label">Move to locus</span>
            <input
              type="text"
              value={moveTarget}
              onChange={e => setMoveTarget(e.target.value)}
              aria-label="Target locus for bulk move"
              className="bulk-move-input"
              data-testid="bulk-move-input"
            />
            <button
              onClick={applyMove}
              className="btn-primary"
              style={{ padding: '6px 14px', fontSize: '12.5px' }}
              data-testid="bulk-apply"
            >
              Apply
            </button>
            <button
              onClick={() => setSelected(new Set())}
              className="bulk-clear-btn"
              data-testid="bulk-clear"
            >
              Clear
            </button>
          </div>
        )}

        {/* Table */}
        <div className="card" style={{ overflow: 'hidden' }}>
          <div className="table-header notes-table-cols">
            <span />
            <span>Note</span>
            <span>Section</span>
            <span>State</span>
            <span>ID</span>
          </div>

          {loading && (
            <div className="loading-text" style={{ padding: '20px' }}>Loading…</div>
          )}
          {error && (
            <div role="alert" className="error-inline" style={{ padding: '16px 18px' }}>
              {error}
            </div>
          )}
          {!loading && !error && notes.length === 0 && (
            <div
              className="loading-text"
              style={{ padding: '24px 18px', color: 'var(--color-text-faint)' }}
              data-testid="notes-empty"
            >
              No notes match the current filters.
            </div>
          )}

          {notes.map(note => {
            const isSelected = selected.has(note.ulid);
            return (
              <div
                key={note.ulid}
                className="table-row notes-table-cols"
                onClick={() => navigate(`/notes/${note.ulid}`)}
                data-testid={`note-row-${note.ulid}`}
              >
                {/* Checkbox — couleurs dynamiques via CSS custom properties */}
                <button
                  onClick={e => toggleSelect(note.ulid, e)}
                  aria-label={`Select note ${note.title ?? note.ulid}`}
                  className="note-checkbox"
                  style={{
                    '--checkbox-border': isSelected ? '#2a5db0' : '#c5c1b8',
                    '--checkbox-bg': isSelected ? '#2a5db0' : '#fff',
                  } as React.CSSProperties}
                  data-testid={`checkbox-${note.ulid}`}
                >
                  {isSelected ? '✓' : ''}
                </button>

                {/* Titre */}
                <span className="note-title-cell">
                  <span className="note-title-text">{note.title ?? '(untitled)'}</span>
                </span>

                {/* Section */}
                <span className="note-section-cell">§ {note.section}</span>

                {/* Badge status */}
                <span>
                  <span style={getBadgeStyle(note.status as NoteStatus)}>
                    {note.status === 'pending-review' ? 'REVIEW'
                     : note.status === 'downgraded' ? 'DEPRECATED'
                     : note.status.toUpperCase()}
                  </span>
                  {note.forgotten && (
                    <span
                      style={{
                        ...getBadgeStyle('forgotten'),
                        marginLeft: '4px',
                        textTransform: 'lowercase',
                      }}
                    >
                      forgotten
                    </span>
                  )}
                </span>

                {/* ULID court */}
                <span className="note-ulid-cell" title={note.ulid}>
                  {note.ulid.slice(0, 10)}…
                </span>
              </div>
            );
          })}

          {/* Pagination */}
          <div className="pagination">
            <span>
              {totalLabel} note{allNotes.length !== 1 ? 's' : ''}
              {saturated && (
                <span className="pagination-limit-note">
                  (limit {FETCH_LIMIT} reached — refine filters)
                </span>
              )}
            </span>
            <div style={{ display: 'flex', gap: '8px', fontFamily: "'JetBrains Mono', monospace", fontSize: '11.5px' }}>
              <button
                onClick={() => setPage(p => Math.max(0, p - 1))}
                disabled={page === 0}
                className="pagination-btn"
                data-testid="prev-page"
              >
                ← prev
              </button>
              <button
                onClick={() => setPage(p => Math.min(totalPages - 1, p + 1))}
                disabled={page >= totalPages - 1 || totalPages === 0}
                className="pagination-btn"
                data-testid="next-page"
              >
                next →
              </button>
            </div>
          </div>
        </div>
      </div>

      {toast && <Toast message={toast} onDismiss={() => setToast(null)} />}
    </Layout>
  );
}
