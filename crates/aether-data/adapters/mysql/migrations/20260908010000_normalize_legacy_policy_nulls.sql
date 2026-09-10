-- Normalize legacy JSON null policies without changing explicit empty allowlists.

UPDATE `api_keys`
SET `allowed_providers` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_providers`) THEN
    JSON_TYPE(`allowed_providers`) = 'NULL'
   OR (JSON_TYPE(`allowed_providers`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_providers`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `api_keys`
SET `allowed_api_formats` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_api_formats`) THEN
    JSON_TYPE(`allowed_api_formats`) = 'NULL'
   OR (JSON_TYPE(`allowed_api_formats`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_api_formats`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `api_keys`
SET `allowed_models` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_models`) THEN
    JSON_TYPE(`allowed_models`) = 'NULL'
   OR (JSON_TYPE(`allowed_models`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_models`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `api_keys`
SET `ip_rules` = NULL
WHERE CASE WHEN JSON_VALID(`ip_rules`) THEN
    JSON_TYPE(`ip_rules`) = 'NULL'
   OR (JSON_TYPE(`ip_rules`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`ip_rules`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `users`
SET `allowed_providers` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_providers`) THEN
    JSON_TYPE(`allowed_providers`) = 'NULL'
   OR (JSON_TYPE(`allowed_providers`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_providers`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `users`
SET `allowed_api_formats` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_api_formats`) THEN
    JSON_TYPE(`allowed_api_formats`) = 'NULL'
   OR (JSON_TYPE(`allowed_api_formats`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_api_formats`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `users`
SET `allowed_models` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_models`) THEN
    JSON_TYPE(`allowed_models`) = 'NULL'
   OR (JSON_TYPE(`allowed_models`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_models`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `user_groups`
SET `allowed_providers` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_providers`) THEN
    JSON_TYPE(`allowed_providers`) = 'NULL'
   OR (JSON_TYPE(`allowed_providers`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_providers`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `user_groups`
SET `allowed_api_formats` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_api_formats`) THEN
    JSON_TYPE(`allowed_api_formats`) = 'NULL'
   OR (JSON_TYPE(`allowed_api_formats`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_api_formats`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `user_groups`
SET `allowed_models` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_models`) THEN
    JSON_TYPE(`allowed_models`) = 'NULL'
   OR (JSON_TYPE(`allowed_models`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_models`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `provider_api_keys`
SET `api_formats` = NULL
WHERE CASE WHEN JSON_VALID(`api_formats`) THEN
    JSON_TYPE(`api_formats`) = 'NULL'
   OR (JSON_TYPE(`api_formats`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`api_formats`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE `provider_api_keys`
SET `allowed_models` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_models`) THEN
    JSON_TYPE(`allowed_models`) = 'NULL'
   OR (JSON_TYPE(`allowed_models`) = 'STRING'
       AND REGEXP_LIKE(JSON_UNQUOTE(`allowed_models`), '^[[:space:]]*(null)?[[:space:]]*$', 'i'))
    ELSE FALSE END;

UPDATE management_tokens
SET `allowed_ips` = NULL
WHERE CASE WHEN JSON_VALID(`allowed_ips`) THEN
    JSON_TYPE(`allowed_ips`) = 'NULL'
    ELSE FALSE END;

UPDATE management_tokens
SET `permissions` = NULL
WHERE CASE WHEN JSON_VALID(`permissions`) THEN
    JSON_TYPE(`permissions`) = 'NULL'
    ELSE FALSE END;
