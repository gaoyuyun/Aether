-- Normalize the reasoning capability metadata for models created before the
-- reasoning-level field existed. Existing explicit values are preserved.
UPDATE global_models
SET config = JSON_SET(
    CASE WHEN JSON_VALID(config) THEN config ELSE '{}' END,
    '$.extended_thinking',
    CASE
        WHEN JSON_EXTRACT(config, '$.extended_thinking') IS NOT NULL
            THEN JSON_EXTRACT(config, '$.extended_thinking')
        ELSE JSON_EXTRACT('false', '$')
    END,
    '$.reasoning_levels',
    CASE
        WHEN JSON_EXTRACT(config, '$.reasoning_levels') IS NOT NULL
            THEN JSON_EXTRACT(config, '$.reasoning_levels')
        WHEN JSON_UNQUOTE(JSON_EXTRACT(config, '$.extended_thinking')) = 'true'
            THEN JSON_ARRAY('low', 'medium', 'high')
        ELSE JSON_EXTRACT('null', '$')
    END
)
WHERE config IS NULL
   OR JSON_EXTRACT(config, '$.extended_thinking') IS NULL
   OR JSON_EXTRACT(config, '$.reasoning_levels') IS NULL;
