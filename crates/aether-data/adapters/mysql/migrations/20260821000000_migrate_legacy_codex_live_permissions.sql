-- Preserve the explicit access granted by the original #741 implementation,
-- which represented Codex Live as openai:responses.  Runtime permission
-- matching remains strict after this one-time data upgrade.

UPDATE users
SET allowed_api_formats = JSON_ARRAY_APPEND(allowed_api_formats, '$', 'codex:live')
WHERE LOWER(TRIM(COALESCE(allowed_api_formats_mode, ''))) = 'specific'
  AND JSON_VALID(allowed_api_formats)
  AND JSON_TYPE(IF(JSON_VALID(allowed_api_formats), allowed_api_formats, NULL)) = 'ARRAY'
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('openai:responses'), '$'
  ) = 1
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('codex:live'), '$'
  ) = 0;

UPDATE user_groups
SET allowed_api_formats = JSON_ARRAY_APPEND(allowed_api_formats, '$', 'codex:live')
WHERE LOWER(TRIM(COALESCE(allowed_api_formats_mode, ''))) = 'specific'
  AND JSON_VALID(allowed_api_formats)
  AND JSON_TYPE(IF(JSON_VALID(allowed_api_formats), allowed_api_formats, NULL)) = 'ARRAY'
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('openai:responses'), '$'
  ) = 1
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('codex:live'), '$'
  ) = 0;

UPDATE api_keys
SET allowed_api_formats = JSON_ARRAY_APPEND(allowed_api_formats, '$', 'codex:live')
WHERE JSON_VALID(allowed_api_formats)
  AND JSON_TYPE(IF(JSON_VALID(allowed_api_formats), allowed_api_formats, NULL)) = 'ARRAY'
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('openai:responses'), '$'
  ) = 1
  AND JSON_CONTAINS(
    IF(JSON_VALID(allowed_api_formats), allowed_api_formats, JSON_ARRAY()),
    JSON_QUOTE('codex:live'), '$'
  ) = 0;

UPDATE provider_api_keys AS provider_key
INNER JOIN providers AS provider ON provider.id = provider_key.provider_id
SET
  provider_key.api_formats = JSON_ARRAY_APPEND(provider_key.api_formats, '$', 'codex:live'),
  provider_key.updated_at = UNIX_TIMESTAMP()
WHERE LOWER(TRIM(COALESCE(provider.provider_type, ''))) = 'codex'
  AND JSON_VALID(provider_key.api_formats)
  AND JSON_TYPE(
    IF(JSON_VALID(provider_key.api_formats), provider_key.api_formats, NULL)
  ) = 'ARRAY'
  AND JSON_CONTAINS(
    IF(JSON_VALID(provider_key.api_formats), provider_key.api_formats, JSON_ARRAY()),
    JSON_QUOTE('openai:responses'), '$'
  ) = 1
  AND JSON_CONTAINS(
    IF(JSON_VALID(provider_key.api_formats), provider_key.api_formats, JSON_ARRAY()),
    JSON_QUOTE('codex:live'), '$'
  ) = 0;

UPDATE provider_api_keys AS provider_key
INNER JOIN providers AS provider ON provider.id = provider_key.provider_id
SET
  provider_key.auth_type_by_format = JSON_SET(
    provider_key.auth_type_by_format,
    '$."codex:live"',
    JSON_EXTRACT(provider_key.auth_type_by_format, '$."openai:responses"')
  ),
  provider_key.updated_at = UNIX_TIMESTAMP()
WHERE LOWER(TRIM(COALESCE(provider.provider_type, ''))) = 'codex'
  AND JSON_VALID(provider_key.auth_type_by_format)
  AND JSON_TYPE(
    IF(JSON_VALID(provider_key.auth_type_by_format), provider_key.auth_type_by_format, NULL)
  ) = 'OBJECT'
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.auth_type_by_format),
      provider_key.auth_type_by_format,
      JSON_OBJECT()
    ),
    'one',
    '$."openai:responses"'
  ) = 1
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.auth_type_by_format),
      provider_key.auth_type_by_format,
      JSON_OBJECT()
    ),
    'one',
    '$."codex:live"'
  ) = 0;

