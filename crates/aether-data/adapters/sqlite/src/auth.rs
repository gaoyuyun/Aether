use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite, Transaction};

use aether_data_contracts::repository::auth::{
    AuthApiKeyExportSummary, AuthApiKeyLookupKey, AuthApiKeyReadRepository,
    AuthApiKeyWriteRepository, CompareAndSwapAuthApiKeyCiphertext, CreateStandaloneApiKeyRecord,
    CreateUserApiKeyRecord, StandaloneApiKeyExportListQuery, StoredAuthApiKeyExportRecord,
    StoredAuthApiKeySnapshot, UpdateStandaloneApiKeyBasicRecord, UpdateUserApiKeyBasicRecord,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::{sqlite_real, SqlitePool};

const SNAPSHOT_COLUMNS: &str = r#"
SELECT
  users.id AS user_id,
  users.username,
  users.email,
  users.role AS user_role,
  users.auth_source AS user_auth_source,
  users.is_active AS user_is_active,
  users.is_deleted AS user_is_deleted,
  users.rate_limit AS user_rate_limit,
  users.allowed_providers AS user_allowed_providers,
  users.allowed_api_formats AS user_allowed_api_formats,
  users.allowed_models AS user_allowed_models,
  api_keys.id AS api_key_id,
  api_keys.name AS api_key_name,
  api_keys.is_active AS api_key_is_active,
  api_keys.is_locked AS api_key_is_locked,
  api_keys.is_standalone AS api_key_is_standalone,
  api_keys.rate_limit AS api_key_rate_limit,
  api_keys.concurrent_limit AS api_key_concurrent_limit,
  api_keys.expires_at AS api_key_expires_at_unix_secs,
  api_keys.allowed_providers AS api_key_allowed_providers,
  api_keys.allowed_api_formats AS api_key_allowed_api_formats,
  api_keys.allowed_models AS api_key_allowed_models,
  api_keys.ip_rules AS api_key_ip_rules
FROM api_keys
JOIN users ON users.id = api_keys.user_id
"#;

const EXPORT_COLUMNS: &str = r#"
SELECT
  api_keys.user_id,
  api_keys.id AS api_key_id,
  api_keys.key_hash,
  api_keys.key_encrypted,
  api_keys.name,
  api_keys.allowed_providers,
  api_keys.allowed_api_formats,
  api_keys.allowed_models,
  api_keys.ip_rules,
  api_keys.rate_limit,
  api_keys.concurrent_limit,
  api_keys.force_capabilities,
  api_keys.feature_settings,
  api_keys.is_active,
  api_keys.expires_at AS expires_at_unix_secs,
  api_keys.auto_delete_on_expiry,
  api_keys.total_requests,
  COALESCE(api_keys.total_tokens, 0) AS total_tokens,
  CAST(COALESCE(api_keys.total_cost_usd, 0) AS REAL) AS total_cost_usd,
  api_keys.last_used_at AS last_used_at_unix_secs,
  api_keys.created_at AS created_at_unix_secs,
  api_keys.updated_at AS updated_at_unix_secs,
  api_keys.is_standalone
FROM api_keys
"#;

const SQLITE_ANONYMIZE_API_KEY_HISTORY_SQL: &[&str] = &[
    "UPDATE request_candidates SET api_key_name = NULL WHERE api_key_id = ?",
    "UPDATE video_tasks SET api_key_name = NULL WHERE api_key_id = ?",
    "UPDATE usage SET api_key_name = NULL WHERE api_key_id = ?",
    "UPDATE stats_daily_api_key SET api_key_name = NULL WHERE api_key_id = ?",
    "UPDATE audit_logs SET description = 'deleted API key event', ip_address = NULL, user_agent = NULL, event_metadata = NULL, error_message = NULL WHERE api_key_id = ?",
    "UPDATE wallet_transactions SET description = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id = ?)",
    "UPDATE payment_callbacks SET payload = NULL, error_message = NULL WHERE EXISTS (SELECT 1 FROM payment_orders AS history_order JOIN wallets AS history_wallet ON history_wallet.id = history_order.wallet_id WHERE history_wallet.api_key_id = ? AND (history_order.id = payment_callbacks.payment_order_id OR (payment_callbacks.order_no IS NOT NULL AND history_order.order_no = payment_callbacks.order_no)))",
    "UPDATE payment_orders SET gateway_response = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id = ?)",
    "UPDATE refund_requests SET reason = NULL, payout_reference = NULL, payout_proof = NULL, failure_reason = NULL WHERE wallet_id IN (SELECT id FROM wallets WHERE api_key_id = ?)",
];

const SQLITE_DELETE_API_KEY_DEPENDENTS_SQL: &[&str] =
    &["DELETE FROM api_key_provider_mappings WHERE api_key_id = ?"];

#[derive(Debug, Clone)]
pub struct SqliteAuthApiKeyReadRepository {
    pool: SqlitePool,
}

impl SqliteAuthApiKeyReadRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn fetch_snapshot_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredAuthApiKeySnapshot>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_auth_api_key_snapshot_row).collect()
    }

    async fn fetch_export_rows(
        &self,
        mut builder: QueryBuilder<'_, Sqlite>,
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_auth_api_key_export_row).collect()
    }

    async fn reload_export_by_id(
        &self,
        api_key_id: &str,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        Ok(self
            .list_export_api_keys_by_ids(&[api_key_id.to_string()])
            .await?
            .into_iter()
            .next())
    }

    async fn reload_export_by_id_in_transaction(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        api_key_id: &str,
        user_id: Option<&str>,
        is_standalone: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        builder
            .push(" WHERE api_keys.id = ")
            .push_bind(api_key_id)
            .push(" AND api_keys.is_standalone = ")
            .push_bind(is_standalone);
        if let Some(user_id) = user_id {
            builder.push(" AND api_keys.user_id = ").push_bind(user_id);
        }
        builder.push(" LIMIT 1");
        let row = builder
            .build()
            .fetch_optional(&mut **tx)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_auth_api_key_export_row).transpose()
    }

    async fn create_api_key(
        &self,
        record: CreateApiKeyInsertRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let now = current_unix_secs();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let owner_exists: Option<String> =
            sqlx::query_scalar("SELECT id FROM users WHERE id = ? AND is_deleted = 0")
                .bind(&record.user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if owner_exists.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        }
        sqlx::query(
            r#"
INSERT INTO api_keys (
  id, user_id, key_hash, key_encrypted, name, allowed_providers,
  allowed_api_formats, allowed_models, ip_rules, rate_limit, concurrent_limit,
  force_capabilities, feature_settings, is_active, expires_at, auto_delete_on_expiry,
  total_requests, total_tokens, total_cost_usd, is_standalone,
  created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&record.api_key_id)
        .bind(&record.user_id)
        .bind(&record.key_hash)
        .bind(&record.key_encrypted)
        .bind(&record.name)
        .bind(json_string_from_string_list(
            record.allowed_providers.as_ref(),
            "api_keys.allowed_providers",
        )?)
        .bind(json_string_from_string_list(
            record.allowed_api_formats.as_ref(),
            "api_keys.allowed_api_formats",
        )?)
        .bind(json_string_from_string_list(
            record.allowed_models.as_ref(),
            "api_keys.allowed_models",
        )?)
        .bind(json_string_from_string_list(
            record.ip_rules.as_ref(),
            "api_keys.ip_rules",
        )?)
        .bind(record.rate_limit)
        .bind(record.concurrent_limit)
        .bind(optional_json_to_string(
            &record.force_capabilities,
            "api_keys.force_capabilities",
        )?)
        .bind(optional_json_to_string(
            &record.feature_settings,
            "api_keys.feature_settings",
        )?)
        .bind(record.is_active)
        .bind(optional_i64_from_u64(
            record.expires_at_unix_secs,
            "api_keys.expires_at",
        )?)
        .bind(record.auto_delete_on_expiry)
        .bind(i64_from_u64(
            record.total_requests,
            "api_keys.total_requests",
        )?)
        .bind(i64_from_u64(record.total_tokens, "api_keys.total_tokens")?)
        .bind(record.total_cost_usd)
        .bind(record.is_standalone)
        .bind(now as i64)
        .bind(now as i64)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        let created = self
            .reload_export_by_id_in_transaction(
                &mut tx,
                &record.api_key_id,
                Some(&record.user_id),
                record.is_standalone,
            )
            .await?
            .ok_or_else(|| {
                DataLayerError::UnexpectedValue(format!(
                    "created api_keys row {} could not be reloaded",
                    record.api_key_id
                ))
            })?;
        tx.commit().await.map_sql_err()?;
        Ok(Some(created))
    }
}

