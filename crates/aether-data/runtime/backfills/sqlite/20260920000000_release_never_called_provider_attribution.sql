-- Release provider attribution from requests that never reached an upstream.
--
-- A gateway that rejected a request locally used to attribute the usage row to the
-- last candidate it had looked at, even when that candidate was skipped before
-- dispatch. Two facts on the row prove it happened: the request ended in a local
-- execution runtime miss, and the candidate it names carries a skip reason, which
-- only a candidate rejected before dispatch ever has.
--
-- Naming that candidate charged its key a request and an error, which lowered the
-- key's success rate and cost it pool score for a request it never served. An
-- upstream that merely lacks an operation (no `count_tokens` endpoint) therefore
-- looked like an upstream that fails, and the channel rollups counted the request
-- against it.
--
-- The gateway no longer writes provider identity onto these rows, and both the
-- incremental counter path and the counter rebuild now skip them. This recovers the
-- counts already accumulated and releases the attribution the rows still carry.
--
-- Only `request_count` and `error_count` need correcting: such a row never reports
-- tokens or cost, and contributes response time only when it succeeded, which it
-- never did. `route_kind` and the miss reason are deliberately preserved, since they
-- are what still explains the row and what lets the records view recognise a token
-- counting request.
--
-- Statement order matters: the counters are corrected while the rows still carry the
-- identity they were counted under, then the routing snapshot is cleared, then the
-- usage rows. Re-running matches nothing, so this is idempotent.

UPDATE provider_api_keys
SET request_count = MAX(
      COALESCE(provider_api_keys.request_count, 0) - (
        SELECT COUNT(*)
        FROM "usage"
        LEFT JOIN usage_routing_snapshots
          ON usage_routing_snapshots.request_id = "usage".request_id
        WHERE "usage".provider_api_key_id = provider_api_keys.id
          AND NULLIF(TRIM(COALESCE(json_extract("usage".request_metadata, '$.routing_candidate_skip_reason'), '')), '') IS NOT NULL
          AND 'local_execution_runtime_miss' IN (
            TRIM(COALESCE("usage".execution_path, '')),
            TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
            TRIM(COALESCE(json_extract("usage".request_metadata, '$.execution_path'), ''))
          )
      ), 0),
    error_count = MAX(
      COALESCE(provider_api_keys.error_count, 0) - (
        SELECT COUNT(*)
        FROM "usage"
        LEFT JOIN usage_routing_snapshots
          ON usage_routing_snapshots.request_id = "usage".request_id
        WHERE "usage".provider_api_key_id = provider_api_keys.id
          AND NULLIF(TRIM(COALESCE(json_extract("usage".request_metadata, '$.routing_candidate_skip_reason'), '')), '') IS NOT NULL
          AND 'local_execution_runtime_miss' IN (
            TRIM(COALESCE("usage".execution_path, '')),
            TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
            TRIM(COALESCE(json_extract("usage".request_metadata, '$.execution_path'), ''))
          )
          AND "usage".status NOT IN ('pending', 'streaming')
          AND NOT (
            "usage".status IN ('completed', 'success', 'ok', 'billed', 'settled')
            AND ("usage".status_code IS NULL OR "usage".status_code < 400)
            AND ("usage".error_message IS NULL OR TRIM("usage".error_message) = '')
          )
      ), 0)
WHERE EXISTS (
  SELECT 1
  FROM "usage"
  LEFT JOIN usage_routing_snapshots
    ON usage_routing_snapshots.request_id = "usage".request_id
  WHERE "usage".provider_api_key_id = provider_api_keys.id
    AND NULLIF(TRIM(COALESCE(json_extract("usage".request_metadata, '$.routing_candidate_skip_reason'), '')), '') IS NOT NULL
    AND 'local_execution_runtime_miss' IN (
      TRIM(COALESCE("usage".execution_path, '')),
      TRIM(COALESCE(usage_routing_snapshots.execution_path, '')),
      TRIM(COALESCE(json_extract("usage".request_metadata, '$.execution_path'), ''))
    )
);

UPDATE usage_routing_snapshots
SET selected_provider_id = NULL,
    selected_endpoint_id = NULL,
    selected_provider_api_key_id = NULL
WHERE request_id IN (
  SELECT "usage".request_id
  FROM "usage"
  LEFT JOIN usage_routing_snapshots AS routing
    ON routing.request_id = "usage".request_id
  WHERE NULLIF(TRIM(COALESCE(json_extract("usage".request_metadata, '$.routing_candidate_skip_reason'), '')), '') IS NOT NULL
    AND 'local_execution_runtime_miss' IN (
      TRIM(COALESCE("usage".execution_path, '')),
      TRIM(COALESCE(routing.execution_path, '')),
      TRIM(COALESCE(json_extract("usage".request_metadata, '$.execution_path'), ''))
    )
);

UPDATE "usage"
SET provider_name = 'unknown',
    provider_id = NULL,
    provider_endpoint_id = NULL,
    provider_api_key_id = NULL
WHERE NULLIF(TRIM(COALESCE(json_extract(request_metadata, '$.routing_candidate_skip_reason'), '')), '') IS NOT NULL
  AND 'local_execution_runtime_miss' IN (
    TRIM(COALESCE(execution_path, '')),
    TRIM(COALESCE(json_extract(request_metadata, '$.execution_path'), '')),
    TRIM(COALESCE((
      SELECT routing.execution_path
      FROM usage_routing_snapshots AS routing
      WHERE routing.request_id = "usage".request_id
    ), ''))
  );
