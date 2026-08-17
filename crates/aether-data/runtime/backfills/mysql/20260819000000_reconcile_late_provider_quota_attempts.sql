-- Candidate lifecycle writes and settlement writes use independent queues. If settlement
-- commits first, recover the subsequently inserted unresolved attempt from the finalized
-- candidate and settlement snapshots without double-applying an already processed delta.
UPDATE usage_counter_deltas AS delta
JOIN request_candidates AS candidate ON candidate.id = delta.request_id
JOIN usage AS usage_record ON usage_record.request_id = candidate.request_id
JOIN usage_settlement_snapshots AS settlement ON settlement.request_id = candidate.request_id
SET delta.provider_quota_cost_usd = COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
      settlement.billing_actual_total_cost_usd,
      usage_record.actual_total_cost_usd,
      0
    ),
    delta.total_cost_usd_delta = COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
      settlement.billing_actual_total_cost_usd,
      usage_record.actual_total_cost_usd,
      0
    ),
    delta.quota_accounting_status = 'ready'
WHERE delta.kind = 'provider_monthly'
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND delta.processed_at IS NULL
  AND candidate.provider_id = usage_record.provider_id
  AND candidate.status = 'success'
  AND settlement.billing_status = 'settled'
  AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
  AND (
    JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.status')) = 'complete'
    OR LOWER(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
  );
