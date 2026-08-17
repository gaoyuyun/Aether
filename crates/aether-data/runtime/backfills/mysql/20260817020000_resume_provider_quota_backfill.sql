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
    cutover.cutover_delta_sequence,
    UNIX_TIMESTAMP(),
    UNIX_TIMESTAMP()
FROM providers AS provider
LEFT JOIN (
    SELECT
        delta.target_id AS provider_id,
        delta.quota_epoch_start_at_usage AS quota_epoch_start,
        MAX(delta.quota_delta_sequence) AS cutover_delta_sequence
    FROM usage_counter_deltas AS delta
    WHERE delta.kind = 'provider_monthly'
    GROUP BY delta.target_id, delta.quota_epoch_start_at_usage
) AS cutover
  ON cutover.provider_id = provider.id
 AND cutover.quota_epoch_start = provider.quota_last_reset_at
WHERE provider.billing_type = 'monthly_quota'
  AND provider.quota_last_reset_at IS NOT NULL
ON DUPLICATE KEY UPDATE
    provider_id = provider_quota_maintenance_state.provider_id;

UPDATE provider_quota_window_counters AS counter
JOIN provider_quota_maintenance_state AS task
  ON task.provider_id = counter.provider_id
 AND task.quota_epoch_start = counter.quota_epoch_start
 AND task.task_kind = 'historical_backfill'
 AND task.status IN ('pending', 'running')
SET counter.status = 'rebuilding',
    counter.rebuild_error = NULL,
    counter.updated_at = UNIX_TIMESTAMP();
