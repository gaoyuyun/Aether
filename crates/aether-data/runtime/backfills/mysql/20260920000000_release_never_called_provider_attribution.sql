-- Release provider attribution from requests that never reached an upstream.
--
-- See the SQLite copy of this backfill for why these rows exist and why both facts
-- (a local execution runtime miss, plus a skip reason on the candidate the row
-- names) are required to identify one.
--
-- MySQL cannot read the table it is updating from inside a subquery, so the two
-- statements that touch `usage` and `usage_routing_snapshots` together are written
-- as multi-table updates rather than with `WHERE request_id IN (...)`.
--
-- Statement order matters: the counters are corrected while the rows still carry the
-- identity they were counted under, then the routing snapshot is cleared, then the
-- usage rows. Re-running matches nothing, so this is idempotent.

UPDATE provider_api_keys
SET request_count = GREATEST(
      COALESCE(provider_api_keys.request_count, 0) - (
        SELECT COUNT(*)
        FROM `usage`
        LEFT JOIN usage_routing_snapshots
          ON usage_routing_snapshots.request_id = `usage`.request_id
        WHERE `usage`.provider_api_key_id = provider_api_keys.id
          AND NULLIF(TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.routing_candidate_skip_reason')), '')), '') IS NOT NULL
          AND 'local_execution_runtime_miss' IN (
            TRIM(COALESCE(`usage`.execution_path, '')),
            TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
            TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.execution_path')), ''))
          )
      ), 0),
    error_count = GREATEST(
      COALESCE(provider_api_keys.error_count, 0) - (
        SELECT COUNT(*)
        FROM `usage`
        LEFT JOIN usage_routing_snapshots
          ON usage_routing_snapshots.request_id = `usage`.request_id
        WHERE `usage`.provider_api_key_id = provider_api_keys.id
          AND NULLIF(TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.routing_candidate_skip_reason')), '')), '') IS NOT NULL
          AND 'local_execution_runtime_miss' IN (
            TRIM(COALESCE(`usage`.execution_path, '')),
            TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
            TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.execution_path')), ''))
          )
          AND `usage`.status NOT IN ('pending', 'streaming')
          AND NOT (
            `usage`.status IN ('completed', 'success', 'ok', 'billed', 'settled')
            AND (`usage`.status_code IS NULL OR `usage`.status_code < 400)
            AND (`usage`.error_message IS NULL OR TRIM(`usage`.error_message) = '')
          )
      ), 0)
WHERE EXISTS (
  SELECT 1
  FROM `usage`
  LEFT JOIN usage_routing_snapshots
    ON usage_routing_snapshots.request_id = `usage`.request_id
  WHERE `usage`.provider_api_key_id = provider_api_keys.id
    AND NULLIF(TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.routing_candidate_skip_reason')), '')), '') IS NOT NULL
    AND 'local_execution_runtime_miss' IN (
      TRIM(COALESCE(`usage`.execution_path, '')),
      TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
      TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(`usage`.request_metadata, '$.execution_path')), ''))
    )
);

UPDATE usage_routing_snapshots AS routing
JOIN `usage` AS usage_row
  ON usage_row.request_id = routing.request_id
SET routing.selected_provider_id = NULL,
    routing.selected_endpoint_id = NULL,
    routing.selected_provider_api_key_id = NULL
WHERE NULLIF(TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(usage_row.request_metadata, '$.routing_candidate_skip_reason')), '')), '') IS NOT NULL
  AND 'local_execution_runtime_miss' IN (
    TRIM(COALESCE(usage_row.execution_path, '')),
    TRIM(COALESCE(routing.execution_path, '')),
    TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(usage_row.request_metadata, '$.execution_path')), ''))
  );

UPDATE `usage` AS usage_row
LEFT JOIN usage_routing_snapshots AS routing
  ON routing.request_id = usage_row.request_id
SET usage_row.provider_name = 'unknown',
    usage_row.provider_id = NULL,
    usage_row.provider_endpoint_id = NULL,
    usage_row.provider_api_key_id = NULL
WHERE NULLIF(TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(usage_row.request_metadata, '$.routing_candidate_skip_reason')), '')), '') IS NOT NULL
  AND 'local_execution_runtime_miss' IN (
    TRIM(COALESCE(usage_row.execution_path, '')),
    TRIM(COALESCE(routing.execution_path, '')),
    TRIM(COALESCE(JSON_UNQUOTE(JSON_EXTRACT(usage_row.request_metadata, '$.execution_path')), ''))
  );
