CREATE TABLE IF NOT EXISTS provider_quota_window_counters (
    provider_id VARCHAR(64) NOT NULL,
    duration_secs BIGINT NOT NULL,
    window_start BIGINT NOT NULL,
    used_usd DOUBLE NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider_id, duration_secs),
    CONSTRAINT provider_quota_window_counters_provider_id_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
