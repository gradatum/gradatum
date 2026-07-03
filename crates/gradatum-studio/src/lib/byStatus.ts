/**
 * parseByStatusResponse — mappe la réponse de GET /api/v1/notes/by-status
 * vers SearchHit[] (réutilise le render existant de NotesPage).
 * Source métadonnées : inclut les notes downgraded (absentes du FTS5).
 */
import type { SearchHit, NoteStatus } from '../types/api';

const KNOWN_STATUSES = new Set<string>([
  'live', 'staging', 'pending-review', 'draft', 'deprecated', 'garbage', 'downgraded',
]);

function coerceStatus(raw: unknown): NoteStatus {
  if (typeof raw === 'string' && KNOWN_STATUSES.has(raw)) return raw as NoteStatus;
  return 'live';
}

interface RawByStatusEntry {
  ulid?: string;
  section?: string;
  title?: string | null;
  status?: string;
  snippet?: string;
}

export function parseByStatusResponse(data: unknown): SearchHit[] {
  if (!data || typeof data !== 'object') return [];
  const d = data as { entries?: unknown };
  if (!Array.isArray(d.entries)) return [];
  return d.entries.map((e: RawByStatusEntry) => {
    const ulid = e.ulid ?? '';
    const section = e.section ?? '';
    return {
      ulid,
      section,
      path: section && ulid ? `${section}/${ulid}` : ulid,
      score: 0,
      title: e.title ?? null,
      snippet: e.snippet ?? '',
      status: coerceStatus(e.status),
      forgotten: false,
      scores: undefined,
    } satisfies SearchHit;
  });
}
