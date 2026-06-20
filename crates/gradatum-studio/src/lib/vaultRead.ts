/**
 * vaultRead.ts — mapping POST /api/v1/vault_read → NoteDetail normalisé
 *
 * Contrat réel vérifiée LIVE 2026-06-11 :
 *   POST body : { path: "<ULID>" }  — deny_unknown_fields
 *   Réponse   : { path, content, metadata, size_bytes, sha256 }
 *   PAS de GET /{id} — 404 systématique sur cette forme
 *
 * flattenVaultRead() produit un NoteDetail compatible avec NoteDetailPage
 * (actions PATCH status / POST move) sans modifier leurs call sites.
 */

import type { VaultReadResponse, NoteDetail, NoteStatus } from '../types/api';

const VALID_STATUSES: NoteStatus[] = [
  'live', 'staging', 'pending-review', 'draft', 'deprecated', 'garbage', 'downgraded',
];

function coerceStatus(s: string | undefined): NoteStatus {
  if (s && VALID_STATUSES.includes(s as NoteStatus)) return s as NoteStatus;
  return 'live';
}

/**
 * Extrait le bloc frontmatter (--- ... ---) en tête du contenu.
 * Retourne { frontmatter, bodyAfter } — bodyAfter = contenu sans le bloc.
 */
function extractFrontmatter(content: string): { frontmatter: string; bodyAfter: string } {
  const lines = content.split('\n');
  if (lines[0]?.trim() !== '---') {
    return { frontmatter: '', bodyAfter: content };
  }
  let closeIdx = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i].trim() === '---') { closeIdx = i; break; }
  }
  if (closeIdx === -1) {
    return { frontmatter: '', bodyAfter: content };
  }
  const frontmatter = lines.slice(0, closeIdx + 1).join('\n');
  const bodyAfter = lines.slice(closeIdx + 1).join('\n').trimStart();
  return { frontmatter, bodyAfter };
}

/**
 * Extrait le titre depuis la première ligne "# …" du corps (sans frontmatter).
 */
function extractTitle(body: string): string | null {
  const firstLine = body.split('\n')[0]?.trim() ?? '';
  if (firstLine.startsWith('# ')) return firstLine.slice(2).trim() || null;
  return null;
}

/**
 * Mappe VaultReadResponse → NoteDetail normalisé.
 */
export function flattenVaultRead(raw: VaultReadResponse): NoteDetail {
  const { frontmatter } = extractFrontmatter(raw.content ?? '');
  const title = extractTitle(
    raw.content?.replace(/^---[\s\S]*?---\n?/m, '').trimStart() ?? '',
  );
  return {
    ulid: raw.path ?? '',
    title,
    body: raw.content ?? '',       // markdown complet, frontmatter inclus
    frontmatter,
    section: raw.metadata?.section ?? '',
    locus: null,                   // non fourni par vault_read
    kind: 'note',
    status: coerceStatus(raw.metadata?.status),
    forgotten: false,
    agent: raw.metadata?.author ?? null,
    created_at: raw.metadata?.created
      ? new Date(raw.metadata.created).toISOString()
      : '',
    updated_at: raw.metadata?.updated
      ? new Date(raw.metadata.updated).toISOString()
      : '',
  };
}

/**
 * Appel POST /api/v1/vault_read avec le bon contrat.
 * Retourne null si la réponse n'est pas un objet valide.
 */
export function parseVaultReadResponse(data: unknown): VaultReadResponse | null {
  if (!data || typeof data !== 'object') return null;
  const d = data as Record<string, unknown>;
  if (typeof d['path'] !== 'string' || typeof d['content'] !== 'string') return null;
  return d as unknown as VaultReadResponse;
}
