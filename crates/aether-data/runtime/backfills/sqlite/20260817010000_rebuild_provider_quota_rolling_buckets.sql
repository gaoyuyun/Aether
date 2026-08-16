-- Historical rows without an explicit monthly billing snapshot remain excluded.
INSERT INTO provider_quota_usage_buckets (
    provider_id, quota_epoch_start, bucket_start, used_usd, updated_at
)
SELECT
    provider.id,
    provider.quota_last_reset_at,
    (usage_record.created_at_unix_ms / 60) * 60,
    SUM(MAX(COALESCE(
        json_extract(snapshot.settlement_snapshot, '$.provider_quota_cost_usd'),
        snapshot.billing_actual_total_cost_usd,
        usage_record.actual_total_cost_usd,
        0
    ), 0)),
    CAST(strftime('%s', 'now') AS INTEGER)
FROM providers AS provider
JOIN "usage" AS usage_record
  ON usage_record.provider_id = provider.id
 AND usage_record.created_at_unix_ms >= provider.quota_last_reset_at
 AND usage_record.created_at_unix_ms < (CAST(strftime('%s', 'now') AS INTEGER) / 60) * 60
JOIN usage_settlement_snapshots AS snapshot
  ON snapshot.request_id = usage_record.request_id
WHERE provider.quota_last_reset_at IS NOT NULL
  AND json_extract(snapshot.settlement_snapshot, '$.pricing_snapshot.provider_billing_type') = 'monthly_quota'
GROUP BY provider.id, provider.quota_last_reset_at, (usage_record.created_at_unix_ms / 60) * 60
ON CONFLICT (provider_id, quota_epoch_start, bucket_start) DO UPDATE SET
    used_usd = excluded.used_usd,
    updated_at = excluded.updated_at;

UPDATE providers
SET monthly_used_usd = COALESCE((
        SELECT SUM(bucket.used_usd)
        FROM provider_quota_usage_buckets AS bucket
        WHERE bucket.provider_id = providers.id
          AND bucket.quota_epoch_start = providers.quota_last_reset_at
    ), 0),
    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE quota_last_reset_at IS NOT NULL;

WITH window_definitions AS (
    SELECT
        provider.id AS provider_id,
        provider.quota_last_reset_at AS quota_epoch_start,
        CAST(json_extract(window.value, '$.duration_secs') AS INTEGER) AS duration_secs
    FROM providers AS provider,
         json_each(COALESCE(provider.config, '{}'), '$.quota_windows') AS window
    WHERE provider.quota_last_reset_at IS NOT NULL
      AND CAST(json_extract(window.value, '$.duration_secs') AS INTEGER) BETWEEN 60 AND 2592000
      AND CAST(json_extract(window.value, '$.duration_secs') AS INTEGER) % 60 = 0
), clock AS (
    SELECT (CAST(strftime('%s', 'now') AS INTEGER) / 60) * 60 AS clock_minute
)
INSERT INTO provider_quota_window_counters (
    provider_id, duration_secs, quota_epoch_start, rolling_start,
    accounted_until, used_usd, status, rebuild_error, updated_at
)
SELECT
    definition.provider_id,
    definition.duration_secs,
    definition.quota_epoch_start,
    MAX(definition.quota_epoch_start, clock.clock_minute - definition.duration_secs),
    clock.clock_minute,
    COALESCE((
        SELECT SUM(bucket.used_usd)
        FROM provider_quota_usage_buckets AS bucket
        WHERE bucket.provider_id = definition.provider_id
          AND bucket.quota_epoch_start = definition.quota_epoch_start
          AND bucket.bucket_start >= MAX(
              definition.quota_epoch_start,
              clock.clock_minute - definition.duration_secs
          )
          AND bucket.bucket_start < clock.clock_minute
    ), 0),
    'ready',
    NULL,
    CAST(strftime('%s', 'now') AS INTEGER)
FROM window_definitions AS definition
CROSS JOIN clock
WHERE TRUE
ON CONFLICT (provider_id, duration_secs) DO UPDATE SET
    quota_epoch_start = excluded.quota_epoch_start,
    rolling_start = excluded.rolling_start,
    accounted_until = excluded.accounted_until,
    used_usd = excluded.used_usd,
    status = excluded.status,
    rebuild_error = NULL,
    updated_at = excluded.updated_at;