struct CreateApiKeyInsertRecord {
    user_id: String,
    api_key_id: String,
    key_hash: String,
    key_encrypted: Option<String>,
    name: Option<String>,
    allowed_providers: Option<Vec<String>>,
    allowed_api_formats: Option<Vec<String>>,
    allowed_models: Option<Vec<String>>,
    ip_rules: Option<Vec<String>>,
    rate_limit: Option<i32>,
    concurrent_limit: Option<i32>,
    force_capabilities: Option<serde_json::Value>,
    feature_settings: Option<serde_json::Value>,
    is_active: bool,
    expires_at_unix_secs: Option<u64>,
    auto_delete_on_expiry: bool,
    total_requests: u64,
    total_tokens: u64,
    total_cost_usd: f64,
    is_standalone: bool,
}

#[async_trait]
impl AuthApiKeyReadRepository for SqliteAuthApiKeyReadRepository {
    async fn find_api_key_snapshot(
        &self,
        key: AuthApiKeyLookupKey<'_>,
    ) -> Result<Option<StoredAuthApiKeySnapshot>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(SNAPSHOT_COLUMNS);
        match key {
            AuthApiKeyLookupKey::KeyHash(key_hash) => {
                builder
                    .push(" WHERE api_keys.key_hash = ")
                    .push_bind(key_hash);
            }
            AuthApiKeyLookupKey::ApiKeyId(api_key_id) => {
                builder.push(" WHERE api_keys.id = ").push_bind(api_key_id);
            }
            AuthApiKeyLookupKey::UserApiKeyIds {
                user_id,
                api_key_id,
            } => {
                builder
                    .push(" WHERE api_keys.id = ")
                    .push_bind(api_key_id)
                    .push(" AND users.id = ")
                    .push_bind(user_id);
            }
        }
        builder.push(" LIMIT 1");
        Ok(self.fetch_snapshot_rows(builder).await?.into_iter().next())
    }

    async fn list_api_key_snapshots_by_ids(
        &self,
        api_key_ids: &[String],
    ) -> Result<Vec<StoredAuthApiKeySnapshot>, DataLayerError> {
        if api_key_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(SNAPSHOT_COLUMNS);
        push_in_clause(&mut builder, " WHERE api_keys.id IN (", api_key_ids);
        builder.push(" ORDER BY api_keys.id ASC");
        self.fetch_snapshot_rows(builder).await
    }

    async fn list_export_api_keys_by_user_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        push_in_clause(&mut builder, " WHERE api_keys.user_id IN (", user_ids);
        builder
            .push(" AND api_keys.is_standalone = 0 ORDER BY api_keys.user_id ASC, api_keys.id ASC");
        self.fetch_export_rows(builder).await
    }

    async fn list_export_api_keys_by_ids(
        &self,
        api_key_ids: &[String],
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        if api_key_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        push_in_clause(&mut builder, " WHERE api_keys.id IN (", api_key_ids);
        builder.push(" ORDER BY api_keys.id ASC");
        self.fetch_export_rows(builder).await
    }

    async fn list_export_api_keys_by_name_search(
        &self,
        name_search: &str,
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let name_search = name_search.trim();
        if name_search.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        builder
            .push(" WHERE LOWER(COALESCE(api_keys.name, '')) LIKE ")
            .push_bind(format!("%{}%", name_search.to_ascii_lowercase()))
            .push(" ORDER BY api_keys.id ASC");
        self.fetch_export_rows(builder).await
    }

    async fn list_export_standalone_api_keys_page(
        &self,
        query: &StandaloneApiKeyExportListQuery,
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        builder.push(" WHERE api_keys.is_standalone = 1");
        if let Some(is_active) = query.is_active {
            builder
                .push(" AND api_keys.is_active = ")
                .push_bind(is_active);
        }
        builder
            .push(" ORDER BY api_keys.id ASC LIMIT ")
            .push_bind(i64::try_from(query.limit).map_err(|_| {
                DataLayerError::InvalidInput(format!(
                    "invalid standalone api key export limit: {}",
                    query.limit
                ))
            })?)
            .push(" OFFSET ")
            .push_bind(i64::try_from(query.skip).map_err(|_| {
                DataLayerError::InvalidInput(format!(
                    "invalid standalone api key export skip: {}",
                    query.skip
                ))
            })?);
        self.fetch_export_rows(builder).await
    }

    async fn count_export_standalone_api_keys(
        &self,
        is_active: Option<bool>,
    ) -> Result<u64, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT COUNT(*) AS total FROM api_keys WHERE is_standalone = 1",
        );
        if let Some(is_active) = is_active {
            builder.push(" AND is_active = ").push_bind(is_active);
        }
        let row = builder.build().fetch_one(&self.pool).await.map_sql_err()?;
        Ok(row.try_get::<i64, _>("total").map_sql_err()?.max(0) as u64)
    }

    async fn summarize_export_api_keys_by_user_ids(
        &self,
        user_ids: &[String],
        now_unix_secs: u64,
    ) -> Result<AuthApiKeyExportSummary, DataLayerError> {
        if user_ids.is_empty() {
            return Ok(AuthApiKeyExportSummary::default());
        }
        let now_unix_secs = i64_from_u64(now_unix_secs, "api_keys.summary_now")?;

        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
  COUNT(*) AS total,
  SUM(CASE WHEN is_active = 1 AND (expires_at IS NULL OR expires_at >=
"#,
        );
        builder.push_bind(now_unix_secs);
        builder.push(
            r#") THEN 1 ELSE 0 END) AS active
FROM api_keys
"#,
        );
        push_in_clause(&mut builder, " WHERE user_id IN (", user_ids);
        builder.push(" AND is_standalone = 0");
        summarize_row(builder.build().fetch_one(&self.pool).await.map_sql_err()?)
    }

    async fn summarize_export_non_standalone_api_keys(
        &self,
        now_unix_secs: u64,
    ) -> Result<AuthApiKeyExportSummary, DataLayerError> {
        summarize_api_keys(&self.pool, false, now_unix_secs).await
    }

    async fn summarize_export_standalone_api_keys(
        &self,
        now_unix_secs: u64,
    ) -> Result<AuthApiKeyExportSummary, DataLayerError> {
        summarize_api_keys(&self.pool, true, now_unix_secs).await
    }

    async fn find_export_standalone_api_key_by_id(
        &self,
        api_key_id: &str,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        builder
            .push(" WHERE api_keys.is_standalone = 1 AND api_keys.id = ")
            .push_bind(api_key_id)
            .push(" LIMIT 1");
        Ok(self.fetch_export_rows(builder).await?.into_iter().next())
    }

    async fn list_export_standalone_api_keys(
        &self,
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(EXPORT_COLUMNS);
        builder.push(" WHERE api_keys.is_standalone = 1 ORDER BY api_keys.id ASC");
        self.fetch_export_rows(builder).await
    }
}

