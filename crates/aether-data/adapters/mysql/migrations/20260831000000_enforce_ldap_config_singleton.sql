-- LDAP configuration is a database-wide singleton. Preserve the row selected by the legacy
-- reader (the smallest id), remove historical duplicates, and let the database arbitrate
-- concurrent first creation.
DELETE FROM ldap_configs
WHERE id <> (
    SELECT keep_id
    FROM (SELECT MIN(id) AS keep_id FROM ldap_configs) AS ldap_singleton_keeper
);

ALTER TABLE ldap_configs
    ADD COLUMN singleton_key INT NOT NULL DEFAULT 1,
    ADD CONSTRAINT ldap_configs_singleton_key_check CHECK (singleton_key = 1),
    ADD UNIQUE KEY ldap_configs_singleton_key_key (singleton_key);
