CREATE TABLE IF NOT EXISTS provider_quota_window_counters (
    provider_id TEXT NOT NULL,
    duration_secs INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    used_usd REAL NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, duration_secs),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
