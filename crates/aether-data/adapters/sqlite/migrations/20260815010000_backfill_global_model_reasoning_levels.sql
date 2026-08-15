-- Normalize the reasoning capability metadata for models created before the
-- reasoning-level field existed. Existing explicit values are preserved.
UPDATE global_models
SET config = json_set(
    json_set(
        CASE WHEN json_valid(config) THEN config ELSE '{}' END,
        '$.extended_thinking',
        CASE
            WHEN json_extract(config, '$.extended_thinking') = 1
                OR json_extract(config, '$.extended_thinking') = 'true'
                THEN json('true')
            ELSE json('false')
        END
    ),
    '$.reasoning_levels',
    CASE
        WHEN json_type(config, '$.reasoning_levels') IS NOT NULL
            THEN json_extract(config, '$.reasoning_levels')
        WHEN json_extract(config, '$.extended_thinking') = 1
            OR json_extract(config, '$.extended_thinking') = 'true'
            THEN json_array('low', 'medium', 'high')
        ELSE json('null')
    END
)
WHERE config IS NULL
   OR json_extract(config, '$.extended_thinking') IS NULL
   OR json_extract(config, '$.reasoning_levels') IS NULL;