#[async_trait]
impl AuthApiKeyWriteRepository for SqliteAuthApiKeyReadRepository {
    async fn touch_last_used_at(&self, api_key_id: &str) -> Result<bool, DataLayerError> {
        let now = current_unix_secs() as i64;
        let rows_affected = sqlx::query(
            r#"
UPDATE api_keys
SET last_used_at = ?, updated_at = ?
WHERE id = ?
"#,
        )
        .bind(now)
        .bind(now)
        .bind(api_key_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn create_user_api_key(
        &self,
        record: CreateUserApiKeyRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.create_api_key(CreateApiKeyInsertRecord {
            user_id: record.user_id,
            api_key_id: record.api_key_id,
            key_hash: record.key_hash,
            key_encrypted: record.key_encrypted,
            name: record.name,
            allowed_providers: record.allowed_providers,
            allowed_api_formats: record.allowed_api_formats,
            allowed_models: record.allowed_models,
            ip_rules: record.ip_rules,
            rate_limit: Some(record.rate_limit),
            concurrent_limit: record.concurrent_limit,
            force_capabilities: record.force_capabilities,
            feature_settings: record.feature_settings,
            is_active: record.is_active,
            expires_at_unix_secs: record.expires_at_unix_secs,
            auto_delete_on_expiry: record.auto_delete_on_expiry,
            total_requests: record.total_requests,
            total_tokens: record.total_tokens,
            total_cost_usd: record.total_cost_usd,
            is_standalone: false,
        })
        .await
    }

    async fn create_standalone_api_key(
        &self,
        record: CreateStandaloneApiKeyRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.create_api_key(CreateApiKeyInsertRecord {
            user_id: record.user_id,
            api_key_id: record.api_key_id,
            key_hash: record.key_hash,
            key_encrypted: record.key_encrypted,
            name: record.name,
            allowed_providers: record.allowed_providers,
            allowed_api_formats: record.allowed_api_formats,
            allowed_models: record.allowed_models,
            ip_rules: record.ip_rules,
            rate_limit: record.rate_limit,
            concurrent_limit: record.concurrent_limit,
            force_capabilities: record.force_capabilities,
            feature_settings: None,
            is_active: record.is_active,
            expires_at_unix_secs: record.expires_at_unix_secs,
            auto_delete_on_expiry: record.auto_delete_on_expiry,
            total_requests: record.total_requests,
            total_tokens: record.total_tokens,
            total_cost_usd: record.total_cost_usd,
            is_standalone: true,
        })
        .await
    }

    async fn update_user_api_key_basic(
        &self,
        record: UpdateUserApiKeyBasicRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.update_user_api_key_basic_scoped(record, false).await
    }

    async fn compare_and_swap_api_key_ciphertext(
        &self,
        mutation: &CompareAndSwapAuthApiKeyCiphertext,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE api_keys
SET key_encrypted = ?
WHERE CAST(id AS BLOB) = CAST(? AS BLOB)
  AND CAST(user_id AS BLOB) = CAST(? AS BLOB)
  AND CAST(key_hash AS BLOB) = CAST(? AS BLOB)
  AND is_standalone = ?
  AND CAST(key_encrypted AS BLOB) = CAST(? AS BLOB)
"#,
        )
        .bind(&mutation.key_encrypted)
        .bind(&mutation.api_key_id)
        .bind(&mutation.user_id)
        .bind(&mutation.key_hash)
        .bind(mutation.is_standalone)
        .bind(&mutation.expected_key_encrypted)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn update_user_api_key_basic_if_unlocked(
        &self,
        record: UpdateUserApiKeyBasicRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.update_user_api_key_basic_scoped(record, true).await
    }

    async fn update_standalone_api_key_basic(
        &self,
        record: UpdateStandaloneApiKeyBasicRecord,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let now = current_unix_secs() as i64;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query(
            r#"
UPDATE api_keys
SET key_encrypted = CASE WHEN ? THEN ? ELSE key_encrypted END,
    name = CASE WHEN ? THEN ? ELSE name END,
    force_capabilities = CASE WHEN ? THEN ? ELSE force_capabilities END,
    rate_limit = CASE WHEN ? THEN ? ELSE rate_limit END,
    concurrent_limit = CASE WHEN ? THEN ? ELSE concurrent_limit END,
    allowed_providers = CASE WHEN ? THEN ? ELSE allowed_providers END,
    allowed_api_formats = CASE WHEN ? THEN ? ELSE allowed_api_formats END,
    allowed_models = CASE WHEN ? THEN ? ELSE allowed_models END,
    ip_rules = CASE WHEN ? THEN ? ELSE ip_rules END,
    expires_at = CASE WHEN ? THEN ? ELSE expires_at END,
    auto_delete_on_expiry = CASE WHEN ? THEN ? ELSE auto_delete_on_expiry END,
    updated_at = ?
WHERE id = ?
  AND is_standalone = 1
"#,
        )
        .bind(record.key_encrypted_present)
        .bind(record.key_encrypted.as_deref())
        .bind(record.name_present)
        .bind(record.name.as_deref())
        .bind(record.force_capabilities.is_some())
        .bind(optional_json_to_string(
            &record.force_capabilities.clone().flatten(),
            "api_keys.force_capabilities",
        )?)
        .bind(record.rate_limit_present)
        .bind(record.rate_limit)
        .bind(record.concurrent_limit_present)
        .bind(record.concurrent_limit)
        .bind(record.allowed_providers.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_providers,
            "api_keys.allowed_providers",
        )?)
        .bind(record.allowed_api_formats.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_api_formats,
            "api_keys.allowed_api_formats",
        )?)
        .bind(record.allowed_models.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_models,
            "api_keys.allowed_models",
        )?)
        .bind(record.ip_rules.is_some())
        .bind(json_string_from_nested_string_list(
            &record.ip_rules,
            "api_keys.ip_rules",
        )?)
        .bind(record.expires_at_present)
        .bind(optional_i64_from_u64(
            record.expires_at_unix_secs,
            "api_keys.expires_at",
        )?)
        .bind(record.auto_delete_on_expiry_present)
        .bind(record.auto_delete_on_expiry)
        .bind(now)
        .bind(&record.api_key_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(updated) = self
            .reload_export_by_id_in_transaction(&mut tx, &record.api_key_id, None, true)
            .await?
        else {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        };
        tx.commit().await.map_sql_err()?;
        Ok(Some(updated))
    }

