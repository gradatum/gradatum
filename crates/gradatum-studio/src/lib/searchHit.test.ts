/**
 * searchHit.test.ts — shape réelle vault_search LIVE 2026-06-11
 *   Réponse : { items: RawSearchHit[] }
 *   Hit     : { path: "section/ULID26", score, title, snippet, scores? }
 *   INTERDITS dans le hit côté frontend : locus, agent, created_ms (non fournis)
 */

import { describe, it, expect } from 'vitest';
import { flattenHit, parseSearchResponse } from './searchHit';
import type { RawSearchHit } from '../types/api';

// ── Shape LIVE nominale ──────────────────────────────────────────────────────

const RAW_HIT: RawSearchHit = {
  path: 'decisions/01KTSGBYGG8KQGTDKGF9VNTHN9',
  score: 0.912,
  title: 'Roadmap option A actée 2026-06-10',
  snippet: 'v0.4.4 distillation → v0.4.5 backends → v0.4.6 studio MVP…',
};

describe('flattenHit', () => {
  it('splitte path→{section, ulid}', () => {
    const hit = flattenHit(RAW_HIT);
    expect(hit.ulid).toBe('01KTSGBYGG8KQGTDKGF9VNTHN9');
    expect(hit.section).toBe('decisions');
  });

  it('conserve path original', () => {
    const hit = flattenHit(RAW_HIT);
    expect(hit.path).toBe('decisions/01KTSGBYGG8KQGTDKGF9VNTHN9');
  });

  it('fallback status→live si absent', () => {
    const hit = flattenHit(RAW_HIT); // pas de status dans le hit
    expect(hit.status).toBe('live');
  });

  it('coerce status connu (garbage)', () => {
    const hit = flattenHit({ ...RAW_HIT, status: 'garbage' });
    expect(hit.status).toBe('garbage');
  });

  it('coerce status inconnu → live (fallback conservateur)', () => {
    const hit = flattenHit({ ...RAW_HIT, status: 'unknown-future-value' });
    expect(hit.status).toBe('live');
  });

  it('forgotten = toujours false (champ non fourni par vault_search)', () => {
    const hit = flattenHit(RAW_HIT);
    expect(hit.forgotten).toBe(false);
  });

  it('title null si absent', () => {
    const hit = flattenHit({ ...RAW_HIT, title: null });
    expect(hit.title).toBeNull();
  });

  it('scores propagé si présent', () => {
    const scores = {
      rrf_score: 0.8,
      recency_factor: 1.0,
      pagerank_factor: 0.5,
      in_degree: 3,
      trust_raw: 0.7,
      composite: 0.85,
    };
    const hit = flattenHit({ ...RAW_HIT, scores });
    expect(hit.scores).toEqual(scores);
  });

  it('scores undefined si absent', () => {
    const hit = flattenHit(RAW_HIT);
    expect(hit.scores).toBeUndefined();
  });

  it('défensif si path sans slash → section=path, ulid=path', () => {
    const hit = flattenHit({ ...RAW_HIT, path: 'ORPHAN_ULID' });
    expect(hit.section).toBe('ORPHAN_ULID');
    expect(hit.ulid).toBe('ORPHAN_ULID');
  });
});

// ── parseSearchResponse ──────────────────────────────────────────────────────

describe('parseSearchResponse', () => {
  it('parse shape LIVE nominale { items: [...] }', () => {
    const data = { items: [RAW_HIT] };
    const hits = parseSearchResponse(data);
    expect(hits).toHaveLength(1);
    expect(hits[0].ulid).toBe('01KTSGBYGG8KQGTDKGF9VNTHN9');
  });

  it('retourne [] sur { items: [] }', () => {
    expect(parseSearchResponse({ items: [] })).toEqual([]);
  });

  it('retourne [] si items manquant — ancienne shape imaginaire { hits: [...] }', () => {
    // ancienne shape fictive qu'on utilisait avant — ne DOIT PAS crasher
    expect(parseSearchResponse({ hits: [RAW_HIT] })).toEqual([]);
  });

  it('retourne [] sur null', () => {
    expect(parseSearchResponse(null)).toEqual([]);
  });

  it('retourne [] sur string', () => {
    expect(parseSearchResponse('not-an-object')).toEqual([]);
  });

  it('retourne [] si items n\'est pas un tableau', () => {
    expect(parseSearchResponse({ items: 'string' })).toEqual([]);
    expect(parseSearchResponse({ items: 42 })).toEqual([]);
  });

  it('parse plusieurs hits', () => {
    const raw2: RawSearchHit = {
      path: 'architecture/01KTP000000000000000000000',
      score: 0.55,
      title: 'B\' v2.1 topologie engines',
      snippet: 'vision+agent+coder = Qwen3-VL-30B-A3B…',
      status: 'live',
    };
    const hits = parseSearchResponse({ items: [RAW_HIT, raw2] });
    expect(hits).toHaveLength(2);
    expect(hits[1].section).toBe('architecture');
  });

  it('champ status disponible après commit feat(search):status-filter', () => {
    const data = { items: [{ ...RAW_HIT, status: 'staging' }] };
    const hits = parseSearchResponse(data);
    expect(hits[0].status).toBe('staging');
  });
});
