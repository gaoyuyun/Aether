INSERT INTO public.provider_quota_usage_buckets (
    provider_id, quota_epoch_start, bucket_start, used_usd, updated_at
)
SELECT
    provider.id,
    provider.quota_last_reset_at,
    DATE_TRUNC('minute', usage_record.created_at),
    SUM(GREATEST(COALESCE(
        NULLIF(snapshot.settlement_snapshot ->> 'provider_quota_cost_usd', '')::double precision,
        snapshot.billing_actual_total_cost_usd,
        usage_record.actual_total_cost_usd,
        0
    ), 0)),
    NOW()
FROM public.providers AS provider
JOIN public.usage AS usage_record
  ON usage_record.provider_id = provider.id
 AND usage_record.created_at >= provider.quota_last_reset_at
 AND usage_record.created_at < DATE_TRUNC('minute', NOW())
JOIN public.usage_settlement_snapshots AS snapshot
  ON snapshot.request_id = usage_record.request_id
WHERE provider.quota_last_reset_at IS NOT NULL
  AND snapshot.settlement_snapshot #>> '{pricing_snapshot,provider_billing_type}' = 'monthly_quota'
GROUP BY provider.id, provider.quota_last_reset_at, DATE_TRUNC('minute', usage_record.created_at)
ON CONFLICT (provider_id, quota_epoch_start, bucket_start) DO UPDATE SET
    used_usd = EXCLUDED.used_usd,
    updated_at = EXCLUDED.updated_at;

UPDATE public.providers AS provider
SET monthly_used_usd = COALESCE(totals.used_usd, 0),
    updated_at = NOW()
FROM (
    SELECT source.id AS provider_id, COALESCE(SUM(bucket.used_usd), 0) AS used_usd
    FROM public.providers AS source
    LEFT JOIN public.provider_quota_usage_buckets AS bucket
      ON bucket.provider_id = source.id
     AND bucket.quota_epoch_start = source.quota_last_reset_at
    WHERE source.quota_last_reset_at IS NOT NULL
    GROUP BY source.id
) AS totals
WHERE provider.id = totals.provider_id;

WITH window_definitions AS (
    SELECT
        provider.id AS provider_id,
        provider.quota_last_reset_at AS quota_epoch_start,
        (window.value ->> 'duration_secs')::bigint AS duration_secs,
        DATE_TRUNC('minute', NOW()) AS clock_minute
    FROM public.providers AS provider
    CROSS JOIN LATERAL jsonb_array_elements(
        COALESCE(provider.config -> 'quota_windows', '[]'::jsonb)
    ) AS window(value)
    WHERE provider.quota_last_reset_at IS NOT NULL
      AND (window.value ->> 'duration_secs')::bigint BETWEEN 60 AND 2592000
      AND MOD((window.value ->> 'duration_secs')::bigint, 60) = 0
)
INSERT INTO public.provider_quota_window_counters (
    provider_id, duration_secs, quota_epoch_start, rolling_start,
    accounted_until, used_usd, status, rebuild_error, updated_at
)
SELECT
    definition.provider_id,
    definition.duration_secs,
    definition.quota_epoch_start,
    GREATEST(
        definition.quota_epoch_start,
        definition.clock_minute - MAKE_INTERVAL(secs => definition.duration_secs::double precision)
    ),
    definition.clock_minute,
    COALESCE(SUM(bucket.used_usd), 0),
    'ready',
    NULL,
    NOW()
FROM window_definitions AS definition
LEFT JOIN public.provider_quota_usage_buckets AS bucket
  ON bucket.provider_id = definition.provider_id
 AND bucket.quota_epoch_start = definition.quota_epoch_start
 AND bucket.bucket_start >= GREATEST(
     definition.quota_epoch_start,
     definition.clock_minute - MAKE_INTERVAL(secs => definition.duration_secs::double precision)
 )
 AND bucket.bucket_start < definition.clock_minute
GROUP BY definition.provider_id, definition.duration_secs,
         definition.quota_epoch_start, definition.clock_minute
ON CONFLICT (provider_id, duration_secs) DO UPDATE SET
    quota_epoch_start = EXCLUDED.quota_epoch_start,
    rolling_start = EXCLUDED.rolling_start,
    accounted_until = EXCLUDED.accounted_until,
    used_usd = EXCLUDED.used_usd,
    status = EXCLUDED.status,
    rebuild_error = NULL,
    updated_at = EXCLUDED.updated_at;
