/**
 * jobs.test.ts — helpers POST /api/v1/jobs (contrats réels F-16.3)
 *
 * Contrats vérifiés LIVE :
 * - triggerCurate(noteId) → POST { spec: { kind: { type: "Curate", data: { note_id } } } }
 * - triggerPurge(dryRun) → POST { spec: { kind: { type: "Purge", data: { mode: "Lifecycle", dry_run } } } }
 * - Header Idempotency-Key obligatoire (crypto.randomUUID)
 * - 202 → { ok: true, id, idempotent }
 * - 400 → { ok: false, error: message_serveur }
 * - network error → { ok: false, error: 'Network error' }
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { triggerCurate, triggerPurge } from './jobs';

const mockFetch = vi.fn();

beforeEach(() => {
  globalThis.fetch = mockFetch;
  mockFetch.mockReset();
  // W4: JWT en localStorage with persistence key
  localStorage.setItem('gradatum_studio_jwt_persist', 'test-jwt');
  // crypto.randomUUID stub
  Object.defineProperty(globalThis, 'crypto', {
    value: { randomUUID: () => 'ffffffff-test-uuid' },
    configurable: true,
  });
});

afterEach(() => {
  sessionStorage.clear();
  localStorage.clear(); // W4: Also clear localStorage
  vi.restoreAllMocks();
});

function make202(id = '01KTWBBCMPHN1X55QZ0HFSMEQX') {
  return {
    ok: true,
    status: 202,
    json: async () => ({ id, idempotent: false }),
  };
}

function make400(msg = 'missing field `note_id`') {
  return {
    ok: false,
    status: 400,
    json: async () => ({ error: msg }),
  };
}

// ── triggerCurate ─────────────────────────────────────────────────────────────

describe('triggerCurate — shape POST et réponse', () => {
  it('POST /api/v1/jobs avec type=Curate et note_id', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerCurate('01KTST1WHWTMY4KYSD383ZF416');

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, opts] = mockFetch.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('/api/v1/jobs');
    expect(opts.method).toBe('POST');

    const body = JSON.parse(opts.body as string) as {
      spec: { kind: { type: string; data: { note_id: string } } };
    };
    expect(body.spec.kind.type).toBe('Curate');
    expect(body.spec.kind.data.note_id).toBe('01KTST1WHWTMY4KYSD383ZF416');
  });

  it('header Idempotency-Key présent', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerCurate('note-ulid-123');

    const [, opts] = mockFetch.mock.calls[0] as [string, RequestInit];
    const headers = opts.headers as Record<string, string>;
    expect(headers['Idempotency-Key']).toBe('ffffffff-test-uuid');
  });

  it('header Authorization Bearer présent', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerCurate('note-ulid-456');

    const [, opts] = mockFetch.mock.calls[0] as [string, RequestInit];
    const headers = opts.headers as Record<string, string>;
    expect(headers['Authorization']).toBe('Bearer test-jwt');
  });

  it('retourne ok:true id idempotent:false sur 202', async () => {
    mockFetch.mockResolvedValue(make202('01KTWBBCMPHN1X55QZ0HFSMEQX'));
    const result = await triggerCurate('note-abc');
    expect(result.ok).toBe(true);
    expect(result.id).toBe('01KTWBBCMPHN1X55QZ0HFSMEQX');
    expect(result.idempotent).toBe(false);
  });

  it('retourne ok:false avec error sur 400', async () => {
    mockFetch.mockResolvedValue(make400("missing field `note_id`"));
    const result = await triggerCurate('');
    expect(result.ok).toBe(false);
    expect(result.error).toContain('note_id');
  });

  it('retourne ok:false sur erreur réseau', async () => {
    mockFetch.mockRejectedValue(new Error('fetch failed'));
    const result = await triggerCurate('note-xyz');
    expect(result.ok).toBe(false);
    expect(result.error).toBe('Network error');
  });
});

// ── triggerPurge ──────────────────────────────────────────────────────────────

describe('triggerPurge — shape POST et réponse', () => {
  it('POST /api/v1/jobs avec type=Purge mode=Lifecycle', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(true);

    const [url, opts] = mockFetch.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('/api/v1/jobs');
    expect(opts.method).toBe('POST');

    const body = JSON.parse(opts.body as string) as {
      spec: { kind: { type: string; data: { mode: string; dry_run: boolean } } };
    };
    expect(body.spec.kind.type).toBe('Purge');
    expect(body.spec.kind.data.mode).toBe('Lifecycle');
  });

  it('dry_run:true quand dryRun=true', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(true);

    const body = JSON.parse((mockFetch.mock.calls[0] as [string, RequestInit])[1].body as string) as {
      spec: { kind: { data: { dry_run: boolean } } };
    };
    expect(body.spec.kind.data.dry_run).toBe(true);
  });

  it('dry_run:false quand dryRun=false', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(false);

    const body = JSON.parse((mockFetch.mock.calls[0] as [string, RequestInit])[1].body as string) as {
      spec: { kind: { data: { dry_run: boolean } } };
    };
    expect(body.spec.kind.data.dry_run).toBe(false);
  });

  it('grace_days inclus si fourni', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(true, 14);

    const body = JSON.parse((mockFetch.mock.calls[0] as [string, RequestInit])[1].body as string) as {
      spec: { kind: { data: { grace_days: number } } };
    };
    expect(body.spec.kind.data.grace_days).toBe(14);
  });

  it('grace_days absent si non fourni', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(true);

    const body = JSON.parse((mockFetch.mock.calls[0] as [string, RequestInit])[1].body as string) as {
      spec: { kind: { data: Record<string, unknown> } };
    };
    expect('grace_days' in body.spec.kind.data).toBe(false);
  });

  it('header Idempotency-Key présent sur Purge', async () => {
    mockFetch.mockResolvedValue(make202());
    await triggerPurge(true);

    const headers = (mockFetch.mock.calls[0] as [string, RequestInit])[1].headers as Record<string, string>;
    expect(headers['Idempotency-Key']).toBe('ffffffff-test-uuid');
  });

  it('retourne ok:true sur 202', async () => {
    mockFetch.mockResolvedValue(make202('01KTWBBCMYB282C9XASGQQPK55'));
    const result = await triggerPurge(true);
    expect(result.ok).toBe(true);
    expect(result.id).toBe('01KTWBBCMYB282C9XASGQQPK55');
  });

  it('retourne ok:false avec error sur 400', async () => {
    mockFetch.mockResolvedValue(make400("missing field `mode`"));
    const result = await triggerPurge(false);
    expect(result.ok).toBe(false);
    expect(result.error).toContain('mode');
  });
});
