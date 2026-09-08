ALTER TABLE routing_groups ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS routing_groups_enabled_sort_idx
    ON routing_groups (enabled, sort_order, name, id);
