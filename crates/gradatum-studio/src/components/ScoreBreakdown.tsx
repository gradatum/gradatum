/**
 * ScoreBreakdown — panneau WHY 2 niveaux (prose honnête + Show formula)
 *
 * Règle A1 : ligne rerank OMISE si rerank_score est null/noop
 * Colonne composite libellée "composite" (jamais "AI score")
 * Source : s02-ux-normatif.md §5.4
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useState } from 'react';
import type { ScoreBreakdown as ScoreBreakdownType } from '../types/api';

interface ScoreBreakdownProps {
  scores: ScoreBreakdownType;
}

export function ScorePanel({ scores }: ScoreBreakdownProps) {
  return (
    <div className="score-panel">
      {scores.bm25_rank !== undefined && (
        <>
          <span className="score-label">bm25</span>
          <span className="score-value score-value--bold">#{scores.bm25_rank}</span>
        </>
      )}
      {scores.sem_rank !== undefined && (
        <>
          <span className="score-label">semantic</span>
          <span className="score-value">#{scores.sem_rank}</span>
        </>
      )}
      <span className="score-label">pagerank</span>
      <span className="score-value">{scores.pagerank_factor.toFixed(4)}</span>
      {/* rerank OMIS — A1 : NoopReranker, donnée trompeuse */}
      <span className="score-fused-label">fused</span>
      <span className="score-fused-value tabular">{scores.composite.toFixed(4)}</span>
    </div>
  );
}

interface WhyPanelProps {
  scores: ScoreBreakdownType;
}

export function WhyPanel({ scores }: WhyPanelProps) {
  const [showFormula, setShowFormula] = useState(false);

  const bm25Part = scores.bm25_rank !== undefined
    ? `1/(60+${scores.bm25_rank})`
    : '–';
  const semPart = scores.sem_rank !== undefined
    ? `1/(60+${scores.sem_rank})`
    : '–';
  const rrfRaw = scores.rrf_score.toFixed(4);
  const composite = scores.composite.toFixed(4);

  return (
    <div className="why-toggle-wrapper">
      <button
        onClick={() => setShowFormula(v => !v)}
        className="why-formula-btn"
        aria-expanded={showFormula}
        data-testid="why-toggle"
      >
        {showFormula ? 'Hide why ▴' : 'Why? ▾'}
      </button>
      {showFormula && (
        <div className="why-formula-panel" data-testid="why-formula">
{`rrf     = ${bm25Part} + ${semPart} = ${rrfRaw}
recency = ${scores.recency_factor.toFixed(4)}
pgrank  = ${scores.pagerank_factor.toFixed(4)}
trust   = ${scores.trust_raw.toFixed(4)}
──────────────────────────────────
composite = ${composite}`}
        </div>
      )}
    </div>
  );
}
