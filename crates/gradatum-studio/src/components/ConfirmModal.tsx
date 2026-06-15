/**
 * ConfirmModal — modale de confirmation destructive
 *
 * Focus trap, Escape pour fermer, restauration focus déclencheur
 * Source : s02-ux-normatif.md §10
 */

import { useEffect, useRef } from 'react';

interface ConfirmModalProps {
  title: string;
  message: string;
  confirmLabel?: string;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmModal({
  title,
  message,
  confirmLabel = 'Confirm',
  onConfirm,
  onCancel,
}: ConfirmModalProps) {
  const cancelRef = useRef<HTMLButtonElement>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);

  // Focus sur Cancel au mount
  useEffect(() => {
    cancelRef.current?.focus();
  }, []);

  // Escape → annuler
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onCancel]);

  // Focus trap : Tab cycle entre Cancel et Confirm
  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key !== 'Tab') return;
    const els = [cancelRef.current, confirmRef.current].filter(Boolean) as HTMLElement[];
    const idx = els.indexOf(document.activeElement as HTMLElement);
    if (e.shiftKey) {
      if (idx <= 0) {
        e.preventDefault();
        els[els.length - 1]?.focus();
      }
    } else {
      if (idx >= els.length - 1) {
        e.preventDefault();
        els[0]?.focus();
      }
    }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="modal-title"
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(29, 28, 25, 0.45)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 50,
      }}
      onKeyDown={handleKeyDown}
      data-testid="confirm-modal"
    >
      <div
        style={{
          background: '#ffffff',
          borderRadius: '12px',
          width: '480px',
          padding: '26px 28px',
          boxShadow: '0 18px 50px rgba(0,0,0,0.25)',
          display: 'flex',
          flexDirection: 'column',
          gap: '16px',
        }}
      >
        <div
          id="modal-title"
          style={{ fontSize: '16px', fontWeight: 600 }}
        >
          {title}
        </div>
        <p style={{ fontSize: '13.5px', color: '#33312c', lineHeight: 1.55 }}>
          {message}
        </p>
        <div style={{ display: 'flex', gap: '10px', justifyContent: 'flex-end' }}>
          <button
            ref={cancelRef}
            onClick={onCancel}
            className="btn-neutral"
            data-testid="modal-cancel"
          >
            Cancel
          </button>
          <button
            ref={confirmRef}
            onClick={onConfirm}
            style={{
              fontFamily: 'var(--font-ui)',
              fontSize: '12.5px',
              fontWeight: 600,
              background: '#b42318',
              color: '#fff',
              border: 'none',
              borderRadius: '6px',
              padding: '7px 16px',
              cursor: 'pointer',
              minHeight: '32px',
            }}
            data-testid="modal-confirm"
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