    async fn restore_api_key_if_matches(
        &self,
        expected: &StoredAuthApiKeyExportRecord,
        restored: &StoredAuthApiKeyExportRecord,
    ) -> Result<bool, DataLayerError> {
        if restored.api_key_id != expected.api_key_id
            || restored.user_id != expected.user_id
            || restored.key_hash != expected.key_hash
            || restored.is_standalone != expected.is_standalone
        {
            return Ok(false);
        }

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let select_sql = format!("{EXPORT_COLUMNS} WHERE api_keys.id = ? LIMIT 1");
        let row = sqlx::query(&select_sql)
            .bind(&expected.api_key_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        };
        let current = map_auth_api_key_export_row(&row)?;
        if current != *expected {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        let result = sqlx::query(
            r#"
UPDATE api_keys
SET key_encrypted = ?,
    name = ?,
    allowed_providers = ?,
    allowed_api_formats = ?,
    allowed_models = ?,
    ip_rules = ?,
    rate_limit = ?,
    concurrent_limit = ?,
    force_capabilities = ?,
    feature_settings = ?,
    is_active = ?,
    expires_at = ?,
    auto_delete_on_expiry = ?,
    total_requests = ?,
    total_tokens = ?,
    total_cost_usd = ?,
    last_used_at = ?,
    updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND key_hash = ?
  AND is_standalone = ?
"#,
        )
        .bind(restored.key_encrypted.as_deref())
        .bind(restored.name.as_deref())
        .bind(json_string_from_string_list(
            restored.allowed_providers.as_ref(),
            "api_keys.allowed_providers",
        )?)
        .bind(json_string_from_string_list(
            restored.allowed_api_formats.as_ref(),
            "api_keys.allowed_api_formats",
        )?)
        .bind(json_string_from_string_list(
            restored.allowed_models.as_ref(),
            "api_keys.allowed_models",
        )?)
        .bind(json_string_from_string_list(
            restored.ip_rules.as_ref(),
            "api_keys.ip_rules",
        )?)
        .bind(restored.rate_limit)
        .bind(restored.concurrent_limit)
        .bind(optional_json_to_string(
            &restored.force_capabilities,
            "api_keys.force_capabilities",
        )?)
        .bind(optional_json_to_string(
            &restored.feature_settings,
            "api_keys.feature_settings",
        )?)
        .bind(restored.is_active)
        .bind(optional_i64_from_u64(
            restored.expires_at_unix_secs,
            "api_keys.expires_at",
        )?)
        .bind(restored.auto_delete_on_expiry)
        .bind(i64_from_u64(
            restored.total_requests,
            "api_keys.total_requests",
        )?)
        .bind(i64_from_u64(
            restored.total_tokens,
            "api_keys.total_tokens",
        )?)
        .bind(restored.total_cost_usd)
        .bind(optional_i64_from_u64(
            restored.last_used_at_unix_secs,
            "api_keys.last_used_at",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(&restored.api_key_id)
        .bind(&restored.user_id)
        .bind(&restored.key_hash)
        .bind(restored.is_standalone)
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

    async fn set_user_api_key_active(
        &self,
        user_id: &str,
        api_key_id: &str,
        is_active: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_active(api_key_id, Some(user_id), is_active, false, false)
            .await
    }

    async fn set_user_api_key_active_if_unlocked(
        &self,
        user_id: &str,
        api_key_id: &str,
        is_active: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_active(api_key_id, Some(user_id), is_active, false, true)
            .await
    }

    async fn set_standalone_api_key_active(
        &self,
        api_key_id: &str,
        is_active: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_active(api_key_id, None, is_active, true, false)
            .await
    }

    async fn set_user_api_key_locked(
        &self,
        user_id: &str,
        api_key_id: &str,
        is_locked: bool,
    ) -> Result<bool, DataLayerError> {
        let rows_affected = sqlx::query(
            r#"
UPDATE api_keys
SET is_locked = ?, updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND is_standalone = 0
"#,
        )
        .bind(is_locked)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn set_user_api_key_allowed_providers(
        &self,
        user_id: &str,
        api_key_id: &str,
        allowed_providers: Option<Vec<String>>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_allowed_providers_scoped(
            user_id,
            api_key_id,
            allowed_providers,
            false,
        )
        .await
    }

    async fn set_user_api_key_allowed_providers_if_unlocked(
        &self,
        user_id: &str,
        api_key_id: &str,
        allowed_providers: Option<Vec<String>>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_allowed_providers_scoped(user_id, api_key_id, allowed_providers, true)
            .await
    }

    async fn set_user_api_key_force_capabilities(
        &self,
        user_id: &str,
        api_key_id: &str,
        force_capabilities: Option<serde_json::Value>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_force_capabilities_scoped(
            user_id,
            api_key_id,
            force_capabilities,
            false,
        )
        .await
    }

    async fn set_user_api_key_force_capabilities_if_unlocked(
        &self,
        user_id: &str,
        api_key_id: &str,
        force_capabilities: Option<serde_json::Value>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_force_capabilities_scoped(
            user_id,
            api_key_id,
            force_capabilities,
            true,
        )
        .await
    }

    async fn set_user_api_key_feature_settings(
        &self,
        user_id: &str,
        api_key_id: &str,
        feature_settings: Option<serde_json::Value>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_feature_settings_scoped(user_id, api_key_id, feature_settings, false)
            .await
    }

    async fn set_user_api_key_feature_settings_if_unlocked(
        &self,
        user_id: &str,
        api_key_id: &str,
        feature_settings: Option<serde_json::Value>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        self.set_user_api_key_feature_settings_scoped(user_id, api_key_id, feature_settings, true)
            .await
    }

    async fn set_api_key_usage_totals(
        &self,
        api_key_id: &str,
        total_requests: u64,
        total_tokens: u64,
        total_cost_usd: f64,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        if !total_cost_usd.is_finite() {
            return Err(DataLayerError::InvalidInput(
                "api_keys.total_cost_usd is not finite".to_string(),
            ));
        }
        sqlx::query(
            r#"
UPDATE api_keys
SET total_requests = ?,
    total_tokens = ?,
    total_cost_usd = ?,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(i64_from_u64(total_requests, "api_keys.total_requests")?)
        .bind(i64_from_u64(total_tokens, "api_keys.total_tokens")?)
        .bind(total_cost_usd)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.reload_export_by_id(api_key_id).await
    }

    async fn delete_user_api_key(
        &self,
        user_id: &str,
        api_key_id: &str,
    ) -> Result<bool, DataLayerError> {
        self.delete_api_key(api_key_id, Some(user_id), false, false)
            .await
    }

    async fn delete_user_api_key_if_unlocked(
        &self,
        user_id: &str,
        api_key_id: &str,
    ) -> Result<bool, DataLayerError> {
        self.delete_api_key(api_key_id, Some(user_id), false, true)
            .await
    }

    async fn delete_standalone_api_key(&self, api_key_id: &str) -> Result<bool, DataLayerError> {
        self.delete_api_key(api_key_id, None, true, false).await
    }

    async fn set_standalone_api_key_feature_settings(
        &self,
        api_key_id: &str,
        feature_settings: Option<serde_json::Value>,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        sqlx::query(
            r#"
UPDATE api_keys
SET feature_settings = ?, updated_at = ?
WHERE id = ?
  AND is_standalone = 1
"#,
        )
        .bind(optional_json_to_string(
            &feature_settings,
            "api_keys.feature_settings",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.reload_export_by_id(api_key_id).await
    }
}

impl SqliteAuthApiKeyReadRepository {
    async fn update_user_api_key_basic_scoped(
        &self,
        record: UpdateUserApiKeyBasicRecord,
        require_unlocked: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let result = sqlx::query(
            r#"
UPDATE api_keys
SET key_encrypted = CASE WHEN ? THEN ? ELSE key_encrypted END,
    name = CASE WHEN ? THEN ? ELSE name END,
    rate_limit = CASE WHEN ? THEN ? ELSE rate_limit END,
    concurrent_limit = CASE WHEN ? THEN ? ELSE concurrent_limit END,
    ip_rules = CASE WHEN ? THEN ? ELSE ip_rules END,
    allowed_providers = CASE WHEN ? THEN ? ELSE allowed_providers END,
    allowed_api_formats = CASE WHEN ? THEN ? ELSE allowed_api_formats END,
    allowed_models = CASE WHEN ? THEN ? ELSE allowed_models END,
    feature_settings = CASE WHEN ? THEN ? ELSE feature_settings END,
    updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND is_standalone = 0
  AND (? = 0 OR is_locked = 0)
"#,
        )
        .bind(record.key_encrypted_present)
        .bind(record.key_encrypted.as_deref())
        .bind(record.name_present)
        .bind(record.name.as_deref())
        .bind(record.rate_limit_present)
        .bind(record.rate_limit)
        .bind(record.concurrent_limit_present)
        .bind(record.concurrent_limit)
        .bind(record.ip_rules.is_some())
        .bind(json_string_from_nested_string_list(
            &record.ip_rules,
            "api_keys.ip_rules",
        )?)
        .bind(record.allowed_providers.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_providers,
            "api_keys.allowed_providers",
        )?)
        .bind(record.allowed_api_formats.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_api_formats,
            "api_keys.allowed_api_formats",
        )?)
        .bind(record.allowed_models.is_some())
        .bind(json_string_from_nested_string_list(
            &record.allowed_models,
            "api_keys.allowed_models",
        )?)
        .bind(record.feature_settings.is_some())
        .bind(optional_json_to_string(
            &record.feature_settings.clone().flatten(),
            "api_keys.feature_settings",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(&record.api_key_id)
        .bind(&record.user_id)
        .bind(require_unlocked)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        }
        let updated = self
            .reload_export_by_id_in_transaction(
                &mut tx,
                &record.api_key_id,
                Some(&record.user_id),
                false,
            )
            .await?;
        tx.commit().await.map_sql_err()?;
        Ok(updated)
    }

    async fn set_active(
        &self,
        api_key_id: &str,
        user_id: Option<&str>,
        is_active: bool,
        is_standalone: bool,
        require_unlocked: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new("UPDATE api_keys SET is_active = ");
        builder
            .push_bind(is_active)
            .push(", updated_at = ")
            .push_bind(current_unix_secs() as i64)
            .push(" WHERE id = ")
            .push_bind(api_key_id)
            .push(" AND is_standalone = ")
            .push_bind(is_standalone);
        if let Some(user_id) = user_id {
            builder.push(" AND user_id = ").push_bind(user_id);
        }
        if require_unlocked {
            builder.push(" AND is_locked = ").push_bind(false);
        }
        let result = builder.build().execute(&self.pool).await.map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.reload_export_by_id(api_key_id).await
    }

    async fn set_user_api_key_allowed_providers_scoped(
        &self,
        user_id: &str,
        api_key_id: &str,
        allowed_providers: Option<Vec<String>>,
        require_unlocked: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE api_keys
SET allowed_providers = ?, updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND is_standalone = 0
  AND (? = 0 OR is_locked = 0)
"#,
        )
        .bind(json_string_from_string_list(
            allowed_providers.as_ref(),
            "api_keys.allowed_providers",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .bind(user_id)
        .bind(require_unlocked)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.reload_export_by_id(api_key_id).await
    }

    async fn set_user_api_key_force_capabilities_scoped(
        &self,
        user_id: &str,
        api_key_id: &str,
        force_capabilities: Option<serde_json::Value>,
        require_unlocked: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE api_keys
SET force_capabilities = ?, updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND is_standalone = 0
  AND (? = 0 OR is_locked = 0)
"#,
        )
        .bind(optional_json_to_string(
            &force_capabilities,
            "api_keys.force_capabilities",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .bind(user_id)
        .bind(require_unlocked)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.reload_export_by_id(api_key_id).await
    }

    async fn set_user_api_key_feature_settings_scoped(
        &self,
        user_id: &str,
        api_key_id: &str,
        feature_settings: Option<serde_json::Value>,
        require_unlocked: bool,
    ) -> Result<Option<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE api_keys
SET feature_settings = ?, updated_at = ?
WHERE id = ?
  AND user_id = ?
  AND is_standalone = 0
  AND (? = 0 OR is_locked = 0)
"#,
        )
        .bind(optional_json_to_string(
            &feature_settings,
            "api_keys.feature_settings",
        )?)
        .bind(current_unix_secs() as i64)
        .bind(api_key_id)
        .bind(user_id)
        .bind(require_unlocked)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.reload_export_by_id(api_key_id).await
    }

    async fn delete_api_key(
        &self,
        api_key_id: &str,
        user_id: Option<&str>,
        is_standalone: bool,
        require_unlocked: bool,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let matching_api_key = if let Some(user_id) = user_id {
            if require_unlocked {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM api_keys WHERE id = ? AND user_id = ? AND is_standalone = 0 AND is_locked = 0",
                )
                .bind(api_key_id)
                .bind(user_id)
                .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?
            } else {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM api_keys WHERE id = ? AND user_id = ? AND is_standalone = 0",
                )
                .bind(api_key_id)
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?
            }
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE id = ? AND is_standalone = 1",
            )
            .bind(api_key_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?
        };
        if matching_api_key.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        sqlx::query(
            "UPDATE wallets SET status = 'disabled', updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE api_key_id = ? AND status <> 'disabled'",
        )
        .bind(api_key_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        for sql in SQLITE_ANONYMIZE_API_KEY_HISTORY_SQL {
            sqlx::query(sql)
                .bind(api_key_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        for sql in SQLITE_DELETE_API_KEY_DEPENDENTS_SQL {
            sqlx::query(sql)
                .bind(api_key_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        let result = sqlx::query("DELETE FROM api_keys WHERE id = ? AND is_standalone = ?")
            .bind(api_key_id)
            .bind(is_standalone)
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
}

fn push_in_clause<'args>(
    builder: &mut QueryBuilder<'args, Sqlite>,
    prefix: &str,
    values: &'args [String],
) {
    builder.push(prefix);
    {
        let mut separated = builder.separated(", ");
        for value in values {
            separated.push_bind(value);
        }
    }
    builder.push(")");
}

async fn summarize_api_keys(
    pool: &SqlitePool,
    is_standalone: bool,
    now_unix_secs: u64,
) -> Result<AuthApiKeyExportSummary, DataLayerError> {
    let now_unix_secs = i64_from_u64(now_unix_secs, "api_keys.summary_now")?;
    let row = sqlx::query(
        r#"
SELECT
  COUNT(*) AS total,
  SUM(CASE WHEN is_active = 1 AND (expires_at IS NULL OR expires_at >= ?) THEN 1 ELSE 0 END) AS active
FROM api_keys
WHERE is_standalone = ?
"#,
    )
    .bind(now_unix_secs)
    .bind(is_standalone)
    .fetch_one(pool)
    .await
    .map_sql_err()?;
    summarize_row(row)
}

fn summarize_row(row: SqliteRow) -> Result<AuthApiKeyExportSummary, DataLayerError> {
    Ok(AuthApiKeyExportSummary {
        total: row.try_get::<i64, _>("total").map_sql_err()?.max(0) as u64,
        active: row
            .try_get::<Option<i64>, _>("active")
            .map_sql_err()?
            .unwrap_or(0)
            .max(0) as u64,
    })
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

fn current_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn i64_from_u64(value: u64, field_name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::InvalidInput(format!("{field_name} exceeds i64: {value}")))
}

fn optional_i64_from_u64(
    value: Option<u64>,
    field_name: &str,
) -> Result<Option<i64>, DataLayerError> {
    value
        .map(|value| i64_from_u64(value, field_name))
        .transpose()
}

fn optional_json_to_string(
    value: &Option<serde_json::Value>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains unserializable JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn json_string_from_string_list(
    value: Option<&Vec<String>>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains unserializable string list: {err}"
                ))
            })
        })
        .transpose()
}

fn json_string_from_nested_string_list(
    value: &Option<Option<Vec<String>>>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    match value {
        Some(Some(values)) => json_string_from_string_list(Some(values), field_name),
        Some(None) | None => Ok(None),
    }
}

fn map_auth_api_key_snapshot_row(
    row: &SqliteRow,
) -> Result<StoredAuthApiKeySnapshot, DataLayerError> {
    let snapshot = StoredAuthApiKeySnapshot::new(
        row.try_get("user_id").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("email").map_sql_err()?,
        row.try_get("user_role").map_sql_err()?,
        row.try_get("user_auth_source").map_sql_err()?,
        row.try_get("user_is_active").map_sql_err()?,
        row.try_get("user_is_deleted").map_sql_err()?,
        optional_json_from_string(
            row.try_get("user_allowed_providers").map_sql_err()?,
            "users.allowed_providers",
        )?,
        optional_json_from_string(
            row.try_get("user_allowed_api_formats").map_sql_err()?,
            "users.allowed_api_formats",
        )?,
        optional_json_from_string(
            row.try_get("user_allowed_models").map_sql_err()?,
            "users.allowed_models",
        )?,
        row.try_get("api_key_id").map_sql_err()?,
        row.try_get("api_key_name").map_sql_err()?,
        row.try_get("api_key_is_active").map_sql_err()?,
        row.try_get("api_key_is_locked").map_sql_err()?,
        row.try_get("api_key_is_standalone").map_sql_err()?,
        row.try_get("api_key_rate_limit").map_sql_err()?,
        row.try_get("api_key_concurrent_limit").map_sql_err()?,
        row.try_get("api_key_expires_at_unix_secs").map_sql_err()?,
        optional_json_from_string(
            row.try_get("api_key_allowed_providers").map_sql_err()?,
            "api_keys.allowed_providers",
        )?,
        optional_json_from_string(
            row.try_get("api_key_allowed_api_formats").map_sql_err()?,
            "api_keys.allowed_api_formats",
        )?,
        optional_json_from_string(
            row.try_get("api_key_allowed_models").map_sql_err()?,
            "api_keys.allowed_models",
        )?,
    )?
    .with_api_key_ip_rules(optional_json_from_string(
        row.try_get("api_key_ip_rules").map_sql_err()?,
        "api_keys.ip_rules",
    )?)?;
    Ok(snapshot.with_user_rate_limit(row.try_get("user_rate_limit").map_sql_err()?))
}

fn map_auth_api_key_export_row(
    row: &SqliteRow,
) -> Result<StoredAuthApiKeyExportRecord, DataLayerError> {
    let feature_settings = optional_json_from_string(
        row.try_get("feature_settings").map_sql_err()?,
        "api_keys.feature_settings",
    )?;
    StoredAuthApiKeyExportRecord::new(
        row.try_get("user_id").map_sql_err()?,
        row.try_get("api_key_id").map_sql_err()?,
        row.try_get("key_hash").map_sql_err()?,
        row.try_get("key_encrypted").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        optional_json_from_string(
            row.try_get("allowed_providers").map_sql_err()?,
            "api_keys.allowed_providers",
        )?,
        optional_json_from_string(
            row.try_get("allowed_api_formats").map_sql_err()?,
            "api_keys.allowed_api_formats",
        )?,
        optional_json_from_string(
            row.try_get("allowed_models").map_sql_err()?,
            "api_keys.allowed_models",
        )?,
        row.try_get("rate_limit").map_sql_err()?,
        row.try_get("concurrent_limit").map_sql_err()?,
        optional_json_from_string(
            row.try_get("force_capabilities").map_sql_err()?,
            "api_keys.force_capabilities",
        )?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("expires_at_unix_secs").map_sql_err()?,
        row.try_get("auto_delete_on_expiry").map_sql_err()?,
        row.try_get("total_requests").map_sql_err()?,
        row.try_get("total_tokens").map_sql_err()?,
        sqlite_real(row, "total_cost_usd")?,
        row.try_get("is_standalone").map_sql_err()?,
    )
    .and_then(|record| {
        record.with_ip_rules(optional_json_from_string(
            row.try_get("ip_rules").map_sql_err()?,
            "api_keys.ip_rules",
        )?)
    })
    .map(|record| record.with_feature_settings(feature_settings))
    .and_then(|record| {
        record.with_activity_timestamps(
            row.try_get("last_used_at_unix_secs").map_sql_err()?,
            row.try_get("created_at_unix_secs").map_sql_err()?,
            row.try_get("updated_at_unix_secs").map_sql_err()?,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::SqliteAuthApiKeyReadRepository;
    use crate::run_migrations;
    use aether_data_contracts::repository::auth::{
        AuthApiKeyLookupKey, AuthApiKeyReadRepository, AuthApiKeyWriteRepository,
        CreateStandaloneApiKeyRecord, CreateUserApiKeyRecord, StandaloneApiKeyExportListQuery,
        UpdateStandaloneApiKeyBasicRecord, UpdateUserApiKeyBasicRecord,
    };
    use serde_json::json;
    use sqlx::Row;

    #[tokio::test]
    async fn sqlite_repository_reads_auth_api_key_contract_views() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_auth_api_key_rows(&pool).await;

        let repository = SqliteAuthApiKeyReadRepository::new(pool);
        let snapshot = repository
            .find_api_key_snapshot(AuthApiKeyLookupKey::KeyHash("hash-user"))
            .await
            .expect("snapshot lookup should run")
            .expect("snapshot should exist");
        assert_eq!(snapshot.user_id, "user-1");
        assert_eq!(
            snapshot.api_key_allowed_models,
            Some(vec!["gpt-4.1".to_string()])
        );

        let by_ids = repository
            .list_api_key_snapshots_by_ids(&["key-user".to_string(), "key-standalone".to_string()])
            .await
            .expect("snapshot list should run");
        assert_eq!(by_ids.len(), 2);

        let user_exports = repository
            .list_export_api_keys_by_user_ids(&["user-1".to_string()])
            .await
            .expect("user exports should load");
        assert_eq!(user_exports.len(), 1);
        assert_eq!(user_exports[0].total_tokens, 456);

        let page = repository
            .list_export_standalone_api_keys_page(&StandaloneApiKeyExportListQuery {
                skip: 0,
                limit: 10,
                is_active: Some(true),
            })
            .await
            .expect("standalone page should load");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].api_key_id, "key-standalone");

        let count = repository
            .count_export_standalone_api_keys(Some(true))
            .await
            .expect("standalone count should load");
        assert_eq!(count, 1);

        let summary = repository
            .summarize_export_api_keys_by_user_ids(&["user-1".to_string()], 100)
            .await
            .expect("summary should load");
        assert_eq!(summary.total, 1);
        assert_eq!(summary.active, 1);

        assert_eq!(
            repository
                .find_export_standalone_api_key_by_id("key-standalone")
                .await
                .expect("standalone find should run")
                .expect("standalone should exist")
                .name,
            Some("Standalone".to_string())
        );
    }

    #[tokio::test]
    async fn sqlite_repository_writes_auth_api_key_contract_views() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_auth_user(&pool).await;

        let repository = SqliteAuthApiKeyReadRepository::new(pool);
        let missing_owner_key = repository
            .create_user_api_key(CreateUserApiKeyRecord {
                user_id: "missing-user".to_string(),
                api_key_id: "key-missing-owner".to_string(),
                key_hash: "hash-missing-owner".to_string(),
                key_encrypted: Some("enc-missing-owner".to_string()),
                name: Some("Missing Owner".to_string()),
                allowed_providers: None,
                allowed_api_formats: None,
                allowed_models: None,
                ip_rules: None,
                rate_limit: 100,
                concurrent_limit: None,
                force_capabilities: None,
                feature_settings: None,
                is_active: true,
                expires_at_unix_secs: None,
                auto_delete_on_expiry: false,
                total_requests: 0,
                total_tokens: 0,
                total_cost_usd: 0.0,
            })
            .await
            .expect("missing-owner creation should resolve");
        assert!(missing_owner_key.is_none());
        let user_key = repository
            .create_user_api_key(CreateUserApiKeyRecord {
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                key_hash: "hash-created-user".to_string(),
                key_encrypted: Some("enc-user".to_string()),
                name: Some("Created User".to_string()),
                allowed_providers: Some(vec!["openai".to_string()]),
                allowed_api_formats: Some(vec!["openai:chat".to_string()]),
                allowed_models: Some(vec!["gpt-4.1".to_string()]),
                ip_rules: Some(vec!["203.0.113.10".to_string()]),
                rate_limit: 100,
                concurrent_limit: Some(5),
                force_capabilities: Some(json!({"cache": true})),
                feature_settings: Some(json!({"chat_pii_redaction": {"enabled": true}})),
                is_active: true,
                expires_at_unix_secs: Some(2_000_000_000),
                auto_delete_on_expiry: false,
                total_requests: 3,
                total_tokens: 42,
                total_cost_usd: 0.25,
            })
            .await
            .expect("user key should create")
            .expect("user key should reload");
        assert_eq!(user_key.allowed_models, Some(vec!["gpt-4.1".to_string()]));
        assert_eq!(
            user_key.feature_settings,
            Some(json!({"chat_pii_redaction": {"enabled": true}}))
        );
        assert_eq!(user_key.total_tokens, 42);
        assert_eq!(
            user_key.feature_settings,
            Some(json!({"chat_pii_redaction": {"enabled": true}}))
        );

        let wrong_owner_update = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                key_encrypted: None,
                key_encrypted_present: false,
                name_present: true,
                rate_limit_present: false,
                concurrent_limit_present: false,
                user_id: "user-other".to_string(),
                api_key_id: "key-created-user".to_string(),
                name: Some("must-not-apply".to_string()),
                rate_limit: None,
                concurrent_limit: None,
                ip_rules: None,
                allowed_providers: Some(Some(vec!["must-not-apply".to_string()])),
                allowed_api_formats: None,
                allowed_models: None,
                feature_settings: None,
            })
            .await
            .expect("wrong owner update should not fail");
        assert_eq!(wrong_owner_update, None);
        let unchanged_after_wrong_owner = repository
            .reload_export_by_id("key-created-user")
            .await
            .expect("user key should reload after rejected update")
            .expect("user key should still exist");
        assert_eq!(
            unchanged_after_wrong_owner.name,
            Some("Created User".to_string())
        );
        assert_eq!(
            unchanged_after_wrong_owner.allowed_providers,
            Some(vec!["openai".to_string()])
        );
        assert!(repository
            .set_user_api_key_allowed_providers(
                "user-other",
                "key-created-user",
                Some(vec!["must-not-apply".to_string()]),
            )
            .await
            .expect("wrong owner provider update should not fail")
            .is_none());
        assert!(repository
            .set_user_api_key_allowed_providers(
                "user-1",
                "key-created-user",
                Some(vec!["openai".to_string()]),
            )
            .await
            .expect("idempotent provider update should succeed")
            .is_some());

        let updated_user_key = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                key_encrypted: Some("enc-user-rotated".to_string()),
                key_encrypted_present: true,
                name: Some("Updated User".to_string()),
                name_present: true,
                rate_limit: Some(150),
                rate_limit_present: true,
                concurrent_limit: Some(6),
                concurrent_limit_present: true,
                ip_rules: Some(Some(vec!["10.0.0.0/24".to_string()])),
                allowed_providers: None,
                allowed_api_formats: None,
                allowed_models: None,
                feature_settings: Some(Some(json!({"compact": true}))),
            })
            .await
            .expect("user key should update")
            .expect("user key should reload");
        assert_eq!(updated_user_key.name, Some("Updated User".to_string()));
        assert_eq!(
            updated_user_key.key_encrypted.as_deref(),
            Some("enc-user-rotated")
        );
        assert_eq!(updated_user_key.concurrent_limit, Some(6));
        assert_eq!(
            updated_user_key.feature_settings,
            Some(json!({"compact": true}))
        );

        // Rollback can explicitly restore nullable values, while a present zero remains a
        // meaningful rate limit rather than being treated as an omitted field.
        let cleared_user_key = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                key_encrypted: None,
                key_encrypted_present: false,
                name_present: false,
                rate_limit_present: false,
                concurrent_limit_present: false,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                name: None,
                rate_limit: None,
                concurrent_limit: None,
                ip_rules: None,
                allowed_providers: Some(None),
                allowed_api_formats: Some(None),
                allowed_models: Some(None),
                feature_settings: Some(None),
            })
            .await
            .expect("user key restrictions should clear")
            .expect("user key should reload after clear");
        assert_eq!(cleared_user_key.allowed_providers, None);
        assert_eq!(cleared_user_key.feature_settings, None);

        let denied_user_key = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                key_encrypted: None,
                key_encrypted_present: false,
                name_present: false,
                rate_limit_present: false,
                concurrent_limit_present: false,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                name: None,
                rate_limit: None,
                concurrent_limit: None,
                ip_rules: None,
                allowed_providers: Some(Some(Vec::new())),
                allowed_api_formats: Some(Some(Vec::new())),
                allowed_models: Some(Some(Vec::new())),
                feature_settings: Some(Some(json!({"chat_pii_redaction": {"enabled": false}}))),
            })
            .await
            .expect("user key deny-all should update")
            .expect("user key should reload after deny-all");
        assert_eq!(denied_user_key.allowed_providers, Some(Vec::new()));
        assert_eq!(denied_user_key.allowed_api_formats, Some(Vec::new()));
        assert_eq!(denied_user_key.allowed_models, Some(Vec::new()));
        assert_eq!(
            denied_user_key.feature_settings,
            Some(json!({"chat_pii_redaction": {"enabled": false}}))
        );

        let restricted_user_key = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                key_encrypted: None,
                key_encrypted_present: false,
                name_present: false,
                rate_limit_present: false,
                concurrent_limit_present: false,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                name: None,
                rate_limit: None,
                concurrent_limit: None,
                ip_rules: None,
                allowed_providers: Some(Some(vec!["anthropic".to_string()])),
                allowed_api_formats: Some(Some(vec!["claude:messages".to_string()])),
                allowed_models: Some(Some(vec!["claude-sonnet-4".to_string()])),
                feature_settings: None,
            })
            .await
            .expect("user key restriction should update")
            .expect("user key should reload after restriction");
        assert_eq!(
            restricted_user_key.allowed_providers,
            Some(vec!["anthropic".to_string()])
        );
        assert_eq!(
            restricted_user_key.allowed_api_formats,
            Some(vec!["claude:messages".to_string()])
        );
        assert_eq!(
            restricted_user_key.allowed_models,
            Some(vec!["claude-sonnet-4".to_string()])
        );
        assert_eq!(
            restricted_user_key.feature_settings,
            Some(json!({"chat_pii_redaction": {"enabled": false}}))
        );
        let cleared_user_key = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                allowed_providers: None,
                allowed_api_formats: None,
                allowed_models: None,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                key_encrypted: None,
                key_encrypted_present: true,
                name: None,
                name_present: true,
                rate_limit: None,
                rate_limit_present: true,
                concurrent_limit: None,
                concurrent_limit_present: true,
                ip_rules: None,
                feature_settings: None,
            })
            .await
            .expect("nullable values should clear")
            .expect("user key should remain");
        assert!(cleared_user_key.key_encrypted.is_none());
        assert!(cleared_user_key.name.is_none());
        assert!(cleared_user_key.rate_limit.is_none());
        assert!(cleared_user_key.concurrent_limit.is_none());

        let zero_rate_limit = repository
            .update_user_api_key_basic(UpdateUserApiKeyBasicRecord {
                allowed_providers: None,
                allowed_api_formats: None,
                allowed_models: None,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                key_encrypted: None,
                key_encrypted_present: false,
                name: None,
                name_present: false,
                rate_limit: Some(0),
                rate_limit_present: true,
                concurrent_limit: None,
                concurrent_limit_present: false,
                ip_rules: None,
                feature_settings: None,
            })
            .await
            .expect("zero rate limit should persist")
            .expect("user key should remain");
        assert_eq!(zero_rate_limit.rate_limit, Some(0));

        assert!(repository
            .set_user_api_key_locked("user-1", "key-created-user", true)
            .await
            .expect("lock should update"));
        let snapshot = repository
            .find_api_key_snapshot(AuthApiKeyLookupKey::ApiKeyId("key-created-user"))
            .await
            .expect("snapshot should load")
            .expect("snapshot should exist");
        assert!(snapshot.api_key_is_locked);

        assert!(repository
            .update_user_api_key_basic_if_unlocked(UpdateUserApiKeyBasicRecord {
                allowed_providers: None,
                allowed_api_formats: None,
                allowed_models: None,
                user_id: "user-1".to_string(),
                api_key_id: "key-created-user".to_string(),
                key_encrypted: None,
                key_encrypted_present: false,
                name: Some("must-not-change".to_string()),
                name_present: true,
                rate_limit: None,
                rate_limit_present: false,
                concurrent_limit: None,
                concurrent_limit_present: false,
                ip_rules: None,
                feature_settings: Some(Some(json!({"must_not_change": true}))),
            })
            .await
            .expect("locked basic update should resolve")
            .is_none());
        assert!(repository
            .set_user_api_key_active_if_unlocked("user-1", "key-created-user", false)
            .await
            .expect("locked status update should resolve")
            .is_none());
        assert!(repository
            .set_user_api_key_allowed_providers_if_unlocked(
                "user-1",
                "key-created-user",
                Some(vec!["must-not-change".to_string()]),
            )
            .await
            .expect("locked provider update should resolve")
            .is_none());
        assert!(repository
            .set_user_api_key_force_capabilities_if_unlocked(
                "user-1",
                "key-created-user",
                Some(json!({"must_not_change": true})),
            )
            .await
            .expect("locked capability update should resolve")
            .is_none());
        assert!(repository
            .set_user_api_key_feature_settings_if_unlocked(
                "user-1",
                "key-created-user",
                Some(json!({"must_not_change": true})),
            )
            .await
            .expect("locked feature update should resolve")
            .is_none());
        assert!(!repository
            .delete_user_api_key_if_unlocked("user-1", "key-created-user")
            .await
            .expect("locked delete should resolve"));

        let unchanged = repository
            .reload_export_by_id("key-created-user")
            .await
            .expect("locked key should reload")
            .expect("locked key should still exist");
        assert_ne!(unchanged.name.as_deref(), Some("must-not-change"));
        assert!(unchanged.is_active);
        assert_ne!(
            unchanged.allowed_providers,
            Some(vec!["must-not-change".to_string()])
        );
        assert_ne!(
            unchanged.force_capabilities,
            Some(json!({"must_not_change": true}))
        );
        assert_eq!(unchanged.feature_settings, zero_rate_limit.feature_settings);

        let active_user_key = repository
            .set_user_api_key_active("user-1", "key-created-user", false)
            .await
            .expect("active flag should update")
            .expect("user key should reload");
        assert!(!active_user_key.is_active);

        assert!(repository
            .set_user_api_key_active("wrong-owner", "key-created-user", true)
            .await
            .expect("wrong-owner status update should resolve")
            .is_none());

        let provider_updated = repository
            .set_user_api_key_allowed_providers(
                "user-1",
                "key-created-user",
                Some(vec!["anthropic".to_string()]),
            )
            .await
            .expect("allowed providers should update")
            .expect("user key should reload");
        assert_eq!(
            provider_updated.allowed_providers,
            Some(vec!["anthropic".to_string()])
        );

        let capabilities_updated = repository
            .set_user_api_key_force_capabilities(
                "user-1",
                "key-created-user",
                Some(json!({"vision": true})),
            )
            .await
            .expect("force capabilities should update")
            .expect("user key should reload");
        assert_eq!(
            capabilities_updated.force_capabilities,
            Some(json!({"vision": true}))
        );

        assert!(repository
            .touch_last_used_at("key-created-user")
            .await
            .expect("touch should update"));
        assert!(repository
            .reload_export_by_id("key-created-user")
            .await
            .expect("touched key should reload")
            .expect("touched key should exist")
            .last_used_at_unix_secs
            .is_some());

        let standalone = repository
            .create_standalone_api_key(CreateStandaloneApiKeyRecord {
                user_id: "user-1".to_string(),
                api_key_id: "key-created-standalone".to_string(),
                key_hash: "hash-created-standalone".to_string(),
                key_encrypted: Some("enc-standalone".to_string()),
                name: Some("Created Standalone".to_string()),
                allowed_providers: Some(vec!["openai".to_string()]),
                allowed_api_formats: None,
                allowed_models: None,
                ip_rules: None,
                rate_limit: None,
                concurrent_limit: Some(2),
                force_capabilities: None,
                is_active: true,
                expires_at_unix_secs: None,
                auto_delete_on_expiry: false,
                total_requests: 0,
                total_tokens: 0,
                total_cost_usd: 0.0,
            })
            .await
            .expect("standalone key should create")
            .expect("standalone key should reload");
        assert!(standalone.is_standalone);

        let standalone = repository
            .update_standalone_api_key_basic(UpdateStandaloneApiKeyBasicRecord {
                api_key_id: "key-created-standalone".to_string(),
                key_encrypted: Some("enc-standalone-rotated".to_string()),
                key_encrypted_present: true,
                name: Some("Updated Standalone".to_string()),
                name_present: true,
                force_capabilities: None,
                rate_limit_present: true,
                rate_limit: Some(20),
                concurrent_limit_present: true,
                concurrent_limit: None,
                allowed_providers: Some(None),
                allowed_api_formats: Some(Some(vec!["openai:responses".to_string()])),
                allowed_models: Some(Some(vec!["gpt-4.1-mini".to_string()])),
                ip_rules: None,
                expires_at_present: true,
                expires_at_unix_secs: Some(2_100_000_000),
                auto_delete_on_expiry_present: true,
                auto_delete_on_expiry: true,
            })
            .await
            .expect("standalone key should update")
            .expect("standalone key should reload");
        assert_eq!(standalone.name, Some("Updated Standalone".to_string()));
        assert_eq!(
            standalone.key_encrypted.as_deref(),
            Some("enc-standalone-rotated")
        );
        assert_eq!(standalone.allowed_providers, None);
        assert_eq!(
            standalone.allowed_api_formats,
            Some(vec!["openai:responses".to_string()])
        );
        assert_eq!(standalone.concurrent_limit, None);
        assert!(standalone.auto_delete_on_expiry);

        let standalone = repository
            .set_standalone_api_key_active("key-created-standalone", false)
            .await
            .expect("standalone active flag should update")
            .expect("standalone key should reload");
        assert!(!standalone.is_active);

        assert!(repository
            .delete_standalone_api_key("key-created-standalone")
            .await
            .expect("standalone key should delete"));
        assert!(repository
            .delete_user_api_key("user-1", "key-created-user")
            .await
            .expect("user key should delete"));
    }

    #[tokio::test]
    async fn sqlite_api_key_delete_is_owner_scoped_and_preserves_anonymized_facts() {
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
  id, username, role, auth_source, is_active, is_deleted, created_at, updated_at
) VALUES
  ('key-owner', 'key-owner', 'user', 'local', 1, 0, 1, 1),
  ('other-owner', 'other-owner', 'user', 'local', 1, 0, 1, 1);

