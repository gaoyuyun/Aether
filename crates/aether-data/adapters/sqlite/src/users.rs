use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite};

use aether_data_contracts::repository::users::{
    is_valid_bcrypt_hash, last_oauth_unbind_denial, normalize_user_group_name,
    BindUserOAuthLinkOutcome, BindUserOAuthLinkSessionExpectation, DeleteUserOAuthLinkOutcome,
    LdapAuthUserProvisioningOutcome, ResolveOAuthLinkedUserOutcome, StoredUserAuthRecord,
    StoredUserExportRow, StoredUserGroup, StoredUserGroupMember, StoredUserGroupMembership,
    StoredUserOAuthLinkSummary, StoredUserPreferenceRecord, StoredUserSessionRecord,
    StoredUserSummary, UpsertUserGroupRecord, UserExportListQuery, UserExportSortBy,
    UserExportSummary, UserReadRepository, LAST_ACTIVE_ADMIN_DELETE_DENIED,
    LAST_ACTIVE_ADMIN_UPDATE_DENIED,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::SqlitePool;

const USER_SUMMARY_COLUMNS: &str = r#"
SELECT
  id,
  username,
  email,
  role,
  is_active,
  is_deleted
FROM users
"#;

const SQLITE_ACTIVE_ADMIN_UPDATE_GUARD: &str = r#"
  AND (
    ? = 0
    OR COALESCE(LOWER(role), '') != 'admin'
    OR is_active = 0
    OR is_deleted != 0
    OR EXISTS (
      SELECT 1
      FROM users AS other_admin
      WHERE other_admin.id != users.id
        AND LOWER(other_admin.role) = 'admin'
        AND other_admin.is_active = 1
        AND other_admin.is_deleted = 0
    )
  )
"#;

const SQLITE_DELETE_USER_SQL: &str = r#"
DELETE FROM users
WHERE id = ?
  AND (
    COALESCE(LOWER(role), '') != 'admin'
    OR is_active = 0
    OR is_deleted != 0
    OR EXISTS (
      SELECT 1
      FROM users AS other_admin
      WHERE other_admin.id != users.id
        AND LOWER(other_admin.role) = 'admin'
        AND other_admin.is_active = 1
        AND other_admin.is_deleted = 0
    )
  )
"#;

const SQLITE_DELETE_USER_IF_WALLET_ABSENT_SQL: &str = r#"
DELETE FROM users
WHERE id = ?
  AND NOT EXISTS (
    SELECT 1
    FROM wallets AS wallet
    WHERE wallet.user_id = ?
       OR EXISTS (
         SELECT 1
         FROM api_keys AS api_key
         WHERE api_key.id = wallet.api_key_id
           AND api_key.user_id = ?
       )
  )
  AND (
    COALESCE(LOWER(role), '') != 'admin'
    OR is_active = 0
    OR is_deleted != 0
    OR EXISTS (
      SELECT 1
      FROM users AS other_admin
      WHERE other_admin.id != users.id
        AND LOWER(other_admin.role) = 'admin'
        AND other_admin.is_active = 1
        AND other_admin.is_deleted = 0
    )
  )
"#;

const SQLITE_DELETE_USER_API_KEYS_SQL: &str = "DELETE FROM api_keys WHERE user_id = ?";

const SQLITE_DELETE_USER_DEPENDENTS_SQL: &[&str] = &[
    "DELETE FROM usage_request_admissions WHERE subject_id = ?",
    "DELETE FROM usage_cost_reservations WHERE subject_id = ?",
    "DELETE FROM gemini_file_mappings WHERE user_id = ?",
    "DELETE FROM api_key_provider_mappings WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?)",
    SQLITE_DELETE_USER_API_KEYS_SQL,
    "DELETE FROM management_tokens WHERE user_id = ?",
    "DELETE FROM user_sessions WHERE user_id = ?",
    "DELETE FROM user_oauth_links WHERE user_id = ?",
    "DELETE FROM user_group_members WHERE user_id = ?",
    "DELETE FROM user_preferences WHERE user_id = ?",
    "DELETE FROM user_invite_codes WHERE user_id = ?",
    "DELETE FROM announcement_reads WHERE user_id = ?",
];

const SQLITE_PREPARE_USER_FACTS_FOR_DELETION_SQL: &[&str] = &[
    "UPDATE referral_rewards SET status = CASE WHEN status IN ('pending', 'failed', 'applying') THEN 'voided' ELSE status END, failure_reason = NULL, admin_note = NULL, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE ? IN (inviter_user_id, invitee_user_id)",
    "UPDATE referral_rewards SET failure_reason = NULL, admin_note = NULL, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE admin_operator_id = ?",
    "UPDATE user_referrals SET invite_code_snapshot = 'deleted-user', source_json = NULL, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE ? IN (inviter_user_id, invitee_user_id)",
    "UPDATE user_plan_entitlements SET status = CASE WHEN status = 'active' THEN 'revoked' ELSE status END, expires_at = MIN(expires_at, CAST(strftime('%s', 'now') AS INTEGER)), updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE user_id = ?",
    "UPDATE wallets SET status = 'disabled', updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE user_id = ?",
    "UPDATE wallets SET status = 'disabled', updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?)",
    "UPDATE audit_logs SET description = 'deleted user event', ip_address = NULL, user_agent = NULL, event_metadata = NULL, error_message = NULL WHERE user_id = ?",
    "UPDATE audit_logs SET description = 'deleted API key event', ip_address = NULL, user_agent = NULL, event_metadata = NULL, error_message = NULL WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?)",
    "UPDATE wallet_transactions SET description = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = ?)",
    "UPDATE wallet_transactions SET description = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?))",
    "UPDATE wallet_transactions SET description = NULL WHERE operator_id = ?",
    "UPDATE payment_callbacks SET payload = NULL, error_message = NULL WHERE EXISTS (SELECT 1 FROM payment_orders AS history_order WHERE history_order.user_id = ? AND (history_order.id = payment_callbacks.payment_order_id OR (payment_callbacks.order_no IS NOT NULL AND history_order.order_no = payment_callbacks.order_no)))",
    "UPDATE payment_callbacks SET payload = NULL, error_message = NULL WHERE EXISTS (SELECT 1 FROM payment_orders AS history_order JOIN wallets AS history_wallet ON history_wallet.id = history_order.wallet_id WHERE history_wallet.user_id = ? AND (history_order.id = payment_callbacks.payment_order_id OR (payment_callbacks.order_no IS NOT NULL AND history_order.order_no = payment_callbacks.order_no)))",
    "UPDATE payment_callbacks SET payload = NULL, error_message = NULL WHERE EXISTS (SELECT 1 FROM payment_orders AS history_order JOIN wallets AS history_wallet ON history_wallet.id = history_order.wallet_id WHERE history_wallet.api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?) AND (history_order.id = payment_callbacks.payment_order_id OR (payment_callbacks.order_no IS NOT NULL AND history_order.order_no = payment_callbacks.order_no)))",
    "UPDATE payment_orders SET gateway_response = NULL WHERE user_id = ?",
    "UPDATE payment_orders SET gateway_response = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?))",
    "UPDATE refund_requests SET reason = NULL, payout_reference = NULL, payout_proof = NULL, failure_reason = NULL WHERE user_id = ?",
    "UPDATE refund_requests SET reason = NULL, payout_reference = NULL, payout_proof = NULL, failure_reason = NULL WHERE ? IN (requested_by, approved_by, processed_by)",
    "UPDATE refund_requests SET reason = NULL, payout_reference = NULL, payout_proof = NULL, failure_reason = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE user_id = ?)",
    "UPDATE refund_requests SET reason = NULL, payout_reference = NULL, payout_proof = NULL, failure_reason = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?))",
    "UPDATE redeem_code_batches SET description = NULL WHERE created_by = ?",
];

const SQLITE_ANONYMIZE_USER_HISTORY_SQL: &[&str] = &[
    "UPDATE request_candidates SET username = NULL, api_key_name = NULL WHERE user_id = ?",
    "UPDATE video_tasks SET username = NULL, api_key_name = NULL WHERE user_id = ?",
    "UPDATE usage SET username = NULL, api_key_name = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_summary SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_model SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_provider SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_api_format SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_model_provider SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_cost_savings SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_cost_savings_provider SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_cost_savings_model SET username = NULL WHERE user_id = ?",
    "UPDATE stats_user_daily_cost_savings_model_provider SET username = NULL WHERE user_id = ?",
];

const SQLITE_ANONYMIZE_USER_API_KEY_HISTORY_SQL: &str =
    "UPDATE stats_daily_api_key SET api_key_name = NULL WHERE api_key_id IN (SELECT id FROM api_keys WHERE user_id = ?)";

const USER_EXPORT_COLUMNS: &str = r#"
SELECT
  id,
  email,
  email_verified,
  username,
  password_hash,
  role,
  auth_source,
  allowed_providers,
  allowed_providers_mode,
  allowed_api_formats,
  allowed_api_formats_mode,
  allowed_models,
  allowed_models_mode,
  rate_limit,
  rate_limit_mode,
  model_capability_settings,
  feature_settings,
  is_active
FROM users
"#;

const USER_AUTH_COLUMNS: &str = r#"
SELECT
  id,
  email,
  email_verified,
  username,
  password_hash,
  role,
  auth_source,
  allowed_providers,
  allowed_providers_mode,
  allowed_api_formats,
  allowed_api_formats_mode,
  allowed_models,
  allowed_models_mode,
  is_active,
  is_deleted,
  security_version,
  created_at,
  last_login_at
FROM users
"#;

const USER_AUTH_COLUMNS_QUALIFIED: &str = r#"
SELECT
  users.id AS id,
  users.email AS email,
  users.email_verified AS email_verified,
  users.username AS username,
  users.password_hash AS password_hash,
  users.role AS role,
  users.auth_source AS auth_source,
  users.allowed_providers AS allowed_providers,
  users.allowed_providers_mode AS allowed_providers_mode,
  users.allowed_api_formats AS allowed_api_formats,
  users.allowed_api_formats_mode AS allowed_api_formats_mode,
  users.allowed_models AS allowed_models,
  users.allowed_models_mode AS allowed_models_mode,
  users.is_active AS is_active,
  users.is_deleted AS is_deleted,
  users.security_version AS security_version,
  users.created_at AS created_at,
  users.last_login_at AS last_login_at
FROM users
"#;

const USER_OAUTH_LINK_SUMMARY_COLUMNS: &str = r#"
SELECT
  user_oauth_links.provider_type,
  oauth_providers.display_name,
  user_oauth_links.provider_username,
  user_oauth_links.provider_email,
  user_oauth_links.linked_at,
  user_oauth_links.last_login_at,
  oauth_providers.is_enabled AS provider_enabled
FROM user_oauth_links
JOIN oauth_providers
  ON oauth_providers.provider_type = user_oauth_links.provider_type
"#;

const USER_PREFERENCES_COLUMNS: &str = r#"
SELECT
  up.user_id,
  up.avatar_url,
  up.bio,
  up.default_provider_id,
  p.name AS default_provider_name,
  up.theme,
  up.language,
  up.timezone,
  up.email_notifications,
  up.usage_alerts,
  up.announcement_notifications
FROM user_preferences up
LEFT JOIN providers p
  ON p.id = up.default_provider_id
"#;

const USER_SESSION_COLUMNS: &str = r#"
SELECT
  id,
  user_id,
  security_version,
  client_device_id,
  device_label,
  refresh_token_hash,
  prev_refresh_token_hash,
  rotated_at,
  last_seen_at,
  expires_at,
  revoked_at,
  revoke_reason,
  ip_address,
  user_agent,
  created_at,
  updated_at
FROM user_sessions
"#;

const USER_GROUP_COLUMNS: &str = r#"
SELECT
  id,
  name,
  normalized_name,
  description,
  priority,
  allowed_providers,
  allowed_providers_mode,
  allowed_api_formats,
  allowed_api_formats_mode,
  allowed_models,
  allowed_models_mode,
  rate_limit,
  rate_limit_mode,
  created_at,
  updated_at
FROM user_groups
"#;

const USER_GROUP_MEMBER_COLUMNS: &str = r#"
SELECT
  user_group_members.group_id,
  users.id AS user_id,
  users.username,
  users.email,
  users.role,
  users.is_active,
  users.is_deleted,
  user_group_members.created_at
FROM user_group_members
JOIN users ON users.id = user_group_members.user_id
"#;

#[derive(Debug, Clone)]
pub struct SqliteUserReadRepository {
    pool: SqlitePool,
}

impl SqliteUserReadRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn fetch_summary_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredUserSummary>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_row).collect()
    }

    async fn fetch_export_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredUserExportRow>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_export_row).collect()
    }

    async fn fetch_auth_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredUserAuthRecord>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_auth_row).collect()
    }

    async fn fetch_group_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredUserGroup>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_group_row).collect()
    }

    async fn fetch_group_member_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredUserGroupMember>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_group_member_row).collect()
    }

    async fn delete_local_auth_user_inner(
        &self,
        user_id: &str,
        require_wallet_absent: bool,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        if require_wallet_absent {
            let wallet_exists: Option<i32> = sqlx::query_scalar(
                r#"
SELECT 1
FROM wallets AS wallet
WHERE wallet.user_id = ?
   OR EXISTS (
     SELECT 1
     FROM api_keys AS api_key
     WHERE api_key.id = wallet.api_key_id
       AND api_key.user_id = ?
   )
LIMIT 1
                "#,
            )
            .bind(user_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
            if wallet_exists.is_some() {
                tx.rollback().await.map_sql_err()?;
                return Ok(false);
            }
        }
        for sql in SQLITE_PREPARE_USER_FACTS_FOR_DELETION_SQL {
            sqlx::query(sql)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        for sql in SQLITE_ANONYMIZE_USER_HISTORY_SQL {
            sqlx::query(sql)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        sqlx::query(SQLITE_ANONYMIZE_USER_API_KEY_HISTORY_SQL)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        for sql in SQLITE_DELETE_USER_DEPENDENTS_SQL {
            if require_wallet_absent && *sql == SQLITE_DELETE_USER_API_KEYS_SQL {
                continue;
            }
            sqlx::query(sql)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        let result = if require_wallet_absent {
            sqlx::query(SQLITE_DELETE_USER_IF_WALLET_ABSENT_SQL)
                .bind(user_id)
                .bind(user_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?
        } else {
            sqlx::query(SQLITE_DELETE_USER_SQL)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?
        };
        if result.rows_affected() > 0 {
            if require_wallet_absent {
                sqlx::query(SQLITE_DELETE_USER_API_KEYS_SQL)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
            }
            tx.commit().await.map_sql_err()?;
            return Ok(true);
        }
        let blocked_active_admin: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM users WHERE id = ? AND LOWER(role) = 'admin' AND is_active = 1 AND is_deleted = 0 LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        tx.rollback().await.map_sql_err()?;
        if blocked_active_admin.is_some() {
            return Err(DataLayerError::InvalidInput(
                LAST_ACTIVE_ADMIN_DELETE_DENIED.to_string(),
            ));
        }
        Ok(false)
    }
}

#[async_trait]
impl UserReadRepository for SqliteUserReadRepository {
    async fn list_users_by_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<StoredUserSummary>, DataLayerError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(USER_SUMMARY_COLUMNS);
        builder.push(" WHERE id IN (");
        {
            let mut separated = builder.separated(", ");
            for user_id in user_ids {
                separated.push_bind(user_id);
            }
        }
        builder.push(") ORDER BY id ASC");
        self.fetch_summary_rows(builder).await
    }

    async fn list_users_by_username_search(
        &self,
        username_search: &str,
    ) -> Result<Vec<StoredUserSummary>, DataLayerError> {
        let username_search = username_search.trim();
        if username_search.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(USER_SUMMARY_COLUMNS);
        builder
            .push(" WHERE is_deleted = 0 AND LOWER(username) LIKE ")
            .push_bind(format!("%{}%", username_search.to_ascii_lowercase()))
            .push(" ORDER BY id ASC");
        self.fetch_summary_rows(builder).await
    }

    async fn list_export_users(&self) -> Result<Vec<StoredUserExportRow>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_EXPORT_COLUMNS);
        builder.push(" WHERE is_deleted = 0 ORDER BY id ASC");
        self.fetch_export_rows(builder).await
    }

    async fn list_export_users_page(
        &self,
        query: &UserExportListQuery,
    ) -> Result<Vec<StoredUserExportRow>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_EXPORT_COLUMNS);
        builder.push(" WHERE is_deleted = 0");
        if let Some(role) = query.role.as_deref() {
            builder
                .push(" AND LOWER(role) = ")
                .push_bind(role.trim().to_ascii_lowercase());
        }
        if let Some(is_active) = query.is_active {
            builder.push(" AND is_active = ").push_bind(is_active);
        }
        if let Some(group_id) = query
            .group_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" AND id IN (SELECT user_id FROM user_group_members WHERE group_id = ");
            builder.push_bind(group_id);
            builder.push(")");
        }
        if let Some(search) = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = format!("%{}%", search.to_ascii_lowercase());
            builder
                .push(" AND (LOWER(id) LIKE ")
                .push_bind(pattern.clone())
                .push(" OR LOWER(username) LIKE ")
                .push_bind(pattern.clone())
                .push(" OR LOWER(COALESCE(email, '')) LIKE ")
                .push_bind(pattern)
                .push(")");
        }
        match query.sort_by {
            UserExportSortBy::CreatedAt => {
                builder
                    .push(" ORDER BY created_at ")
                    .push(if query.sort_order.is_desc() {
                        "DESC"
                    } else {
                        "ASC"
                    })
                    .push(", id ASC");
            }
            UserExportSortBy::Id => {
                builder.push(" ORDER BY id ASC");
            }
        }

        builder
            .push(" LIMIT ")
            .push_bind(i64::try_from(query.limit).map_err(|_| {
                DataLayerError::InvalidInput(format!("invalid user export limit: {}", query.limit))
            })?)
            .push(" OFFSET ")
            .push_bind(i64::try_from(query.skip).map_err(|_| {
                DataLayerError::InvalidInput(format!("invalid user export skip: {}", query.skip))
            })?);
        self.fetch_export_rows(builder).await
    }

    async fn count_export_users(&self, query: &UserExportListQuery) -> Result<u64, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) AS total FROM users");
        builder.push(" WHERE is_deleted = 0");
        if let Some(role) = query.role.as_deref() {
            builder
                .push(" AND LOWER(role) = ")
                .push_bind(role.trim().to_ascii_lowercase());
        }
        if let Some(is_active) = query.is_active {
            builder.push(" AND is_active = ").push_bind(is_active);
        }
        if let Some(group_id) = query
            .group_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" AND id IN (SELECT user_id FROM user_group_members WHERE group_id = ");
            builder.push_bind(group_id);
            builder.push(")");
        }
        if let Some(search) = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = format!("%{}%", search.to_ascii_lowercase());
            builder
                .push(" AND (LOWER(id) LIKE ")
                .push_bind(pattern.clone())
                .push(" OR LOWER(username) LIKE ")
                .push_bind(pattern.clone())
                .push(" OR LOWER(COALESCE(email, '')) LIKE ")
                .push_bind(pattern)
                .push(")");
        }

        let row = builder.build().fetch_one(&self.pool).await.map_sql_err()?;
        Ok(row.try_get::<i64, _>("total").map_sql_err()?.max(0) as u64)
    }

    async fn summarize_export_users(&self) -> Result<UserExportSummary, DataLayerError> {
        let row = sqlx::query(
            r#"
SELECT
  COUNT(*) AS total,
  SUM(CASE WHEN is_active = 1 THEN 1 ELSE 0 END) AS active
FROM users
WHERE is_deleted = 0
"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_sql_err()?;

        Ok(UserExportSummary {
            total: row.try_get::<i64, _>("total").map_sql_err()?.max(0) as u64,
            active: row
                .try_get::<Option<i64>, _>("active")
                .map_sql_err()?
                .unwrap_or(0)
                .max(0) as u64,
        })
    }

    async fn find_export_user_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<StoredUserExportRow>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_EXPORT_COLUMNS);
        builder
            .push(" WHERE is_deleted = 0 AND id = ")
            .push_bind(user_id)
            .push(" LIMIT 1");
        Ok(self.fetch_export_rows(builder).await?.into_iter().next())
    }

    async fn list_non_admin_export_users(
        &self,
    ) -> Result<Vec<StoredUserExportRow>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_EXPORT_COLUMNS);
        builder.push(" WHERE is_deleted = 0 AND LOWER(role) != 'admin' ORDER BY id ASC");
        self.fetch_export_rows(builder).await
    }

    async fn list_user_groups(&self) -> Result<Vec<StoredUserGroup>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_COLUMNS);
        builder.push(" ORDER BY name ASC, id ASC");
        self.fetch_group_rows(builder).await
    }

    async fn find_user_group_by_id(
        &self,
        group_id: &str,
    ) -> Result<Option<StoredUserGroup>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_COLUMNS);
        builder
            .push(" WHERE id = ")
            .push_bind(group_id)
            .push(" LIMIT 1");
        Ok(self.fetch_group_rows(builder).await?.into_iter().next())
    }

    async fn list_user_groups_by_ids(
        &self,
        group_ids: &[String],
    ) -> Result<Vec<StoredUserGroup>, DataLayerError> {
        if group_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_COLUMNS);
        builder.push(" WHERE id IN (");
        {
            let mut separated = builder.separated(", ");
            for group_id in group_ids {
                separated.push_bind(group_id);
            }
        }
        builder.push(") ORDER BY name ASC, id ASC");
        self.fetch_group_rows(builder).await
    }

    async fn create_user_group(
        &self,
        record: UpsertUserGroupRecord,
    ) -> Result<Option<StoredUserGroup>, DataLayerError> {
        let now = current_unix_secs();
        let id = uuid::Uuid::new_v4().to_string();
        let name = normalize_user_group_name(&record.name);
        let normalized_name = name.to_ascii_lowercase();
        let result = sqlx::query(
            r#"
INSERT INTO user_groups (
  id, name, normalized_name, description, priority,
  allowed_providers, allowed_providers_mode,
  allowed_api_formats, allowed_api_formats_mode,
  allowed_models, allowed_models_mode,
  rate_limit, rate_limit_mode, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&id)
        .bind(name)
        .bind(normalized_name)
        .bind(record.description)
        .bind(record.priority)
        .bind(json_string_from_option_vec(
            record.allowed_providers.as_ref(),
        ))
        .bind(record.allowed_providers_mode)
        .bind(json_string_from_option_vec(
            record.allowed_api_formats.as_ref(),
        ))
        .bind(record.allowed_api_formats_mode)
        .bind(json_string_from_option_vec(record.allowed_models.as_ref()))
        .bind(record.allowed_models_mode)
        .bind(record.rate_limit)
        .bind(record.rate_limit_mode)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => self.find_user_group_by_id(&id).await,
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Err(
                DataLayerError::InvalidInput("duplicate user group name".to_string()),
            ),
            Err(err) => Err(err).map_sql_err(),
        }
    }

    async fn update_user_group(
        &self,
        group_id: &str,
        record: UpsertUserGroupRecord,
    ) -> Result<Option<StoredUserGroup>, DataLayerError> {
        let now = current_unix_secs();
        let name = normalize_user_group_name(&record.name);
        let normalized_name = name.to_ascii_lowercase();
        let result = sqlx::query(
            r#"
UPDATE user_groups
SET name = ?,
    normalized_name = ?,
    description = ?,
    priority = ?,
    allowed_providers = ?,
    allowed_providers_mode = ?,
    allowed_api_formats = ?,
    allowed_api_formats_mode = ?,
    allowed_models = ?,
    allowed_models_mode = ?,
    rate_limit = ?,
    rate_limit_mode = ?,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(name)
        .bind(normalized_name)
        .bind(record.description)
        .bind(record.priority)
        .bind(json_string_from_option_vec(
            record.allowed_providers.as_ref(),
        ))
        .bind(record.allowed_providers_mode)
        .bind(json_string_from_option_vec(
            record.allowed_api_formats.as_ref(),
        ))
        .bind(record.allowed_api_formats_mode)
        .bind(json_string_from_option_vec(record.allowed_models.as_ref()))
        .bind(record.allowed_models_mode)
        .bind(record.rate_limit)
        .bind(record.rate_limit_mode)
        .bind(now)
        .bind(group_id)
        .execute(&self.pool)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 0 => Ok(None),
            Ok(_) => self.find_user_group_by_id(group_id).await,
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Err(
                DataLayerError::InvalidInput("duplicate user group name".to_string()),
            ),
            Err(err) => Err(err).map_sql_err(),
        }
    }

    /// BEGIN IMMEDIATE serializes writers while the complete snapshot is
    /// compared and restored, preventing a rollback from overwriting a newer
    /// administrator update.
    async fn restore_user_group_if_matches(
        &self,
        expected: &StoredUserGroup,
        restored: &StoredUserGroup,
    ) -> Result<bool, DataLayerError> {
        if expected.id != restored.id || expected.id.trim().is_empty() {
            return Ok(false);
        }

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_COLUMNS);
        builder.push(" WHERE id = ").push_bind(&expected.id);
        let row = builder
            .build()
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        };
        let current = map_user_group_row(&row)?;
        if &current != expected {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        let result = sqlx::query(
            r#"
UPDATE user_groups
SET name = ?,
    normalized_name = ?,
    description = ?,
    priority = ?,
    allowed_providers = ?,
    allowed_providers_mode = ?,
    allowed_api_formats = ?,
    allowed_api_formats_mode = ?,
    allowed_models = ?,
    allowed_models_mode = ?,
    rate_limit = ?,
    rate_limit_mode = ?,
    created_at = ?,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(&restored.name)
        .bind(&restored.normalized_name)
        .bind(&restored.description)
        .bind(restored.priority)
        .bind(json_string_from_option_vec(
            restored.allowed_providers.as_ref(),
        ))
        .bind(&restored.allowed_providers_mode)
        .bind(json_string_from_option_vec(
            restored.allowed_api_formats.as_ref(),
        ))
        .bind(&restored.allowed_api_formats_mode)
        .bind(json_string_from_option_vec(
            restored.allowed_models.as_ref(),
        ))
        .bind(&restored.allowed_models_mode)
        .bind(restored.rate_limit)
        .bind(&restored.rate_limit_mode)
        .bind(restored.created_at.map(|value| value.timestamp()))
        .bind(restored.updated_at.map(|value| value.timestamp()))
        .bind(&restored.id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if result.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        tx.commit().await.map_sql_err()?;
        Ok(true)
    }

    async fn delete_user_group(&self, group_id: &str) -> Result<bool, DataLayerError> {
        let result = sqlx::query("DELETE FROM user_groups WHERE id = ?")
            .bind(group_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_user_group_members(
        &self,
        group_id: &str,
    ) -> Result<Vec<StoredUserGroupMember>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_MEMBER_COLUMNS);
        builder
            .push(" WHERE user_group_members.group_id = ")
            .push_bind(group_id)
            .push(" ORDER BY users.username ASC, users.id ASC");
        self.fetch_group_member_rows(builder).await
    }

    async fn replace_user_group_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<StoredUserGroupMember>, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        sqlx::query("DELETE FROM user_group_members WHERE group_id = ?")
            .bind(group_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let now = current_unix_secs();
        for user_id in normalized_ids(user_ids) {
            sqlx::query(
                "INSERT OR IGNORE INTO user_group_members (group_id, user_id, created_at) VALUES (?, ?, ?)",
            )
            .bind(group_id)
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        self.list_user_group_members(group_id).await
    }

    async fn list_user_groups_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUserGroup>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_GROUP_COLUMNS);
        builder
            .push(" WHERE id IN (SELECT group_id FROM user_group_members WHERE user_id = ")
            .push_bind(user_id)
            .push(") ORDER BY name ASC, id ASC");
        self.fetch_group_rows(builder).await
    }

    async fn list_user_group_memberships_by_user_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<StoredUserGroupMembership>, DataLayerError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
  user_group_members.user_id,
  user_groups.id AS group_id,
  user_groups.name AS group_name,
  user_groups.priority AS group_priority,
  user_group_members.created_at
FROM user_group_members
JOIN user_groups ON user_groups.id = user_group_members.group_id
WHERE user_group_members.user_id IN (
"#,
        );
        {
            let mut separated = builder.separated(", ");
            for user_id in user_ids {
                separated.push_bind(user_id);
            }
        }
        builder.push(
            ") ORDER BY user_group_members.user_id ASC, user_groups.name ASC, user_groups.id ASC",
        );
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_group_membership_row).collect()
    }

    async fn replace_user_groups_for_user(
        &self,
        user_id: &str,
        group_ids: &[String],
    ) -> Result<Vec<StoredUserGroup>, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let user_exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        if user_exists.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(Vec::new());
        }
        sqlx::query("DELETE FROM user_group_members WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let now = current_unix_secs();
        for group_id in normalized_ids(group_ids) {
            sqlx::query(
                "INSERT OR IGNORE INTO user_group_members (group_id, user_id, created_at) VALUES (?, ?, ?)",
            )
            .bind(group_id)
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        self.list_user_groups_for_user(user_id).await
    }

    async fn restore_user_groups_if_matches(
        &self,
        user_id: &str,
        expected_group_ids: &[String],
        restored_group_ids: &[String],
    ) -> Result<bool, DataLayerError> {
        let expected = normalized_ids(expected_group_ids);
        let restored = normalized_ids(restored_group_ids);
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let user_exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        if user_exists.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let current = sqlx::query_scalar::<_, String>(
            "SELECT group_id FROM user_group_members WHERE user_id = ? ORDER BY group_id ASC",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        if current != expected {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        if !restored.is_empty() {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "SELECT COUNT(*) AS count FROM user_groups WHERE id IN (",
            );
            {
                let mut separated = builder.separated(", ");
                for group_id in &restored {
                    separated.push_bind(group_id);
                }
            }
            builder.push(")");
            let count = builder
                .build()
                .fetch_one(&mut *tx)
                .await
                .map_sql_err()?
                .try_get::<i64, _>("count")
                .map_sql_err()?;
            if count != restored.len() as i64 {
                tx.rollback().await.map_sql_err()?;
                return Ok(false);
            }
        }
        sqlx::query("DELETE FROM user_group_members WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let now = current_unix_secs();
        for group_id in restored {
            sqlx::query(
                "INSERT OR IGNORE INTO user_group_members (group_id, user_id, created_at) VALUES (?, ?, ?)",
            )
            .bind(group_id)
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        Ok(true)
    }

    async fn add_user_to_group(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let user_exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        if user_exists.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let result = sqlx::query(
            "INSERT OR IGNORE INTO user_group_members (group_id, user_id, created_at) VALUES (?, ?, ?)",
        )
        .bind(group_id)
        .bind(user_id)
        .bind(current_unix_secs())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn find_user_auth_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder
            .push(" WHERE id = ")
            .push_bind(user_id)
            .push(" LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn list_user_auth_by_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<StoredUserAuthRecord>, DataLayerError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder.push(" WHERE id IN (");
        {
            let mut separated = builder.separated(", ");
            for user_id in user_ids {
                separated.push_bind(user_id);
            }
        }
        builder.push(") ORDER BY id ASC");
        self.fetch_auth_rows(builder).await
    }

    async fn find_user_auth_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder
            .push(" WHERE email = ")
            .push_bind(identifier)
            .push(" OR username = ")
            .push_bind(identifier)
            .push(" LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn find_user_auth_by_email(
        &self,
        email: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder
            .push(" WHERE email = ")
            .push_bind(email)
            .push(" LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn find_active_user_auth_by_email_ci(
        &self,
        email: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder
            .push(" WHERE LOWER(email) = LOWER(")
            .push_bind(email)
            .push(") AND is_deleted = 0 LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn find_user_auth_by_username(
        &self,
        username: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS);
        builder
            .push(" WHERE username = ")
            .push_bind(username)
            .push(" LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn list_user_oauth_links(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUserOAuthLinkSummary>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_OAUTH_LINK_SUMMARY_COLUMNS);
        builder
            .push(" WHERE user_oauth_links.user_id = ")
            .push_bind(user_id)
            .push(" ORDER BY user_oauth_links.linked_at ASC");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_oauth_link_summary_row).collect()
    }

    async fn find_oauth_linked_user(
        &self,
        provider_type: &str,
        provider_user_id: &str,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_AUTH_COLUMNS_QUALIFIED);
        builder
            .push(" JOIN user_oauth_links ON users.id = user_oauth_links.user_id")
            .push(" WHERE user_oauth_links.provider_type = ")
            .push_bind(provider_type)
            .push(" AND user_oauth_links.provider_user_id = ")
            .push_bind(provider_user_id)
            .push(" LIMIT 1");
        Ok(self.fetch_auth_rows(builder).await?.into_iter().next())
    }

    async fn resolve_enabled_oauth_linked_user(
        &self,
        provider_type: &str,
        provider_user_id: &str,
        provider_username: Option<&str>,
        provider_email: Option<&str>,
        extra_data: Option<serde_json::Value>,
        verified_email: Option<&str>,
        touched_at: DateTime<Utc>,
        _provider_enabled_snapshot: bool,
    ) -> Result<ResolveOAuthLinkedUserOutcome, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let provider_enabled: Option<bool> =
            sqlx::query_scalar("SELECT is_enabled FROM oauth_providers WHERE provider_type = ?")
                .bind(provider_type)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if provider_enabled != Some(true) {
            tx.rollback().await.map_sql_err()?;
            return Ok(ResolveOAuthLinkedUserOutcome::ProviderUnavailable);
        }
        let row = sqlx::query(&format!(
            "{USER_AUTH_COLUMNS_QUALIFIED} JOIN user_oauth_links ON users.id = user_oauth_links.user_id WHERE user_oauth_links.provider_type = ? AND user_oauth_links.provider_user_id = ? LIMIT 1"
        ))
        .bind(provider_type)
        .bind(provider_user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(ResolveOAuthLinkedUserOutcome::NotLinked);
        };
        let mut user = map_user_auth_row(&row)?;
        sqlx::query(
            "UPDATE user_oauth_links SET provider_username = COALESCE(?, provider_username), provider_email = COALESCE(?, provider_email), extra_data = COALESCE(?, extra_data), last_login_at = ? WHERE provider_type = ? AND provider_user_id = ?",
        )
        .bind(provider_username)
        .bind(provider_email)
        .bind(optional_json_string(extra_data, "user_oauth_links.extra_data")?)
        .bind(touched_at.timestamp())
        .bind(provider_type)
        .bind(provider_user_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if let Some(verified_email) = verified_email {
            let result = sqlx::query(
                "UPDATE users SET email_verified = 1, updated_at = ? WHERE id = ? AND email_verified = 0 AND LOWER(TRIM(email)) = LOWER(TRIM(?))",
            )
            .bind(touched_at.timestamp())
            .bind(&user.id)
            .bind(verified_email)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            if result.rows_affected() == 1 {
                user.email_verified = true;
            }
        }
        tx.commit().await.map_sql_err()?;
        Ok(ResolveOAuthLinkedUserOutcome::Linked(user))
    }

    async fn touch_oauth_link(
        &self,
        provider_type: &str,
        provider_user_id: &str,
        provider_username: Option<&str>,
        provider_email: Option<&str>,
        extra_data: Option<serde_json::Value>,
        touched_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE user_oauth_links
SET provider_username = COALESCE(?, provider_username),
    provider_email = COALESCE(?, provider_email),
    extra_data = COALESCE(?, extra_data),
    last_login_at = ?
WHERE provider_type = ?
  AND provider_user_id = ?
"#,
        )
        .bind(provider_username)
        .bind(provider_email)
        .bind(optional_json_string(
            extra_data,
            "user_oauth_links.extra_data",
        )?)
        .bind(touched_at.timestamp())
        .bind(provider_type)
        .bind(provider_user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn create_oauth_auth_user(
        &self,
        email: Option<String>,
        email_verified: bool,
        username: String,
        created_at: DateTime<Utc>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let user_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers_mode, allowed_api_formats_mode, allowed_models_mode, rate_limit_mode,
  is_active, is_deleted, created_at, updated_at, last_login_at
)
VALUES (?, ?, ?, ?, NULL, 'user', 'oauth', 'inherit', 'inherit', 'inherit', 'inherit', 1, 0, ?, ?, ?)
"#,
        )
        .bind(&user_id)
        .bind(email)
        .bind(email_verified)
        .bind(username)
        .bind(created_at.timestamp())
        .bind(created_at.timestamp())
        .bind(created_at.timestamp())
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.find_user_auth_by_id(&user_id).await
    }

    async fn find_oauth_link_owner(
        &self,
        provider_type: &str,
        provider_user_id: &str,
    ) -> Result<Option<String>, DataLayerError> {
        sqlx::query_scalar(
            "SELECT user_id FROM user_oauth_links WHERE provider_type = ? AND provider_user_id = ? LIMIT 1",
        )
        .bind(provider_type)
        .bind(provider_user_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()
    }

    async fn has_user_oauth_provider_link(
        &self,
        user_id: &str,
        provider_type: &str,
    ) -> Result<bool, DataLayerError> {
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM user_oauth_links WHERE user_id = ? AND provider_type = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(provider_type)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        Ok(owner.is_some())
    }

    async fn count_user_oauth_links(&self, user_id: &str) -> Result<u64, DataLayerError> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_oauth_links WHERE user_id = ?")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await
                .map_sql_err()?;
        Ok(total.max(0) as u64)
    }

    async fn has_oauth_links_for_provider(
        &self,
        provider_type: &str,
    ) -> Result<bool, DataLayerError> {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM user_oauth_links WHERE provider_type = ? LIMIT 1")
                .bind(provider_type)
                .fetch_optional(&self.pool)
                .await
                .map_sql_err()?;
        Ok(exists.is_some())
    }

    async fn bind_user_oauth_link_if_provider_enabled(
        &self,
        user_id: &str,
        provider_type: &str,
        provider_user_id: &str,
        provider_username: Option<&str>,
        provider_email: Option<&str>,
        extra_data: Option<serde_json::Value>,
        linked_at: DateTime<Utc>,
        _provider_enabled_snapshot: bool,
        session_expectation: Option<&BindUserOAuthLinkSessionExpectation>,
    ) -> Result<BindUserOAuthLinkOutcome, DataLayerError> {
        let extra_data = optional_json_string(extra_data, "user_oauth_links.extra_data")?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let provider_enabled: Option<bool> =
            sqlx::query_scalar("SELECT is_enabled FROM oauth_providers WHERE provider_type = ?")
                .bind(provider_type)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if provider_enabled.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(BindUserOAuthLinkOutcome::ProviderNotFound);
        }
        if provider_enabled != Some(true) {
            tx.rollback().await.map_sql_err()?;
            return Ok(BindUserOAuthLinkOutcome::ProviderDisabled);
        }
        if let Some(expectation) = session_expectation {
            let session_is_current: Option<i32> = sqlx::query_scalar(
                r#"
SELECT 1
FROM users
JOIN user_sessions
  ON user_sessions.user_id = users.id
WHERE users.id = ?
  AND users.is_active = 1
  AND users.is_deleted = 0
  AND users.security_version = ?
  AND user_sessions.id = ?
  AND user_sessions.security_version = ?
  AND user_sessions.client_device_id = ?
  AND user_sessions.revoked_at IS NULL
  AND user_sessions.expires_at > MAX(?, CAST(strftime('%s', 'now') AS INTEGER))
"#,
            )
            .bind(user_id)
            .bind(expectation.security_version)
            .bind(&expectation.session_id)
            .bind(expectation.security_version)
            .bind(&expectation.client_device_id)
            .bind(expectation.checked_at.timestamp())
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
            if session_is_current.is_none() {
                tx.rollback().await.map_sql_err()?;
                return Ok(BindUserOAuthLinkOutcome::SessionUnavailable);
            }
        } else {
            let user_exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM users WHERE id = ?")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
            if user_exists.is_none() {
                tx.rollback().await.map_sql_err()?;
                return Ok(BindUserOAuthLinkOutcome::UserNotFound);
            }
        }
        if let Some(owner) = sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM user_oauth_links WHERE provider_type = ? AND provider_user_id = ? LIMIT 1",
        )
        .bind(provider_type)
        .bind(provider_user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?
        {
            tx.rollback().await.map_sql_err()?;
            return Ok(if owner == user_id {
                BindUserOAuthLinkOutcome::IdentityAlreadyBoundToUser
            } else {
                BindUserOAuthLinkOutcome::IdentityBoundToAnotherUser
            });
        }
        if sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM user_oauth_links WHERE user_id = ? AND provider_type = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(provider_type)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?
        .is_some()
        {
            tx.rollback().await.map_sql_err()?;
            return Ok(BindUserOAuthLinkOutcome::UserAlreadyLinkedProvider);
        }
        sqlx::query(
            r#"
INSERT INTO user_oauth_links (
  id, user_id, provider_type, provider_user_id, provider_username, provider_email,
  extra_data, linked_at, last_login_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(provider_type)
        .bind(provider_user_id)
        .bind(provider_username)
        .bind(provider_email)
        .bind(extra_data.as_deref())
        .bind(linked_at.timestamp())
        .bind(linked_at.timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(BindUserOAuthLinkOutcome::Bound)
    }

    async fn upgrade_oauth_email_verification_if_matches(
        &self,
        user_id: &str,
        verified_email: &str,
        verified_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            "UPDATE users SET email_verified = 1, updated_at = ? WHERE id = ? AND email_verified = 0 AND LOWER(TRIM(email)) = LOWER(TRIM(?))",
        )
        .bind(verified_at.timestamp())
        .bind(user_id)
        .bind(verified_email)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn delete_user_oauth_link(
        &self,
        user_id: &str,
        provider_type: &str,
        local_password_login_allowed: bool,
        _enabled_provider_types_snapshot: &[String],
    ) -> Result<DeleteUserOAuthLinkOutcome, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let user = sqlx::query("SELECT auth_source, password_hash FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(user) = user else {
            tx.rollback().await.map_sql_err()?;
            return Ok(DeleteUserOAuthLinkOutcome::NotFound);
        };
        let auth_source = user.try_get::<String, _>("auth_source").map_sql_err()?;
        let password_hash = user
            .try_get::<Option<String>, _>("password_hash")
            .map_sql_err()?;
        let provider_types = sqlx::query_scalar::<_, String>(
            "SELECT provider_type FROM user_oauth_links WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        if !provider_types.iter().any(|value| value == provider_type) {
            tx.rollback().await.map_sql_err()?;
            return Ok(DeleteUserOAuthLinkOutcome::NotFound);
        }
        let enabled_provider_types = sqlx::query_scalar::<_, String>(
            r#"
SELECT user_oauth_links.provider_type
FROM user_oauth_links
JOIN oauth_providers
  ON oauth_providers.provider_type = user_oauth_links.provider_type
WHERE user_oauth_links.user_id = ?
  AND oauth_providers.is_enabled = 1
"#,
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        let has_remaining_enabled_oauth_link = enabled_provider_types
            .iter()
            .any(|value| value != provider_type);
        if !has_remaining_enabled_oauth_link {
            if let Some(outcome) = last_oauth_unbind_denial(
                &auth_source,
                password_hash.as_deref(),
                local_password_login_allowed,
            ) {
                tx.rollback().await.map_sql_err()?;
                return Ok(outcome);
            }
        }
        let result =
            sqlx::query("DELETE FROM user_oauth_links WHERE user_id = ? AND provider_type = ?")
                .bind(user_id)
                .bind(provider_type)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        if result.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(DeleteUserOAuthLinkOutcome::NotFound);
        }
        tx.commit().await.map_sql_err()?;
        Ok(DeleteUserOAuthLinkOutcome::Deleted)
    }

    async fn get_or_create_ldap_auth_user(
        &self,
        email: String,
        username: String,
        ldap_dn: Option<String>,
        ldap_username: Option<String>,
        logged_in_at: DateTime<Utc>,
    ) -> Result<Option<LdapAuthUserProvisioningOutcome>, DataLayerError> {
        get_or_create_sqlite_ldap_auth_user(
            &self.pool,
            email,
            username,
            ldap_dn,
            ldap_username,
            logged_in_at,
        )
        .await
    }

    async fn touch_auth_user_last_login(
        &self,
        user_id: &str,
        logged_in_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query("UPDATE users SET last_login_at = ?, updated_at = ? WHERE id = ?")
            .bind(logged_in_at.timestamp())
            .bind(logged_in_at.timestamp())
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_local_auth_user_profile(
        &self,
        user_id: &str,
        email_present: bool,
        email: Option<String>,
        email_verified: Option<bool>,
        username: Option<String>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE users SET email = CASE WHEN ? THEN ? ELSE email END, email_verified = COALESCE(?, email_verified), username = COALESCE(?, username), updated_at = ? WHERE id = ?",
        )
        .bind(email_present)
        .bind(email)
        .bind(email_verified)
        .bind(username)
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.find_user_auth_by_id(user_id).await
    }

    async fn restore_local_auth_user_state_if_matches(
        &self,
        expected_auth: &StoredUserAuthRecord,
        restored_auth: &StoredUserAuthRecord,
        expected_export: &StoredUserExportRow,
        restored_export: &StoredUserExportRow,
        expected_model_capability_settings: Option<&serde_json::Value>,
        restored_model_capability_settings: Option<serde_json::Value>,
        expected_feature_settings: Option<&serde_json::Value>,
        restored_feature_settings: Option<serde_json::Value>,
    ) -> Result<bool, DataLayerError> {
        if expected_auth.id != restored_auth.id
            || expected_export.id != expected_auth.id
            || restored_export.id != restored_auth.id
        {
            return Ok(false);
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let auth_row = sqlx::query(&format!("{USER_AUTH_COLUMNS} WHERE id = ? LIMIT 1"))
            .bind(&expected_auth.id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let export_row = sqlx::query(&format!("{USER_EXPORT_COLUMNS} WHERE id = ? LIMIT 1"))
            .bind(&expected_auth.id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let (Some(auth_row), Some(export_row)) = (auth_row, export_row) else {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        };
        let current_auth = map_user_auth_row(&auth_row)?;
        let current_export = map_user_export_row(&export_row)?;
        if !current_auth.matches_restore_state(expected_auth)
            || !current_export.matches_restore_state(expected_export)
            || current_export.rate_limit != expected_export.rate_limit
            || current_export.rate_limit_mode != expected_export.rate_limit_mode
            || current_export.model_capability_settings.as_ref()
                != expected_model_capability_settings
            || current_export.feature_settings.as_ref() != expected_feature_settings
        {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        let removes_active_admin = current_auth.role.eq_ignore_ascii_case("admin")
            && current_auth.is_active
            && !current_auth.is_deleted
            && (!restored_auth.role.eq_ignore_ascii_case("admin") || !restored_auth.is_active);
        if removes_active_admin {
            let active_admin_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM users WHERE LOWER(role) = 'admin' AND is_active = 1 AND is_deleted = 0",
            )
            .fetch_one(&mut *tx)
            .await
            .map_sql_err()?;
            if active_admin_count <= 1 {
                tx.rollback().await.map_sql_err()?;
                return Err(DataLayerError::InvalidInput(
                    LAST_ACTIVE_ADMIN_UPDATE_DENIED.to_string(),
                ));
            }
        }

        let security_state_changed = expected_auth.role != restored_auth.role
            || expected_auth.is_active != restored_auth.is_active;
        let result = sqlx::query(
            r#"
UPDATE users
SET email = ?,
    email_verified = ?,
    username = ?,
    role = ?,
    allowed_providers = ?,
    allowed_providers_mode = ?,
    allowed_api_formats = ?,
    allowed_api_formats_mode = ?,
    allowed_models = ?,
    allowed_models_mode = ?,
    rate_limit = ?,
    rate_limit_mode = ?,
    model_capability_settings = ?,
    feature_settings = ?,
    is_active = ?,
    security_version = security_version + CASE WHEN ? THEN 1 ELSE 0 END,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(restored_auth.email.as_deref())
        .bind(restored_auth.email_verified)
        .bind(&restored_auth.username)
        .bind(&restored_auth.role)
        .bind(optional_string_list_json(
            restored_auth.allowed_providers.clone(),
            "users.allowed_providers",
        )?)
        .bind(&restored_auth.allowed_providers_mode)
        .bind(optional_string_list_json(
            restored_auth.allowed_api_formats.clone(),
            "users.allowed_api_formats",
        )?)
        .bind(&restored_auth.allowed_api_formats_mode)
        .bind(optional_string_list_json(
            restored_auth.allowed_models.clone(),
            "users.allowed_models",
        )?)
        .bind(&restored_auth.allowed_models_mode)
        .bind(restored_export.rate_limit)
        .bind(&restored_export.rate_limit_mode)
        .bind(optional_json_string(
            restored_model_capability_settings.clone(),
            "users.model_capability_settings",
        )?)
        .bind(optional_json_string(
            restored_feature_settings.clone(),
            "users.feature_settings",
        )?)
        .bind(restored_auth.is_active)
        .bind(security_state_changed)
        .bind(current_unix_secs())
        .bind(&expected_auth.id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if result.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        if security_state_changed {
            let now = current_unix_secs();
            sqlx::query(
                "UPDATE user_sessions SET revoked_at = ?, revoke_reason = 'user_security_state_changed', updated_at = ? WHERE user_id = ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(now)
            .bind(&expected_auth.id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query(
                "UPDATE api_keys SET is_active = 0, updated_at = ? WHERE user_id = ? AND is_active = 1",
            )
            .bind(now)
            .bind(&expected_auth.id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query(
                "UPDATE management_tokens SET is_active = 0, updated_at = ? WHERE user_id = ? AND is_active = 1",
            )
            .bind(now)
            .bind(&expected_auth.id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        Ok(true)
    }

    async fn update_local_auth_user_password_hash(
        &self,
        user_id: &str,
        password_hash: String,
        updated_at: DateTime<Utc>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let result = sqlx::query(
            "UPDATE users SET password_hash = ?, security_version = security_version + 1, updated_at = ? WHERE id = ?",
        )
            .bind(password_hash)
            .bind(updated_at.timestamp())
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.find_user_auth_by_id(user_id).await
    }

    async fn restore_local_auth_user_password_hash_if_matches(
        &self,
        user_id: &str,
        expected_password_hash: Option<&str>,
        password_hash: Option<String>,
        updated_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE users
SET password_hash = ?,
    security_version = security_version + 1,
    updated_at = ?
WHERE id = ?
  AND ((? IS NULL AND password_hash IS NULL) OR password_hash = ?)
"#,
        )
        .bind(password_hash)
        .bind(updated_at.timestamp())
        .bind(user_id)
        .bind(expected_password_hash)
        .bind(expected_password_hash)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn reset_local_auth_user_password_and_revoke_sessions(
        &self,
        user_id: &str,
        password_hash: String,
        changed_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let updated = sqlx::query(
            "UPDATE users SET password_hash = ?, security_version = security_version + 1, updated_at = ? WHERE id = ? AND is_deleted = 0",
        )
        .bind(password_hash)
        .bind(changed_at.timestamp())
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE user_sessions SET revoked_at = ?, revoke_reason = 'admin_password_reset', updated_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(changed_at.timestamp())
        .bind(changed_at.timestamp())
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(true)
    }

    async fn change_local_auth_password_and_revoke_sessions(
        &self,
        user_id: &str,
        current_session_id: &str,
        expected_password_hash: Option<&str>,
        next_password_hash: String,
        changed_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let updated = sqlx::query(
            r#"
UPDATE users
SET password_hash = ?, security_version = security_version + 1, updated_at = ?
WHERE id = ?
  AND is_active = 1
  AND is_deleted = 0
  AND ((? IS NULL AND password_hash IS NULL) OR password_hash = ?)
  AND EXISTS (
    SELECT 1 FROM user_sessions
    WHERE user_id = ? AND id = ? AND revoked_at IS NULL AND expires_at > ?
  )
"#,
        )
        .bind(next_password_hash)
        .bind(changed_at.timestamp())
        .bind(user_id)
        .bind(expected_password_hash)
        .bind(expected_password_hash)
        .bind(user_id)
        .bind(current_session_id)
        .bind(changed_at.timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let revoked = sqlx::query(
            "UPDATE user_sessions SET revoked_at = ?, revoke_reason = 'password_changed', updated_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(changed_at.timestamp())
        .bind(changed_at.timestamp())
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if revoked.rows_affected() == 0 {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        tx.commit().await.map_sql_err()?;
        Ok(true)
    }

    async fn update_local_auth_user_admin_fields(
        &self,
        user_id: &str,
        role: Option<String>,
        allowed_providers_present: bool,
        allowed_providers: Option<Vec<String>>,
        allowed_api_formats_present: bool,
        allowed_api_formats: Option<Vec<String>>,
        allowed_models_present: bool,
        allowed_models: Option<Vec<String>>,
        rate_limit_present: bool,
        rate_limit: Option<i32>,
        is_active: Option<bool>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let removes_active_admin = role
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("admin"))
            || is_active == Some(false);
        let allowed_providers_mode = if allowed_providers
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let allowed_api_formats_mode = if allowed_api_formats
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let allowed_models_mode = if allowed_models
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let rate_limit_mode = if rate_limit.is_some() {
            "custom"
        } else {
            "system"
        };
        let update_sql = format!(
            r#"
UPDATE users
SET role = CASE WHEN ? THEN COALESCE(?, role) ELSE role END,
    allowed_providers = CASE WHEN ? THEN ? ELSE allowed_providers END,
    allowed_providers_mode = CASE WHEN ? THEN ? ELSE allowed_providers_mode END,
    allowed_api_formats = CASE WHEN ? THEN ? ELSE allowed_api_formats END,
    allowed_api_formats_mode = CASE WHEN ? THEN ? ELSE allowed_api_formats_mode END,
    allowed_models = CASE WHEN ? THEN ? ELSE allowed_models END,
    allowed_models_mode = CASE WHEN ? THEN ? ELSE allowed_models_mode END,
    rate_limit = CASE WHEN ? THEN ? ELSE rate_limit END,
    rate_limit_mode = CASE WHEN ? THEN ? ELSE rate_limit_mode END,
    is_active = CASE WHEN ? THEN ? ELSE is_active END,
    security_version = security_version + CASE WHEN ? THEN 1 ELSE 0 END,
    updated_at = ?
WHERE id = ?
{SQLITE_ACTIVE_ADMIN_UPDATE_GUARD}
"#,
        );
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let current_security_state = sqlx::query("SELECT role, is_active FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let security_state_changed = current_security_state.as_ref().is_some_and(|row| {
            role.as_deref().is_some_and(|next_role| {
                row.try_get::<String, _>("role")
                    .is_ok_and(|current_role| !current_role.eq_ignore_ascii_case(next_role))
            }) || is_active.is_some_and(|next_active| {
                row.try_get::<bool, _>("is_active")
                    .is_ok_and(|current_active| current_active != next_active)
            })
        });
        let result = sqlx::query(&update_sql)
            .bind(role.is_some())
            .bind(role)
            .bind(allowed_providers_present)
            .bind(optional_string_list_json(
                allowed_providers,
                "users.allowed_providers",
            )?)
            .bind(allowed_providers_present)
            .bind(allowed_providers_mode)
            .bind(allowed_api_formats_present)
            .bind(optional_string_list_json(
                allowed_api_formats,
                "users.allowed_api_formats",
            )?)
            .bind(allowed_api_formats_present)
            .bind(allowed_api_formats_mode)
            .bind(allowed_models_present)
            .bind(optional_string_list_json(
                allowed_models,
                "users.allowed_models",
            )?)
            .bind(allowed_models_present)
            .bind(allowed_models_mode)
            .bind(rate_limit_present)
            .bind(rate_limit)
            .bind(rate_limit_present)
            .bind(rate_limit_mode)
            .bind(is_active.is_some())
            .bind(is_active)
            .bind(security_state_changed)
            .bind(chrono::Utc::now().timestamp())
            .bind(user_id)
            .bind(removes_active_admin)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        if result.rows_affected() == 0 {
            let blocked_active_admin: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM users WHERE id = ? AND LOWER(role) = 'admin' AND is_active = 1 AND is_deleted = 0 LIMIT 1",
            )
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
            tx.rollback().await.map_sql_err()?;
            if removes_active_admin && blocked_active_admin.is_some() {
                return Err(DataLayerError::InvalidInput(
                    LAST_ACTIVE_ADMIN_UPDATE_DENIED.to_string(),
                ));
            }
            return Ok(None);
        }
        if security_state_changed {
            let revoked_at = chrono::Utc::now().timestamp();
            sqlx::query(
                "UPDATE user_sessions SET revoked_at = ?, revoke_reason = 'user_security_state_changed', updated_at = ? WHERE user_id = ? AND revoked_at IS NULL",
            )
            .bind(revoked_at)
            .bind(revoked_at)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query(
                "UPDATE api_keys SET is_active = 0, updated_at = ? WHERE user_id = ? AND is_active = 1",
            )
            .bind(revoked_at)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query(
                "UPDATE management_tokens SET is_active = 0, updated_at = ? WHERE user_id = ? AND is_active = 1",
            )
            .bind(revoked_at)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        self.find_user_auth_by_id(user_id).await
    }

    async fn update_local_auth_user_policy_modes(
        &self,
        user_id: &str,
        allowed_providers_mode: Option<String>,
        allowed_api_formats_mode: Option<String>,
        allowed_models_mode: Option<String>,
        rate_limit_mode: Option<String>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE users
SET allowed_providers_mode = CASE WHEN ? THEN ? ELSE allowed_providers_mode END,
    allowed_api_formats_mode = CASE WHEN ? THEN ? ELSE allowed_api_formats_mode END,
    allowed_models_mode = CASE WHEN ? THEN ? ELSE allowed_models_mode END,
    rate_limit_mode = CASE WHEN ? THEN ? ELSE rate_limit_mode END,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(allowed_providers_mode.is_some())
        .bind(allowed_providers_mode)
        .bind(allowed_api_formats_mode.is_some())
        .bind(allowed_api_formats_mode)
        .bind(allowed_models_mode.is_some())
        .bind(allowed_models_mode)
        .bind(rate_limit_mode.is_some())
        .bind(rate_limit_mode)
        .bind(chrono::Utc::now().timestamp())
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.find_user_auth_by_id(user_id).await
    }

    async fn update_user_model_capability_settings(
        &self,
        user_id: &str,
        settings: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>, DataLayerError> {
        let normalized = normalize_optional_json_value(settings);
        let result = sqlx::query(
            "UPDATE users SET model_capability_settings = ?, updated_at = ? WHERE id = ?",
        )
        .bind(optional_json_string(
            normalized.clone(),
            "users.model_capability_settings",
        )?)
        .bind(chrono::Utc::now().timestamp())
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(normalized)
    }

    async fn update_user_feature_settings(
        &self,
        user_id: &str,
        settings: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>, DataLayerError> {
        let normalized = normalize_optional_json_value(settings);
        let result =
            sqlx::query("UPDATE users SET feature_settings = ?, updated_at = ? WHERE id = ?")
                .bind(optional_json_string(
                    normalized.clone(),
                    "users.feature_settings",
                )?)
                .bind(chrono::Utc::now().timestamp())
                .bind(user_id)
                .execute(&self.pool)
                .await
                .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(normalized)
    }

    async fn create_local_auth_user(
        &self,
        email: Option<String>,
        email_verified: bool,
        username: String,
        password_hash: String,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let user_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers_mode, allowed_api_formats_mode, allowed_models_mode, rate_limit_mode,
  is_active, is_deleted, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, 'user', 'local', 'inherit', 'inherit', 'inherit', 'inherit', 1, 0, ?, ?)
"#,
        )
        .bind(&user_id)
        .bind(email)
        .bind(email_verified)
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.find_user_auth_by_id(&user_id).await
    }

    async fn create_local_auth_user_with_settings(
        &self,
        email: Option<String>,
        email_verified: bool,
        username: String,
        password_hash: String,
        role: String,
        allowed_providers: Option<Vec<String>>,
        allowed_api_formats: Option<Vec<String>>,
        allowed_models: Option<Vec<String>>,
        rate_limit: Option<i32>,
    ) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
        let user_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let allowed_providers_mode = if allowed_providers
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let allowed_api_formats_mode = if allowed_api_formats
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let allowed_models_mode = if allowed_models
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            "specific"
        } else {
            "unrestricted"
        };
        let rate_limit_mode = if rate_limit.is_some() {
            "custom"
        } else {
            "system"
        };
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers, allowed_providers_mode,
  allowed_api_formats, allowed_api_formats_mode,
  allowed_models, allowed_models_mode,
  rate_limit, rate_limit_mode,
  is_active, is_deleted, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, 'local', ?, ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, ?)
"#,
        )
        .bind(&user_id)
        .bind(email)
        .bind(email_verified)
        .bind(username)
        .bind(password_hash)
        .bind(role)
        .bind(optional_string_list_json(
            allowed_providers,
            "users.allowed_providers",
        )?)
        .bind(allowed_providers_mode)
        .bind(optional_string_list_json(
            allowed_api_formats,
            "users.allowed_api_formats",
        )?)
        .bind(allowed_api_formats_mode)
        .bind(optional_string_list_json(
            allowed_models,
            "users.allowed_models",
        )?)
        .bind(allowed_models_mode)
        .bind(rate_limit)
        .bind(rate_limit_mode)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.find_user_auth_by_id(&user_id).await
    }

    async fn delete_local_auth_user(&self, user_id: &str) -> Result<bool, DataLayerError> {
        self.delete_local_auth_user_inner(user_id, false).await
    }

    async fn delete_local_auth_user_if_wallet_absent(
        &self,
        user_id: &str,
    ) -> Result<bool, DataLayerError> {
        self.delete_local_auth_user_inner(user_id, true).await
    }

    async fn count_active_admin_users(&self) -> Result<u64, DataLayerError> {
        let total: i64 = sqlx::query_scalar(
            r#"
SELECT COUNT(*)
FROM users
WHERE LOWER(role) = 'admin'
  AND is_deleted = 0
  AND is_active = 1
"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_sql_err()?;
        Ok(total.max(0) as u64)
    }

    async fn read_user_preferences(
        &self,
        user_id: &str,
    ) -> Result<Option<StoredUserPreferenceRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_PREFERENCES_COLUMNS);
        builder.push(" WHERE up.user_id = ").push_bind(user_id);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_user_preference_row).transpose()
    }

    async fn write_user_preferences(
        &self,
        preferences: &StoredUserPreferenceRecord,
    ) -> Result<Option<StoredUserPreferenceRecord>, DataLayerError> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
INSERT INTO user_preferences (
  id, user_id, avatar_url, bio, default_provider_id, theme, language, timezone,
  email_notifications, usage_alerts, announcement_notifications, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(user_id) DO UPDATE SET
  avatar_url = excluded.avatar_url,
  bio = excluded.bio,
  default_provider_id = excluded.default_provider_id,
  theme = excluded.theme,
  language = excluded.language,
  timezone = excluded.timezone,
  email_notifications = excluded.email_notifications,
  usage_alerts = excluded.usage_alerts,
  announcement_notifications = excluded.announcement_notifications,
  updated_at = excluded.updated_at
"#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&preferences.user_id)
        .bind(preferences.avatar_url.as_deref())
        .bind(preferences.bio.as_deref())
        .bind(preferences.default_provider_id.as_deref())
        .bind(&preferences.theme)
        .bind(&preferences.language)
        .bind(&preferences.timezone)
        .bind(preferences.email_notifications)
        .bind(preferences.usage_alerts)
        .bind(preferences.announcement_notifications)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.read_user_preferences(&preferences.user_id).await
    }

    async fn find_user_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<StoredUserSessionRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_SESSION_COLUMNS);
        builder
            .push(" WHERE user_id = ")
            .push_bind(user_id)
            .push(" AND id = ")
            .push_bind(session_id)
            .push(" LIMIT 1");
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_user_session_row).transpose()
    }

    async fn list_user_sessions(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUserSessionRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(USER_SESSION_COLUMNS);
        builder
            .push(" WHERE user_id = ")
            .push_bind(user_id)
            .push(" AND revoked_at IS NULL AND expires_at > ")
            .push_bind(Utc::now().timestamp())
            .push(" ORDER BY last_seen_at DESC, created_at DESC");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_user_session_row).collect()
    }

    async fn create_user_session(
        &self,
        session: &StoredUserSessionRecord,
    ) -> Result<Option<StoredUserSessionRecord>, DataLayerError> {
        let now = session
            .created_at
            .or(session.updated_at)
            .or(session.last_seen_at)
            .unwrap_or_else(Utc::now);
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let user_is_eligible: Option<i64> = sqlx::query_scalar(
            "SELECT security_version FROM users WHERE id = ? AND is_active = 1 AND is_deleted = 0 AND security_version = ?",
        )
        .bind(&session.user_id)
        .bind(session.security_version)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        if user_is_eligible.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        }
        sqlx::query(
            r#"
UPDATE user_sessions
SET revoked_at = ?, revoke_reason = 'replaced_by_new_login', updated_at = ?
WHERE user_id = ? AND client_device_id = ? AND revoked_at IS NULL AND expires_at > ?
"#,
        )
        .bind(now.timestamp())
        .bind(now.timestamp())
        .bind(&session.user_id)
        .bind(&session.client_device_id)
        .bind(now.timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        sqlx::query(
            r#"
INSERT INTO user_sessions (
  id, user_id, security_version, client_device_id, device_label, device_type, ip_address, user_agent,
  refresh_token_hash, last_seen_at, expires_at, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, 'unknown', ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&session.id)
        .bind(&session.user_id)
        .bind(session.security_version)
        .bind(&session.client_device_id)
        .bind(session.device_label.as_deref())
        .bind(session.ip_address.as_deref())
        .bind(session.user_agent.as_deref())
        .bind(&session.refresh_token_hash)
        .bind(session.last_seen_at.unwrap_or(now).timestamp())
        .bind(session.expires_at.unwrap_or(now).timestamp())
        .bind(session.created_at.unwrap_or(now).timestamp())
        .bind(session.updated_at.unwrap_or(now).timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        let mut builder = QueryBuilder::<Sqlite>::new(USER_SESSION_COLUMNS);
        builder
            .push(" WHERE user_id = ")
            .push_bind(&session.user_id)
            .push(" AND id = ")
            .push_bind(&session.id)
            .push(" LIMIT 1");
        let row = builder
            .build()
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let created = row.as_ref().map(map_user_session_row).transpose()?;
        tx.commit().await.map_sql_err()?;
        Ok(created)
    }

    async fn create_user_session_if_password_matches(
        &self,
        session: &StoredUserSessionRecord,
        expected_password_hash: &str,
    ) -> Result<Option<StoredUserSessionRecord>, DataLayerError> {
        let now = session
            .created_at
            .or(session.updated_at)
            .or(session.last_seen_at)
            .unwrap_or_else(Utc::now);
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let matched = sqlx::query_scalar::<_, String>(
            r#"
SELECT password_hash FROM users
WHERE id = ? AND password_hash = ? AND LOWER(auth_source) = 'local'
  AND is_active = 1 AND is_deleted = 0 AND security_version = ?
"#,
        )
        .bind(&session.user_id)
        .bind(expected_password_hash)
        .bind(session.security_version)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        if matched.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        }
        sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
            .bind(now.timestamp())
            .bind(&session.user_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        sqlx::query(
            r#"
UPDATE user_sessions
SET revoked_at = ?, revoke_reason = 'replaced_by_new_login', updated_at = ?
WHERE user_id = ? AND client_device_id = ? AND revoked_at IS NULL AND expires_at > ?
"#,
        )
        .bind(now.timestamp())
        .bind(now.timestamp())
        .bind(&session.user_id)
        .bind(&session.client_device_id)
        .bind(now.timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        sqlx::query(
            r#"
INSERT INTO user_sessions (
  id, user_id, security_version, client_device_id, device_label, device_type, ip_address, user_agent,
  refresh_token_hash, last_seen_at, expires_at, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, 'unknown', ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&session.id)
        .bind(&session.user_id)
        .bind(session.security_version)
        .bind(&session.client_device_id)
        .bind(session.device_label.as_deref())
        .bind(session.ip_address.as_deref())
        .bind(session.user_agent.as_deref())
        .bind(&session.refresh_token_hash)
        .bind(session.last_seen_at.unwrap_or(now).timestamp())
        .bind(session.expires_at.unwrap_or(now).timestamp())
        .bind(session.created_at.unwrap_or(now).timestamp())
        .bind(session.updated_at.unwrap_or(now).timestamp())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        self.find_user_session(&session.user_id, &session.id).await
    }

    async fn touch_user_session(
        &self,
        user_id: &str,
        session_id: &str,
        touched_at: DateTime<Utc>,
        ip_address: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE user_sessions
SET last_seen_at = ?, ip_address = COALESCE(?, ip_address),
    user_agent = COALESCE(?, user_agent), updated_at = ?
WHERE user_id = ? AND id = ?
"#,
        )
        .bind(touched_at.timestamp())
        .bind(ip_address)
        .bind(user_agent.map(|value| value.chars().take(1000).collect::<String>()))
        .bind(touched_at.timestamp())
        .bind(user_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_user_session_device_label(
        &self,
        user_id: &str,
        session_id: &str,
        device_label: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE user_sessions
SET device_label = ?, updated_at = ?
WHERE user_id = ? AND id = ?
"#,
        )
        .bind(device_label.chars().take(120).collect::<String>())
        .bind(updated_at.timestamp())
        .bind(user_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn rotate_user_session_refresh_token(
        &self,
        user_id: &str,
        session_id: &str,
        expected_refresh_token_hash: &str,
        next_refresh_token_hash: &str,
        rotated_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        ip_address: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE user_sessions
SET prev_refresh_token_hash = ?, rotated_at = ?, refresh_token_hash = ?,
    expires_at = ?, last_seen_at = ?, ip_address = COALESCE(?, ip_address),
    user_agent = COALESCE(?, user_agent), updated_at = ?
WHERE user_id = ? AND id = ? AND refresh_token_hash = ?
  AND revoked_at IS NULL AND expires_at > ?
"#,
        )
        .bind(expected_refresh_token_hash)
        .bind(rotated_at.timestamp())
        .bind(next_refresh_token_hash)
        .bind(expires_at.timestamp())
        .bind(rotated_at.timestamp())
        .bind(ip_address)
        .bind(user_agent.map(|value| value.chars().take(1000).collect::<String>()))
        .bind(rotated_at.timestamp())
        .bind(user_id)
        .bind(session_id)
        .bind(expected_refresh_token_hash)
        .bind(rotated_at.timestamp())
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn revoke_user_session(
        &self,
        user_id: &str,
        session_id: &str,
        revoked_at: DateTime<Utc>,
        reason: &str,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            "UPDATE user_sessions SET revoked_at = ?, revoke_reason = ?, updated_at = ? WHERE user_id = ? AND id = ?",
        )
        .bind(revoked_at.timestamp())
        .bind(reason.chars().take(100).collect::<String>())
        .bind(revoked_at.timestamp())
        .bind(user_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn revoke_all_user_sessions(
        &self,
        user_id: &str,
        revoked_at: DateTime<Utc>,
        reason: &str,
    ) -> Result<u64, DataLayerError> {
        let result = sqlx::query(
            "UPDATE user_sessions SET revoked_at = ?, revoke_reason = ?, updated_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(revoked_at.timestamp())
        .bind(reason.chars().take(100).collect::<String>())
        .bind(revoked_at.timestamp())
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected())
    }

    async fn count_active_local_admin_users_with_valid_password(
        &self,
    ) -> Result<u64, DataLayerError> {
        let hashes = sqlx::query_scalar::<_, String>(
            r#"
SELECT password_hash
FROM users
WHERE LOWER(role) = 'admin'
  AND LOWER(auth_source) = 'local'
  AND is_deleted = 0
  AND is_active = 1
  AND password_hash IS NOT NULL
"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        Ok(hashes
            .iter()
            .filter(|hash| is_valid_bcrypt_hash(hash))
            .count() as u64)
    }
}

fn optional_json_from_string(
    value: Option<String>,
    field_name: &str,
) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains invalid JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn optional_string_list_json(
    value: Option<Vec<String>>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} could not be serialized as JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn json_string_from_option_vec(value: Option<&Vec<String>>) -> Option<String> {
    value.and_then(|items| serde_json::to_string(items).ok())
}

fn normalized_ids(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn current_unix_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn optional_json_string(
    value: Option<serde_json::Value>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} could not be serialized as JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn normalize_optional_json_value(value: Option<serde_json::Value>) -> Option<serde_json::Value> {
    match value {
        Some(serde_json::Value::Null) | None => None,
        Some(value) => Some(value),
    }
}

async fn get_or_create_sqlite_ldap_auth_user(
    pool: &SqlitePool,
    email: String,
    username: String,
    ldap_dn: Option<String>,
    ldap_username: Option<String>,
    logged_in_at: DateTime<Utc>,
) -> Result<Option<LdapAuthUserProvisioningOutcome>, DataLayerError> {
    let existing =
        find_sqlite_ldap_auth_user(pool, ldap_dn.as_deref(), ldap_username.as_deref(), &email)
            .await?;
    if let Some(existing) = existing {
        if existing.is_deleted
            || !existing.is_active
            || !existing.auth_source.eq_ignore_ascii_case("ldap")
        {
            return Ok(None);
        }
        if existing.email.as_deref() != Some(email.as_str()) {
            let taken: Option<i64> =
                sqlx::query_scalar("SELECT 1 FROM users WHERE email = ? AND id <> ? LIMIT 1")
                    .bind(&email)
                    .bind(&existing.id)
                    .fetch_optional(pool)
                    .await
                    .map_sql_err()?;
            if taken.is_some() {
                return Ok(None);
            }
        }
        sqlx::query("UPDATE users SET email = ?, email_verified = 1, ldap_dn = COALESCE(?, ldap_dn), ldap_username = COALESCE(?, ldap_username), last_login_at = ?, updated_at = ? WHERE id = ?")
            .bind(&email)
            .bind(ldap_dn.as_deref())
            .bind(ldap_username.as_deref())
            .bind(logged_in_at.timestamp())
            .bind(logged_in_at.timestamp())
            .bind(&existing.id)
            .execute(pool)
            .await
            .map_sql_err()?;
        let user = find_sqlite_auth_by_id(pool, &existing.id)
            .await?
            .ok_or_else(|| {
                DataLayerError::UnexpectedValue("updated LDAP user disappeared".to_string())
            })?;
        return Ok(Some(LdapAuthUserProvisioningOutcome {
            user,
            created: false,
        }));
    }

    let base_username = ldap_username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(username.as_str())
        .trim()
        .to_string();
    let mut candidate_username = base_username.clone();
    for _attempt in 0..3 {
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM users WHERE username = ? LIMIT 1")
                .bind(&candidate_username)
                .fetch_optional(pool)
                .await
                .map_sql_err()?;
        if taken.is_some() {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            candidate_username = format!(
                "{}_ldap_{}{}",
                base_username,
                logged_in_at.timestamp(),
                &suffix[..4]
            );
            continue;
        }
        let user_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, email, email_verified, username, password_hash, role, auth_source, ldap_dn, ldap_username, is_active, is_deleted, created_at, updated_at, last_login_at) VALUES (?, ?, 1, ?, NULL, 'user', 'ldap', ?, ?, 1, 0, ?, ?, ?)")
            .bind(&user_id)
            .bind(&email)
            .bind(&candidate_username)
            .bind(ldap_dn.as_deref())
            .bind(ldap_username.as_deref())
            .bind(logged_in_at.timestamp())
            .bind(logged_in_at.timestamp())
            .bind(logged_in_at.timestamp())
            .execute(pool)
            .await
            .map_sql_err()?;
        let user = find_sqlite_auth_by_id(pool, &user_id)
            .await?
            .ok_or_else(|| {
                DataLayerError::UnexpectedValue("created LDAP user disappeared".to_string())
            })?;
        return Ok(Some(LdapAuthUserProvisioningOutcome {
            user,
            created: true,
        }));
    }
    Ok(None)
}

async fn find_sqlite_ldap_auth_user(
    pool: &SqlitePool,
    ldap_dn: Option<&str>,
    ldap_username: Option<&str>,
    email: &str,
) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
    if let Some(ldap_dn) = ldap_dn.filter(|value| !value.trim().is_empty()) {
        let row = sqlx::query(&format!(
            "{USER_AUTH_COLUMNS} WHERE auth_source = 'ldap' AND ldap_dn = ? LIMIT 1"
        ))
        .bind(ldap_dn)
        .fetch_optional(pool)
        .await
        .map_sql_err()?;
        if let Some(row) = row.as_ref() {
            return map_user_auth_row(row).map(Some);
        }
    }
    if let Some(ldap_username) = ldap_username.filter(|value| !value.trim().is_empty()) {
        let row = sqlx::query(&format!(
            "{USER_AUTH_COLUMNS} WHERE auth_source = 'ldap' AND ldap_username = ? LIMIT 1"
        ))
        .bind(ldap_username)
        .fetch_optional(pool)
        .await
        .map_sql_err()?;
        if let Some(row) = row.as_ref() {
            return map_user_auth_row(row).map(Some);
        }
    }
    let row = sqlx::query(&format!("{USER_AUTH_COLUMNS} WHERE email = ? LIMIT 1"))
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_sql_err()?;
    row.as_ref().map(map_user_auth_row).transpose()
}

async fn find_sqlite_auth_by_id(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Option<StoredUserAuthRecord>, DataLayerError> {
    let row = sqlx::query(&format!("{USER_AUTH_COLUMNS} WHERE id = ? LIMIT 1"))
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_sql_err()?;
    row.as_ref().map(map_user_auth_row).transpose()
}

fn optional_datetime_from_unix_secs(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.and_then(|value| Utc.timestamp_opt(value, 0).single())
}

fn map_user_row(row: &SqliteRow) -> Result<StoredUserSummary, DataLayerError> {
    StoredUserSummary::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("email").map_sql_err()?,
        row.try_get("role").map_sql_err()?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("is_deleted").map_sql_err()?,
    )
}

fn map_user_export_row(row: &SqliteRow) -> Result<StoredUserExportRow, DataLayerError> {
    let feature_settings = optional_json_from_string(
        row.try_get("feature_settings").map_sql_err()?,
        "users.feature_settings",
    )?;
    StoredUserExportRow::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("email").map_sql_err()?,
        row.try_get("email_verified").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("password_hash").map_sql_err()?,
        row.try_get("role").map_sql_err()?,
        row.try_get("auth_source").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_providers").map_sql_err()?,
            "users.allowed_providers",
        )?,
        optional_json_from_string(
            row.try_get("allowed_api_formats").map_sql_err()?,
            "users.allowed_api_formats",
        )?,
        optional_json_from_string(
            row.try_get("allowed_models").map_sql_err()?,
            "users.allowed_models",
        )?,
        row.try_get("rate_limit").map_sql_err()?,
        optional_json_from_string(
            row.try_get("model_capability_settings").map_sql_err()?,
            "users.model_capability_settings",
        )?,
        row.try_get("is_active").map_sql_err()?,
    )
    .map(|record| record.with_feature_settings(feature_settings))
    .and_then(|record| {
        record.with_policy_modes(
            row.try_get("allowed_providers_mode").map_sql_err()?,
            row.try_get("allowed_api_formats_mode").map_sql_err()?,
            row.try_get("allowed_models_mode").map_sql_err()?,
            row.try_get("rate_limit_mode").map_sql_err()?,
        )
    })
}

fn map_user_auth_row(row: &SqliteRow) -> Result<StoredUserAuthRecord, DataLayerError> {
    StoredUserAuthRecord::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("email").map_sql_err()?,
        row.try_get("email_verified").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("password_hash").map_sql_err()?,
        row.try_get("role").map_sql_err()?,
        row.try_get("auth_source").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_providers").map_sql_err()?,
            "users.allowed_providers",
        )?,
        optional_json_from_string(
            row.try_get("allowed_api_formats").map_sql_err()?,
            "users.allowed_api_formats",
        )?,
        optional_json_from_string(
            row.try_get("allowed_models").map_sql_err()?,
            "users.allowed_models",
        )?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("is_deleted").map_sql_err()?,
        optional_datetime_from_unix_secs(row.try_get("created_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("last_login_at").map_sql_err()?),
    )
    .and_then(|record| record.with_security_version(row.try_get("security_version").map_sql_err()?))
    .and_then(|record| {
        record.with_policy_modes(
            row.try_get("allowed_providers_mode").map_sql_err()?,
            row.try_get("allowed_api_formats_mode").map_sql_err()?,
            row.try_get("allowed_models_mode").map_sql_err()?,
        )
    })
}

fn map_user_group_row(row: &SqliteRow) -> Result<StoredUserGroup, DataLayerError> {
    StoredUserGroup::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        row.try_get("normalized_name").map_sql_err()?,
        row.try_get("description").map_sql_err()?,
        row.try_get("priority").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_providers").map_sql_err()?,
            "user_groups.allowed_providers",
        )?,
        row.try_get("allowed_providers_mode").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_api_formats").map_sql_err()?,
            "user_groups.allowed_api_formats",
        )?,
        row.try_get("allowed_api_formats_mode").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_models").map_sql_err()?,
            "user_groups.allowed_models",
        )?,
        row.try_get("allowed_models_mode").map_sql_err()?,
        row.try_get("rate_limit").map_sql_err()?,
        row.try_get("rate_limit_mode").map_sql_err()?,
        optional_datetime_from_unix_secs(row.try_get("created_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("updated_at").map_sql_err()?),
    )
}

fn map_user_group_member_row(row: &SqliteRow) -> Result<StoredUserGroupMember, DataLayerError> {
    Ok(StoredUserGroupMember {
        group_id: row.try_get("group_id").map_sql_err()?,
        user_id: row.try_get("user_id").map_sql_err()?,
        username: row.try_get("username").map_sql_err()?,
        email: row.try_get("email").map_sql_err()?,
        role: row.try_get("role").map_sql_err()?,
        is_active: row.try_get("is_active").map_sql_err()?,
        is_deleted: row.try_get("is_deleted").map_sql_err()?,
        created_at: optional_datetime_from_unix_secs(row.try_get("created_at").map_sql_err()?),
    })
}

fn map_user_group_membership_row(
    row: &SqliteRow,
) -> Result<StoredUserGroupMembership, DataLayerError> {
    Ok(StoredUserGroupMembership {
        user_id: row.try_get("user_id").map_sql_err()?,
        group_id: row.try_get("group_id").map_sql_err()?,
        group_name: row.try_get("group_name").map_sql_err()?,
        group_priority: row.try_get("group_priority").map_sql_err()?,
        created_at: optional_datetime_from_unix_secs(row.try_get("created_at").map_sql_err()?),
    })
}

fn map_oauth_link_summary_row(
    row: &SqliteRow,
) -> Result<StoredUserOAuthLinkSummary, DataLayerError> {
    StoredUserOAuthLinkSummary::new(
        row.try_get("provider_type").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("provider_username").map_sql_err()?,
        row.try_get("provider_email").map_sql_err()?,
        optional_datetime_from_unix_secs(row.try_get("linked_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("last_login_at").map_sql_err()?),
        row.try_get("provider_enabled").map_sql_err()?,
    )
}

fn map_user_preference_row(row: &SqliteRow) -> Result<StoredUserPreferenceRecord, DataLayerError> {
    let user_id: String = row.try_get("user_id").map_sql_err()?;
    if user_id.trim().is_empty() {
        return Err(DataLayerError::UnexpectedValue(
            "user_preferences.user_id is empty".to_string(),
        ));
    }

    Ok(StoredUserPreferenceRecord {
        user_id,
        avatar_url: row.try_get("avatar_url").map_sql_err()?,
        bio: row.try_get("bio").map_sql_err()?,
        default_provider_id: row.try_get("default_provider_id").map_sql_err()?,
        default_provider_name: row.try_get("default_provider_name").map_sql_err()?,
        theme: row.try_get("theme").map_sql_err()?,
        language: row.try_get("language").map_sql_err()?,
        timezone: row.try_get("timezone").map_sql_err()?,
        email_notifications: row.try_get("email_notifications").map_sql_err()?,
        usage_alerts: row.try_get("usage_alerts").map_sql_err()?,
        announcement_notifications: row.try_get("announcement_notifications").map_sql_err()?,
    })
}

fn map_user_session_row(row: &SqliteRow) -> Result<StoredUserSessionRecord, DataLayerError> {
    StoredUserSessionRecord::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("user_id").map_sql_err()?,
        row.try_get("client_device_id").map_sql_err()?,
        row.try_get("device_label").map_sql_err()?,
        row.try_get("refresh_token_hash").map_sql_err()?,
        row.try_get("prev_refresh_token_hash").map_sql_err()?,
        optional_datetime_from_unix_secs(row.try_get("rotated_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("last_seen_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("expires_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("revoked_at").map_sql_err()?),
        row.try_get("revoke_reason").map_sql_err()?,
        row.try_get("ip_address").map_sql_err()?,
        row.try_get("user_agent").map_sql_err()?,
        optional_datetime_from_unix_secs(row.try_get("created_at").map_sql_err()?),
        optional_datetime_from_unix_secs(row.try_get("updated_at").map_sql_err()?),
    )
    .and_then(|record| record.with_security_version(row.try_get("security_version").map_sql_err()?))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        SqliteUserReadRepository, SQLITE_ANONYMIZE_USER_API_KEY_HISTORY_SQL,
        SQLITE_ANONYMIZE_USER_HISTORY_SQL,
    };
    use crate::run_migrations;
    use aether_data_contracts::repository::users::{
        is_last_active_admin_delete_denied, is_last_active_admin_update_denied,
        BindUserOAuthLinkOutcome, DeleteUserOAuthLinkOutcome, StoredUserPreferenceRecord,
        StoredUserSessionRecord, UserExportListQuery, UserReadRepository,
    };
    use sqlx::Row;

    const USER_HISTORY_TABLES: &[&str] = &[
        "request_candidates",
        "video_tasks",
        "usage",
        "stats_user_daily",
        "stats_user_summary",
        "stats_user_daily_model",
        "stats_user_daily_provider",
        "stats_user_daily_api_format",
        "stats_user_daily_model_provider",
        "stats_user_daily_cost_savings",
        "stats_user_daily_cost_savings_provider",
        "stats_user_daily_cost_savings_model",
        "stats_user_daily_cost_savings_model_provider",
    ];
    const STABLE_USER_ID_ONLY_TABLES: &[&str] = &[
        "stats_hourly_user",
        "stats_hourly_user_model",
        "user_model_usage_counts",
    ];

    async fn seed_sqlite_admin(pool: &crate::SqlitePool, id: &str, username: &str) {
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  is_active, is_deleted, created_at, updated_at
) VALUES (?, ?, 1, ?, NULL, 'admin', 'local', 1, 0, 1, 1)
"#,
        )
        .bind(id)
        .bind(format!("{username}@example.com"))
        .bind(username)
        .execute(pool)
        .await
        .expect("admin should insert");
    }

    fn test_user_session(
        id: &str,
        user_id: &str,
        client_device_id: &str,
        refresh_token: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoredUserSessionRecord {
        StoredUserSessionRecord::new(
            id.to_string(),
            user_id.to_string(),
            client_device_id.to_string(),
            None,
            StoredUserSessionRecord::hash_refresh_token(refresh_token),
            None,
            None,
            Some(now),
            Some(now + chrono::Duration::hours(1)),
            None,
            None,
            None,
            None,
            Some(now),
            Some(now),
        )
        .expect("session should build")
    }

    #[test]
    fn hard_delete_anonymizes_every_sqlite_history_snapshot() {
        assert_eq!(
            SQLITE_ANONYMIZE_USER_HISTORY_SQL.len(),
            USER_HISTORY_TABLES.len()
        );
        for table in USER_HISTORY_TABLES {
            let statement = SQLITE_ANONYMIZE_USER_HISTORY_SQL
                .iter()
                .find(|sql| sql.starts_with(&format!("UPDATE {table} ")))
                .unwrap_or_else(|| panic!("missing history anonymization for {table}"));
            assert!(statement.contains("username = NULL"));
            assert!(statement.ends_with("WHERE user_id = ?"));
        }
        for table in ["request_candidates", "video_tasks", "usage"] {
            let statement = SQLITE_ANONYMIZE_USER_HISTORY_SQL
                .iter()
                .find(|sql| sql.starts_with(&format!("UPDATE {table} ")))
                .expect("identity snapshot table should be covered");
            assert!(statement.contains("api_key_name = NULL"));
        }
        assert!(SQLITE_ANONYMIZE_USER_API_KEY_HISTORY_SQL
            .starts_with("UPDATE stats_daily_api_key SET api_key_name = NULL"));
        assert!(SQLITE_ANONYMIZE_USER_API_KEY_HISTORY_SQL
            .contains("SELECT id FROM api_keys WHERE user_id = ?"));
    }

    #[tokio::test]
    async fn sqlite_hard_delete_preserves_history_ids_and_anonymizes_snapshots() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::raw_sql(
            r#"
INSERT INTO users (
  id, username, password_hash, role, auth_source,
  is_active, is_deleted, created_at, updated_at
) VALUES ('history-user', 'history-name', 'history-hash', 'user', 'local', 1, 0, 1, 1);

INSERT INTO request_candidates (
  id, request_id, user_id, api_key_id, username, api_key_name,
  candidate_index, status, created_at
) VALUES (
  'history-row', 'history-request-candidate', 'history-user', 'history-key',
  'history-name', 'history-key-name', 0, 'success', 1
);

INSERT INTO video_tasks (
  id, request_id, user_id, api_key_id, username, api_key_name, created_at, updated_at
) VALUES (
  'history-row', 'history-video-request', 'history-user', 'history-key',
  'history-name', 'history-key-name', 1, 1
);

INSERT INTO usage (
  request_id, id, user_id, api_key_id, username, api_key_name
) VALUES (
  'history-usage-request', 'history-row', 'history-user', 'history-key',
  'history-name', 'history-key-name'
);

INSERT INTO stats_user_daily (id, user_id, date, username, created_at, updated_at)
VALUES ('history-row', 'history-user', 1, 'history-name', 1, 1);
INSERT INTO stats_user_summary (id, user_id, username, cutoff_date, created_at, updated_at)
VALUES ('history-row', 'history-user', 'history-name', 1, 1, 1);
INSERT INTO stats_user_daily_model (id, user_id, username, date, model, created_at, updated_at)
VALUES ('history-row', 'history-user', 'history-name', 1, 'history-model', 1, 1);
INSERT INTO stats_user_daily_provider (
  id, user_id, username, date, provider_name, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-name', 1, 'history-provider', 1, 1);
INSERT INTO stats_user_daily_api_format (
  id, user_id, username, date, api_format, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-name', 1, 'history-format', 1, 1);
INSERT INTO stats_user_daily_model_provider (
  id, user_id, username, date, model, provider_name, created_at, updated_at
) VALUES (
  'history-row', 'history-user', 'history-name', 1,
  'history-model', 'history-provider', 1, 1
);
INSERT INTO stats_user_daily_cost_savings (
  id, user_id, username, date, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-name', 1, 1, 1);
INSERT INTO stats_user_daily_cost_savings_provider (
  id, user_id, username, date, provider_name, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-name', 1, 'history-provider', 1, 1);
INSERT INTO stats_user_daily_cost_savings_model (
  id, user_id, username, date, model, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-name', 1, 'history-model', 1, 1);
INSERT INTO stats_user_daily_cost_savings_model_provider (
  id, user_id, username, date, model, provider_name, created_at, updated_at
) VALUES (
  'history-row', 'history-user', 'history-name', 1,
  'history-model', 'history-provider', 1, 1
);
INSERT INTO stats_hourly_user (
  id, hour_utc, user_id, created_at, updated_at
) VALUES ('history-row', 1, 'history-user', 1, 1);
INSERT INTO stats_hourly_user_model (
  id, hour_utc, user_id, model, created_at, updated_at
) VALUES ('history-row', 1, 'history-user', 'history-model', 1, 1);
INSERT INTO user_model_usage_counts (
  id, user_id, model, created_at, updated_at
) VALUES ('history-row', 'history-user', 'history-model', 1, 1);
INSERT INTO stats_daily_api_key (
  id, api_key_id, date, api_key_name, created_at, updated_at
) VALUES ('history-row', 'history-key', 1, 'history-key-name', 1, 1);
INSERT INTO api_keys (
  id, user_id, name, key_hash, created_at, updated_at
) VALUES ('history-key', 'history-user', 'history-key-name', 'history-key-hash', 1, 1);

INSERT INTO users (
  id, username, password_hash, role, auth_source,
  is_active, is_deleted, created_at, updated_at
) VALUES ('history-inviter', 'history-inviter', 'history-hash', 'user', 'local', 1, 0, 1, 1);

INSERT INTO wallets (
  id, user_id, balance, gift_balance, status, created_at, updated_at
) VALUES ('history-wallet', 'history-user', 12, 3, 'active', 1, 1);

INSERT INTO wallet_transactions (
  id, wallet_id, category, reason_code, amount,
  balance_before, balance_after,
  recharge_balance_before, recharge_balance_after,
  gift_balance_before, gift_balance_after,
  operator_id, description, created_at
) VALUES (
  'history-wallet-tx', 'history-wallet', 'adjust', 'manual', 1,
  14, 15, 11, 12, 3, 3,
  'history-user', 'private wallet note', 1
);

INSERT INTO payment_orders (
  id, order_no, wallet_id, user_id, amount_usd, payment_method,
  gateway_response, status, created_at
) VALUES (
  'history-order', 'history-order-no', 'history-wallet', 'history-user', 12,
  'test', '{"customer_email":"history@example.com"}', 'credited', 1
);

INSERT INTO payment_callbacks (
  id, payment_order_id, payment_method, callback_key, order_no,
  payload_hash, signature_valid, status, payload, error_message, created_at
) VALUES (
  'history-callback', 'history-order', 'test', 'history-callback-key',
  'history-order-no', 'history-payload-hash', 1, 'processed',
  '{"customer_email":"history@example.com"}', 'private callback error', 1
);

INSERT INTO billing_plans (
  id, title, price_amount, duration_unit, duration_value,
  entitlements_json, created_at, updated_at
) VALUES ('history-plan', 'history plan', 12, 'day', 30, '[]', 1, 1);

INSERT INTO user_plan_entitlements (
  id, user_id, plan_id, payment_order_id, status, starts_at, expires_at,
  entitlements_snapshot, created_at, updated_at
) VALUES (
  'history-entitlement', 'history-user', 'history-plan', 'history-order',
  'active', 1, 4102444800, '[]', 1, 1
);

INSERT INTO entitlement_usage_ledgers (
  id, user_entitlement_id, user_id, request_id, amount_usd,
  balance_before, balance_after, usage_date, created_at
) VALUES (
  'history-entitlement-ledger', 'history-entitlement', 'history-user',
  'history-entitlement-request', 1, 12, 11, '2026-08-27', 1
);

INSERT INTO user_referrals (
  id, inviter_user_id, invitee_user_id, invite_code_snapshot, source_json,
  first_paid_order_id, first_paid_at, created_at, updated_at
) VALUES (
  'history-referral', 'history-inviter', 'history-user', 'PRIVATE-CODE',
  '{"ip":"192.0.2.1"}', 'history-order', 1, 1, 1
);

INSERT INTO referral_rewards (
  id, referral_id, inviter_user_id, invitee_user_id, reward_type,
  trigger_point, source_order_id, idempotency_key, amount_usd, status,
  failure_reason, admin_operator_id, admin_note, created_at, updated_at
) VALUES (
  'history-reward', 'history-referral', 'history-inviter', 'history-user',
  'percent', 'paid_order', 'history-order', 'history-reward-key', 1, 'failed',
  'private failure', 'history-user', 'private admin note', 1, 1
);

INSERT INTO audit_logs (
  id, event_type, user_id, description, ip_address, user_agent,
  event_metadata, error_message, created_at
) VALUES (
  'history-audit', 'history_event', 'history-user', 'private description',
  '192.0.2.2', 'private agent', '{"private":true}', 'private error', 1
);
"#,
        )
        .execute(&pool)
        .await
        .expect("history fixtures should insert");

        let repository = SqliteUserReadRepository::new(pool.clone());
        assert!(repository
            .delete_local_auth_user("history-user")
            .await
            .expect("hard delete should succeed"));

        for table in USER_HISTORY_TABLES {
            let row = sqlx::query(&format!(
                "SELECT user_id, username FROM {table} WHERE id = 'history-row'"
            ))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("{table} history should remain: {error}"));
            assert_eq!(
                row.try_get::<Option<String>, _>("user_id")
                    .expect("user_id should decode")
                    .as_deref(),
                Some("history-user"),
                "{table} user_id must remain stable"
            );
            assert_eq!(
                row.try_get::<Option<String>, _>("username")
                    .expect("username should decode"),
                None,
                "{table} username snapshot must be removed"
            );
        }
        for table in ["request_candidates", "video_tasks", "usage"] {
            let row = sqlx::query(&format!(
                "SELECT api_key_id, api_key_name FROM {table} WHERE id = 'history-row'"
            ))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("{table} identity snapshot should remain: {error}"));
            assert_eq!(
                row.try_get::<Option<String>, _>("api_key_id")
                    .expect("api_key_id should decode")
                    .as_deref(),
                Some("history-key"),
                "{table} api_key_id must remain stable"
            );
            assert_eq!(
                row.try_get::<Option<String>, _>("api_key_name")
                    .expect("api_key_name should decode"),
                None,
                "{table} API key name snapshot must be removed"
            );
        }
        for table in STABLE_USER_ID_ONLY_TABLES {
            let user_id: String = sqlx::query_scalar(&format!(
                "SELECT user_id FROM {table} WHERE id = 'history-row'"
            ))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("{table} fact should remain: {error}"));
            assert_eq!(
                user_id, "history-user",
                "{table} user_id must remain stable"
            );
        }
        let api_key_fact = sqlx::query(
            "SELECT api_key_id, api_key_name FROM stats_daily_api_key WHERE id = 'history-row'",
        )
        .fetch_one(&pool)
        .await
        .expect("API key aggregate should remain");
        assert_eq!(
            api_key_fact
                .try_get::<String, _>("api_key_id")
                .expect("aggregate api_key_id should decode"),
            "history-key"
        );
        assert_eq!(
            api_key_fact
                .try_get::<Option<String>, _>("api_key_name")
                .expect("aggregate api_key_name should decode"),
            None
        );
        let wallet_fact =
            sqlx::query("SELECT user_id, status FROM wallets WHERE id = 'history-wallet'")
                .fetch_one(&pool)
                .await
                .expect("wallet fact should remain");
        assert_eq!(
            wallet_fact
                .try_get::<Option<String>, _>("user_id")
                .expect("wallet user_id should decode")
                .as_deref(),
            Some("history-user")
        );
        assert_eq!(
            wallet_fact
                .try_get::<String, _>("status")
                .expect("wallet status should decode"),
            "disabled"
        );
        let order_fact = sqlx::query(
            "SELECT user_id, gateway_response FROM payment_orders WHERE id = 'history-order'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment order fact should remain");
        assert_eq!(
            order_fact
                .try_get::<Option<String>, _>("user_id")
                .expect("order user_id should decode")
                .as_deref(),
            Some("history-user")
        );
        assert_eq!(
            order_fact
                .try_get::<Option<String>, _>("gateway_response")
                .expect("gateway response should decode"),
            None
        );
        let callback_fact = sqlx::query(
            "SELECT payment_order_id, order_no, payload, error_message FROM payment_callbacks WHERE id = 'history-callback'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment callback fact should remain");
        assert_eq!(
            callback_fact
                .try_get::<Option<String>, _>("payment_order_id")
                .expect("callback order id should decode")
                .as_deref(),
            Some("history-order")
        );
        assert_eq!(
            callback_fact
                .try_get::<Option<String>, _>("order_no")
                .expect("callback order number should decode")
                .as_deref(),
            Some("history-order-no")
        );
        assert_eq!(
            callback_fact
                .try_get::<Option<String>, _>("payload")
                .expect("callback payload should decode"),
            None
        );
        assert_eq!(
            callback_fact
                .try_get::<Option<String>, _>("error_message")
                .expect("callback error should decode"),
            None
        );
        let entitlement_fact = sqlx::query(
            "SELECT user_id, status FROM user_plan_entitlements WHERE id = 'history-entitlement'",
        )
        .fetch_one(&pool)
        .await
        .expect("entitlement fact should remain");
        assert_eq!(
            entitlement_fact
                .try_get::<String, _>("user_id")
                .expect("entitlement user_id should decode"),
            "history-user"
        );
        assert_eq!(
            entitlement_fact
                .try_get::<String, _>("status")
                .expect("entitlement status should decode"),
            "revoked"
        );
        let ledger_user_id: String = sqlx::query_scalar(
            "SELECT user_id FROM entitlement_usage_ledgers WHERE id = 'history-entitlement-ledger'",
        )
        .fetch_one(&pool)
        .await
        .expect("entitlement ledger should remain");
        assert_eq!(ledger_user_id, "history-user");
        let referral_fact = sqlx::query(
            "SELECT invitee_user_id, invite_code_snapshot, source_json FROM user_referrals WHERE id = 'history-referral'",
        )
        .fetch_one(&pool)
        .await
        .expect("referral fact should remain");
        assert_eq!(
            referral_fact
                .try_get::<String, _>("invitee_user_id")
                .expect("invitee user_id should decode"),
            "history-user"
        );
        assert_eq!(
            referral_fact
                .try_get::<String, _>("invite_code_snapshot")
                .expect("invite code snapshot should decode"),
            "deleted-user"
        );
        assert_eq!(
            referral_fact
                .try_get::<Option<String>, _>("source_json")
                .expect("referral source should decode"),
            None
        );
        let reward_fact = sqlx::query(
            "SELECT invitee_user_id, status, failure_reason, admin_note FROM referral_rewards WHERE id = 'history-reward'",
        )
        .fetch_one(&pool)
        .await
        .expect("referral reward fact should remain");
        assert_eq!(
            reward_fact
                .try_get::<String, _>("invitee_user_id")
                .expect("reward invitee should decode"),
            "history-user"
        );
        assert_eq!(
            reward_fact
                .try_get::<String, _>("status")
                .expect("reward status should decode"),
            "voided"
        );
        assert_eq!(
            reward_fact
                .try_get::<Option<String>, _>("failure_reason")
                .expect("reward failure reason should decode"),
            None
        );
        assert_eq!(
            reward_fact
                .try_get::<Option<String>, _>("admin_note")
                .expect("reward admin note should decode"),
            None
        );
        let audit_fact = sqlx::query(
            "SELECT user_id, ip_address, user_agent, event_metadata, error_message FROM audit_logs WHERE id = 'history-audit'",
        )
        .fetch_one(&pool)
        .await
        .expect("audit fact should remain");
        assert_eq!(
            audit_fact
                .try_get::<Option<String>, _>("user_id")
                .expect("audit user_id should decode")
                .as_deref(),
            Some("history-user")
        );
        for column in [
            "ip_address",
            "user_agent",
            "event_metadata",
            "error_message",
        ] {
            assert_eq!(
                audit_fact
                    .try_get::<Option<String>, _>(column)
                    .unwrap_or_else(|error| panic!("audit {column} should decode: {error}")),
                None,
                "audit {column} must be removed"
            );
        }
        let transaction_description: Option<String> = sqlx::query_scalar(
            "SELECT description FROM wallet_transactions WHERE id = 'history-wallet-tx'",
        )
        .fetch_one(&pool)
        .await
        .expect("wallet transaction should remain");
        assert_eq!(transaction_description, None);
        let user_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = 'history-user'")
                .fetch_one(&pool)
                .await
                .expect("deleted user count should load");
        assert_eq!(user_count, 0);
    }

    #[tokio::test]
    async fn sqlite_atomic_user_delete_requires_wallet_absence() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_source, is_active, is_deleted, created_at, updated_at) VALUES ('atomic-user', 'atomic-user', 'atomic@example.com', 'user', 'local', 1, 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("user should seed");
        sqlx::query(
            "INSERT INTO wallets (id, user_id, balance, gift_balance, status, created_at, updated_at) VALUES ('atomic-wallet', 'atomic-user', 0, 0, 'active', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("wallet should seed");

        let repository = SqliteUserReadRepository::new(pool.clone());
        assert!(!repository
            .delete_local_auth_user_if_wallet_absent("atomic-user")
            .await
            .expect("wallet guard should resolve"));
        assert!(repository
            .find_user_auth_by_id("atomic-user")
            .await
            .expect("user lookup should succeed")
            .is_some());

        sqlx::query("DELETE FROM wallets WHERE id = 'atomic-wallet'")
            .execute(&pool)
            .await
            .expect("wallet should remove");
        assert!(repository
            .delete_local_auth_user_if_wallet_absent("atomic-user")
            .await
            .expect("wallet-free user should delete"));
        assert!(repository
            .find_user_auth_by_id("atomic-user")
            .await
            .expect("user lookup should succeed")
            .is_none());
    }

    #[tokio::test]
    async fn sqlite_atomic_user_delete_detects_api_key_wallet() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_source, is_active, is_deleted, created_at, updated_at) VALUES ('api-wallet-user', 'api-wallet-user', 'api-wallet@example.com', 'user', 'local', 1, 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("user should seed");
        sqlx::query(
            "INSERT INTO api_keys (id, user_id, key_hash, created_at, updated_at) VALUES ('api-wallet-key', 'api-wallet-user', 'api-wallet-key-hash', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("api key should seed");
        sqlx::query(
            "INSERT INTO wallets (id, api_key_id, balance, gift_balance, status, created_at, updated_at) VALUES ('api-wallet', 'api-wallet-key', 25, 0, 'active', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("api key wallet should seed");

        let repository = SqliteUserReadRepository::new(pool.clone());
        assert!(!repository
            .delete_local_auth_user_if_wallet_absent("api-wallet-user")
            .await
            .expect("api key wallet guard should resolve"));

        assert!(repository
            .find_user_auth_by_id("api-wallet-user")
            .await
            .expect("user lookup should succeed")
            .is_some());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM api_keys WHERE id = 'api-wallet-key'",
            )
            .fetch_one(&pool)
            .await
            .expect("api key count should query"),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM wallets WHERE id = 'api-wallet' AND api_key_id = 'api-wallet-key'",
            )
            .fetch_one(&pool)
            .await
            .expect("wallet count should query"),
            1
        );

        sqlx::query("DELETE FROM wallets WHERE id = 'api-wallet'")
            .execute(&pool)
            .await
            .expect("api key wallet should remove");
        assert!(repository
            .delete_local_auth_user_if_wallet_absent("api-wallet-user")
            .await
            .expect("wallet-free api key owner should delete"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM api_keys WHERE id = 'api-wallet-key'",
            )
            .fetch_one(&pool)
            .await
            .expect("api key count should query after delete"),
            0
        );
    }

    #[tokio::test]
    async fn sqlite_atomically_preserves_last_active_admin_and_revokes_on_security_change() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_sqlite_admin(&pool, "admin-1", "admin_one").await;
        sqlx::query(
            "INSERT INTO management_tokens (id, user_id, name, token_hash, created_at, updated_at) VALUES ('token-admin-1', 'admin-1', 'admin token', 'token-hash-admin-1', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("management token should insert");
        let repository = SqliteUserReadRepository::new(pool.clone());

        let update_error = repository
            .update_local_auth_user_admin_fields(
                "admin-1",
                Some("audit_admin".to_string()),
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                None,
            )
            .await
            .expect_err("last active admin demotion must be rejected");
        assert!(is_last_active_admin_update_denied(&update_error));
        assert_eq!(
            repository
                .find_user_auth_by_id("admin-1")
                .await
                .expect("admin lookup should succeed")
                .expect("admin should remain")
                .role,
            "admin"
        );

        let delete_error = repository
            .delete_local_auth_user("admin-1")
            .await
            .expect_err("last active admin delete must be rejected");
        assert!(is_last_active_admin_delete_denied(&delete_error));
        let token_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM management_tokens WHERE user_id = 'admin-1'")
                .fetch_one(&pool)
                .await
                .expect("management token count should load");
        assert_eq!(
            token_count, 1,
            "rejected delete must roll back credential cleanup"
        );

        seed_sqlite_admin(&pool, "admin-2", "admin_two").await;
        let now = chrono::Utc::now();
        let session = test_user_session("session-admin-1", "admin-1", "device-1", "refresh", now);
        repository
            .create_user_session(&session)
            .await
            .expect("admin session should create")
            .expect("admin session should exist");
        sqlx::query(
            "INSERT INTO api_keys (id, user_id, key_hash, created_at, updated_at) VALUES ('key-admin-1', 'admin-1', 'key-hash-admin-1', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("api key should insert");
        sqlx::query(
            "INSERT INTO api_key_provider_mappings (id, api_key_id, provider_id, created_at, updated_at) VALUES ('mapping-admin-1', 'key-admin-1', 'provider-1', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("api key mapping should insert");
        sqlx::query(
            "INSERT INTO user_oauth_links (id, user_id, provider_type, provider_user_id, linked_at) VALUES ('oauth-admin-1', 'admin-1', 'test', 'subject-admin-1', 1)",
        )
        .execute(&pool)
        .await
        .expect("oauth link should insert");
        sqlx::query(
            "INSERT INTO user_group_members (group_id, user_id, created_at) VALUES ('00000000-0000-0000-0000-000000000001', 'admin-1', 1)",
        )
        .execute(&pool)
        .await
        .expect("group membership should insert");
        sqlx::query(
            "INSERT INTO user_preferences (id, user_id, created_at, updated_at) VALUES ('preferences-admin-1', 'admin-1', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("preferences should insert");
        sqlx::query(
            "INSERT INTO announcements (id, title, content, created_at, updated_at) VALUES ('announcement-1', 'notice', 'content', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("announcement should insert");
        sqlx::query(
            "INSERT INTO announcement_reads (id, user_id, announcement_id, read_at) VALUES ('read-admin-1', 'admin-1', 'announcement-1', 1)",
        )
        .execute(&pool)
        .await
        .expect("announcement read should insert");
        let updated = repository
            .update_local_auth_user_admin_fields(
                "admin-1",
                Some("audit_admin".to_string()),
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                None,
            )
            .await
            .expect("demotion with another active admin should succeed")
            .expect("admin should exist");
        assert_eq!(updated.role, "audit_admin");
        let revoked = repository
            .find_user_session("admin-1", "session-admin-1")
            .await
            .expect("session lookup should succeed")
            .expect("session should remain as audit record");
        assert!(revoked.revoked_at.is_some());
        assert_eq!(
            revoked.revoke_reason.as_deref(),
            Some("user_security_state_changed")
        );
        assert!(repository
            .delete_local_auth_user("admin-1")
            .await
            .expect("non-full-admin delete should succeed"));
        for (table, predicate) in [
            ("api_key_provider_mappings", "api_key_id = 'key-admin-1'"),
            ("api_keys", "user_id = 'admin-1'"),
            ("management_tokens", "user_id = 'admin-1'"),
            ("user_sessions", "user_id = 'admin-1'"),
            ("user_oauth_links", "user_id = 'admin-1'"),
            ("user_group_members", "user_id = 'admin-1'"),
            ("user_preferences", "user_id = 'admin-1'"),
            ("announcement_reads", "user_id = 'admin-1'"),
        ] {
            let count: i64 =
                sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"))
                    .fetch_one(&pool)
                    .await
                    .expect("dependent row count should load");
            assert_eq!(count, 0, "{table} credentials must be removed");
        }
    }

    async fn seed_active_session_test_user(pool: &crate::SqlitePool, user_id: &str) {
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  is_active, is_deleted, created_at, updated_at
) VALUES (?, ?, 1, ?, NULL, 'user', 'oauth', 1, 0, ?, ?)
"#,
        )
        .bind(user_id)
        .bind(format!("{user_id}@example.com"))
        .bind(user_id)
        .bind(chrono::Utc::now().timestamp())
        .bind(chrono::Utc::now().timestamp())
        .execute(pool)
        .await
        .expect("session test user should insert");
    }

    #[tokio::test]
    async fn sqlite_repository_reads_user_contract_views() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers, allowed_api_formats, allowed_models, model_capability_settings,
  rate_limit, is_active, is_deleted, created_at, updated_at, last_login_at
) VALUES
  (
    'admin-1', 'admin@example.com', 1, 'admin', NULL, 'admin', 'local',
    NULL, NULL, NULL, NULL, 100, 1, 0, 1, 1, NULL
  ),
  (
    'user-1', 'user-1@example.com', 1, 'alice', 'hash', 'user', 'local',
    '["openai"]', '["openai:chat"]', '["gpt-4.1"]', '{"gpt-4.1":{"cache_1h":true}}',
    60, 1, 0, 2, 2, 3
  ),
  (
    'user-2', NULL, 0, 'deleted', NULL, 'user', 'local',
    NULL, NULL, NULL, NULL, NULL, 0, 1, 4, 4, NULL
  )
"#,
        )
        .execute(&pool)
        .await
        .expect("seed users should insert");
        let valid_hash = "$2b$12$4qL4tdcsFwVaDTw5Ck3xzu8GpNdre56DiNR6Dnw7t6gCXaEnqAe7G".to_string();
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers, allowed_api_formats, allowed_models, model_capability_settings,
  rate_limit, is_active, is_deleted, created_at, updated_at, last_login_at
) VALUES (
  'admin-2', 'admin-2@example.com', 1, 'admin2', ?, 'admin', 'local',
  NULL, NULL, NULL, NULL, 100, 1, 0, 5, 5, NULL
)
"#,
        )
        .bind(valid_hash)
        .execute(&pool)
        .await
        .expect("valid local admin should insert");

        let repository = SqliteUserReadRepository::new(pool.clone());
        let summaries = repository
            .list_users_by_ids(&["user-1".to_string(), "admin-1".to_string()])
            .await
            .expect("summaries should load");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "admin-1");

        let searched = repository
            .list_users_by_username_search("ali")
            .await
            .expect("username search should load");
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].id, "user-1");

        let exports = repository
            .list_non_admin_export_users()
            .await
            .expect("non-admin exports should load");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].allowed_models, Some(vec!["gpt-4.1".to_string()]));

        let page = repository
            .list_export_users_page(&UserExportListQuery {
                skip: 0,
                limit: 10,
                role: Some("user".to_string()),
                is_active: Some(true),
                search: None,
                group_id: None,
                ..Default::default()
            })
            .await
            .expect("export page should load");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "user-1");

        let summary = repository
            .summarize_export_users()
            .await
            .expect("export summary should load");
        assert_eq!(summary.total, 3);
        assert_eq!(summary.active, 3);

        let auth = repository
            .find_user_auth_by_identifier("user-1@example.com")
            .await
            .expect("auth lookup should load")
            .expect("auth user should exist");
        assert_eq!(auth.id, "user-1");
        assert_eq!(auth.last_login_at.expect("last login").timestamp(), 3);
        let logged_in_at = chrono::DateTime::from_timestamp(123, 0).expect("valid time");
        assert!(repository
            .touch_auth_user_last_login("user-1", logged_in_at)
            .await
            .expect("last login touch should update"));
        assert!(!repository
            .touch_auth_user_last_login("missing-user", logged_in_at)
            .await
            .expect("missing last login touch should be harmless"));
        let touched_auth = repository
            .find_user_auth_by_id("user-1")
            .await
            .expect("auth lookup should load")
            .expect("auth user should exist");
        assert_eq!(
            touched_auth.last_login_at.expect("last login").timestamp(),
            123
        );
        let profile_updated = repository
            .update_local_auth_user_profile(
                "user-1",
                true,
                Some("user-1b@example.com".to_string()),
                Some(true),
                Some("alice-b".to_string()),
            )
            .await
            .expect("profile update should succeed")
            .expect("profile update should return user");
        assert_eq!(
            profile_updated.email.as_deref(),
            Some("user-1b@example.com")
        );
        assert!(profile_updated.email_verified);
        assert_eq!(profile_updated.username, "alice-b");
        let password_updated = repository
            .update_local_auth_user_password_hash(
                "user-1",
                "new-password-hash".to_string(),
                logged_in_at,
            )
            .await
            .expect("password update should succeed")
            .expect("password update should return user");
        assert_eq!(
            password_updated.password_hash.as_deref(),
            Some("new-password-hash")
        );
        let created = repository
            .create_local_auth_user_with_settings(
                Some("created@example.com".to_string()),
                true,
                "created-user".to_string(),
                "created-hash".to_string(),
                "admin".to_string(),
                Some(vec!["openai".to_string()]),
                Some(vec!["chat".to_string()]),
                Some(vec!["gpt-4.1".to_string()]),
                Some(25),
            )
            .await
            .expect("local user create should succeed")
            .expect("local user create should return user");
        assert_eq!(created.email.as_deref(), Some("created@example.com"));
        assert_eq!(created.username, "created-user");
        assert_eq!(created.role, "admin");
        assert_eq!(created.allowed_providers, Some(vec!["openai".to_string()]));
        assert_eq!(created.allowed_api_formats, Some(vec!["chat".to_string()]));
        assert_eq!(created.allowed_models, Some(vec!["gpt-4.1".to_string()]));
        let admin_updated = repository
            .update_local_auth_user_admin_fields(
                &created.id,
                Some("user".to_string()),
                true,
                None,
                true,
                Some(vec!["responses".to_string()]),
                true,
                Some(vec!["gpt-4.1-mini".to_string()]),
                true,
                Some(5),
                Some(false),
            )
            .await
            .expect("admin fields update should succeed")
            .expect("admin fields update should return user");
        assert_eq!(admin_updated.role, "user");
        assert_eq!(admin_updated.allowed_providers, None);
        assert_eq!(
            admin_updated.allowed_api_formats,
            Some(vec!["responses".to_string()])
        );
        assert_eq!(
            admin_updated.allowed_models,
            Some(vec!["gpt-4.1-mini".to_string()])
        );
        assert!(!admin_updated.is_active);
        assert_eq!(
            repository
                .update_user_model_capability_settings(
                    &created.id,
                    Some(serde_json::json!({"gpt-4.1-mini": {"enabled": true}})),
                )
                .await
                .expect("model settings update should succeed"),
            Some(serde_json::json!({"gpt-4.1-mini": {"enabled": true}}))
        );
        assert_eq!(
            repository
                .update_user_model_capability_settings(&created.id, Some(serde_json::Value::Null))
                .await
                .expect("model settings clear should succeed"),
            None
        );

        let by_email = repository
            .find_user_auth_by_email("user-1b@example.com")
            .await
            .expect("email lookup should load")
            .expect("email lookup should find user");
        assert_eq!(by_email.id, "user-1");
        let by_username = repository
            .find_user_auth_by_username("alice-b")
            .await
            .expect("username lookup should load")
            .expect("username lookup should find user");
        assert_eq!(by_username.id, "user-1");
        assert!(repository
            .find_user_auth_by_email("alice")
            .await
            .expect("email lookup should load")
            .is_none());
        let cleared_profile = repository
            .update_local_auth_user_profile("user-1", true, None, Some(false), None)
            .await
            .expect("nullable email update should succeed")
            .expect("profile should remain");
        assert!(cleared_profile.email.is_none());
        assert!(!cleared_profile.email_verified);
        assert_eq!(
            repository
                .count_active_admin_users()
                .await
                .expect("active admin count should load"),
            2
        );
        assert_eq!(
            repository
                .count_active_local_admin_users_with_valid_password()
                .await
                .expect("valid local admin count should load"),
            1
        );
        let preferences = StoredUserPreferenceRecord {
            user_id: "user-1".to_string(),
            avatar_url: Some("https://example.test/avatar.png".to_string()),
            bio: Some("hello".to_string()),
            default_provider_id: None,
            default_provider_name: None,
            theme: "dark".to_string(),
            language: "en-US".to_string(),
            timezone: "UTC".to_string(),
            email_notifications: false,
            usage_alerts: true,
            announcement_notifications: false,
        };
        assert_eq!(
            repository
                .write_user_preferences(&preferences)
                .await
                .expect("preferences should write"),
            Some(preferences.clone())
        );
        assert_eq!(
            repository
                .read_user_preferences("user-1")
                .await
                .expect("preferences should read"),
            Some(preferences)
        );
        let now = chrono::Utc::now();
        let session = StoredUserSessionRecord::new(
            "session-1".to_string(),
            "user-1".to_string(),
            "device-1".to_string(),
            Some("Laptop".to_string()),
            StoredUserSessionRecord::hash_refresh_token("refresh-1"),
            None,
            None,
            Some(now),
            Some(now + chrono::Duration::hours(1)),
            None,
            None,
            Some("127.0.0.1".to_string()),
            Some("agent".to_string()),
            Some(now),
            Some(now),
        )
        .expect("session should build")
        .with_security_version(1)
        .expect("session security version should be valid");
        assert_eq!(
            repository
                .create_user_session(&session)
                .await
                .expect("session should create")
                .map(|session| session.id),
            Some("session-1".to_string())
        );
        assert_eq!(
            repository
                .list_user_sessions("user-1")
                .await
                .expect("sessions should list")
                .len(),
            1
        );
        assert!(repository
            .revoke_user_session("user-1", "session-1", now, "logout")
            .await
            .expect("session should revoke"));
        assert!(repository
            .list_user_sessions("user-1")
            .await
            .expect("sessions should list")
            .is_empty());

        let by_ids = repository
            .list_user_auth_by_ids(&["user-1".to_string()])
            .await
            .expect("auth list should load");
        assert_eq!(by_ids.len(), 1);
        assert_eq!(by_ids[0].username, "alice-b");
        assert!(repository
            .delete_local_auth_user("user-1")
            .await
            .expect("delete should succeed"));
        assert!(!repository
            .delete_local_auth_user("user-1")
            .await
            .expect("second delete should succeed"));
        assert!(repository
            .find_user_auth_by_id("user-1")
            .await
            .expect("deleted auth lookup should load")
            .is_none());

        assert!(repository
            .find_export_user_by_id("user-2")
            .await
            .expect("deleted user lookup should run")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_password_session_replacement_is_atomic_under_concurrency() {
        let database_path = std::env::temp_dir().join(format!(
            "aether-sqlite-session-replacement-race-{}.db",
            uuid::Uuid::new_v4()
        ));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(30));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_active_session_test_user(&pool, "session-race-user").await;

        let repository = SqliteUserReadRepository::new(pool.clone());
        let now = chrono::Utc::now();
        let first = test_user_session(
            "session-race-first",
            "session-race-user",
            "shared-device",
            "refresh-first",
            now,
        );
        let second = test_user_session(
            "session-race-second",
            "session-race-user",
            "shared-device",
            "refresh-second",
            now,
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_repository = repository.clone();
        let first_barrier = Arc::clone(&barrier);
        let first_create = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repository.create_user_session(&first).await
        });
        let second_repository = repository.clone();
        let second_barrier = Arc::clone(&barrier);
        let second_create = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repository.create_user_session(&second).await
        });

        assert!(first_create
            .await
            .expect("first login task should join")
            .expect("first login should succeed")
            .is_some());
        assert!(second_create
            .await
            .expect("second login task should join")
            .expect("second login should succeed")
            .is_some());
        let active = repository
            .list_user_sessions("session-race-user")
            .await
            .expect("active sessions should list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].client_device_id, "shared-device");

        pool.close().await;
        let _ = std::fs::remove_file(database_path);
    }

    #[tokio::test]
    async fn failed_non_password_session_insert_rolls_back_device_revocation() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_active_session_test_user(&pool, "session-rollback-user").await;
        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let current = test_user_session(
            "session-duplicate-id",
            "session-rollback-user",
            "shared-device",
            "refresh-current",
            now,
        );
        repository
            .create_user_session(&current)
            .await
            .expect("initial session should create")
            .expect("initial session should exist");

        let duplicate = test_user_session(
            "session-duplicate-id",
            "session-rollback-user",
            "shared-device",
            "refresh-duplicate",
            now + chrono::Duration::seconds(1),
        );
        assert!(repository.create_user_session(&duplicate).await.is_err());

        let active = repository
            .list_user_sessions("session-rollback-user")
            .await
            .expect("active sessions should list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].refresh_token_hash, current.refresh_token_hash);
        assert!(!active[0].is_revoked());
    }

    #[tokio::test]
    async fn security_state_changes_revoke_sessions_without_reactivation() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_active_session_test_user(&pool, "sqlite-security-state-user").await;
        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let session = test_user_session(
            "sqlite-security-state-session",
            "sqlite-security-state-user",
            "sqlite-security-state-device",
            "sqlite-security-state-refresh",
            now,
        );
        repository
            .create_user_session(&session)
            .await
            .expect("initial session should create")
            .expect("initial session should exist");

        repository
            .update_local_auth_user_admin_fields(
                "sqlite-security-state-user",
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                Some(false),
            )
            .await
            .expect("disable should succeed")
            .expect("user should exist");
        let revoked = repository
            .find_user_session(
                "sqlite-security-state-user",
                "sqlite-security-state-session",
            )
            .await
            .expect("revoked session should load")
            .expect("revoked session should remain stored");
        assert!(revoked.is_revoked());
        assert_eq!(
            revoked.revoke_reason.as_deref(),
            Some("user_security_state_changed")
        );

        repository
            .update_local_auth_user_admin_fields(
                "sqlite-security-state-user",
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                Some(true),
            )
            .await
            .expect("reactivation should succeed")
            .expect("user should exist");
        assert!(repository
            .list_user_sessions("sqlite-security-state-user")
            .await
            .expect("sessions should list")
            .is_empty());

        let replacement = test_user_session(
            "sqlite-security-state-replacement",
            "sqlite-security-state-user",
            "sqlite-security-state-device",
            "sqlite-security-state-replacement-refresh",
            now + chrono::Duration::seconds(1),
        );
        let replacement = replacement
            .with_security_version(
                repository
                    .find_user_auth_by_id("sqlite-security-state-user")
                    .await
                    .expect("user lookup should succeed")
                    .expect("user should exist")
                    .security_version,
            )
            .expect("security version should be valid");
        let created = repository
            .create_user_session(&replacement)
            .await
            .expect("replacement session should create")
            .expect("replacement session should exist");
        assert_eq!(created.security_version, replacement.security_version);
        let persisted = repository
            .find_user_session(
                "sqlite-security-state-user",
                "sqlite-security-state-replacement",
            )
            .await
            .expect("replacement session should load")
            .expect("replacement session should remain stored");
        assert_eq!(persisted.security_version, replacement.security_version);
        let active = repository
            .list_user_sessions("sqlite-security-state-user")
            .await
            .expect("replacement session should list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].security_version, replacement.security_version);
        repository
            .update_local_auth_user_admin_fields(
                "sqlite-security-state-user",
                Some("audit_admin".to_string()),
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                None,
            )
            .await
            .expect("role update should succeed")
            .expect("user should exist");
        assert!(repository
            .list_user_sessions("sqlite-security-state-user")
            .await
            .expect("sessions should list")
            .is_empty());
    }

    #[tokio::test]
    async fn unchanged_security_state_preserves_sqlite_sessions() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_active_session_test_user(&pool, "sqlite-unchanged-security-user").await;
        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let session = test_user_session(
            "sqlite-unchanged-security-session",
            "sqlite-unchanged-security-user",
            "sqlite-unchanged-security-device",
            "sqlite-unchanged-security-refresh",
            now,
        );
        repository
            .create_user_session(&session)
            .await
            .expect("initial session should create")
            .expect("initial session should exist");

        repository
            .update_local_auth_user_admin_fields(
                "sqlite-unchanged-security-user",
                Some("USER".to_string()),
                false,
                None,
                false,
                None,
                false,
                None,
                false,
                None,
                Some(true),
            )
            .await
            .expect("idempotent security update should succeed")
            .expect("user should exist");
        assert_eq!(
            repository
                .list_user_sessions("sqlite-unchanged-security-user")
                .await
                .expect("sessions should list")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn sqlite_repository_manages_oauth_users_and_links() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES
  ('linuxdo', 'Linux.do', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1),
  ('github', 'GitHub', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("provider should insert");

        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let user = repository
            .create_oauth_auth_user(
                Some("OAuth@Example.com".to_string()),
                false,
                "oauth_user".to_string(),
                now,
            )
            .await
            .expect("oauth user should create")
            .expect("oauth user should exist");
        assert_eq!(user.auth_source, "oauth");
        assert!(!user.email_verified);
        assert_eq!(
            repository
                .find_active_user_auth_by_email_ci("oauth@example.com")
                .await
                .expect("ci lookup should work")
                .map(|user| user.id),
            Some(user.id.clone())
        );

        assert!(!repository
            .upgrade_oauth_email_verification_if_matches(&user.id, "different@example.com", now,)
            .await
            .expect("mismatched verification should resolve"));
        assert!(repository
            .upgrade_oauth_email_verification_if_matches(&user.id, "oauth@example.com", now)
            .await
            .expect("matching verification should resolve"));
        assert!(
            repository
                .find_user_auth_by_id(&user.id)
                .await
                .expect("user should load")
                .expect("user should exist")
                .email_verified
        );

        assert_eq!(
            repository
                .bind_user_oauth_link(
                    &user.id,
                    "linuxdo",
                    "subject-1",
                    Some("alice"),
                    Some("alice@example.com"),
                    Some(serde_json::json!({"sub": "subject-1"})),
                    now,
                )
                .await
                .expect("oauth link should bind"),
            BindUserOAuthLinkOutcome::Bound
        );
        assert_eq!(
            repository
                .find_oauth_link_owner("linuxdo", "subject-1")
                .await
                .expect("owner lookup should work"),
            Some(user.id.clone())
        );
        assert!(repository
            .find_oauth_linked_user("linuxdo", "subject-1")
            .await
            .expect("linked user should load")
            .is_some());
        assert_eq!(
            repository
                .list_user_oauth_links(&user.id)
                .await
                .expect("links should list")
                .len(),
            1
        );

        assert!(repository
            .touch_oauth_link(
                "linuxdo",
                "subject-1",
                Some("alice2"),
                None,
                Some(serde_json::json!({"sub": "subject-1", "fresh": true})),
                now + chrono::Duration::seconds(10),
            )
            .await
            .expect("link should touch"));
        assert_eq!(
            repository
                .count_user_oauth_links(&user.id)
                .await
                .expect("link count should load"),
            1
        );
        assert_eq!(
            repository
                .delete_user_oauth_link(&user.id, "linuxdo", false, &[])
                .await
                .expect("last link deletion should resolve"),
            DeleteUserOAuthLinkOutcome::LastOAuthBinding
        );
        repository
            .bind_user_oauth_link(
                &user.id,
                "github",
                "subject-2",
                Some("alice"),
                Some("alice@example.com"),
                None,
                now,
            )
            .await
            .expect("second link should upsert");
        assert_eq!(
            repository
                .delete_user_oauth_link(&user.id, "linuxdo", false, &[])
                .await
                .expect("link should delete"),
            DeleteUserOAuthLinkOutcome::Deleted
        );
        assert_eq!(
            repository
                .count_user_oauth_links(&user.id)
                .await
                .expect("link count should load"),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_sqlite_oauth_unbinds_preserve_one_login_method() {
        let database_path = std::env::temp_dir().join(format!(
            "aether-oauth-unbind-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES
  ('linuxdo', 'Linux.do', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1),
  ('github', 'GitHub', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("providers should insert");

        let repository = Arc::new(SqliteUserReadRepository::new(pool.clone()));
        let now = chrono::Utc::now();
        let user = repository
            .create_oauth_auth_user(
                Some("concurrent-oauth@example.com".to_string()),
                true,
                "concurrent-oauth".to_string(),
                now,
            )
            .await
            .expect("oauth user should create")
            .expect("oauth user should exist");
        for (provider_type, subject) in [("linuxdo", "subject-1"), ("github", "subject-2")] {
            repository
                .bind_user_oauth_link(&user.id, provider_type, subject, None, None, None, now)
                .await
                .expect("oauth link should upsert");
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let first_repository = Arc::clone(&repository);
        let first_barrier = Arc::clone(&barrier);
        let first_user_id = user.id.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repository
                .delete_user_oauth_link(&first_user_id, "linuxdo", false, &[])
                .await
                .expect("first unlink should resolve")
        });
        let second_repository = Arc::clone(&repository);
        let second_barrier = Arc::clone(&barrier);
        let second_user_id = user.id.clone();
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repository
                .delete_user_oauth_link(&second_user_id, "github", false, &[])
                .await
                .expect("second unlink should resolve")
        });
        barrier.wait().await;
        let outcomes = [
            first.await.expect("first unlink task should join"),
            second.await.expect("second unlink task should join"),
        ];

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DeleteUserOAuthLinkOutcome::Deleted)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DeleteUserOAuthLinkOutcome::LastOAuthBinding)
                .count(),
            1
        );
        assert_eq!(
            repository
                .count_user_oauth_links(&user.id)
                .await
                .expect("remaining links should count"),
            1
        );

        drop(repository);
        pool.close().await;
        let _ = std::fs::remove_file(database_path);
    }

    #[tokio::test]
    async fn concurrent_sqlite_oauth_binds_preserve_single_identity_owner() {
        let database_path =
            std::env::temp_dir().join(format!("aether-oauth-bind-{}.sqlite", uuid::Uuid::new_v4()));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES (
  'linuxdo', 'Linux.do', 'client', 'https://example.test/callback',
  'https://example.test/app', 1, 1, 1
)
"#,
        )
        .execute(&pool)
        .await
        .expect("provider should insert");

        let repository = Arc::new(SqliteUserReadRepository::new(pool.clone()));
        let now = chrono::Utc::now();
        let first_user = repository
            .create_oauth_auth_user(None, false, "bind-first".to_string(), now)
            .await
            .expect("first user should create")
            .expect("first user should exist");
        let second_user = repository
            .create_oauth_auth_user(None, false, "bind-second".to_string(), now)
            .await
            .expect("second user should create")
            .expect("second user should exist");
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let first_repository = Arc::clone(&repository);
        let first_barrier = Arc::clone(&barrier);
        let first_id = first_user.id.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repository
                .bind_user_oauth_link(
                    &first_id,
                    "linuxdo",
                    "shared-subject",
                    None,
                    None,
                    None,
                    now,
                )
                .await
                .expect("first bind should resolve")
        });
        let second_repository = Arc::clone(&repository);
        let second_barrier = Arc::clone(&barrier);
        let second_id = second_user.id.clone();
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repository
                .bind_user_oauth_link(
                    &second_id,
                    "linuxdo",
                    "shared-subject",
                    None,
                    None,
                    None,
                    now,
                )
                .await
                .expect("second bind should resolve")
        });
        barrier.wait().await;
        let outcomes = [
            first.await.expect("first bind task should join"),
            second.await.expect("second bind task should join"),
        ];

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == BindUserOAuthLinkOutcome::Bound)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| {
                    **outcome == BindUserOAuthLinkOutcome::IdentityBoundToAnotherUser
                })
                .count(),
            1
        );
        let link_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_oauth_links WHERE provider_type = 'linuxdo' AND provider_user_id = 'shared-subject'",
        )
        .fetch_one(&pool)
        .await
        .expect("link count should load");
        assert_eq!(link_count, 1);

        drop(repository);
        pool.close().await;
        let _ = std::fs::remove_file(database_path);
    }

    #[tokio::test]
    async fn sqlite_oauth_bind_rejects_disabled_provider() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES (
  'linuxdo', 'Linux.do', 'client', 'https://example.test/callback',
  'https://example.test/app', 0, 1, 1
)
"#,
        )
        .execute(&pool)
        .await
        .expect("disabled provider should insert");
        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let user = repository
            .create_oauth_auth_user(None, false, "disabled-provider-user".to_string(), now)
            .await
            .expect("user should create")
            .expect("user should exist");

        assert_eq!(
            repository
                .bind_user_oauth_link(&user.id, "linuxdo", "subject", None, None, None, now)
                .await
                .expect("bind should resolve"),
            BindUserOAuthLinkOutcome::ProviderDisabled
        );
        assert!(!repository
            .has_user_oauth_provider_link(&user.id, "linuxdo")
            .await
            .expect("link lookup should succeed"));
    }

    #[tokio::test]
    async fn sqlite_oauth_unbind_only_counts_enabled_provider_links() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES
  ('linuxdo', 'Linux.do', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1),
  ('github', 'GitHub', 'client', 'https://example.test/callback', 'https://example.test/app', 1, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("providers should insert");
        let repository = SqliteUserReadRepository::new(pool.clone());
        let now = chrono::Utc::now();
        let user = repository
            .create_oauth_auth_user(
                Some("enabled-link@example.com".to_string()),
                true,
                "enabled-link".to_string(),
                now,
            )
            .await
            .expect("oauth user should create")
            .expect("oauth user should exist");
        for (provider_type, subject) in [("linuxdo", "subject-1"), ("github", "subject-2")] {
            assert_eq!(
                repository
                    .bind_user_oauth_link(&user.id, provider_type, subject, None, None, None, now)
                    .await
                    .expect("oauth link should bind"),
                BindUserOAuthLinkOutcome::Bound
            );
        }
        sqlx::query("UPDATE oauth_providers SET is_enabled = 0 WHERE provider_type = 'github'")
            .execute(&pool)
            .await
            .expect("provider should disable after its link is created");

        assert_eq!(
            repository
                .delete_user_oauth_link(&user.id, "linuxdo", false, &[])
                .await
                .expect("enabled link deletion should resolve"),
            DeleteUserOAuthLinkOutcome::LastOAuthBinding
        );
        assert_eq!(
            repository
                .delete_user_oauth_link(&user.id, "github", false, &[])
                .await
                .expect("disabled link deletion should resolve"),
            DeleteUserOAuthLinkOutcome::Deleted
        );
        assert!(repository
            .has_user_oauth_provider_link(&user.id, "linuxdo")
            .await
            .expect("enabled provider link lookup should work"));
    }

    #[tokio::test]
    async fn sqlite_oauth_unbind_respects_ldap_exclusive_local_login_policy() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES (
  'linuxdo', 'Linux.do', 'client', 'https://example.test/callback',
  'https://example.test/app', 1, 1, 1
)
"#,
        )
        .execute(&pool)
        .await
        .expect("provider should insert");
        let repository = SqliteUserReadRepository::new(pool);
        let now = chrono::Utc::now();
        let valid_hash = "$2b$12$4qL4tdcsFwVaDTw5Ck3xzu8GpNdre56DiNR6Dnw7t6gCXaEnqAe7G".to_string();
        let user = repository
            .create_local_auth_user(
                Some("ldap-exclusive-local@example.com".to_string()),
                true,
                "ldap-exclusive-local".to_string(),
                valid_hash,
            )
            .await
            .expect("local user should create")
            .expect("local user should exist");
        repository
            .bind_user_oauth_link(&user.id, "linuxdo", "subject-1", None, None, None, now)
            .await
            .expect("oauth link should upsert");

        assert_eq!(
            repository
                .delete_user_oauth_link(&user.id, "linuxdo", false, &[])
                .await
                .expect("unlink should resolve"),
            DeleteUserOAuthLinkOutcome::LastLoginMethod
        );
        assert!(repository
            .has_user_oauth_provider_link(&user.id, "linuxdo")
            .await
            .expect("oauth link lookup should work"));
    }
}
