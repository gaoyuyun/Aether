CREATE TABLE provider_quota_maintenance_state (
    provider_id VARCHAR(64) NOT NULL,
    quota_epoch_start BIGINT NOT NULL,
    task_kind VARCHAR(32) NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'pending',
    cursor_dispatch_at BIGINT NOT NULL DEFAULT 0,
    cursor_request_id VARCHAR(128) NOT NULL DEFAULT '',
    cutover_delta_sequence BIGINT,
    absorbed_delta_sequence BIGINT NOT NULL DEFAULT 0,
    included_rows BIGINT NOT NULL DEFAULT 0,
    excluded_payg_rows BIGINT NOT NULL DEFAULT 0,
    excluded_free_tier_rows BIGINT NOT NULL DEFAULT 0,
    unknown_rows BIGINT NOT NULL DEFAULT 0,
    lock_owner VARCHAR(128),
    lock_expires_at BIGINT,
    last_error TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start, task_kind),
    KEY ix_provider_quota_maintenance_state_status (status, updated_at),
    CONSTRAINT provider_quota_maintenance_state_provider_id_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);

CREATE INDEX ix_usage_counter_deltas_provider_quota_pending
    ON usage_counter_deltas (
        kind, target_id(191), quota_accounting_status, quota_delta_sequence
    );

CREATE TABLE provider_quota_applied_watermarks (
    provider_id VARCHAR(64) NOT NULL,
    quota_epoch_start BIGINT NOT NULL,
    applied_delta_sequence BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start),
    CONSTRAINT provider_quota_applied_watermarks_provider_id_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
