-- Fresh PostgreSQL databases receive explicit false values from the snapshot
-- bootstrap. Replayed incrementals therefore only need the upgrade-compatible
-- default for existing installations.
INSERT INTO public.system_configs (id, key, value, description, created_at, updated_at)
SELECT
    '00000000-0000-0000-0000-000000000101',
    'module.wallet.enabled',
    'true'::json,
    'Wallet module enabled state initialized during commerce module upgrade',
    now(),
    now()
ON CONFLICT (key) DO NOTHING;

INSERT INTO public.system_configs (id, key, value, description, created_at, updated_at)
SELECT
    '00000000-0000-0000-0000-000000000102',
    'module.billing_plans.enabled',
    'true'::json,
    'Billing plans module enabled state initialized during commerce module upgrade',
    now(),
    now()
ON CONFLICT (key) DO NOTHING;
