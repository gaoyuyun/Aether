-- LDAP configuration is a database-wide singleton. Preserve the row selected by the legacy
-- reader (the smallest id), remove historical duplicates, and let the database arbitrate
-- concurrent first creation.
DELETE FROM ldap_configs
WHERE id <> (SELECT MIN(id) FROM ldap_configs);

ALTER TABLE ldap_configs
ADD COLUMN singleton_key INTEGER NOT NULL DEFAULT 1 CHECK (singleton_key = 1);

CREATE UNIQUE INDEX ldap_configs_singleton_key_key
ON ldap_configs (singleton_key);
