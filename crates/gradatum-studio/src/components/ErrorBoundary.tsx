/**
 * ErrorBoundary — fallback global anti-écran-blanc
 * Toute erreur React non gérée affiche un message honnête
 * avec un bouton de rechargement plutôt qu'un écran vide.
 */

import { Component, type ReactNode } from 'react';

interface Props {
  children: ReactNode;
}

interface State {
  hasError: boolean;
  message: string;
}

export class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props);
    this.state = { hasError: false, message: '' };
  }

  static getDerivedStateFromError(error: unknown): State {
    const message = error instanceof Error ? error.message : String(error);
    return { hasError: true, message };
  }

  override componentDidCatch(error: unknown, info: { componentStack?: string | null }) {
    // Log explicite — jamais de catch vide
    console.error('[ErrorBoundary] Uncaught error:', error);
    if (info.componentStack) {
      console.error('[ErrorBoundary] Component stack:', info.componentStack);
    }
  }

  handleReload = () => {
    this.setState({ hasError: false, message: '' });
    window.location.reload();
  };

  override render() {
    if (this.state.hasError) {
      return (
        <div
          style={{
            height: '100vh',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            background: '#f7f7f5',
            fontFamily: "'IBM Plex Sans', sans-serif",
          }}
          role="alert"
          data-testid="error-boundary"
        >
          <div
            style={{
              background: '#ffffff',
              border: '1px solid #e0ded8',
              borderRadius: '9px',
              padding: '36px 40px',
              maxWidth: '480px',
              width: '90%',
              display: 'flex',
              flexDirection: 'column',
              gap: '16px',
            }}
          >
            <div style={{ fontSize: '15px', fontWeight: 600, color: '#b42318' }}>
              Something went wrong
            </div>
            <div
              style={{
                fontFamily: "'JetBrains Mono', monospace",
                fontSize: '12px',
                color: '#66635b',
                background: '#fdf1f0',
                border: '1px solid #f5cdc8',
                borderRadius: '6px',
                padding: '10px 14px',
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
                maxHeight: '160px',
                overflowY: 'auto',
              }}
              data-testid="error-boundary-message"
            >
              {this.state.message || 'Unknown error'}
            </div>
            <div style={{ fontSize: '12.5px', color: '#8a857c' }}>
              If the problem persists, check the browser console for details.
            </div>
            <button
              onClick={this.handleReload}
              className="btn-outline"
              style={{ alignSelf: 'flex-start', padding: '8px 18px' }}
              data-testid="error-boundary-reload"
            >
              Reload page
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
