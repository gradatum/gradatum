/**
 * vaultRead.test.ts — shape réelle POST /api/v1/vault_read vérifiée LIVE 2026-06-11
 *
 * Fixture issue du curl LIVE sur ULID 01KTT56A6T46E9T07EJ2N2T8KW :
 *   { path, content (markdown), metadata { author, created, section, status, tags, updated, vault_id }, size_bytes, sha256 }
 *   PAS de title/body/locus/kind séparés — contrat inventé qui causait 404 systématique.
 */

import { describe, it, expect } from 'vitest';
import { flattenVaultRead, parseVaultReadResponse } from './vaultRead';
import type { VaultReadResponse } from '../types/api';

// ── Fixture réelle LIVE (curl 2026-06-11) ────────────────────────────────────

const LIVE_RESPONSE: VaultReadResponse = {
  path: '01KTT56A6T46E9T07EJ2N2T8KW',
  content: '## Milestone\ngradatum v0.4.4 « distillation » — LIVRÉ LIVE INTERNE 2026-06-11.\n\n## Livré\n- **F-19** events sémantiques\n- **F-22** Distill Semantic\n\n## Tags\n[gradatum, v0.4.4, milestone]',
  metadata: {
    author: 'main-agent',
    created: 1781141809543,
    section: 'retrospectives',
    status: 'live',
    tags: ['gradatum', 'milestone', 'retro', 'run-autonome'],
    updated: 1781141809544,
    vault_id: 'main',
  },
  size_bytes: 2946,
  sha256: '6a941e0d77d94a7e5f0a7cc4ddb93608da2278fe0b9f8a5703a8e440061654bc',
};

// Fixture avec frontmatter en tête
const WITH_FRONTMATTER: VaultReadResponse = {
  ...LIVE_RESPONSE,
  path: '01KTS000000000000000000001',
  content: '---\nname: test-note\ntags: [foo, bar]\n---\n# Mon titre\n\nCorps de la note.',
};

// ── parseVaultReadResponse ───────────────────────────────────────────────────

describe('parseVaultReadResponse', () => {
  it('parse la shape réelle LIVE', () => {
    const result = parseVaultReadResponse(LIVE_RESPONSE);
    expect(result).not.toBeNull();
    expect(result?.path).toBe('01KTT56A6T46E9T07EJ2N2T8KW');
    expect(result?.content).toContain('Milestone');
  });

  it('retourne null sur null', () => {
    expect(parseVaultReadResponse(null)).toBeNull();
  });

  it('retourne null si path manquant', () => {
    const bad = { ...LIVE_RESPONSE, path: undefined };
    expect(parseVaultReadResponse(bad)).toBeNull();
  });

  it('retourne null si content manquant', () => {
    const bad = { ...LIVE_RESPONSE, content: undefined };
    expect(parseVaultReadResponse(bad)).toBeNull();
  });

  it('retourne null sur string', () => {
    expect(parseVaultReadResponse('raw text')).toBeNull();
  });

  it('retourne null sur ancien contrat GET (objet title+body séparés)', () => {
    // Vérifie qu'un éventuel résidu de l'ancien contrat inventé ne passe pas
    const oldShape = { ulid: 'abc', title: 'T', body: 'B', section: 'decisions', status: 'live' };
    expect(parseVaultReadResponse(oldShape)).toBeNull();
  });
});

// ── flattenVaultRead ─────────────────────────────────────────────────────────

describe('flattenVaultRead — fixture LIVE', () => {
  it('ulid = path', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.ulid).toBe('01KTT56A6T46E9T07EJ2N2T8KW');
  });

  it('section depuis metadata.section', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.section).toBe('retrospectives');
  });

  it('status coercé depuis metadata.status', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.status).toBe('live');
  });

  it('agent depuis metadata.author', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.agent).toBe('main-agent');
  });

  it('created_at ISO depuis metadata.created ms', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.created_at).toBe(new Date(1781141809543).toISOString());
  });

  it('updated_at ISO depuis metadata.updated ms', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.updated_at).toBe(new Date(1781141809544).toISOString());
  });

  it('body = content complet (frontmatter inclus si présent)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.body).toBe(LIVE_RESPONSE.content);
  });

  it('locus = null (non fourni par vault_read)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.locus).toBeNull();
  });

  it('forgotten = false (non fourni)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.forgotten).toBe(false);
  });

  it('kind = note (non fourni)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.kind).toBe('note');
  });
});

describe('flattenVaultRead — extraction titre et frontmatter', () => {
  it('titre null si contenu ne commence pas par # (fixture LIVE — débute par ##)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    // "## Milestone" → pas un H1 → title null
    expect(note.title).toBeNull();
  });

  it('titre extrait si première ligne H1 (après frontmatter si présent)', () => {
    const note = flattenVaultRead(WITH_FRONTMATTER);
    expect(note.title).toBe('Mon titre');
  });

  it('frontmatter extrait du bloc ---', () => {
    const note = flattenVaultRead(WITH_FRONTMATTER);
    expect(note.frontmatter).toContain('name: test-note');
    expect(note.frontmatter).toMatch(/^---/);
    expect(note.frontmatter).toMatch(/---$/);
  });

  it('frontmatter vide si pas de bloc --- (fixture LIVE)', () => {
    const note = flattenVaultRead(LIVE_RESPONSE);
    expect(note.frontmatter).toBe('');
  });
});

describe('flattenVaultRead — status coercion', () => {
  it('status staging accepté', () => {
    const note = flattenVaultRead({ ...LIVE_RESPONSE, metadata: { ...LIVE_RESPONSE.metadata, status: 'staging' } });
    expect(note.status).toBe('staging');
  });

  it('status inconnu → fallback live', () => {
    const note = flattenVaultRead({ ...LIVE_RESPONSE, metadata: { ...LIVE_RESPONSE.metadata, status: 'unknown-future' } });
    expect(note.status).toBe('live');
  });

  it('status pending-review accepté', () => {
    const note = flattenVaultRead({ ...LIVE_RESPONSE, metadata: { ...LIVE_RESPONSE.metadata, status: 'pending-review' } });
    expect(note.status).toBe('pending-review');
  });
});
