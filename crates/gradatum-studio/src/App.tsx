/**
 * App — Router + auth gate
 * JWT stocké en localStorage (clé: gradatum_studio_jwt_persist) pour persistence cross-reloads.
 * 401 → logout + redirect login (intercepteur D3.3 centralisé dans apiFetch).
 */

import { BrowserRouter, Routes, Route, Navigate, useNavigate } from 'react-router-dom';
import { useCallback } from 'react';
import { useAuth, useUnauthorizedHandler } from './hooks/useAuth';
import { ErrorBoundary } from './components/ErrorBoundary';
import { LoginPage } from './pages/LoginPage';
import { DashboardPage } from './pages/DashboardPage';
import { NotesPage } from './pages/NotesPage';
import { NoteDetailPage } from './pages/NoteDetailPage';
import { SearchPage } from './pages/SearchPage';
import { ReviewPage } from './pages/ReviewPage';
import { JobsPage } from './pages/JobsPage';
import { SystemPage } from './pages/SystemPage';
import ActivityPage from './pages/ActivityPage';

function AppRoutes() {
  const { isAuthenticated, login, logout, loading, error } = useAuth();
  const navigate = useNavigate();

  // D3.3 — handler 401 enregistré globalement dans apiFetch via useUnauthorizedHandler.
  // Les pages n'ont plus besoin de prop onUnauthorized.
  const handleUnauthorized = useCallback(() => {
    logout();
    navigate('/login', { replace: true });
  }, [logout, navigate]);

  useUnauthorizedHandler(handleUnauthorized);

  if (!isAuthenticated) {
    return (
      <Routes>
        <Route path="/login" element={<LoginPage onLogin={login} loading={loading} error={error} />} />
        <Route path="*" element={<Navigate to="/login" replace />} />
      </Routes>
    );
  }

  return (
    <Routes>
      <Route path="/" element={<DashboardPage />} />
      <Route path="/notes" element={<NotesPage />} />
      <Route path="/notes/:id" element={<NoteDetailPage />} />
      <Route path="/search" element={<SearchPage />} />
      <Route path="/review" element={<ReviewPage />} />
      <Route path="/jobs" element={<JobsPage />} />
      <Route path="/system" element={<SystemPage />} />
      <Route path="/activity" element={<ActivityPage />} />
      <Route path="/login" element={<Navigate to="/" replace />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

export function App() {
  return (
    <ErrorBoundary>
      <BrowserRouter basename="/ui">
        <AppRoutes />
      </BrowserRouter>
    </ErrorBoundary>
  );
}
