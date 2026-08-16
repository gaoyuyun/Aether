-- Usage timestamps are Unix seconds despite the legacy *_unix_ms column name.
-- Rebuild provider monthly counters from settled usage so rows lost by the
-- double conversion in the quota-window release are recovered idempotently.
WITH provider_totals AS (
    SELECT
        provider.id,
        COALESCE(SUM(
            CASE
                WHEN COALESCE(snapshot.billing_status, usage_record.billing_status) = 'settled'
                THEN MAX(COALESCE(snapshot.billing_actual_total_cost_usd, usage_record.actual_total_cost_usd, 0), 0)
                ELSE 0
            END
        ), 0) AS actual_total_cost_usd
    FROM providers AS provider
    LEFT JOIN "usage" AS usage_record
      ON usage_record.provider_id = provider.id
     AND usage_record.created_at_unix_ms >= COALESCE(provider.quota_last_reset_at, 0)
     AND usage_record.created_at_unix_ms <= CAST(strftime('%s', 'now') AS INTEGER)
    LEFT JOIN usage_settlement_snapshots AS snapshot
      ON snapshot.request_id = usage_record.request_id
    WHERE provider.billing_type IN ('monthly_quota', 'free_tier')
    GROUP BY provider.id
)
UPDATE providers
SET monthly_used_usd = (
        SELECT actual_total_cost_usd
        FROM provider_totals
        WHERE provider_totals.id = providers.id
    ),
    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE billing_type IN ('monthly_quota', 'free_tier');
