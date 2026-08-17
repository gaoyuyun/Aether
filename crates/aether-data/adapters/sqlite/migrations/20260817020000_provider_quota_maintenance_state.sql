CREATE TABLE provider_quota_maintenance_state (
    provider_id TEXT NOT NULL,
    quota_epoch_start INTEGER NOT NULL,
    task_kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    cursor_dispatch_at INTEGER NOT NULL DEFAULT 0,
    cursor_request_id TEXT NOT NULL DEFAULT '',
    cutover_delta_sequence INTEGER,
    absorbed_delta_sequence INTEGER NOT NULL DEFAULT 0,
    included_rows INTEGER NOT NULL DEFAULT 0,
    excluded_payg_rows INTEGER NOT NULL DEFAULT 0,
    excluded_free_tier_rows INTEGER NOT NULL DEFAULT 0,
    unknown_rows INTEGER NOT NULL DEFAULT 0,
    lock_owner TEXT,
    lock_expires_at INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start, task_kind),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);

CREATE INDEX ix_provider_quota_maintenance_state_status
    ON provider_quota_maintenance_state (status, updated_at);

CREATE INDEX IF NOT EXISTS ix_usage_counter_deltas_provider_quota_pending
    ON usage_counter_deltas (
        kind, target_id, quota_accounting_status, quota_delta_sequence
    );

CREATE UNIQUE INDEX IF NOT EXISTS ix_usage_counter_deltas_quota_sequence
    ON usage_counter_deltas (quota_delta_sequence);

CREATE TABLE provider_quota_applied_watermarks (
    provider_id TEXT NOT NULL,
    quota_epoch_start INTEGER NOT NULL,
    applied_delta_sequence INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
