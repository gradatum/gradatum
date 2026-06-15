/**
 * flattenHit — mapping RawSearchHit → SearchHit
 * path "section/ULID26" → { section, ulid }
 * status: from hit.status if present, else fallback 'live'
 * forgotten: not provided by vault_search — always false
 */

import type { RawSearchHit, SearchHit, NoteStatus } from '../types/api';

const KNOWN_STATUSES = new Set<string>([
  'live', 'staging', 'pending-review', 'draft',
  'deprecated', 'garbage', 'downgraded',
]);

function coerceStatus(raw: string | undefined): NoteStatus {
  if (raw && KNOWN_STATUSES.has(raw)) return raw as NoteStatus;
  return 'live'; // fallback conservateur
}

/** Splitte "section/ULID26" → { section, ulid }. Défensif. */
function splitPath(path: string): { section: string; ulid: string } {
  const idx = path.indexOf('/');
  if (idx === -1) return { section: path, ulid: path };
  return { section: path.slice(0, idx), ulid: path.slice(idx + 1) };
}

export function flattenHit(raw: RawSearchHit): SearchHit {
  const { section, ulid } = splitPath(raw.path ?? '');
  return {
    ulid,
    section,
    path: raw.path,
    score: raw.score ?? 0,
    title: raw.title ?? null,
    snippet: raw.snippet ?? '',
    status: coerceStatus(raw.status),
    forgotten: false,
    scores: raw.scores,
  };
}

/** Parse la réponse entière, défensif sur items manquant */
export function parseSearchResponse(data: unknown): SearchHit[] {
  if (!data || typeof data !== 'object') return [];
  const d = data as { items?: unknown };
  if (!Array.isArray(d.items)) return [];
  return d.items.map(h => flattenHit(h as RawSearchHit));
}
