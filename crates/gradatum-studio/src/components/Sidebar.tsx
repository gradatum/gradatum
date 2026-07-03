/**
 * Sidebar — navigation principale 216px
 * Source : s02-ux-normatif.md §8
 *
 * Section Admin retirée (F-45/F-57 = Gold, hors scope MVP — E2E finding P1 scope)
 * Version affichée = runtime via prop depuis App (fetch /health)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useLocation, useNavigate } from 'react-router-dom';

interface SidebarProps {
  reviewCount?: number;
  vaultName?: string;
  version?: string;
  servicesHealthy?: boolean;
}

interface NavItem {
  label: string;
  path: string;
}

const NAV_MAIN: NavItem[] = [
  { label: 'Dashboard', path: '/' },
  { label: 'Notes', path: '/notes' },
  { label: 'Search', path: '/search' },
  { label: 'Review', path: '/review' },
  { label: 'Jobs', path: '/jobs' },
  { label: 'Système', path: '/system' },
  { label: 'Activité', path: '/activity' },
];

export function Sidebar({
  reviewCount = 0,
  vaultName = 'main',
  version,
  servicesHealthy = true,
}: SidebarProps) {
  const location = useLocation();
  const navigate = useNavigate();

  const isActive = (path: string) =>
    path === '/'
      ? location.pathname === '/'
      : location.pathname.startsWith(path);

  const versionLabel = version ? `v${version}` : '…';

  return (
    <aside className="sidebar" data-testid="sidebar">
      {/* Logo block */}
      <div className="sidebar-logo-block">
        <div className="sidebar-logo-icon" aria-hidden="true">G</div>
        <div>
          <div className="sidebar-logo-name">Gradatum Studio</div>
          <div className="sidebar-logo-meta">
            vault: {vaultName} · {versionLabel}
          </div>
        </div>
      </div>

      {/* Nav principal */}
      <nav aria-label="Main navigation" className="sidebar-nav">
        {NAV_MAIN.map(item => {
          const active = isActive(item.path);
          const showBadge = item.path === '/review' && reviewCount > 0;
          const label = item.label === 'Dashboard' && !servicesHealthy
            ? 'Dashboard !'
            : item.label;
          return (
            <button
              key={item.path}
              onClick={() => navigate(item.path)}
              className={`sidebar-nav-item${active ? ' is-active' : ''}`}
              aria-current={active ? 'page' : undefined}
              data-testid={`nav-${item.path.replace('/', '') || 'dashboard'}`}
            >
              <span>{label}</span>
              {showBadge && (
                <span
                  className="sidebar-review-badge"
                  aria-label={`${reviewCount} items awaiting review`}
                >
                  {reviewCount}
                </span>
              )}
            </button>
          );
        })}
      </nav>

      {/* Footer */}
      <div className="sidebar-footer">
        <div className="sidebar-health-row">
          <div
            className={`sidebar-health-dot${servicesHealthy ? '' : ' degraded'}`}
            aria-hidden="true"
          />
          <span className="sidebar-health-label">
            {servicesHealthy ? 'all services healthy' : 'service degraded'}
          </span>
        </div>
        <div className="sidebar-footer-meta">admin · bearer scoped *</div>
      </div>
    </aside>
  );
}
