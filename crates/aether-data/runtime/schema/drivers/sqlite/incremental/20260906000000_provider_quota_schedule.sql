-- Separate subscription history, natural cycle scheduling and accounting epochs.
ALTER TABLE providers ADD COLUMN quota_subscription_started_at INTEGER;
ALTER TABLE providers ADD COLUMN quota_cycle_start_at INTEGER;
ALTER TABLE providers ADD COLUMN pending_quota_reset_mode VARCHAR(20);
ALTER TABLE providers ADD COLUMN pending_quota_reset_days INTEGER;
ALTER TABLE providers ADD COLUMN pending_quota_reset_usage BOOLEAN;
-- The original activation time may already have been overwritten by older versions.
-- Preserve the best available time without resetting usage or rewriting dispatch snapshots.
UPDATE providers SET quota_subscription_started_at = quota_last_reset_at, quota_cycle_start_at = quota_last_reset_at;
