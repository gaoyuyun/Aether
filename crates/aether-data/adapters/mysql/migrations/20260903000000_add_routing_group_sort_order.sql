ALTER TABLE routing_groups
    ADD COLUMN sort_order BIGINT NOT NULL DEFAULT 0,
    ADD KEY routing_groups_enabled_sort_idx (enabled, sort_order, name, id);
