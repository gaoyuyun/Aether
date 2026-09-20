-- Release provider attribution from requests that never reached an upstream.
--
-- See the SQLite copy of this backfill for why these rows exist and why both facts
-- (a local execution runtime miss, plus a skip reason on the candidate the row
-- names) are required to identify one.
--
-- On Postgres the routing fields live only on `usage_routing_snapshots` and in the
-- request metadata mirror, never as `usage` columns, so the predicate reads both.
--
-- Statement order matters: the counters are corrected while the rows still carry the
-- identity they were counted under, then the routing snapshot is cleared, then the
-- usage rows. Re-running matches nothing, so this is idempotent.

WITH never_called AS (
    SELECT
        usage_record.request_id,
        usage_record.provider_api_key_id,
        usage_record.status,
        usage_record.status_code,
        usage_record.error_message
    FROM public.usage AS usage_record
    LEFT JOIN public.usage_routing_snapshots AS routing
      ON routing.request_id = usage_record.request_id
    WHERE NULLIF(BTRIM(COALESCE(usage_record.request_metadata ->> 'routing_candidate_skip_reason', '')), '') IS NOT NULL
      AND 'local_execution_runtime_miss' IN (
        BTRIM(COALESCE(routing.execution_path, '')),
        BTRIM(COALESCE(usage_record.request_metadata ->> 'execution_path', ''))
      )
),
counter_relief AS (
    SELECT
        provider_api_key_id,
        COUNT(*) AS request_count_relief,
        COUNT(*) FILTER (
            WHERE status NOT IN ('pending', 'streaming')
              AND NOT (
                status IN ('completed', 'success', 'ok', 'billed', 'settled')
                AND (status_code IS NULL OR status_code < 400)
                AND NULLIF(BTRIM(COALESCE(error_message, '')), '') IS NULL
              )
        ) AS error_count_relief
    FROM never_called
    WHERE provider_api_key_id IS NOT NULL
      AND BTRIM(provider_api_key_id) <> ''
    GROUP BY provider_api_key_id
)
UPDATE public.provider_api_keys AS keys
SET request_count = GREATEST(COALESCE(keys.request_count, 0) - counter_relief.request_count_relief, 0),
    error_count = GREATEST(COALESCE(keys.error_count, 0) - counter_relief.error_count_relief, 0)
FROM counter_relief
WHERE keys.id = counter_relief.provider_api_key_id;

UPDATE public.usage_routing_snapshots AS routing
SET selected_provider_id = NULL,
    selected_endpoint_id = NULL,
    selected_provider_api_key_id = NULL
FROM public.usage AS usage_record
WHERE usage_record.request_id = routing.request_id
  AND NULLIF(BTRIM(COALESCE(usage_record.request_metadata ->> 'routing_candidate_skip_reason', '')), '') IS NOT NULL
  AND 'local_execution_runtime_miss' IN (
    BTRIM(COALESCE(routing.execution_path, '')),
    BTRIM(COALESCE(usage_record.request_metadata ->> 'execution_path', ''))
  );

UPDATE public.usage AS usage_record
SET provider_name = 'unknown',
    provider_id = NULL,
    provider_endpoint_id = NULL,
    provider_api_key_id = NULL
WHERE NULLIF(BTRIM(COALESCE(usage_record.request_metadata ->> 'routing_candidate_skip_reason', '')), '') IS NOT NULL
  AND 'local_execution_runtime_miss' IN (
    BTRIM(COALESCE(usage_record.request_metadata ->> 'execution_path', '')),
    BTRIM(COALESCE((
      SELECT routing.execution_path
      FROM public.usage_routing_snapshots AS routing
      WHERE routing.request_id = usage_record.request_id
    ), ''))
  );
