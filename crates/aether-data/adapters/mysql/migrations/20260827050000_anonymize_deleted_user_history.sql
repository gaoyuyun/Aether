SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'user_plan_entitlements'
          AND CONSTRAINT_NAME = 'user_plan_entitlements_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE user_plan_entitlements DROP FOREIGN KEY user_plan_entitlements_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'entitlement_usage_ledgers'
          AND CONSTRAINT_NAME = 'entitlement_usage_ledgers_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE entitlement_usage_ledgers DROP FOREIGN KEY entitlement_usage_ledgers_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'user_referrals'
          AND CONSTRAINT_NAME = 'user_referrals_inviter_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE user_referrals DROP FOREIGN KEY user_referrals_inviter_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'user_referrals'
          AND CONSTRAINT_NAME = 'user_referrals_invitee_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE user_referrals DROP FOREIGN KEY user_referrals_invitee_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'referral_rewards'
          AND CONSTRAINT_NAME = 'referral_rewards_inviter_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE referral_rewards DROP FOREIGN KEY referral_rewards_inviter_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

SET @aether_drop_fact_user_fk_sql := IF(
    EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
          AND TABLE_NAME = 'referral_rewards'
          AND CONSTRAINT_NAME = 'referral_rewards_invitee_user_id_fkey'
          AND CONSTRAINT_TYPE = 'FOREIGN KEY'
    ),
    'ALTER TABLE referral_rewards DROP FOREIGN KEY referral_rewards_invitee_user_id_fkey',
    'DO 0'
);
PREPARE aether_drop_fact_user_fk_stmt FROM @aether_drop_fact_user_fk_sql;
EXECUTE aether_drop_fact_user_fk_stmt;
DEALLOCATE PREPARE aether_drop_fact_user_fk_stmt;

-- Keep existing historical row values unchanged. Runtime deletion paths enforce
-- the current anonymization policy for newly deleted users.
SELECT 1;
