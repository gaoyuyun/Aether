-- A pre-20260817 settlement path could mark an unresolved monthly attempt as processed
-- before its final billing snapshot arrived. Recover only attempts whose settled snapshot
-- explicitly proves that the provider cost was zero; unresolved or non-zero attempts remain
-- fail-closed until they can be reconciled without losing quota accounting.
UPDATE public.usage_counter_deltas AS delta
SET quota_accounting_status = 'ready'
FROM public.usage_routing_snapshots AS routing
JOIN public.usage AS usage_record
  ON usage_record.request_id = routing.request_id
JOIN public.usage_settlement_snapshots AS settlement
  ON settlement.request_id = routing.request_id
WHERE routing.candidate_id = delta.request_id
  AND delta.kind = 'provider_monthly'
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND delta.processed_at IS NOT NULL
  AND ABS(COALESCE(delta.provider_quota_cost_usd, delta.total_cost_usd_delta)) <= 0.00000001
  AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
  AND settlement.billing_status = 'settled'
  AND settlement.settlement_snapshot ? 'provider_quota_cost_usd'
  AND ABS(
    (settlement.settlement_snapshot ->> 'provider_quota_cost_usd')::double precision
  ) <= 0.00000001;
