/**
 * Tests ScoreBreakdown — panneau WHY + règle A1 (no rerank)
 */

import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ScorePanel, WhyPanel } from './ScoreBreakdown';
import type { ScoreBreakdown } from '../types/api';

const BASE_SCORES: ScoreBreakdown = {
  rrf_score: 0.0312,
  recency_factor: 0.9,
  pagerank_factor: 0.0012,
  in_degree: 3,
  trust_raw: 0.75,
  composite: 0.0421,
};

const SCORES_WITH_RANKS: ScoreBreakdown = {
  ...BASE_SCORES,
  bm25_rank: 2,
  sem_rank: 5,
};

describe('ScorePanel', () => {
  it('affiche le score composite (fused)', () => {
    render(<ScorePanel scores={BASE_SCORES} />);
    // "fused" label présent
    expect(screen.getByText('fused')).toBeInTheDocument();
    // Valeur composite formatée
    expect(screen.getByText('0.0421')).toBeInTheDocument();
  });

  it('affiche bm25 et semantic si fournis', () => {
    render(<ScorePanel scores={SCORES_WITH_RANKS} />);
    expect(screen.getByText('bm25')).toBeInTheDocument();
    expect(screen.getByText('#2')).toBeInTheDocument();
    expect(screen.getByText('semantic')).toBeInTheDocument();
    expect(screen.getByText('#5')).toBeInTheDocument();
  });

  it('ne contient JAMAIS le mot "rerank" (règle A1)', () => {
    const { container } = render(<ScorePanel scores={SCORES_WITH_RANKS} />);
    expect(container.innerHTML.toLowerCase()).not.toContain('rerank');
  });

  it('ne contient JAMAIS "AI score" (normatif §5.4)', () => {
    const { container } = render(<ScorePanel scores={BASE_SCORES} />);
    expect(container.innerHTML).not.toContain('AI score');
  });
});

describe('WhyPanel', () => {
  it('affiche le bouton Why? par défaut', () => {
    render(<WhyPanel scores={BASE_SCORES} />);
    expect(screen.getByTestId('why-toggle')).toHaveTextContent('Why?');
  });

  it('ouvre la formule au clic', async () => {
    const user = userEvent.setup();
    render(<WhyPanel scores={BASE_SCORES} />);
    await user.click(screen.getByTestId('why-toggle'));
    expect(screen.getByTestId('why-formula')).toBeInTheDocument();
  });

  it('ferme la formule au deuxième clic', async () => {
    const user = userEvent.setup();
    render(<WhyPanel scores={BASE_SCORES} />);
    await user.click(screen.getByTestId('why-toggle'));
    await user.click(screen.getByTestId('why-toggle'));
    expect(screen.queryByTestId('why-formula')).not.toBeInTheDocument();
  });

  it('formule ne contient pas "rerank" (règle A1)', async () => {
    const user = userEvent.setup();
    render(<WhyPanel scores={SCORES_WITH_RANKS} />);
    await user.click(screen.getByTestId('why-toggle'));
    const formula = screen.getByTestId('why-formula');
    expect(formula.textContent?.toLowerCase()).not.toContain('rerank');
  });

  it('formule affiche composite', async () => {
    const user = userEvent.setup();
    render(<WhyPanel scores={BASE_SCORES} />);
    await user.click(screen.getByTestId('why-toggle'));
    expect(screen.getByTestId('why-formula').textContent).toContain('composite');
  });
});
