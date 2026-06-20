/**
 * Header persistant 58px
 * Source : s02-ux-normatif.md §9
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

interface HeaderProps {
  title: string;
  subtitle?: string;
  vaultName?: string;
  vaultPort?: string;
}

export function Header({
  title,
  subtitle,
  vaultName = 'main',
  vaultPort = ':19090',
}: HeaderProps) {
  return (
    <header className="studio-header" data-testid="page-header">
      <div className="studio-header-titles">
        <div className="studio-header-title">{title}</div>
        {subtitle && (
          <div className="studio-header-subtitle">{subtitle}</div>
        )}
      </div>
      <div className="studio-header-badges">
        <div className="studio-header-badge">
          vault <strong style={{ color: 'var(--color-text-primary)' }}>{vaultName}</strong>
          {' · '}
          <span style={{ color: 'var(--color-ok)' }}>LIVE</span>
        </div>
        <div className="studio-header-badge">{vaultPort}</div>
      </div>
    </header>
  );
}
