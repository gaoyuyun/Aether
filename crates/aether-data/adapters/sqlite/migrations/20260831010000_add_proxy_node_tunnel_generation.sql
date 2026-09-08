ALTER TABLE proxy_nodes
    ADD COLUMN tunnel_generation TEXT NOT NULL DEFAULT '';

UPDATE proxy_nodes
SET tunnel_generation = lower(hex(randomblob(16)))
WHERE tunnel_generation = '';

-- SQLite only permits a constant default when adding a NOT NULL column. Keep
-- the upgrade compatible with legacy rows, then replace the temporary empty
-- default for future legacy writers with a per-row random generation. This
-- also covers importers that omit the newly added column.
CREATE TRIGGER IF NOT EXISTS proxy_nodes_fill_tunnel_generation
AFTER INSERT ON proxy_nodes
WHEN NEW.tunnel_generation IS NULL OR trim(NEW.tunnel_generation) = ''
BEGIN
    UPDATE proxy_nodes
    SET tunnel_generation = lower(hex(randomblob(16)))
    WHERE id = NEW.id
      AND (tunnel_generation IS NULL OR trim(tunnel_generation) = '');
END;