INSERT INTO api_keys (
  id, user_id, key_hash, name, is_standalone, created_at, updated_at
) VALUES (
  'key-to-delete', 'key-owner', 'key-to-delete-hash', 'private key name', 0, 1, 1
);

INSERT INTO wallets (
  id, api_key_id, balance, gift_balance, status, created_at, updated_at
) VALUES (
  'key-wallet', 'key-to-delete', 12, 3, 'active', 1, 1
);

INSERT INTO request_candidates (
  id, request_id, user_id, api_key_id, username, api_key_name,
  candidate_index, status, created_at
) VALUES (
  'key-history-row', 'key-candidate-request', 'key-owner', 'key-to-delete',
  'key-owner', 'private key name', 0, 'success', 1
);

INSERT INTO video_tasks (
  id, request_id, user_id, api_key_id, username, api_key_name, created_at, updated_at
) VALUES (
  'key-history-row', 'key-video-request', 'key-owner', 'key-to-delete',
  'key-owner', 'private key name', 1, 1
);

INSERT INTO usage (
  request_id, id, user_id, api_key_id, username, api_key_name
) VALUES (
  'key-usage-request', 'key-history-row', 'key-owner', 'key-to-delete',
  'key-owner', 'private key name'
);

INSERT INTO stats_daily_api_key (
  id, api_key_id, date, api_key_name, created_at, updated_at
) VALUES (
  'key-history-row', 'key-to-delete', 1, 'private key name', 1, 1
);

