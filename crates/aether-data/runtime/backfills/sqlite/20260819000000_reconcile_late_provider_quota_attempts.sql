-- Candidate lifecycle writes and settlement writes use independent queues. If settlement
-- commits first, recover the subsequently inserted unresolved attempt from the finalized
-- candidate and settlement snapshots without double-applying an already processed delta.
UPDATE usage_counter_deltas AS delta
SET provider_quota_cost_usd = COALESCE((
      SELECT COALESCE(
        CAST(json_extract(settlement.settlement_snapshot, '$.provider_quota_cost_usd') AS REAL),
        CAST(settlement.billing_actual_total_cost_usd AS REAL),
        CAST(usage_record.actual_total_cost_usd AS REAL)
      )
      FROM request_candidates AS candidate
      JOIN "usage" AS usage_record ON usage_record.request_id = candidate.request_id
      JOIN usage_settlement_snapshots AS settlement ON settlement.request_id = candidate.request_id
      WHERE candidate.id = delta.request_id
        AND candidate.provider_id = usage_record.provider_id
        AND candidate.status = 'success'
        AND settlement.billing_status = 'settled'
        AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
        AND (
          json_extract(settlement.settlement_snapshot, '$.status') = 'complete'
          OR lower(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
        )
      LIMIT 1
    ), 0),
    total_cost_usd_delta = COALESCE((
      SELECT COALESCE(
        CAST(json_extract(settlement.settlement_snapshot, '$.provider_quota_cost_usd') AS REAL),
        CAST(settlement.billing_actual_total_cost_usd AS REAL),
        CAST(usage_record.actual_total_cost_usd AS REAL)
      )
      FROM request_candidates AS candidate
      JOIN "usage" AS usage_record ON usage_record.request_id = candidate.request_id
      JOIN usage_settlement_snapshots AS settlement ON settlement.request_id = candidate.request_id
      WHERE candidate.id = delta.request_id
        AND candidate.provider_id = usage_record.provider_id
        AND candidate.status = 'success'
        AND settlement.billing_status = 'settled'
        AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
        AND (
          json_extract(settlement.settlement_snapshot, '$.status') = 'complete'
          OR lower(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
        )
      LIMIT 1
    ), 0),
    quota_accounting_status = 'ready'
WHERE delta.kind = 'provider_monthly'
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND delta.processed_at IS NULL
  AND EXISTS (
    SELECT 1
    FROM request_candidates AS candidate
    JOIN "usage" AS usage_record ON usage_record.request_id = candidate.request_id
    JOIN usage_settlement_snapshots AS settlement ON settlement.request_id = candidate.request_id
    WHERE candidate.id = delta.request_id
      AND candidate.provider_id = usage_record.provider_id
      AND candidate.status = 'success'
      AND settlement.billing_status = 'settled'
      AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
      AND (
        json_extract(settlement.settlement_snapshot, '$.status') = 'complete'
        OR lower(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
      )
  );
