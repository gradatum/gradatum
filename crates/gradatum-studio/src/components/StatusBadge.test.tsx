/**
 * Tests StatusBadge — 7 états + forgotten overlay
 * Amendement A4 : DEPRECATED = #44423c (WCAG AA ~4.6:1) — vérifié via classe CSS
 * D3.1 : les couleurs viennent de classes CSS (studio.css) pas d'inline styles
 *        → assertions sur className + textContent (toHaveStyle ne fonctionne pas en jsdom sans CSS)
 */

import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { StatusBadge } from './StatusBadge';

describe('StatusBadge', () => {
  it('affiche LIVE pour status=live', () => {
    render(<StatusBadge status="live" />);
    const badge = screen.getByTestId('badge-live');
    expect(badge).toHaveTextContent('LIVE');
    expect(badge).toHaveClass('status-badge--live');
  });

  it('affiche STAGING pour status=staging', () => {
    render(<StatusBadge status="staging" />);
    const badge = screen.getByTestId('badge-staging');
    expect(badge).toHaveTextContent('STAGING');
    expect(badge).toHaveClass('status-badge--staging');
  });

  it('affiche REVIEW pour status=pending-review', () => {
    render(<StatusBadge status="pending-review" />);
    const badge = screen.getByTestId('badge-pending-review');
    expect(badge).toHaveTextContent('REVIEW');
    expect(badge).toHaveClass('status-badge--pending-review');
  });

  it('affiche DRAFT pour status=draft', () => {
    render(<StatusBadge status="draft" />);
    const badge = screen.getByTestId('badge-draft');
    expect(badge).toHaveTextContent('DRAFT');
    expect(badge).toHaveClass('status-badge--draft');
  });

  it('affiche DEPRECATED pour status=deprecated (A4 : color #44423c via .status-badge--deprecated)', () => {
    render(<StatusBadge status="deprecated" />);
    const badge = screen.getByTestId('badge-deprecated');
    expect(badge).toHaveTextContent('DEPRECATED');
    // Couleur vérifiée via classe CSS (studio.css .status-badge--deprecated { color: #44423c })
    expect(badge).toHaveClass('status-badge--deprecated');
  });

  it('affiche DEPRECATED pour status=downgraded (legacy bucket normatif §7)', () => {
    render(<StatusBadge status="downgraded" />);
    const badge = screen.getByTestId('badge-downgraded');
    expect(badge).toHaveTextContent('DEPRECATED');
    // Classe identique à deprecated (legacy bucket)
    expect(badge).toHaveClass('status-badge--downgraded');
  });

  it('affiche GARBAGE pour status=garbage', () => {
    render(<StatusBadge status="garbage" />);
    const badge = screen.getByTestId('badge-garbage');
    expect(badge).toHaveTextContent('GARBAGE');
    expect(badge).toHaveClass('status-badge--garbage');
  });

  it('affiche overlay forgotten si prop forgotten=true', () => {
    render(<StatusBadge status="live" forgotten={true} />);
    expect(screen.getByTestId('badge-live')).toBeInTheDocument();
    const forgottenBadge = screen.getByTestId('badge-forgotten');
    expect(forgottenBadge).toHaveTextContent('forgotten');
    expect(forgottenBadge).toHaveClass('status-badge--forgotten');
  });

  it("n'affiche pas l'overlay forgotten par défaut", () => {
    render(<StatusBadge status="live" />);
    expect(screen.queryByTestId('badge-forgotten')).not.toBeInTheDocument();
  });

  it("n'affiche pas l'overlay forgotten si forgotten=false", () => {
    render(<StatusBadge status="staging" forgotten={false} />);
    expect(screen.queryByTestId('badge-forgotten')).not.toBeInTheDocument();
  });
});
