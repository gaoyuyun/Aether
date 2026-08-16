INSERT INTO provider_quota_usage_buckets (
    provider_id, quota_epoch_start, bucket_start, used_usd, updated_at
)
SELECT
    provider.id,
    provider.quota_last_reset_at,
    FLOOR(usage_record.created_at_unix_ms / 60) * 60,
    SUM(GREATEST(COALESCE(
        CAST(JSON_UNQUOTE(JSON_EXTRACT(snapshot.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
        snapshot.billing_actual_total_cost_usd,
        usage_record.actual_total_cost_usd,
        0
    ), 0)),
    UNIX_TIMESTAMP()
FROM providers AS provider
JOIN `usage` AS usage_record
  ON usage_record.provider_id = provider.id
 AND usage_record.created_at_unix_ms >= provider.quota_last_reset_at
 AND usage_record.created_at_unix_ms < FLOOR(UNIX_TIMESTAMP() / 60) * 60
JOIN usage_settlement_snapshots AS snapshot
  ON snapshot.request_id = usage_record.request_id
WHERE provider.quota_last_reset_at IS NOT NULL
  AND JSON_UNQUOTE(JSON_EXTRACT(snapshot.settlement_snapshot, '$.pricing_snapshot.provider_billing_type')) = 'monthly_quota'
GROUP BY provider.id, provider.quota_last_reset_at, FLOOR(usage_record.created_at_unix_ms / 60) * 60
ON DUPLICATE KEY UPDATE
    used_usd = VALUES(used_usd),
    updated_at = VALUES(updated_at);

UPDATE providers AS provider
LEFT JOIN (
    SELECT bucket.provider_id, bucket.quota_epoch_start, SUM(bucket.used_usd) AS used_usd
    FROM provider_quota_usage_buckets AS bucket
    GROUP BY bucket.provider_id, bucket.quota_epoch_start
) AS totals
  ON totals.provider_id = provider.id
 AND totals.quota_epoch_start = provider.quota_last_reset_at
SET provider.monthly_used_usd = COALESCE(totals.used_usd, 0),
    provider.updated_at = UNIX_TIMESTAMP()
WHERE provider.quota_last_reset_at IS NOT NULL;

INSERT INTO provider_quota_window_counters (
    provider_id, duration_secs, quota_epoch_start, rolling_start,
    accounted_until, used_usd, status, rebuild_error, updated_at
)
SELECT
    definition.provider_id,
    definition.duration_secs,
    definition.quota_epoch_start,
    GREATEST(definition.quota_epoch_start, definition.clock_minute - definition.duration_secs),
    definition.clock_minute,
    COALESCE(SUM(bucket.used_usd), 0),
    'ready',
    NULL,
    UNIX_TIMESTAMP()
FROM (
    SELECT
        provider.id AS provider_id,
        provider.quota_last_reset_at AS quota_epoch_start,
        windows.duration_secs,
        FLOOR(UNIX_TIMESTAMP() / 60) * 60 AS clock_minute
    FROM providers AS provider
    JOIN JSON_TABLE(
        COALESCE(JSON_EXTRACT(provider.config, '$.quota_windows'), JSON_ARRAY()),
        '$[*]' COLUMNS (duration_secs BIGINT PATH '$.duration_secs')
    ) AS windows
    WHERE provider.quota_last_reset_at IS NOT NULL
      AND windows.duration_secs BETWEEN 60 AND 2592000
      AND MOD(windows.duration_secs, 60) = 0
) AS definition
LEFT JOIN provider_quota_usage_buckets AS bucket
  ON bucket.provider_id = definition.provider_id
 AND bucket.quota_epoch_start = definition.quota_epoch_start
 AND bucket.bucket_start >= GREATEST(
     definition.quota_epoch_start,
     definition.clock_minute - definition.duration_secs
 )
 AND bucket.bucket_start < definition.clock_minute
GROUP BY definition.provider_id, definition.duration_secs,
         definition.quota_epoch_start, definition.clock_minute
ON DUPLICATE KEY UPDATE
    quota_epoch_start = VALUES(quota_epoch_start),
    rolling_start = VALUES(rolling_start),
    accounted_until = VALUES(accounted_until),
    used_usd = VALUES(used_usd),
    status = VALUES(status),
    rebuild_error = NULL,
    updated_at = VALUES(updated_at);
