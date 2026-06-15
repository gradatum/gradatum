-- Migration 0004 : ajout colonne replaced_by dans notes (Phase 2.1.2 alpha.9)
--
-- Raison : la fonctionnalité vault_downgrade (parité MCP legacy vault) nécessite
-- de tracer vers quelle note canon une note downgradée a été remplacée.
-- La colonne est nullable — NULL signifie downgrade autonome sans remplacement.
-- La contrainte REFERENCES notes(id) garantit l'intégrité référentielle.
--
-- Naming : 'replaced_by' aligné sur DTO `VaultDowngradeRequest.replaced_by`
-- (champ pré-existant dans gradatum-dto, déjà consommé par worker dispatch
-- + mcp-stub tests). Le terme 'superseded_by' du plan initial a été corrigé
-- post Task 2 pour cohérence codebase.
--
-- L'idempotence est gérée côté Rust dans migrations.rs (vérification
-- _schema_migrations avant exécution du batch).

ALTER TABLE notes ADD COLUMN replaced_by TEXT REFERENCES notes(id);

CREATE INDEX IF NOT EXISTS idx_notes_status_downgrade ON notes(status, status_changed)
WHERE status = 'downgraded';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0004_vault_downgrade', strftime('%s', 'now') * 1000);
