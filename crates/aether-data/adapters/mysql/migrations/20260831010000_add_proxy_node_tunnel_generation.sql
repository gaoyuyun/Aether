ALTER TABLE proxy_nodes
    ADD COLUMN tunnel_generation VARCHAR(64) NULL AFTER id;

UPDATE proxy_nodes
SET tunnel_generation = UUID()
WHERE tunnel_generation IS NULL OR TRIM(tunnel_generation) = '';

ALTER TABLE proxy_nodes
    MODIFY COLUMN tunnel_generation VARCHAR(64) NOT NULL;
