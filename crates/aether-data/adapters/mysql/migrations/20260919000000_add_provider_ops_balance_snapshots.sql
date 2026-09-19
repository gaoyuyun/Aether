-- Last known upstream balance per provider plus refresh bookkeeping. The admin
-- page reads this snapshot; upstream queries run in the background.
CREATE TABLE provider_ops_balance_snapshots (
    provider_id VARCHAR(64) PRIMARY KEY,
    payload_json JSON NULL,
    last_success_at BIGINT NULL,
    last_attempt_at BIGINT NULL,
    last_status VARCHAR(32) NULL,
    last_error TEXT NULL,
    consecutive_failures INT NOT NULL DEFAULT 0,
    next_refresh_at BIGINT NULL,
    updated_at BIGINT NOT NULL,
    CONSTRAINT provider_ops_balance_snapshots_provider_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
