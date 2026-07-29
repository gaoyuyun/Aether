-- Incremental migrations only apply this compatibility default to existing
-- installations. The MySQL migration runner explicitly changes these values
-- to false when it detects a database being initialized for the first time.
INSERT IGNORE INTO system_configs (
    id,
    `key`,
    value,
    description,
    created_at,
    updated_at
)
SELECT
    '00000000-0000-0000-0000-000000000101',
    'module.wallet.enabled',
    'true',
    'Wallet module enabled state initialized during commerce module upgrade',
    UNIX_TIMESTAMP(),
    UNIX_TIMESTAMP();

INSERT IGNORE INTO system_configs (
    id,
    `key`,
    value,
    description,
    created_at,
    updated_at
)
SELECT
    '00000000-0000-0000-0000-000000000102',
    'module.billing_plans.enabled',
    'true',
    'Billing plans module enabled state initialized during commerce module upgrade',
    UNIX_TIMESTAMP(),
    UNIX_TIMESTAMP();
