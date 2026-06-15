-- Migration 0002 : table des liens wikilink entre notes (T3 P2.0c)
--
-- Chaque ligne représente un lien orienté src → dst dans un vault donné.
-- La clé primaire (src, dst, vault_id) garantit l'unicité des arcs.
-- CASCADE DELETE : si src ou dst est supprimé, le lien disparaît automatiquement.
--
-- FOREIGN KEY désactivé pour dst_note_id : une note liée peut référencer
-- une note inexistante dans le vault courant (lien brisé) — comportement voulu.

CREATE TABLE IF NOT EXISTS note_links (
    src_note_id TEXT NOT NULL,
    dst_note_id TEXT NOT NULL,
    vault_id    TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (src_note_id, dst_note_id, vault_id),
    FOREIGN KEY (src_note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_note_links_dst ON note_links (dst_note_id, vault_id);
CREATE INDEX IF NOT EXISTS idx_note_links_src ON note_links (src_note_id, vault_id);

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0002_wikilinks', strftime('%s', 'now') * 1000);
