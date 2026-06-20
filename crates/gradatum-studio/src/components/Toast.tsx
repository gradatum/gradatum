/**
 * Toast — notification auto-dismiss 3-5s
 * Source : s02-ux-normatif.md §11
 * D3.1 : styles inline migrés en classes CSS (studio.css)
 */

import { useEffect } from 'react';

interface ToastProps {
  message: string;
  onDismiss: () => void;
  durationMs?: number;
}

export function Toast({ message, onDismiss, durationMs = 3500 }: ToastProps) {
  useEffect(() => {
    const timer = setTimeout(onDismiss, durationMs);
    return () => clearTimeout(timer);
  }, [onDismiss, durationMs]);

  return (
    <div
      role="status"
      className="toast"
      data-testid="toast"
    >
      {message}
    </div>
  );
}
