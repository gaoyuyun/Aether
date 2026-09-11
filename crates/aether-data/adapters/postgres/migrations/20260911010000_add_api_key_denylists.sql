-- Explicit denylists complement the existing allowlists. NULL preserves legacy behavior.
ALTER TABLE api_keys ADD COLUMN denied_providers JSONB;
ALTER TABLE api_keys ADD COLUMN denied_api_formats JSONB;
ALTER TABLE api_keys ADD COLUMN denied_models JSONB;
