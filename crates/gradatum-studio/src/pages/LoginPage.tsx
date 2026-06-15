/**
 * LoginPage — saisie api-key → POST /auth/exchange → JWT sessionStorage
 *
 * Règles de sécurité (S2 verdict §5) :
 * - Jamais l'api-key stockée après exchange
 * - JWT en sessionStorage UNIQUEMENT
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useState, type FormEvent } from 'react';

interface LoginPageProps {
  onLogin: (apiKey: string) => Promise<void>;
  loading: boolean;
  error: string | null;
}

export function LoginPage({ onLogin, loading, error }: LoginPageProps) {
  const [apiKey, setApiKey] = useState('');

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!apiKey.trim()) return;
    await onLogin(apiKey.trim());
    // Effacer la valeur du champ après tentative (ne jamais laisser en mémoire)
    setApiKey('');
  };

  return (
    <div className="login-page">
      <div className="login-card">
        {/* Logo */}
        <div className="login-logo-block">
          <div className="login-logo-icon" aria-hidden="true">G</div>
          <div>
            <div className="login-logo-name">Gradatum Studio</div>
            <div className="login-logo-meta">admin interface</div>
          </div>
        </div>

        <div className="login-divider" />

        <form onSubmit={handleSubmit} className="login-form">
          <div className="login-field">
            <label htmlFor="apikey-input" className="login-label">
              API key
            </label>
            <input
              id="apikey-input"
              type="password"
              value={apiKey}
              onChange={e => setApiKey(e.target.value)}
              placeholder="grd_…"
              autoComplete="current-password"
              required
              className="login-input"
              data-testid="apikey-input"
            />
          </div>

          {error && (
            <div role="alert" className="login-error" data-testid="login-error">
              {error}
            </div>
          )}

          <button
            type="submit"
            disabled={loading || !apiKey.trim()}
            className="btn-primary login-submit"
            data-testid="login-submit"
          >
            {loading ? 'Authenticating…' : 'Sign in'}
          </button>
        </form>
      </div>
    </div>
  );
}
