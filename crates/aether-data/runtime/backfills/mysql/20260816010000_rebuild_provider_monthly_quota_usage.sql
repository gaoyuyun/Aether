-- Usage timestamps are Unix seconds despite the legacy *_unix_ms column name.
-- Rebuild provider monthly counters from settled usage so rows lost by the
-- double conversion in the quota-window release are recovered idempotently.
UPDATE providers AS provider
LEFT JOIN (
    SELECT
        provider_source.id,
        COALESCE(SUM(
            CASE
                WHEN COALESCE(snapshot.billing_status, usage_record.billing_status) = 'settled'
                THEN GREATEST(COALESCE(snapshot.billing_actual_total_cost_usd, usage_record.actual_total_cost_usd, 0), 0)
                ELSE 0
            END
        ), 0) AS actual_total_cost_usd
    FROM providers AS provider_source
    LEFT JOIN `usage` AS usage_record
      ON usage_record.provider_id = provider_source.id
     AND usage_record.created_at_unix_ms >= COALESCE(provider_source.quota_last_reset_at, 0)
     AND usage_record.created_at_unix_ms <= UNIX_TIMESTAMP()
    LEFT JOIN usage_settlement_snapshots AS snapshot
      ON snapshot.request_id = usage_record.request_id
    WHERE provider_source.billing_type IN ('monthly_quota', 'free_tier')
    GROUP BY provider_source.id
) AS totals
  ON totals.id = provider.id
SET provider.monthly_used_usd = COALESCE(totals.actual_total_cost_usd, 0),
    provider.updated_at = UNIX_TIMESTAMP()
WHERE provider.billing_type IN ('monthly_quota', 'free_tier');
