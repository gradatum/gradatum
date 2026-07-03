/**
 * StatusBadge — 7 états NoteStatus × forgotten overlay
 *
 * Table normative : s02-ux-normatif.md §6
 * Amendement A4 : DEPRECATED utilise #44423c (ratio ~4.6:1 WCAG AA)
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import type { NoteStatus } from '../types/api';

interface BadgeStyle {
  color: string;
  background: string;
  border: string;
  borderStyle?: string;
  text: string;
}

// Conservé pour getBadgeStyle (utilisé dans NotesPage pour les badges inline-grid)
const BADGE_STYLES: Record<string, BadgeStyle> = {
  live: {
    color: '#15803d',
    background: '#ecf8ef',
    border: '#a7d5b5',
    text: 'LIVE',
  },
  staging: {
    color: '#b54708',
    background: '#fef0e6',
    border: '#f5d9c1',
    text: 'STAGING',
  },
  'pending-review': {
    color: '#2a5db0',
    background: '#eef2f9',
    border: '#c4d3ec',
    text: 'REVIEW',
  },
  draft: {
    color: '#77736a',
    background: '#f2f1ed',
    border: '#ddd9d1',
    text: 'DRAFT',
  },
  deprecated: {
    color: '#44423c', // A4 : #44423c → ratio ~4.6:1
    background: '#eeede9',
    border: '#c8c5bf',
    text: 'DEPRECATED',
  },
  downgraded: {
    color: '#44423c',
    background: '#eeede9',
    border: '#c8c5bf',
    text: 'DOWNGRADED',
  },
  garbage: {
    color: '#b42318',
    background: '#fdf1f0',
    border: '#f5cdc8',
    text: 'GARBAGE',
  },
  forgotten: {
    color: '#77736a',
    background: 'transparent',
    border: '#c8c5bf',
    borderStyle: 'dashed',
    text: 'forgotten',
  },
};

// Mapping status → classe CSS (status-badge--<key>)
const STATUS_CLASS: Record<string, string> = {
  live:            'status-badge--live',
  staging:         'status-badge--staging',
  'pending-review':'status-badge--pending-review',
  draft:           'status-badge--draft',
  deprecated:      'status-badge--deprecated',
  downgraded:      'status-badge--downgraded',
  garbage:         'status-badge--garbage',
};

const STATUS_TEXT: Record<string, string> = {
  live:            'LIVE',
  staging:         'STAGING',
  'pending-review':'REVIEW',
  draft:           'DRAFT',
  deprecated:      'DEPRECATED',
  downgraded:      'DOWNGRADED',
  garbage:         'GARBAGE',
};

interface StatusBadgeProps {
  status: NoteStatus;
  forgotten?: boolean;
  className?: string;
}

export function StatusBadge({ status, forgotten = false, className }: StatusBadgeProps) {
  const modifier = STATUS_CLASS[status] ?? 'status-badge--draft';
  const text = STATUS_TEXT[status] ?? status.toUpperCase();

  return (
    <>
      <span
        className={`status-badge ${modifier}${className ? ` ${className}` : ''}`}
        data-testid={`badge-${status}`}
      >
        {text}
      </span>
      {forgotten && (
        <span
          className="status-badge status-badge--forgotten"
          data-testid="badge-forgotten"
        >
          forgotten
        </span>
      )}
    </>
  );
}

/**
 * Retourne les styles inline pour un badge de status (utile dans les grilles où
 * className ne peut pas être appliqué directement sur le conteneur grid).
 * Conservé pour la compatibilité avec NotesPage (span inline dans table-row grid).
 */
export function getBadgeStyle(status: NoteStatus | 'forgotten'): React.CSSProperties {
  const s = BADGE_STYLES[status] ?? BADGE_STYLES['draft'];
  return {
    fontFamily: "'JetBrains Mono', monospace",
    fontSize: '10.5px',
    fontWeight: 600,
    letterSpacing: '0.05em',
    textTransform: 'uppercase',
    padding: '1px 7px',
    borderRadius: '4px',
    border: '1px solid',
    display: 'inline-block',
    whiteSpace: 'nowrap',
    color: s.color,
    background: s.background,
    borderColor: s.border,
    ...(s.borderStyle ? { borderStyle: s.borderStyle } : {}),
  };
}
