ALTER TABLE providers ADD COLUMN pending_quota_reset_at INTEGER;
UPDATE providers
SET quota_last_reset_at = (quota_last_reset_at / 60) * 60
WHERE quota_last_reset_at IS NOT NULL;

ALTER TABLE usage_counter_deltas ADD COLUMN provider_billing_type_at_usage TEXT;
ALTER TABLE usage_counter_deltas ADD COLUMN quota_epoch_start_at_usage INTEGER;
ALTER TABLE usage_counter_deltas ADD COLUMN provider_dispatch_at_unix_secs INTEGER;
ALTER TABLE usage_counter_deltas ADD COLUMN provider_quota_cost_usd REAL;
ALTER TABLE usage_counter_deltas ADD COLUMN pricing_rule_version_at_usage TEXT;
ALTER TABLE usage_counter_deltas ADD COLUMN provider_pricing_snapshot_at_usage TEXT;
ALTER TABLE usage_counter_deltas ADD COLUMN quota_delta_sequence INTEGER;
ALTER TABLE usage_counter_deltas ADD COLUMN quota_accounting_status TEXT;

ALTER TABLE provider_quota_window_counters
    RENAME TO provider_quota_window_counters_legacy;

CREATE TABLE provider_quota_window_counters (
    provider_id TEXT NOT NULL,
    duration_secs INTEGER NOT NULL,
    window_start INTEGER,
    quota_epoch_start INTEGER NOT NULL,
    rolling_start INTEGER NOT NULL,
    accounted_until INTEGER NOT NULL,
    used_usd REAL NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'rebuilding',
    rebuild_error TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, duration_secs),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);

CREATE TABLE provider_quota_usage_buckets (
    provider_id TEXT NOT NULL,
    quota_epoch_start INTEGER NOT NULL,
    bucket_start INTEGER NOT NULL,
    used_usd REAL NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, quota_epoch_start, bucket_start),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
