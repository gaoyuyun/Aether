CREATE TABLE IF NOT EXISTS stats_hourly_model_provider (
    id VARCHAR(64) PRIMARY KEY NOT NULL,
    hour_utc BIGINT NOT NULL,
    model VARCHAR(255) NOT NULL,
    provider_name VARCHAR(255) NOT NULL,
    total_requests BIGINT NOT NULL DEFAULT 0,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens BIGINT NOT NULL DEFAULT 0,
    total_cost DOUBLE PRECISION NOT NULL DEFAULT 0,
    settled_total_cost DOUBLE PRECISION NOT NULL DEFAULT 0,
    response_time_sum_ms DOUBLE PRECISION NOT NULL DEFAULT 0,
    response_time_samples BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (hour_utc, model, provider_name)
);

CREATE INDEX idx_stats_hourly_model_provider_hour
    ON stats_hourly_model_provider (hour_utc);
CREATE INDEX idx_stats_hourly_model_provider_model_hour
    ON stats_hourly_model_provider (model, hour_utc);
