-- 0040_grants_section_scope.sql — L3 (F-121, ledger pré-flip) : grant SECTION-SCOPÉ.
--
-- Défaut fermé par C2/P2-b : la lecture cross-tenant du `main/lessons-learned` partagé
-- exigeait un grant read, mais ce grant portait sur le vault `main` ENTIER — un tenant
-- granté pour les leçons pouvait lire TOUT `main`. Cette migration ajoute l'axe SECTION
-- à l'allow-list ; le serveur (tenant_guard) exige désormais que le grant COUVRE la
-- section demandée.
--
-- Design (option (a) arbitrée le 2026-07-23) :
--   * colonne `section` NULLABLE — `NULL` = grant vault-entier = sémantique C1 STRICTE.
--     Toutes les lignes existantes (seed `main↔main`, self-grants `provision_vault`)
--     restent `NULL` → aucun changement de comportement, AUCUNE migration de données.
--   * les leçons restent dans le vault `main` (pas de vault dédié, pas de déplacement
--     de notes) : seul le contrôle d'accès gagne une dimension.
--
-- Additif pur : `ALTER TABLE ... ADD COLUMN` nullable, aucune réécriture de table,
-- aucun ALTER sur `notes` (règle transversale A5). La PRIMARY KEY reste
-- `(tenant_id, vault_id)` — un couple (tenant, vault) porte donc AU PLUS une ligne :
-- soit un grant vault-entier, soit un grant borné à UNE section. Ouvrir plusieurs
-- sections distinctes du même vault au même tenant exigerait un rebuild de PK
-- (hors périmètre L3, non requis par le besoin d'aujourd'hui).
--
-- Flag OFF (défaut LIVE) : cette table n'est JAMAIS lue par le chemin legacy mono-vault
-- (cf. en-tête 0030) — la colonne est donc inerte à OFF, byte-identical v0.9.0.
--
-- Idempotence : garantie par le registre `_schema_migrations` du runner (forward-only),
-- au même niveau que 0007/0012/0014 (`ADD COLUMN` n'a pas de forme `IF NOT EXISTS` en
-- SQLite). Rollback manuel : `0040_grants_section_scope.down.sql`.

ALTER TABLE tenant_vault_grants ADD COLUMN section TEXT;

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0040_grants_section_scope', CAST(strftime('%s','now') AS INTEGER) * 1000);
