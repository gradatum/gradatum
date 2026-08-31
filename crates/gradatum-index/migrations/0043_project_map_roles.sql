-- 0043_project_map_roles : rôles typés project-map filtrables sans lire body_text (F-171).
--
-- role_kind / role_status portent le wire canonique (SCREAMING_SNAKE) de [[kind:…]] /
-- [[status:…]], dérivé à l'écriture par gradatum_core::project_map::roles_of_body (source
-- unique = parse_link). NULL pour toute note hors project-map (dérivation gatée sur la
-- section dans upsert_note, même patron que c_kind/doc_kind).
--
-- Rollback manuel : 0043_project_map_roles.down.sql (runner forward-only).
ALTER TABLE notes ADD COLUMN role_kind   TEXT NULL;
ALTER TABLE notes ADD COLUMN role_status TEXT NULL;
CREATE INDEX IF NOT EXISTS idx_notes_roles
    ON notes(vault_id, section, role_kind, role_status);

-- Enregistrement dans le registre de migrations (guard idempotence du runner :
-- run() ne consigne pas lui-même, chaque fichier de migration s'auto-enregistre).
INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0043_project_map_roles', CAST(strftime('%s','now') AS INTEGER) * 1000);
