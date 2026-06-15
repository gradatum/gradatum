/**
 * Layout principal — Sidebar + Header + contenu scrollable
 * Fetch /health au mount pour version runtime (P3 E2E finding)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { type ReactNode } from 'react';
import { Sidebar } from './Sidebar';
import { Header } from './Header';
import { useHealth } from '../hooks/useHealth';

interface LayoutProps {
  children: ReactNode;
  title: string;
  subtitle?: string;
  reviewCount?: number;
}

export function Layout({
  children,
  title,
  subtitle,
  reviewCount = 0,
}: LayoutProps) {
  const { version, healthy } = useHealth();

  return (
    <div className="studio-root" data-testid="studio-root">
      <Sidebar
        reviewCount={reviewCount}
        version={version}
        servicesHealthy={healthy}
      />
      <div className="studio-root-right">
        <Header title={title} subtitle={subtitle} />
        <main
          className="studio-main"
          data-testid="main-content"
        >
          {children}
        </main>
      </div>
    </div>
  );
}
