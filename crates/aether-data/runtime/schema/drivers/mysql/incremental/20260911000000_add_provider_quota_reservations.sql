-- Each upstream attempt owns a durable estimate until actual usage replaces it.
CREATE TABLE provider_quota_reservations (
    candidate_id VARCHAR(64) PRIMARY KEY,
    provider_id VARCHAR(64) NOT NULL,
    quota_epoch_start BIGINT NOT NULL,
    dispatch_at BIGINT NOT NULL,
    reserved_cost_units BIGINT NOT NULL CHECK (reserved_cost_units >= 0),
    state VARCHAR(16) NOT NULL DEFAULT 'reserved',
    created_at BIGINT NOT NULL,
    finalized_at BIGINT NULL,
    CONSTRAINT provider_quota_reservations_provider_fkey
        FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
CREATE INDEX provider_quota_reservations_active_idx
    ON provider_quota_reservations (provider_id, quota_epoch_start, state, dispatch_at);
CREATE INDEX provider_quota_reservations_finalized_idx
    ON provider_quota_reservations (finalized_at);
