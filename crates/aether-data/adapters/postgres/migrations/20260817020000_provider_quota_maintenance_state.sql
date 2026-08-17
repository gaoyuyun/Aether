CREATE TABLE public.provider_quota_maintenance_state (
    provider_id text NOT NULL REFERENCES public.providers(id) ON DELETE CASCADE,
    quota_epoch_start bigint NOT NULL,
    task_kind text NOT NULL,
    status text NOT NULL DEFAULT 'pending',
    cursor_dispatch_at bigint NOT NULL DEFAULT 0,
    cursor_request_id text NOT NULL DEFAULT '',
    cutover_delta_sequence bigint,
    absorbed_delta_sequence bigint NOT NULL DEFAULT 0,
    included_rows bigint NOT NULL DEFAULT 0,
    excluded_payg_rows bigint NOT NULL DEFAULT 0,
    excluded_free_tier_rows bigint NOT NULL DEFAULT 0,
    unknown_rows bigint NOT NULL DEFAULT 0,
    lock_owner text,
    lock_expires_at bigint,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT NOW(),
    updated_at timestamptz NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider_id, quota_epoch_start, task_kind)
);

CREATE INDEX ix_provider_quota_maintenance_state_status
    ON public.provider_quota_maintenance_state (status, updated_at);

CREATE INDEX IF NOT EXISTS ix_usage_counter_deltas_provider_quota_pending
    ON public.usage_counter_deltas (
        kind, target_id, quota_accounting_status, quota_delta_sequence
    );

CREATE TABLE public.provider_quota_applied_watermarks (
    provider_id text NOT NULL REFERENCES public.providers(id) ON DELETE CASCADE,
    quota_epoch_start bigint NOT NULL,
    applied_delta_sequence bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider_id, quota_epoch_start)
);
