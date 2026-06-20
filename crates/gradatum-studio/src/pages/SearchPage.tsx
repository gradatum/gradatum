/**
 * SearchPage — /search
 * POST /api/v1/vault_search (shape réelle vérifiée LIVE 2026-06-11)
 *   Request : { query, limit, section?, locus?, include_scores?, status? }
 *   Response : { items: RawSearchHit[] }  — pas de hits/total/elapsed_ms
 *   Hit : { path: "section/ULID26", score, title, snippet, trust(ignore), scores? }
 * Toggle Scored / Synthetic answer
 * Panneau WHY 2 niveaux, amendement A1 (no rerank)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useState, useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { Layout } from '../components/Layout';
import { StatusBadge } from '../components/StatusBadge';
import { ScorePanel, WhyPanel } from '../components/ScoreBreakdown';
import type { SearchHit } from '../types/api';
import { parseSearchResponse } from '../lib/searchHit';
import { apiFetch } from '../hooks/useAuth';

// D3.3 : onUnauthorized retiré — intercepteur centralisé dans apiFetch (useAuth)

type SearchMode = 'scored' | 'synthetic';

export function SearchPage() {
  const navigate = useNavigate();
  const [query, setQuery] = useState('');
  const [mode, setMode] = useState<SearchMode>('scored');
  const [results, setResults] = useState<SearchHit[]>([]);
  const [resultCount, setResultCount] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [openWhy, setOpenWhy] = useState<Set<string>>(new Set());

  const doSearch = useCallback(async (q: string) => {
    if (!q.trim()) return;
    setLoading(true);
    setError(null);
    setResults([]);
    setResultCount(null);
    try {
      const res = await apiFetch('/api/v1/vault_search', {
        method: 'POST',
        body: JSON.stringify({
          query: q,
          limit: 20,
          include_scores: true,
        }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json() as unknown;
      const hits = parseSearchResponse(data);
      setResults(hits);
      setResultCount(hits.length);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') doSearch(query);
  };

  const toggleWhy = (ulid: string) => {
    setOpenWhy(prev => {
      const next = new Set(prev);
      next.has(ulid) ? next.delete(ulid) : next.add(ulid);
      return next;
    });
  };

  const garbageCount = results.filter(r => r.status === 'garbage').length;

  return (
    <Layout title="Search" subtitle="Vault full-text + semantic search">
      <div className="studio-page-search" data-testid="search-page">
        {/* Barre de recherche */}
        <div className="search-bar-wrap">
          <div className="search-input-wrap">
            <svg width="15" height="15" viewBox="0 0 15 15" fill="none" aria-hidden="true">
              <circle cx="6.5" cy="6.5" r="4.75" stroke="#8a857c" strokeWidth="1.5" />
              <path d="M10 10L13.5 13.5" stroke="#8a857c" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
            <input
              type="search"
              value={query}
              onChange={e => setQuery(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Search the vault…"
              aria-label="Search the vault"
              className="search-input"
              data-testid="search-input"
            />
          </div>

          {/* Mode toggle */}
          <div className="search-mode-toggle" role="group" aria-label="Search mode">
            <button
              onClick={() => setMode('scored')}
              className={`search-mode-btn ${mode === 'scored' ? 'search-mode-btn--active' : 'search-mode-btn--inactive'}`}
              aria-pressed={mode === 'scored'}
              data-testid="mode-scored"
            >
              Scored results
            </button>
            <button
              onClick={() => setMode('synthetic')}
              className={`search-mode-btn ${mode === 'synthetic' ? 'search-mode-btn--active' : 'search-mode-btn--inactive'}`}
              aria-pressed={mode === 'synthetic'}
              data-testid="mode-synthetic"
            >
              Synthetic answer
            </button>
          </div>
        </div>

        {error && (
          <div role="alert" className="error-inline">Search failed: {error}</div>
        )}

        {loading && (
          <div className="loading-text">Searching…</div>
        )}

        {/* Mode Scored */}
        {mode === 'scored' && !loading && results.length > 0 && (
          <>
            <div className="search-result-meta-row">
              <div className="search-result-count">
                {resultCount} result{resultCount !== 1 ? 's' : ''}
              </div>
              {garbageCount > 0 && (
                <div className="search-garbage-note">
                  {garbageCount} GARBAGE note{garbageCount !== 1 ? 's' : ''} matched
                </div>
              )}
            </div>

            <div className="search-results-list">
              {results.map((r, idx) => {
                const isGarbage = r.status === 'garbage';
                return (
                  <div
                    key={r.ulid}
                    className={`search-result-card${isGarbage ? ' search-result-card--garbage' : ' search-result-card--clickable'}`}
                    onClick={() => !isGarbage && navigate(`/notes/${r.ulid}`)}
                    data-testid={`search-result-${r.ulid}`}
                  >
                    {/* Rang */}
                    <div className="search-result-rank">#{idx + 1}</div>

                    {/* Contenu */}
                    <div className="search-result-body">
                      <div className="search-result-title-row">
                        <div className={`search-result-title${isGarbage ? ' search-result-title--garbage' : ''}`}>
                          {r.title ?? '(untitled)'}
                        </div>
                        <StatusBadge status={r.status} forgotten={r.forgotten} />
                      </div>
                      <div className="search-result-snippet">{r.snippet}</div>
                      <div className="search-result-footer tabular">
                        <span>§ {r.section}</span>
                        <span className="search-result-ulid" title={r.path}>
                          {r.ulid.slice(0, 10)}…
                        </span>
                      </div>

                      {/* WHY panel (A1 : pas de rerank) */}
                      {r.scores && (
                        <div>
                          <button
                            onClick={e => { e.stopPropagation(); toggleWhy(r.ulid); }}
                            className="why-formula-btn"
                            aria-expanded={openWhy.has(r.ulid)}
                            data-testid={`why-btn-${r.ulid}`}
                          >
                            {openWhy.has(r.ulid) ? 'Hide why ▴' : 'Why? ▾'}
                          </button>
                          {openWhy.has(r.ulid) && <WhyPanel scores={r.scores} />}
                        </div>
                      )}
                    </div>

                    {/* Score panel */}
                    {r.scores && <ScorePanel scores={r.scores} />}
                  </div>
                );
              })}
            </div>
          </>
        )}

        {/* Mode Scored — empty state après recherche */}
        {mode === 'scored' && !loading && results.length === 0 && query && !error && (
          <div className="search-no-results">No results for "{query}".</div>
        )}

        {/* Mode Synthetic — message honnête */}
        {mode === 'synthetic' && (
          <div className="synthetic-card" data-testid="synthetic-unavailable">
            <div className="synthetic-title">
              Synthetic answers not available in this version.
            </div>
            The{' '}
            <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: '12px' }}>
              vault_ask
            </span>{' '}
            endpoint is planned for a future release. Use{' '}
            <button
              onClick={() => setMode('scored')}
              className="link-accent"
              style={{ fontSize: '13.5px' }}
            >
              Scored results
            </button>{' '}
            to search the vault directly.
          </div>
        )}
      </div>
    </Layout>
  );
}
