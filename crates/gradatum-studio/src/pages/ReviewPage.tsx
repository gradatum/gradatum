/**
 * ReviewPage — /review
 * GET /api/v1/review → approve (POST /move + PATCH status=live) / reject (PATCH garbage)
 * Copie honnête : "No confidence score available yet."
 * Badge staging legacy distinct
 * Source : s02-ux-normatif.md §5.5 + contrats s1-contrats-endpoints.md
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useEffect, useState, useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { Layout } from '../components/Layout';
import { StatusBadge } from '../components/StatusBadge';
import { MarkdownBody } from '../components/MarkdownBody';
import { Toast } from '../components/Toast';
import type { ReviewItem } from '../types/api';
import { flattenVaultRead, parseVaultReadResponse } from '../lib/vaultRead';
import { apiFetch } from '../hooks/useAuth';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

const LOCUS_SUGGESTIONS = [
  'knowledge/',
  'decisions/',
  'architecture/',
  'debug/',
  'lessons-learned/',
  'reference/',
];

export function ReviewPage() {
  const navigate = useNavigate();
  const [items, setItems] = useState<ReviewItem[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [processingId, setProcessingId] = useState<string | null>(null);

  const [destinations, setDestinations] = useState<Record<string, string>>({});

  const [expandedBody, setExpandedBody] = useState<Record<string, string | 'loading' | 'error'>>({});

  const toggleExpand = useCallback((item: ReviewItem) => {
    const current = expandedBody[item.ulid];
    if (current !== undefined) {
      setExpandedBody(prev => {
        const next = { ...prev };
        delete next[item.ulid];
        return next;
      });
      return;
    }
    setExpandedBody(prev => ({ ...prev, [item.ulid]: 'loading' }));
    apiFetch('/api/v1/vault_read', {
      method: 'POST',
      body: JSON.stringify({ path: item.ulid }),
    })
      .then(async res => {
        if (!res.ok) {
          setExpandedBody(prev => ({ ...prev, [item.ulid]: 'error' }));
          return;
        }
        const data = await res.json() as unknown;
        const raw = parseVaultReadResponse(data);
        const content = raw ? flattenVaultRead(raw).body : '';
        setExpandedBody(prev => ({ ...prev, [item.ulid]: content }));
      })
      .catch(() => {
        setExpandedBody(prev => ({ ...prev, [item.ulid]: 'error' }));
      });
  }, [expandedBody]);

  const fetchReview = useCallback(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    apiFetch('/api/v1/review?limit=20')
      .then(async res => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const json = (await res.json()) as { items: ReviewItem[]; total: number };
        if (!cancelled) {
          setItems(json.items);
          setTotal(json.total);
          const dests: Record<string, string> = {};
          json.items.forEach(it => {
            dests[it.ulid] = it.locus ?? 'knowledge/';
          });
          setDestinations(dests);
        }
      })
      .catch(err => {
        if (!cancelled) setError(String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    return fetchReview();
  }, [fetchReview]);

  const approve = async (item: ReviewItem) => {
    setProcessingId(item.ulid);
    const dest = destinations[item.ulid] ?? item.locus ?? 'knowledge/';
    if (dest && dest !== item.locus) {
      const moveRes = await apiFetch(`/api/v1/notes/${item.ulid}/move`, {
        method: 'POST',
        body: JSON.stringify({ locus: dest }),
      });
      if (!moveRes.ok && moveRes.status !== 204) {
        setToast(`Move failed: HTTP ${moveRes.status}`);
        setProcessingId(null);
        return;
      }
    }
    const patchRes = await apiFetch(`/api/v1/notes/${item.ulid}`, {
      method: 'PATCH',
      body: JSON.stringify({ status: 'live' }),
    });
    setProcessingId(null);
    if (patchRes.ok) {
      setToast(`Approved: "${item.title ?? item.ulid}"`);
      setItems(prev => prev.filter(i => i.ulid !== item.ulid));
      setTotal(t => t - 1);
    } else {
      setToast(`Approve failed: HTTP ${patchRes.status}`);
    }
  };

  const reject = async (item: ReviewItem) => {
    setProcessingId(item.ulid);
    const res = await apiFetch(`/api/v1/notes/${item.ulid}`, {
      method: 'PATCH',
      body: JSON.stringify({ status: 'garbage' }),
    });
    setProcessingId(null);
    if (res.ok) {
      setToast(`Rejected: "${item.title ?? item.ulid}"`);
      setItems(prev => prev.filter(i => i.ulid !== item.ulid));
      setTotal(t => t - 1);
    } else {
      setToast(`Reject failed: HTTP ${res.status}`);
    }
  };

  function timeAgo(ms: number): string {
    const diff = Date.now() - ms;
    const s = Math.floor(diff / 1000);
    if (s < 60) return `${s}s ago`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}min ago`;
    return `${Math.floor(m / 60)}h ago`;
  }

  return (
    <Layout
      title="Review"
      subtitle={`${total} note${total !== 1 ? 's' : ''} awaiting review`}
      reviewCount={total}
    >
      <div className="studio-page-review" data-testid="review-page">
        {loading && <div className="loading-text">Loading…</div>}
        {error && (
          <div role="alert" className="error-inline">{error}</div>
        )}

        {/* État vide */}
        {!loading && !error && items.length === 0 && (
          <div className="card review-empty-card" data-testid="review-empty">
            {/* SVG check — pas d'emoji */}
            <div className="review-empty-icon" aria-hidden="true">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                <path d="M5 13l4 4L19 7" stroke="#15803d" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </div>
            <div className="review-empty-title">Inbox clear</div>
            <div className="review-empty-sub">
              No notes awaiting human review. The curator will route low-confidence notes here.
            </div>
          </div>
        )}

        {/* Items */}
        {items.map(item => {
          const isProcessing = processingId === item.ulid;
          const isStagingLegacy = item.status === 'staging';
          return (
            <div
              key={item.ulid}
              className="card"
              style={{ overflow: 'hidden', opacity: isProcessing ? 0.6 : 1 }}
              data-testid={`review-item-${item.ulid}`}
            >
              <div className="review-item-header">
                <div className="review-item-title-row">
                  {/* Titre cliquable → NoteDetailPage */}
                  <button
                    onClick={() => navigate(`/notes/${item.ulid}`)}
                    className="review-item-title-btn"
                    data-testid={`title-link-${item.ulid}`}
                  >
                    {item.title ?? '(untitled)'}
                  </button>

                  {/* Badge staging legacy distinct */}
                  {isStagingLegacy ? (
                    <StatusBadge status="staging" />
                  ) : (
                    <StatusBadge status="pending-review" />
                  )}

                  {/* Copie honnête — pas de confidence non persistée */}
                  <span className="review-confidence-badge">
                    No confidence score available yet.
                  </span>

                  <span className="review-item-locus-time">
                    {item.locus ?? 'inbox/'} · {timeAgo(item.created_ms)}
                  </span>

                  {/* Bouton Expand ▾ / Collapse ▴ */}
                  <button
                    onClick={() => toggleExpand(item)}
                    className="review-expand-btn"
                    data-testid={`expand-${item.ulid}`}
                    aria-expanded={expandedBody[item.ulid] !== undefined}
                  >
                    {expandedBody[item.ulid] !== undefined ? 'Collapse ▴' : 'Expand ▾'}
                  </button>
                </div>

                {/* Provenance */}
                {item.provenance && (
                  <div className="review-provenance">
                    provenance: {item.provenance}
                    {isStagingLegacy && (
                      <span className="review-legacy-note">(staging legacy)</span>
                    )}
                  </div>
                )}

                {/* Section */}
                <div className="review-section-label">§ {item.section}</div>
              </div>

              {/* Corps expandable — chargé lazily via vault_read */}
              {expandedBody[item.ulid] !== undefined && (
                <div
                  className="review-expanded-body"
                  data-testid={`body-expanded-${item.ulid}`}
                >
                  {expandedBody[item.ulid] === 'loading' && (
                    <div className="loading-text" style={{ paddingTop: '12px' }}>Loading…</div>
                  )}
                  {expandedBody[item.ulid] === 'error' && (
                    <div
                      role="alert"
                      className="error-inline"
                      style={{ paddingTop: '12px' }}
                    >
                      Failed to load note content.
                    </div>
                  )}
                  {expandedBody[item.ulid] !== 'loading' && expandedBody[item.ulid] !== 'error' && (
                    <MarkdownBody
                      content={expandedBody[item.ulid] as string}
                      style={{ paddingTop: '14px', maxHeight: '420px', overflowY: 'auto' }}
                    />
                  )}
                </div>
              )}

              {/* Footer actions */}
              <div className="review-footer">
                <label htmlFor={`dest-${item.ulid}`} className="review-footer-label">
                  Move to:
                </label>
                <select
                  id={`dest-${item.ulid}`}
                  value={destinations[item.ulid] ?? ''}
                  onChange={e =>
                    setDestinations(prev => ({ ...prev, [item.ulid]: e.target.value }))
                  }
                  className="review-dest-select"
                  data-testid={`dest-select-${item.ulid}`}
                >
                  {LOCUS_SUGGESTIONS.map(l => (
                    <option key={l} value={l}>{l}</option>
                  ))}
                </select>

                <button
                  onClick={() => approve(item)}
                  disabled={isProcessing}
                  className="btn-primary"
                  style={{ padding: '7px 16px', fontSize: '12.5px' }}
                  data-testid={`approve-${item.ulid}`}
                >
                  Approve →
                </button>
                <button
                  onClick={() => reject(item)}
                  disabled={isProcessing}
                  className="btn-danger"
                  style={{ fontSize: '12.5px' }}
                  data-testid={`reject-${item.ulid}`}
                >
                  Reject
                </button>
                <button
                  onClick={() => navigate(`/notes/${item.ulid}`)}
                  className="btn-neutral"
                  style={{ fontSize: '12.5px' }}
                  data-testid={`edit-${item.ulid}`}
                >
                  Edit
                </button>
              </div>
            </div>
          );
        })}
      </div>

      {toast && <Toast message={toast} onDismiss={() => setToast(null)} />}
    </Layout>
  );
}