INSERT INTO audit_logs (
  id, event_type, api_key_id, description, ip_address, user_agent,
  event_metadata, error_message, created_at
) VALUES (
  'key-audit', 'key_event', 'key-to-delete', 'private description',
  '192.0.2.20', 'private agent', '{"private":true}', 'private error', 1
);

INSERT INTO api_key_provider_mappings (
  id, api_key_id, provider_id, created_at, updated_at
) VALUES (
  'key-mapping', 'key-to-delete', 'provider-1', 1, 1
);

INSERT INTO payment_orders (
  id, order_no, wallet_id, amount_usd, payment_method,
  gateway_response, status, created_at
) VALUES (
  'key-order', 'key-order-no', 'key-wallet', 12, 'test',
  '{"customer_email":"key@example.com"}', 'credited', 1
);

INSERT INTO payment_callbacks (
  id, payment_order_id, payment_method, callback_key, order_no,
  payload_hash, signature_valid, status, payload, error_message, created_at
) VALUES (
  'key-callback', 'key-order', 'test', 'key-callback-key', 'key-order-no',
  'key-payload-hash', 1, 'processed',
  '{"customer_email":"key@example.com"}', 'private callback error', 1
);
"#,
        )
        .execute(&pool)
        .await
        .expect("API key deletion fixtures should insert");

        let repository = SqliteAuthApiKeyReadRepository::new(pool.clone());
        assert!(!repository
            .delete_user_api_key("other-owner", "key-to-delete")
            .await
            .expect("wrong-owner delete should resolve"));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM wallets WHERE id = 'key-wallet'",)
                .fetch_one(&pool)
                .await
                .expect("wallet status should load"),
            "active"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT api_key_name FROM request_candidates WHERE id = 'key-history-row'",
            )
            .fetch_one(&pool)
            .await
            .expect("candidate key name should load"),
            "private key name"
        );

        assert!(repository
            .delete_user_api_key("key-owner", "key-to-delete")
            .await
            .expect("owner-scoped delete should succeed"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM api_keys WHERE id = 'key-to-delete'",
            )
            .fetch_one(&pool)
            .await
            .expect("API key count should load"),
            0
        );

        let wallet = sqlx::query("SELECT api_key_id, status FROM wallets WHERE id = 'key-wallet'")
            .fetch_one(&pool)
            .await
            .expect("wallet fact should remain");
        assert_eq!(
            wallet
                .try_get::<Option<String>, _>("api_key_id")
                .expect("wallet API key id should decode")
                .as_deref(),
            Some("key-to-delete")
        );
        assert_eq!(
            wallet
                .try_get::<String, _>("status")
                .expect("wallet status should decode"),
            "disabled"
        );

        for table in [
            "request_candidates",
            "video_tasks",
            "usage",
            "stats_daily_api_key",
        ] {
            let row = sqlx::query(&format!(
                "SELECT api_key_id, api_key_name FROM {table} WHERE id = 'key-history-row'",
            ))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("{table} fact should remain: {error}"));
            assert_eq!(
                row.try_get::<String, _>("api_key_id")
                    .expect("history API key id should decode"),
                "key-to-delete",
                "{table} API key id must remain stable"
            );
            assert_eq!(
                row.try_get::<Option<String>, _>("api_key_name")
                    .expect("history API key name should decode"),
                None,
                "{table} API key name must be removed"
            );
        }

        let audit = sqlx::query(
            "SELECT api_key_id, description, ip_address, user_agent, event_metadata, error_message FROM audit_logs WHERE id = 'key-audit'",
        )
        .fetch_one(&pool)
        .await
        .expect("audit fact should remain");
        assert_eq!(
            audit
                .try_get::<Option<String>, _>("api_key_id")
                .expect("audit API key id should decode")
                .as_deref(),
            Some("key-to-delete")
        );
        assert_eq!(
            audit
                .try_get::<String, _>("description")
                .expect("audit description should decode"),
            "deleted API key event"
        );
        for column in [
            "ip_address",
            "user_agent",
            "event_metadata",
            "error_message",
        ] {
            assert_eq!(
                audit
                    .try_get::<Option<String>, _>(column)
                    .unwrap_or_else(|error| panic!("audit {column} should decode: {error}")),
                None,
                "audit {column} must be removed"
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM api_key_provider_mappings WHERE id = 'key-mapping'",
            )
            .fetch_one(&pool)
            .await
            .expect("mapping count should load"),
            0
        );

        let order_gateway_response: Option<String> = sqlx::query_scalar(
            "SELECT gateway_response FROM payment_orders WHERE id = 'key-order'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment order should remain");
        assert_eq!(order_gateway_response, None);
        let callback = sqlx::query(
            "SELECT payload, error_message FROM payment_callbacks WHERE id = 'key-callback'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment callback should remain");
        assert_eq!(
            callback
                .try_get::<Option<String>, _>("payload")
                .expect("callback payload should decode"),
            None
        );
        assert_eq!(
            callback
                .try_get::<Option<String>, _>("error_message")
                .expect("callback error should decode"),
            None
        );
    }

    async fn seed_auth_api_key_rows(pool: &sqlx::SqlitePool) {
        seed_auth_user(pool).await;
        sqlx::query(
            r#"
INSERT INTO api_keys (
  id, user_id, key_hash, key_encrypted, name, allowed_providers,
  allowed_api_formats, allowed_models, rate_limit, concurrent_limit,
  force_capabilities, is_active, expires_at, auto_delete_on_expiry,
  total_requests, total_tokens, total_cost_usd, last_used_at, created_at,
  updated_at, is_standalone
) VALUES
  (
    'key-user', 'user-1', 'hash-user', 'enc-user', 'User Key', '["openai"]',
    '["openai:chat"]', '["gpt-4.1"]', 30, 2, '{"cache":true}', 1, 200, 0,
    123, 456, 1.25, 10, 1, 2, 0
  ),
  (
    'key-standalone', 'user-1', 'hash-standalone', 'enc-standalone', 'Standalone', NULL,
    NULL, NULL, NULL, NULL, NULL, 1, NULL, 0, 0, 0, 0, NULL, 3, 4, 1
  )
"#,
        )
        .execute(pool)
        .await
        .expect("api keys should seed");
    }

    async fn seed_auth_user(pool: &sqlx::SqlitePool) {
        sqlx::query(
            r#"
INSERT INTO users (
  id, email, email_verified, username, password_hash, role, auth_source,
  allowed_providers, allowed_api_formats, allowed_models, rate_limit,
  is_active, is_deleted, created_at, updated_at
) VALUES (
  'user-1', 'user@example.com', 1, 'alice', NULL, 'user', 'local',
  '["openai"]', '["openai:chat"]', '["gpt-4.1"]', 60, 1, 0, 1, 1
)
"#,
        )
        .execute(pool)
        .await
        .expect("user should seed");
    }
}
