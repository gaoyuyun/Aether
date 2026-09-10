-- Normalize legacy JSON null policies without changing explicit empty allowlists.

UPDATE "api_keys"
SET "allowed_providers" = NULL
WHERE CASE WHEN json_valid("allowed_providers") THEN
    json_type("allowed_providers") = 'null'
    OR (json_type("allowed_providers") = 'text'
        AND lower(trim(json_extract("allowed_providers", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "api_keys"
SET "allowed_api_formats" = NULL
WHERE CASE WHEN json_valid("allowed_api_formats") THEN
    json_type("allowed_api_formats") = 'null'
    OR (json_type("allowed_api_formats") = 'text'
        AND lower(trim(json_extract("allowed_api_formats", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "api_keys"
SET "allowed_models" = NULL
WHERE CASE WHEN json_valid("allowed_models") THEN
    json_type("allowed_models") = 'null'
    OR (json_type("allowed_models") = 'text'
        AND lower(trim(json_extract("allowed_models", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "api_keys"
SET "ip_rules" = NULL
WHERE CASE WHEN json_valid("ip_rules") THEN
    json_type("ip_rules") = 'null'
    OR (json_type("ip_rules") = 'text'
        AND lower(trim(json_extract("ip_rules", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "users"
SET "allowed_providers" = NULL
WHERE CASE WHEN json_valid("allowed_providers") THEN
    json_type("allowed_providers") = 'null'
    OR (json_type("allowed_providers") = 'text'
        AND lower(trim(json_extract("allowed_providers", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "users"
SET "allowed_api_formats" = NULL
WHERE CASE WHEN json_valid("allowed_api_formats") THEN
    json_type("allowed_api_formats") = 'null'
    OR (json_type("allowed_api_formats") = 'text'
        AND lower(trim(json_extract("allowed_api_formats", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "users"
SET "allowed_models" = NULL
WHERE CASE WHEN json_valid("allowed_models") THEN
    json_type("allowed_models") = 'null'
    OR (json_type("allowed_models") = 'text'
        AND lower(trim(json_extract("allowed_models", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "user_groups"
SET "allowed_providers" = NULL
WHERE CASE WHEN json_valid("allowed_providers") THEN
    json_type("allowed_providers") = 'null'
    OR (json_type("allowed_providers") = 'text'
        AND lower(trim(json_extract("allowed_providers", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "user_groups"
SET "allowed_api_formats" = NULL
WHERE CASE WHEN json_valid("allowed_api_formats") THEN
    json_type("allowed_api_formats") = 'null'
    OR (json_type("allowed_api_formats") = 'text'
        AND lower(trim(json_extract("allowed_api_formats", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "user_groups"
SET "allowed_models" = NULL
WHERE CASE WHEN json_valid("allowed_models") THEN
    json_type("allowed_models") = 'null'
    OR (json_type("allowed_models") = 'text'
        AND lower(trim(json_extract("allowed_models", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "provider_api_keys"
SET "api_formats" = NULL
WHERE CASE WHEN json_valid("api_formats") THEN
    json_type("api_formats") = 'null'
    OR (json_type("api_formats") = 'text'
        AND lower(trim(json_extract("api_formats", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE "provider_api_keys"
SET "allowed_models" = NULL
WHERE CASE WHEN json_valid("allowed_models") THEN
    json_type("allowed_models") = 'null'
    OR (json_type("allowed_models") = 'text'
        AND lower(trim(json_extract("allowed_models", '$'), char(9) || char(10) || char(11) || char(12) || char(13) || ' ')) IN ('', 'null'))
    ELSE 0 END;

UPDATE management_tokens
SET "allowed_ips" = NULL
WHERE CASE WHEN json_valid("allowed_ips") THEN json_type("allowed_ips") = 'null' ELSE 0 END;

UPDATE management_tokens
SET "permissions" = NULL
WHERE CASE WHEN json_valid("permissions") THEN json_type("permissions") = 'null' ELSE 0 END;
