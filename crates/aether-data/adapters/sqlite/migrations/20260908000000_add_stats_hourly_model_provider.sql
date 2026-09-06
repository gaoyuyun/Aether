CREATE TABLE IF NOT EXISTS stats_hourly_model_provider (
    id TEXT PRIMARY KEY NOT NULL,
    hour_utc INTEGER NOT NULL,
    model TEXT NOT NULL,
    provider_name TEXT NOT NULL,
    total_requests INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    total_cost REAL NOT NULL DEFAULT 0,
    settled_total_cost REAL NOT NULL DEFAULT 0,
    response_time_sum_ms REAL NOT NULL DEFAULT 0,
    response_time_samples INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (hour_utc, model, provider_name)
);

CREATE INDEX IF NOT EXISTS idx_stats_hourly_model_provider_hour
    ON stats_hourly_model_provider (hour_utc);
