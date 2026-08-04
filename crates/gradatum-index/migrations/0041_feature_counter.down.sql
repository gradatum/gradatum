-- Rollback manuel de 0041_feature_counter.sql (le runner est forward-only).
--
-- Supprime le compteur de features et l'entrée de registre. Après ce down, la prochaine
-- allocation re-dérivera le seed depuis les cartes project-map (auto-cicatrisation) si la
-- table est recréée par un re-forward.

DROP TABLE IF EXISTS feature_counter;

DELETE FROM _schema_migrations WHERE version = '0041_feature_counter';
