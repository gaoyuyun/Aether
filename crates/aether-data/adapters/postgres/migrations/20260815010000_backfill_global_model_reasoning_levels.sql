-- Normalize the reasoning capability metadata for models created before the
-- reasoning-level field existed. Existing explicit values are preserved.
UPDATE public.global_models
SET config = jsonb_set(
    jsonb_set(
        COALESCE(config, '{}'::jsonb),
        '{extended_thinking}',
        CASE
            WHEN config ? 'extended_thinking' THEN config->'extended_thinking'
            ELSE 'false'::jsonb
        END,
        true
    ),
    '{reasoning_levels}',
    CASE
        WHEN config ? 'reasoning_levels' THEN config->'reasoning_levels'
        WHEN config->>'extended_thinking' = 'true' THEN '["low", "medium", "high"]'::jsonb
        ELSE 'null'::jsonb
    END,
    true
)
WHERE config IS NULL
   OR NOT (config ? 'extended_thinking')
   OR NOT (config ? 'reasoning_levels');
