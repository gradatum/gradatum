/**
 * Tests useAuth + apiFetch
 * Règles sécurité testées :
 * - JWT stocké en localStorage (W4 livrable-1) avec clé gradatum_studio_jwt_persist
 * - JWT expiré au mount est supprimé
 * - api-key jamais conservée après exchange
 * - login() efface l'api-key de la mémoire après usage
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useAuth, apiFetch, setUnauthorizedHandler, clearUnauthorizedHandler } from './useAuth';

const SESSION_KEY = 'gradatum_studio_jwt';
const PERSIST_KEY = 'gradatum_studio_jwt_persist'; // W4

// Stub fetch global
const mockFetch = vi.fn();
beforeEach(() => {
  globalThis.fetch = mockFetch;
  sessionStorage.clear();
  localStorage.clear(); // W4: Also clear localStorage
  mockFetch.mockReset();
  // Nettoyer le handler global entre les tests (D3.3)
  clearUnauthorizedHandler();
});

afterEach(() => {
  sessionStorage.clear();
  localStorage.clear(); // W4: Also clear localStorage
  clearUnauthorizedHandler();
  vi.restoreAllMocks();
});

describe('useAuth — login', () => {
  it('place le JWT en localStorage après login réussi (W4 livrable-1)', async () => {
    mockFetch.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({ token: 'test-jwt-token' }),
    });

    const { result } = renderHook(() => useAuth());

    await act(async () => {
      await result.current.login('my-api-key');
    });

    // W4: JWT should be in localStorage with persistence key
    expect(localStorage.getItem(PERSIST_KEY)).toBe('test-jwt-token');
    expect(result.current.isAuthenticated).toBe(true);
    expect(result.current.token).toBe('test-jwt-token');
    expect(result.current.error).toBeNull();
  });

  it('stocke le JWT en localStorage pour persistence (W4 livrable-1 — relâchement délibéré tracé)', async () => {
    // JWT stocké en localStorage (et non sessionStorage) : TTL 24 h, survit au rechargement d'onglet.
    mockFetch.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({ token: 'test-jwt-token' }),
    });

    const { result } = renderHook(() => useAuth());

    await act(async () => {
      await result.current.login('my-api-key');
    });

    // W4: localStorage MUST contain JWT for persistence across reloads
    expect(localStorage.getItem(PERSIST_KEY)).toBe('test-jwt-token');
    // But api-key is never stored (security invariant preserved)
    expect(localStorage.getItem('api-key')).toBeNull();
  });

  it("l'api-key n'est pas stockée après exchange", async () => {
    const apiKey = 'secret-api-key-12345';
    mockFetch.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({ token: 'jwt' }),
    });

    const { result } = renderHook(() => useAuth());
    await act(async () => {
      await result.current.login(apiKey);
    });

    // L'api-key ne doit pas apparaître dans sessionStorage
    for (let i = 0; i < sessionStorage.length; i++) {
      const key = sessionStorage.key(i) as string;
      expect(sessionStorage.getItem(key)).not.toBe(apiKey);
    }
    // Ni dans localStorage
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i) as string;
      expect(localStorage.getItem(key)).not.toBe(apiKey);
    }
  });

  it('JWT expiré en localStorage est supprimé au mount', async () => {
    // W4: Create an expired JWT (exp in the past)
    // JWT format: header.payload.signature
    // payload: { exp: 1000000000 } (year 2001, definitely expired)
    const expiredJwt = 'eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjEwMDAwMDAwMDB9.test';
    localStorage.setItem(PERSIST_KEY, expiredJwt);

    const { result } = renderHook(() => useAuth());

    // Mount should detect expiration and clear the token
    expect(result.current.isAuthenticated).toBe(false);
    expect(result.current.token).toBeNull();
    expect(localStorage.getItem(PERSIST_KEY)).toBeNull();
  });

  it('retourne error "Invalid API key" sur 401', async () => {
    mockFetch.mockResolvedValueOnce({ ok: false, status: 401 });

    const { result } = renderHook(() => useAuth());
    await act(async () => {
      await result.current.login('wrong-key');
    });

    expect(result.current.isAuthenticated).toBe(false);
    expect(result.current.error).toBe('Invalid API key');
    expect(sessionStorage.getItem(SESSION_KEY)).toBeNull();
  });

  it('retourne error sur 500', async () => {
    mockFetch.mockResolvedValueOnce({ ok: false, status: 500 });

    const { result } = renderHook(() => useAuth());
    await act(async () => {
      await result.current.login('any-key');
    });

    expect(result.current.isAuthenticated).toBe(false);
    expect(result.current.error).toContain('500');
  });

  it('retourne error réseau si fetch throw', async () => {
    mockFetch.mockRejectedValueOnce(new Error('Network error'));

    const { result } = renderHook(() => useAuth());
    await act(async () => {
      await result.current.login('any-key');
    });

    expect(result.current.isAuthenticated).toBe(false);
    expect(result.current.error).toContain('unreachable');
  });
});

describe('useAuth — logout', () => {
  it('efface le JWT de localStorage et remet isAuthenticated=false (W4)', async () => {
    // W4: Set JWT in localStorage (as login would)
    const jwt = 'eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjk5OTk5OTk5OTl9.test'; // far future exp
    localStorage.setItem(PERSIST_KEY, jwt);

    const { result } = renderHook(() => useAuth());
    expect(result.current.isAuthenticated).toBe(true);

    act(() => { result.current.logout(); });

    expect(result.current.isAuthenticated).toBe(false);
    expect(result.current.token).toBeNull();
    expect(localStorage.getItem(PERSIST_KEY)).toBeNull(); // W4: localStorage cleared
  });
});

describe('apiFetch', () => {
  it('injecte le header Authorization Bearer si JWT présent (W4: reads from localStorage)', async () => {
    // W4: JWT is now in localStorage, not sessionStorage
    localStorage.setItem(PERSIST_KEY, 'my-jwt');
    mockFetch.mockResolvedValueOnce({ ok: true, status: 200 });

    await apiFetch('/api/v1/test');

    const [, options] = mockFetch.mock.calls[0] as [string, RequestInit & { headers: Record<string, string> }];
    const headers = options.headers as Record<string, string>;
    expect(headers['Authorization']).toBe('Bearer my-jwt');
  });

  it("n'injecte pas Authorization si JWT absent", async () => {
    localStorage.removeItem(PERSIST_KEY); // W4: Clear from localStorage
    mockFetch.mockResolvedValueOnce({ ok: true, status: 200 });

    await apiFetch('/api/v1/test');

    const [, options] = mockFetch.mock.calls[0] as [string, RequestInit & { headers: Record<string, string> }];
    const headers = options.headers as Record<string, string>;
    expect(headers['Authorization']).toBeUndefined();
  });

  it('transmet Content-Type application/json par défaut', async () => {
    mockFetch.mockResolvedValueOnce({ ok: true, status: 200 });

    await apiFetch('/api/v1/test');

    const [, options] = mockFetch.mock.calls[0] as [string, RequestInit & { headers: Record<string, string> }];
    expect((options.headers as Record<string, string>)['Content-Type']).toBe('application/json');
  });
});

describe('apiFetch — intercepteur 401 (D3.3)', () => {
  it('déclenche le handler enregistré sur réponse 401', async () => {
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    mockFetch.mockResolvedValueOnce({ ok: false, status: 401 });

    const res = await apiFetch('/api/v1/test');

    expect(handler).toHaveBeenCalledOnce();
    expect(res.status).toBe(401); // la réponse est quand même retournée
  });

  it("ne déclenche pas le handler si status !== 401", async () => {
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    mockFetch.mockResolvedValueOnce({ ok: false, status: 403 });

    await apiFetch('/api/v1/test');

    expect(handler).not.toHaveBeenCalled();
  });

  it("ne déclenche pas le handler si aucun handler enregistré", async () => {
    // handler non enregistré — aucune exception attendue
    mockFetch.mockResolvedValueOnce({ ok: false, status: 401 });

    await expect(apiFetch('/api/v1/test')).resolves.toBeDefined();
  });

  it('clearUnauthorizedHandler retire le handler — plus de déclenchement', async () => {
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    clearUnauthorizedHandler();
    mockFetch.mockResolvedValueOnce({ ok: false, status: 401 });

    await apiFetch('/api/v1/test');

    expect(handler).not.toHaveBeenCalled();
  });

  it('retourne la réponse 200 normalement si handler enregistré', async () => {
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    mockFetch.mockResolvedValueOnce({ ok: true, status: 200 });

    const res = await apiFetch('/api/v1/test');

    expect(handler).not.toHaveBeenCalled();
    expect(res.status).toBe(200);
  });
});
