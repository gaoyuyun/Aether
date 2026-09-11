-- Explicit denylists complement the existing allowlists. NULL preserves legacy behavior.
ALTER TABLE api_keys ADD COLUMN denied_providers TEXT;
ALTER TABLE api_keys ADD COLUMN denied_api_formats TEXT;
ALTER TABLE api_keys ADD COLUMN denied_models TEXT;
