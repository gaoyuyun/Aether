INSERT INTO provider_quota_maintenance_state (
    provider_id, quota_epoch_start, task_kind, status,
    cursor_dispatch_at, cursor_request_id, cutover_delta_sequence,
    created_at, updated_at
)
SELECT
    provider.id,
    provider.quota_last_reset_at,
    'historical_backfill',
    'pending',
    provider.quota_last_reset_at,
    '',
    (
        SELECT MAX(delta.quota_delta_sequence)
        FROM usage_counter_deltas AS delta
        WHERE delta.kind = 'provider_monthly'
          AND delta.target_id = provider.id
          AND delta.quota_epoch_start_at_usage = provider.quota_last_reset_at
    ),
    CAST(strftime('%s', 'now') AS INTEGER),
    CAST(strftime('%s', 'now') AS INTEGER)
FROM providers AS provider
WHERE provider.billing_type = 'monthly_quota'
  AND provider.quota_last_reset_at IS NOT NULL
ON CONFLICT (provider_id, quota_epoch_start, task_kind) DO NOTHING;

UPDATE provider_quota_window_counters
SET status = 'rebuilding',
    rebuild_error = NULL,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE EXISTS (
    SELECT 1
    FROM provider_quota_maintenance_state AS task
    WHERE task.provider_id = provider_quota_window_counters.provider_id
      AND task.quota_epoch_start = provider_quota_window_counters.quota_epoch_start
      AND task.task_kind = 'historical_backfill'
      AND task.status IN ('pending', 'running')
);