UPDATE provider_api_keys AS provider_key
INNER JOIN providers AS provider ON provider.id = provider_key.provider_id
SET
  provider_key.allow_auth_channel_mismatch_formats = JSON_ARRAY_APPEND(
    provider_key.allow_auth_channel_mismatch_formats,
    '$',
    'codex:live'
  ),
  provider_key.updated_at = UNIX_TIMESTAMP()
WHERE LOWER(TRIM(COALESCE(provider.provider_type, ''))) = 'codex'
  AND JSON_VALID(provider_key.allow_auth_channel_mismatch_formats)
  AND JSON_TYPE(
    IF(
      JSON_VALID(provider_key.allow_auth_channel_mismatch_formats),
      provider_key.allow_auth_channel_mismatch_formats,
      NULL
    )
  ) = 'ARRAY'
  AND JSON_CONTAINS(
    IF(
      JSON_VALID(provider_key.allow_auth_channel_mismatch_formats),
      provider_key.allow_auth_channel_mismatch_formats,
      JSON_ARRAY()
    ),
    JSON_QUOTE('openai:responses'),
    '$'
  ) = 1
  AND JSON_CONTAINS(
    IF(
      JSON_VALID(provider_key.allow_auth_channel_mismatch_formats),
      provider_key.allow_auth_channel_mismatch_formats,
      JSON_ARRAY()
    ),
    JSON_QUOTE('codex:live'),
    '$'
  ) = 0;

UPDATE provider_api_keys AS provider_key
INNER JOIN providers AS provider ON provider.id = provider_key.provider_id
SET
  provider_key.rate_multipliers = JSON_SET(
    provider_key.rate_multipliers,
    '$."codex:live"',
    JSON_EXTRACT(provider_key.rate_multipliers, '$."openai:responses"')
  ),
  provider_key.updated_at = UNIX_TIMESTAMP()
WHERE LOWER(TRIM(COALESCE(provider.provider_type, ''))) = 'codex'
  AND JSON_VALID(provider_key.rate_multipliers)
  AND JSON_TYPE(
    IF(JSON_VALID(provider_key.rate_multipliers), provider_key.rate_multipliers, NULL)
  ) = 'OBJECT'
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.rate_multipliers),
      provider_key.rate_multipliers,
      JSON_OBJECT()
    ),
    'one',
    '$."openai:responses"'
  ) = 1
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.rate_multipliers),
      provider_key.rate_multipliers,
      JSON_OBJECT()
    ),
    'one',
    '$."codex:live"'
  ) = 0;

UPDATE provider_api_keys AS provider_key
INNER JOIN providers AS provider ON provider.id = provider_key.provider_id
SET
  provider_key.global_priority_by_format = JSON_SET(
    provider_key.global_priority_by_format,
    '$."codex:live"',
    JSON_EXTRACT(provider_key.global_priority_by_format, '$."openai:responses"')
  ),
  provider_key.updated_at = UNIX_TIMESTAMP()
WHERE LOWER(TRIM(COALESCE(provider.provider_type, ''))) = 'codex'
  AND JSON_VALID(provider_key.global_priority_by_format)
  AND JSON_TYPE(
    IF(
      JSON_VALID(provider_key.global_priority_by_format),
      provider_key.global_priority_by_format,
      NULL
    )
  ) = 'OBJECT'
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.global_priority_by_format),
      provider_key.global_priority_by_format,
      JSON_OBJECT()
    ),
    'one',
    '$."openai:responses"'
  ) = 1
  AND JSON_CONTAINS_PATH(
    IF(
      JSON_VALID(provider_key.global_priority_by_format),
      provider_key.global_priority_by_format,
      JSON_OBJECT()
    ),
    'one',
    '$."codex:live"'
  ) = 0;
