WITH aggregated AS (
  SELECT
    provider_api_key_id,
    COUNT(*)::BIGINT AS request_count,
    COALESCE(SUM(
      CASE
        WHEN status IN ('completed', 'success', 'ok', 'billed', 'settled')
             AND (status_code IS NULL OR status_code < 400)
             AND NULLIF(BTRIM(error_message), '') IS NULL
        THEN 1
        ELSE 0
      END
    ), 0)::BIGINT AS success_count,
    COALESCE(SUM(
      CASE
        WHEN status NOT IN ('pending', 'streaming')
             AND NOT (
               status IN ('completed', 'success', 'ok', 'billed', 'settled')
               AND (status_code IS NULL OR status_code < 400)
               AND NULLIF(BTRIM(error_message), '') IS NULL
             )
        THEN 1
        ELSE 0
      END
    ), 0)::BIGINT AS error_count,
    COALESCE(SUM(
      CASE
        WHEN status IN ('pending', 'streaming') THEN 0
        ELSE GREATEST(
          COALESCE(total_tokens, 0),
          0
        )::BIGINT
      END
    ), 0)::BIGINT AS total_tokens,
    COALESCE(SUM(
      CASE
        WHEN status IN ('pending', 'streaming') THEN 0
        ELSE COALESCE(total_cost_usd, 0)
      END
    ), 0)::NUMERIC(20,8) AS total_cost_usd,
    COALESCE(SUM(
      CASE
        WHEN status IN ('completed', 'success', 'ok', 'billed', 'settled')
             AND (status_code IS NULL OR status_code < 400)
             AND NULLIF(BTRIM(error_message), '') IS NULL
             AND response_time_ms IS NOT NULL
        THEN GREATEST(response_time_ms, 0)
        ELSE 0
      END
    ), 0)::BIGINT AS total_response_time_ms,
    MAX(created_at) AS last_used_at
  FROM usage_billing_facts AS "usage"
  WHERE provider_api_key_id IS NOT NULL
    AND BTRIM(provider_api_key_id) <> ''
    -- Skip a row that names a provider key the request never reached. It mirrors
    -- `usage_names_a_provider_key_that_was_never_called` in the data contracts,
    -- which applies the same rule on the incremental counter path; without it a
    -- rebuild would put back the request and error counts that path took out.
    --
    -- Both facts are required. A locally rejected request always ends in a runtime
    -- miss, but so does a genuine exhaustion whose upstream really did fail; only a
    -- candidate rejected before dispatch carries a skip reason.
    --
    -- The billing view exposes neither the routing columns nor the request
    -- metadata, so both have to be read back off the base tables.
    AND NOT EXISTS (
      SELECT 1
      FROM public."usage" AS never_called_usage
      LEFT JOIN public.usage_routing_snapshots AS never_called_routing
        ON never_called_routing.request_id = never_called_usage.request_id
      WHERE never_called_usage.request_id = "usage".request_id
        AND NULLIF(BTRIM(COALESCE(never_called_usage.request_metadata->>'routing_candidate_skip_reason', '')), '') IS NOT NULL
        AND (
          BTRIM(COALESCE(never_called_routing.execution_path, '')) = 'local_execution_runtime_miss'
          OR BTRIM(COALESCE(never_called_usage.request_metadata->>'execution_path', '')) = 'local_execution_runtime_miss'
        )
    )
  GROUP BY provider_api_key_id
)
UPDATE provider_api_keys
SET
  request_count = aggregated.request_count,
  success_count = aggregated.success_count,
  error_count = aggregated.error_count,
  total_tokens = aggregated.total_tokens,
  total_cost_usd = aggregated.total_cost_usd,
  total_response_time_ms = aggregated.total_response_time_ms,
  last_used_at = aggregated.last_used_at
FROM aggregated
WHERE provider_api_keys.id = aggregated.provider_api_key_id
