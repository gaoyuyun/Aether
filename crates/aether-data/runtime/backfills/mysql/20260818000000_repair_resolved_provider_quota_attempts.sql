-- A pre-20260817 settlement path could mark an unresolved monthly attempt as processed
-- before its final billing snapshot arrived. Recover only attempts whose settled snapshot
-- explicitly proves that the provider cost was zero; unresolved or non-zero attempts remain
-- fail-closed until they can be reconciled without losing quota accounting.
UPDATE usage_counter_deltas AS delta
JOIN usage_routing_snapshots AS routing
  ON routing.candidate_id = delta.request_id
JOIN usage AS usage_record
  ON usage_record.request_id = routing.request_id
JOIN usage_settlement_snapshots AS settlement
  ON settlement.request_id = routing.request_id
SET delta.quota_accounting_status = 'ready'
WHERE delta.kind = 'provider_monthly'
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND delta.processed_at IS NOT NULL
  AND ABS(COALESCE(delta.provider_quota_cost_usd, delta.total_cost_usd_delta)) <= 0.00000001
  AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
  AND settlement.billing_status = 'settled'
  AND JSON_CONTAINS_PATH(settlement.settlement_snapshot, 'one', '$.provider_quota_cost_usd')
  AND ABS(
    CAST(JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE)
  ) <= 0.00000001;
