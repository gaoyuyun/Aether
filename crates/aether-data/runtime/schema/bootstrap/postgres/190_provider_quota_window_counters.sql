CREATE TABLE IF NOT EXISTS public.provider_quota_window_counters (
    provider_id character varying(36) NOT NULL
        REFERENCES public.providers(id) ON DELETE CASCADE,
    duration_secs bigint NOT NULL,
    window_start timestamp with time zone NOT NULL,
    used_usd double precision NOT NULL DEFAULT 0,
    updated_at timestamp with time zone NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider_id, duration_secs)
);
