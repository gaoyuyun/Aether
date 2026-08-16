ALTER TABLE providers ADD COLUMN pending_quota_reset_at BIGINT NULL;
UPDATE providers
SET quota_last_reset_at = FLOOR(quota_last_reset_at / 60) * 60
WHERE quota_last_reset_at IS NOT NULL;

ALTER TABLE usage_counter_deltas
    ADD COLUMN provider_billing_type_at_usage VARCHAR(64) NULL,
    ADD COLUMN quota_epoch_start_at_usage BIGINT NULL,
    ADD COLUMN provider_dispatch_at_unix_secs BIGINT NULL,
    ADD COLUMN provider_quota_cost_usd DOUBLE NULL,
    ADD COLUMN pricing_rule_version_at_usage VARCHAR(128) NULL,
    ADD COLUMN provider_pricing_snapshot_at_usage JSON NULL,
    ADD COLUMN quota_delta_sequence BIGINT NOT NULL AUTO_INCREMENT UNIQUE,
    ADD COLUMN quota_accounting_status VARCHAR(32) NULL;

RENAME TABLE provider_quota_window_counters TO provider_quota_window_counters_legacy;

CREATE TABLE provider_quota_window_counters (
    provider_id VARCHAR(64) NOT NULL,
    duration_secs BIGINT NOT NULL,
    window_start BIGINT,
    quota_epoch_start BIGINT NOT NULL,
    rolling_start BIGINT NOT NULL,
    accounted_until BIGINT NOT NULL,
    used_usd DOUBLE NOT NULL DEFAULT 0,
    status VARCHAR(32) NOT NULL DEFAULT 'rebuilding',
    rebuild_error TEXT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider_id, duration_secs),
    CONSTRAINT provider_quota_window_counters_provider_id_fkey_v2
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);

CREATE TABLE provider_quota_usage_buckets (
    provider_id VARCHAR(64) NOT NULL,
    quota_epoch_start BIGINT NOT NULL,
    bucket_start BIGINT NOT NULL,
    used_usd DOUBLE NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start, bucket_start),
    CONSTRAINT provider_quota_usage_buckets_provider_id_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
