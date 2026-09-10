-- Analytics needs small scalar fields, but usage rows also contain large bodies.
-- Keep time-range scans on this index, including the null check used by provider
-- success counts, without copying unbounded error messages into the index.
CREATE INDEX IF NOT EXISTS idx_usage_analytics_covering ON usage (
    created_at_unix_ms, request_id, user_id, api_key_id, provider_name, model,
    api_format, endpoint_api_format, status, status_code,
    input_tokens, output_tokens, total_tokens,
    cache_creation_input_tokens, cache_creation_ephemeral_5m_input_tokens,
    cache_creation_ephemeral_1h_input_tokens, cache_read_input_tokens,
    total_cost_usd, actual_total_cost_usd, response_time_ms, (error_message IS NULL)
);

-- Canonical token counts must still prefer settlement values. Avoid fetching
-- the full settlement snapshot for every request in an analytics time range.
CREATE INDEX IF NOT EXISTS idx_usage_settlement_analytics_covering
ON usage_settlement_snapshots (
    request_id, billing_effective_input_tokens, billing_output_tokens,
    billing_cache_creation_tokens, billing_cache_creation_5m_tokens,
    billing_cache_creation_1h_tokens, billing_cache_read_tokens,
    billing_total_input_context, billing_cache_read_cost_usd,
    billing_cache_creation_cost_usd, input_price_per_1m
);
