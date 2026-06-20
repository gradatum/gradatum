/**
 * useAuth — gestion de l'auth JWT via POST /auth/exchange
 *
 * Règles de sécurité (S2 verdict §5, W4 livrable-1) :
 * - JWT stocké en localStorage pour persistence (clé: gradatum_studio_jwt_persist)
 * - L'api-key n'est jamais stockée (effacée après exchange)
 * - Au mount : check JWT expiration, si exp < now → supprime localStorage + non-authentifié
 * - intercepteur 401 → retour page login (centralisé D3.3)
 *
 * D3.3 — Intercepteur 401 centralisé :
 * - `setUnauthorizedHandler(fn)` : enregistre le handler global (logout + redirect).
 *   Appelé une seule fois dans App.tsx via `useUnauthorizedHandler`.
 * - `apiFetch` déclenche automatiquement ce handler sur toute réponse 401.
 *   Les pages n'ont plus besoin de prop `onUnauthorized`.
 */

import { useState, useCallback, useEffect } from 'react';

const SESSION_KEY = 'gradatum_studio_jwt';
const PERSIST_KEY = 'gradatum_studio_jwt_persist'; // W4: localStorage key for JWT persistence

/**
 * Decode JWT payload (base64) without signature verification.
 * Used client-side to check JWT expiration.
 */
function decodeJwtPayload(token: string): Record<string, unknown> | null {
  try {
    const parts = token.split('.');
    if (parts.length !== 3) return null;
    const payload = JSON.parse(atob(parts[1]));
    return payload;
  } catch {
    return null;
  }
}

/**
 * Check if JWT is expired based on `exp` claim (seconds since epoch).
 * Returns true if expired, false if valid or unparseable.
 */
function isJwtExpired(token: string): boolean {
  const payload = decodeJwtPayload(token);
  if (!payload || typeof payload.exp !== 'number') return false;
  // Convert seconds to milliseconds and compare with current time
  return payload.exp * 1000 < Date.now();
}

// ─── D3.3 — Intercepteur 401 centralisé ──────────────────────────────────────
// Handler enregistré une fois par App.tsx ; appelé par apiFetch sur toute
// réponse 401, sans que les pages aient à gérer la redirection manuellement.
let _unauthorizedHandler: (() => void) | null = null;

/** Enregistre le handler global 401 (logout + redirect). Appelé depuis App.tsx. */
export function setUnauthorizedHandler(fn: () => void): void {
  _unauthorizedHandler = fn;
}

/** Désenregistre le handler (utile en tests pour éviter les fuites). */
export function clearUnauthorizedHandler(): void {
  _unauthorizedHandler = null;
}

/** Hook de confort : enregistre + nettoie au démontage (utilisé dans App.tsx). */
export function useUnauthorizedHandler(fn: () => void): void {
  useEffect(() => {
    setUnauthorizedHandler(fn);
    return () => { clearUnauthorizedHandler(); };
    // fn vient de useCallback dans AppRoutes — stable entre renders
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
// ─────────────────────────────────────────────────────────────────────────────

export interface AuthState {
  token: string | null;
  loading: boolean;
  error: string | null;
}

export interface UseAuthReturn extends AuthState {
  login: (apiKey: string) => Promise<void>;
  logout: () => void;
  isAuthenticated: boolean;
}

export function useAuth(): UseAuthReturn {
  const [state, setState] = useState<AuthState>({
    token: null,
    loading: false,
    error: null,
  });

  // W4: Load token from localStorage on mount and check expiration
  useEffect(() => {
    const stored = localStorage.getItem(PERSIST_KEY);
    if (stored && isJwtExpired(stored)) {
      // Token expired: remove it and reset state
      localStorage.removeItem(PERSIST_KEY);
      setState(prev => ({ ...prev, token: null }));
    } else if (stored) {
      // Token valid: restore state
      setState(prev => ({ ...prev, token: stored }));
    }
  }, []);

  const login = useCallback(async (apiKey: string) => {
    setState(prev => ({ ...prev, loading: true, error: null }));
    try {
      const res = await fetch('/auth/exchange', {
        method: 'POST',
        headers: {
          'Authorization': `Bearer ${apiKey}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ scope: 'human' }),
      });
      if (!res.ok) {
        const msg = res.status === 401
          ? 'Invalid API key'
          : `Auth failed (${res.status})`;
        setState(prev => ({ ...prev, loading: false, error: msg }));
        return;
      }
      const data = (await res.json()) as { token: string };
      // W4: Store JWT in localStorage for persistence across reloads
      localStorage.setItem(PERSIST_KEY, data.token);
      setState({ token: data.token, loading: false, error: null });
    } catch (_err) {
      setState(prev => ({
        ...prev,
        loading: false,
        error: 'Network error — server unreachable',
      }));
    }
  }, []);

  const logout = useCallback(() => {
    sessionStorage.removeItem(SESSION_KEY);
    localStorage.removeItem(PERSIST_KEY); // W4: Clear persistent JWT
    setState({ token: null, loading: false, error: null });
  }, []);

  return {
    ...state,
    login,
    logout,
    isAuthenticated: state.token !== null,
  };
}

/**
 * Effectue un fetch authentifié avec le JWT de localStorage.
 * W4: Lit depuis PERSIST_KEY (localStorage).
 * D3.3 : sur réponse 401, déclenche automatiquement le handler global
 * (logout + redirect login) enregistré via `setUnauthorizedHandler`.
 * Les pages n'ont plus à vérifier `res.status === 401` manuellement.
 */
export async function apiFetch(
  path: string,
  options?: RequestInit,
): Promise<Response> {
  const token = localStorage.getItem(PERSIST_KEY); // W4: Read from localStorage
  const headers: HeadersInit = {
    'Content-Type': 'application/json',
    ...(options?.headers ?? {}),
  };
  if (token) {
    (headers as Record<string, string>)['Authorization'] = `Bearer ${token}`;
  }
  const res = await fetch(path, { ...options, headers });
  if (res.status === 401 && _unauthorizedHandler !== null) {
    _unauthorizedHandler();
  }
  return res;
}
