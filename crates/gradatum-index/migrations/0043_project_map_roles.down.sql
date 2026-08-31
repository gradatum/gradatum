-- Rollback manuel de 0043_project_map_roles (SQLite >= 3.35 supporte DROP COLUMN).
-- Le runner de migrations est forward-only : ce script n'est PAS enregistré dans
-- MIGRATIONS ; il est chargé explicitement (revert Task 5, ou test de réversibilité
-- up->down->up). L'index tombe d'abord, avant les colonnes qu'il référence.
DROP INDEX IF EXISTS idx_notes_roles;
ALTER TABLE notes DROP COLUMN role_kind;
ALTER TABLE notes DROP COLUMN role_status;

-- Retire la ligne de registre pour que le runner ré-applique 0043 au prochain démarrage
-- (revert Task 5 = down + re-déploiement N-1) et que le round-trip up->down->up soit exact.
DELETE FROM _schema_migrations WHERE version = '0043_project_map_roles';
