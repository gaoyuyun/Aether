INSERT INTO public.provider_quota_maintenance_state (
    provider_id, quota_epoch_start, task_kind, status,
    cursor_dispatch_at, cursor_request_id, cutover_delta_sequence,
    created_at, updated_at
)
SELECT
    provider.id,
    FLOOR(EXTRACT(EPOCH FROM provider.quota_last_reset_at))::bigint,
    'historical_backfill',
    'pending',
    FLOOR(EXTRACT(EPOCH FROM provider.quota_last_reset_at))::bigint,
    '',
    (
        SELECT MAX(delta.quota_delta_sequence)
        FROM public.usage_counter_deltas AS delta
        WHERE delta.kind = 'provider_monthly'
          AND delta.target_id = provider.id
          AND delta.quota_epoch_start_at_usage = FLOOR(EXTRACT(EPOCH FROM provider.quota_last_reset_at))::bigint
    ),
    NOW(),
    NOW()
FROM public.providers AS provider
WHERE provider.billing_type = 'monthly_quota'
  AND provider.quota_last_reset_at IS NOT NULL
ON CONFLICT (provider_id, quota_epoch_start, task_kind) DO NOTHING;

UPDATE public.provider_quota_window_counters AS counter
SET status = 'rebuilding',
    rebuild_error = NULL,
    updated_at = NOW()
FROM public.provider_quota_maintenance_state AS task
WHERE task.provider_id = counter.provider_id
  AND TO_TIMESTAMP(task.quota_epoch_start::double precision) = counter.quota_epoch_start
  AND task.task_kind = 'historical_backfill'
  AND task.status IN ('pending', 'running');
