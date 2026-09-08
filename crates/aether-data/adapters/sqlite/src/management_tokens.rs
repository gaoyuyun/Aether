use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite};

use aether_data_contracts::repository::management_tokens::{
    ActivateManagementTokenIfMatches, CreateManagementTokenRecord, ManagementTokenListQuery,
    ManagementTokenReadRepository, ManagementTokenWriteRepository, RegenerateManagementTokenSecret,
    StoredManagementToken, StoredManagementTokenListPage, StoredManagementTokenUserSummary,
    StoredManagementTokenWithUser, UpdateManagementTokenRecord,
};
use aether_data_contracts::DataLayerError;
use aether_data_query::{push_eq, push_limit, push_limit_offset, push_optional_eq, WhereClause};

use crate::error::SqlResultExt;
use crate::SqlitePool;

#[derive(Debug, Clone)]
pub struct SqliteManagementTokenRepository {
    pool: SqlitePool,
}

impl SqliteManagementTokenRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn get_token(
        &self,
        token_id: &str,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(TOKEN_COLUMNS);
        let mut where_clause = WhereClause::new();
        push_eq(&mut builder, &mut where_clause, "id", token_id.to_string());
        push_limit(&mut builder, 1);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_token_row).transpose()
    }

    async fn get_token_scoped(
        &self,
        token_id: &str,
        expected_user_id: Option<&str>,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(TOKEN_COLUMNS);
        let mut where_clause = WhereClause::new();
        push_eq(&mut builder, &mut where_clause, "id", token_id.to_string());
        push_optional_eq(
            &mut builder,
            &mut where_clause,
            "user_id",
            expected_user_id.map(ToOwned::to_owned),
        );
        push_limit(&mut builder, 1);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_token_row).transpose()
    }

    async fn update_management_token_scoped(
        &self,
        record: &UpdateManagementTokenRecord,
        expected_user_id: Option<&str>,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        record.validate()?;
        let allowed_ips = json_to_string(record.allowed_ips.as_ref())?;
        let permissions = json_to_string(record.permissions.as_ref())?;
        let now = now_unix_secs();

        sqlx::query(UPDATE_MANAGEMENT_TOKEN_SQL)
            .bind(record.name.as_deref())
            .bind(record.clear_description)
            .bind(record.description.as_deref())
            .bind(record.clear_allowed_ips)
            .bind(allowed_ips)
            .bind(permissions)
            .bind(record.clear_expires_at)
            .bind(
                record
                    .expires_at_unix_secs
                    .and_then(|value| i64::try_from(value).ok()),
            )
            .bind(record.is_active)
            .bind(now as i64)
            .bind(&record.token_id)
            .bind(expected_user_id)
            .bind(expected_user_id)
            .execute(&self.pool)
            .await
            .map_err(|err| map_sqlite_write_error(err, record.name.as_deref()))?;
        self.get_token_scoped(&record.token_id, expected_user_id)
            .await
    }

    async fn delete_management_token_scoped(
        &self,
        token_id: &str,
        expected_user_id: Option<&str>,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            "DELETE FROM management_tokens WHERE id = ? AND (? IS NULL OR user_id = ?)",
        )
        .bind(token_id)
        .bind(expected_user_id)
        .bind(expected_user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_management_token_active_scoped(
        &self,
        token_id: &str,
        expected_user_id: Option<&str>,
        is_active: bool,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        let result = sqlx::query(
            "UPDATE management_tokens SET is_active = ?, updated_at = ? WHERE id = ? AND (? IS NULL OR user_id = ?)",
        )
        .bind(is_active)
        .bind(now_unix_secs() as i64)
        .bind(token_id)
        .bind(expected_user_id)
        .bind(expected_user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_token_scoped(token_id, expected_user_id).await
    }

    async fn regenerate_management_token_secret_scoped(
        &self,
        mutation: &RegenerateManagementTokenSecret,
        expected_user_id: Option<&str>,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        mutation.validate()?;
        let result = sqlx::query(
            r#"
UPDATE management_tokens
SET token_hash = ?, token_prefix = ?, updated_at = ?
WHERE id = ? AND (? IS NULL OR user_id = ?)
"#,
        )
        .bind(&mutation.token_hash)
        .bind(mutation.token_prefix.as_deref())
        .bind(now_unix_secs() as i64)
        .bind(&mutation.token_id)
        .bind(expected_user_id)
        .bind(expected_user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_token_scoped(&mutation.token_id, expected_user_id)
            .await
    }
}

const UPDATE_MANAGEMENT_TOKEN_SQL: &str = r#"
UPDATE management_tokens
SET name = COALESCE(?, name),
    description = CASE WHEN ? THEN NULL ELSE COALESCE(?, description) END,
    allowed_ips = CASE WHEN ? THEN NULL ELSE COALESCE(?, allowed_ips) END,
    permissions = COALESCE(?, permissions),
    expires_at = CASE WHEN ? THEN NULL ELSE COALESCE(?, expires_at) END,
    is_active = COALESCE(?, is_active),
    updated_at = ?
WHERE id = ? AND (? IS NULL OR user_id = ?)
"#;

const TOKEN_COLUMNS: &str = r#"
SELECT
  id,
  user_id,
  name,
  description,
  token_prefix,
  allowed_ips,
  permissions,
  expires_at AS expires_at_unix_secs,
  last_used_at AS last_used_at_unix_secs,
  last_used_ip,
  COALESCE(usage_count, 0) AS usage_count,
  is_active,
  created_at AS created_at_unix_ms,
  updated_at AS updated_at_unix_secs
FROM management_tokens
"#;

const LOCK_ELIGIBLE_MANAGEMENT_TOKEN_ADMIN_SQL: &str = r#"
SELECT id
FROM users
WHERE id = ?
  AND is_active = 1
  AND is_deleted = 0
  AND LOWER(role) = 'admin'
  AND security_version = ?
"#;

const LOCK_MANAGEMENT_TOKEN_ACTIVATION_SNAPSHOT_SQL: &str = r#"
SELECT
  id,
  user_id,
  token_hash,
  name,
  description,
  token_prefix,
  allowed_ips,
  permissions,
  expires_at AS expires_at_unix_secs,
  last_used_at AS last_used_at_unix_secs,
  last_used_ip,
  COALESCE(usage_count, 0) AS usage_count,
  is_active,
  created_at AS created_at_unix_ms,
  updated_at AS updated_at_unix_secs
FROM management_tokens
WHERE id = ?
"#;

const TOKEN_WITH_USER_COLUMNS: &str = r#"
SELECT
  mt.id,
  mt.user_id,
  mt.name,
  mt.description,
  mt.token_prefix,
  mt.allowed_ips,
  mt.permissions,
  mt.expires_at AS expires_at_unix_secs,
  mt.last_used_at AS last_used_at_unix_secs,
  mt.last_used_ip,
  COALESCE(mt.usage_count, 0) AS usage_count,
  mt.is_active,
  mt.created_at AS created_at_unix_ms,
  mt.updated_at AS updated_at_unix_secs,
  u.id AS user_row_id,
  u.email AS user_email,
  u.username AS user_username,
  u.role AS user_role
FROM management_tokens mt
JOIN users u ON u.id = mt.user_id
"#;

#[async_trait]
impl ManagementTokenReadRepository for SqliteManagementTokenRepository {
    async fn list_management_tokens(
        &self,
        query: &ManagementTokenListQuery,
    ) -> Result<StoredManagementTokenListPage, DataLayerError> {
        let mut count_builder =
            QueryBuilder::<Sqlite>::new("SELECT COUNT(mt.id) AS total FROM management_tokens mt");
        let mut count_where = WhereClause::new();
        apply_management_token_filters(&mut count_builder, &mut count_where, query);
        let total = count_builder
            .build_query_scalar::<i64>()
            .fetch_one(&self.pool)
            .await
            .map_sql_err()?;

        let mut list_builder = QueryBuilder::<Sqlite>::new(TOKEN_WITH_USER_COLUMNS);
        let mut list_where = WhereClause::new();
        apply_management_token_filters(&mut list_builder, &mut list_where, query);
        list_builder.push(" ORDER BY mt.created_at DESC, mt.id DESC");
        push_limit_offset(
            &mut list_builder,
            i64::try_from(query.limit).unwrap_or(i64::MAX),
            i64::try_from(query.offset).unwrap_or(i64::MAX),
        );
        let rows = list_builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;

        Ok(StoredManagementTokenListPage {
            items: rows
                .iter()
                .map(map_token_with_user_row)
                .collect::<Result<Vec<_>, _>>()?,
            total: usize::try_from(total.max(0)).unwrap_or(usize::MAX),
        })
    }

    async fn get_management_token_with_user(
        &self,
        token_id: &str,
    ) -> Result<Option<StoredManagementTokenWithUser>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(TOKEN_WITH_USER_COLUMNS);
        let mut where_clause = WhereClause::new();
        push_eq(
            &mut builder,
            &mut where_clause,
            "mt.id",
            token_id.to_string(),
        );
        push_limit(&mut builder, 1);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_token_with_user_row).transpose()
    }

    async fn get_management_token_with_user_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<StoredManagementTokenWithUser>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(TOKEN_WITH_USER_COLUMNS);
        let mut where_clause = WhereClause::new();
        push_eq(
            &mut builder,
            &mut where_clause,
            "mt.token_hash",
            token_hash.to_string(),
        );
        push_limit(&mut builder, 1);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_token_with_user_row).transpose()
    }
}

fn apply_management_token_filters(
    builder: &mut QueryBuilder<'_, Sqlite>,
    where_clause: &mut WhereClause,
    query: &ManagementTokenListQuery,
) {
    push_optional_eq(builder, where_clause, "mt.user_id", query.user_id.clone());
    push_optional_eq(builder, where_clause, "mt.is_active", query.is_active);
}

#[async_trait]
impl ManagementTokenWriteRepository for SqliteManagementTokenRepository {
    async fn create_management_token(
        &self,
        record: &CreateManagementTokenRecord,
    ) -> Result<StoredManagementToken, DataLayerError> {
        record.validate()?;
        let now = now_unix_secs();
        sqlx::query(
            r#"
INSERT INTO management_tokens (
  id, user_id, token_hash, token_prefix, name, description, allowed_ips,
  permissions, expires_at, is_active, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&record.id)
        .bind(&record.user_id)
        .bind(&record.token_hash)
        .bind(record.token_prefix.as_deref())
        .bind(&record.name)
        .bind(record.description.as_deref())
        .bind(json_to_string(record.allowed_ips.as_ref())?)
        .bind(json_to_string(record.permissions.as_ref())?)
        .bind(
            record
                .expires_at_unix_secs
                .and_then(|value| i64::try_from(value).ok()),
        )
        .bind(record.is_active)
        .bind(now as i64)
        .bind(now as i64)
        .execute(&self.pool)
        .await
        .map_err(|err| map_sqlite_write_error(err, Some(record.name.as_str())))?;

        self.get_token(&record.id).await?.ok_or_else(|| {
            DataLayerError::UnexpectedValue("created management token missing".to_string())
        })
    }

    async fn update_management_token(
        &self,
        record: &UpdateManagementTokenRecord,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.update_management_token_scoped(record, None).await
    }

    async fn update_management_token_for_user(
        &self,
        record: &UpdateManagementTokenRecord,
        user_id: &str,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.update_management_token_scoped(record, Some(user_id))
            .await
    }

    async fn delete_management_token(&self, token_id: &str) -> Result<bool, DataLayerError> {
        self.delete_management_token_scoped(token_id, None).await
    }

    async fn delete_management_token_for_user(
        &self,
        token_id: &str,
        user_id: &str,
    ) -> Result<bool, DataLayerError> {
        self.delete_management_token_scoped(token_id, Some(user_id))
            .await
    }

    async fn set_management_token_active(
        &self,
        token_id: &str,
        is_active: bool,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.set_management_token_active_scoped(token_id, None, is_active)
            .await
    }

    async fn set_management_token_active_for_user(
        &self,
        token_id: &str,
        user_id: &str,
        is_active: bool,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.set_management_token_active_scoped(token_id, Some(user_id), is_active)
            .await
    }

    async fn activate_management_token_if_matches(
        &self,
        mutation: &ActivateManagementTokenIfMatches,
    ) -> Result<bool, DataLayerError> {
        mutation.validate()?;
        // BEGIN IMMEDIATE prevents a concurrent user downgrade or token mutation between the
        // canonical pre-state reads and the activation write.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let eligible_user =
            sqlx::query_scalar::<_, String>(LOCK_ELIGIBLE_MANAGEMENT_TOKEN_ADMIN_SQL)
                .bind(&mutation.expected_token.user_id)
                .bind(mutation.expected_user_security_version)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if eligible_user.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        let locked = sqlx::query(LOCK_MANAGEMENT_TOKEN_ACTIVATION_SNAPSHOT_SQL)
            .bind(&mutation.expected_token.id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let snapshot_matches = match locked.as_ref() {
            Some(row) => {
                let token_hash: String = row.try_get("token_hash").map_sql_err()?;
                let token = map_token_row(row)?;
                mutation.matches_locked_token_snapshot(&token, &token_hash)
            }
            None => false,
        };
        if !snapshot_matches {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }

        let result = sqlx::query(
            r#"
UPDATE management_tokens
SET is_active = 1, updated_at = ?
WHERE id = ?
  AND token_hash = ?
  AND is_active = 0
  AND (expires_at IS NULL OR expires_at > ?)
"#,
        )
        .bind(now_unix_secs() as i64)
        .bind(&mutation.expected_token.id)
        .bind(&mutation.token_hash)
        .bind(i64::try_from(mutation.now_unix_secs).unwrap_or(i64::MAX))
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

    async fn delete_inactive_management_token_if_matches(
        &self,
        mutation: &ActivateManagementTokenIfMatches,
    ) -> Result<bool, DataLayerError> {
        mutation.validate()?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let locked = sqlx::query(LOCK_MANAGEMENT_TOKEN_ACTIVATION_SNAPSHOT_SQL)
            .bind(&mutation.expected_token.id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let snapshot_matches = match locked.as_ref() {
            Some(row) => {
                let token_hash: String = row.try_get("token_hash").map_sql_err()?;
                let token = map_token_row(row)?;
                mutation.matches_locked_token_snapshot(&token, &token_hash)
            }
            None => false,
        };
        if !snapshot_matches {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let result = sqlx::query(
            "DELETE FROM management_tokens WHERE id = ? AND token_hash = ? AND is_active = 0",
        )
        .bind(&mutation.expected_token.id)
        .bind(&mutation.token_hash)
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

    async fn regenerate_management_token_secret(
        &self,
        mutation: &RegenerateManagementTokenSecret,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.regenerate_management_token_secret_scoped(mutation, None)
            .await
    }

    async fn regenerate_management_token_secret_for_user(
        &self,
        mutation: &RegenerateManagementTokenSecret,
        user_id: &str,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        self.regenerate_management_token_secret_scoped(mutation, Some(user_id))
            .await
    }

    async fn record_management_token_usage(
        &self,
        token_id: &str,
        last_used_ip: Option<&str>,
    ) -> Result<Option<StoredManagementToken>, DataLayerError> {
        let now = now_unix_secs();
        let result = sqlx::query(
            r#"
UPDATE management_tokens
SET last_used_at = ?,
    last_used_ip = ?,
    usage_count = COALESCE(usage_count, 0) + 1,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(now as i64)
        .bind(last_used_ip)
        .bind(now as i64)
        .bind(token_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_token(token_id).await
    }
}

fn now_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn non_negative_u64(value: i64, field_name: &str) -> Result<u64, DataLayerError> {
    u64::try_from(value).map_err(|_| {
        DataLayerError::UnexpectedValue(format!(
            "management_tokens.{field_name} must not be negative"
        ))
    })
}

fn optional_unix_secs(value: Option<i64>, field_name: &str) -> Result<Option<u64>, DataLayerError> {
    value
        .map(|value| non_negative_u64(value, field_name))
        .transpose()
}

fn json_to_string(value: Option<&serde_json::Value>) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "invalid management token JSON field: {err}"
                ))
            })
        })
        .transpose()
}

fn json_from_string(value: Option<String>) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "invalid management token JSON field: {err}"
                ))
            })
        })
        .transpose()
}

fn map_sqlite_write_error(err: sqlx::Error, requested_name: Option<&str>) -> DataLayerError {
    let message = err.to_string();
    if message.contains("management_tokens.user_id, management_tokens.name") {
        return DataLayerError::InvalidInput(
            requested_name
                .map(|name| format!("已存在名为 '{}' 的 Token", name))
                .unwrap_or_else(|| "Management Token 名称已存在".to_string()),
        );
    }
    DataLayerError::sql(err)
}

fn map_token_row(row: &SqliteRow) -> Result<StoredManagementToken, DataLayerError> {
    Ok(StoredManagementToken::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("user_id").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
    )?
    .with_display_fields(
        row.try_get("description").map_sql_err()?,
        row.try_get("token_prefix").map_sql_err()?,
        json_from_string(row.try_get("allowed_ips").map_sql_err()?)?,
    )
    .with_permissions(json_from_string(row.try_get("permissions").map_sql_err()?)?)
    .with_runtime_fields(
        optional_unix_secs(
            row.try_get("expires_at_unix_secs").map_sql_err()?,
            "expires_at",
        )?,
        optional_unix_secs(
            row.try_get("last_used_at_unix_secs").map_sql_err()?,
            "last_used_at",
        )?,
        row.try_get("last_used_ip").map_sql_err()?,
        non_negative_u64(
            row.try_get::<i64, _>("usage_count").map_sql_err()?,
            "usage_count",
        )?,
        row.try_get("is_active").map_sql_err()?,
    )
    .with_timestamps(
        optional_unix_secs(
            row.try_get("created_at_unix_ms").map_sql_err()?,
            "created_at",
        )?,
        optional_unix_secs(
            row.try_get("updated_at_unix_secs").map_sql_err()?,
            "updated_at",
        )?,
    ))
}

fn map_user_summary_row(
    row: &SqliteRow,
) -> Result<StoredManagementTokenUserSummary, DataLayerError> {
    StoredManagementTokenUserSummary::new(
        row.try_get("user_row_id").map_sql_err()?,
        row.try_get("user_email").map_sql_err()?,
        row.try_get("user_username").map_sql_err()?,
        row.try_get("user_role").map_sql_err()?,
    )
}

fn map_token_with_user_row(
    row: &SqliteRow,
) -> Result<StoredManagementTokenWithUser, DataLayerError> {
    Ok(StoredManagementTokenWithUser::new(
        map_token_row(row)?,
        map_user_summary_row(row)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        non_negative_u64, optional_unix_secs, SqliteManagementTokenRepository,
        UPDATE_MANAGEMENT_TOKEN_SQL,
    };
    use crate::run_migrations;
    use aether_data_contracts::repository::management_tokens::{
        ActivateManagementTokenIfMatches, CreateManagementTokenRecord, ManagementTokenListQuery,
        ManagementTokenReadRepository, ManagementTokenWriteRepository,
        RegenerateManagementTokenSecret, StoredManagementTokenUserSummary,
        UpdateManagementTokenRecord,
    };

    #[test]
    fn sqlite_management_token_mapping_rejects_negative_integer_state() {
        assert!(optional_unix_secs(Some(-1), "expires_at").is_err());
        assert_eq!(
            optional_unix_secs(None, "expires_at").expect("SQL NULL should remain optional"),
            None
        );
        assert!(non_negative_u64(-1, "usage_count").is_err());
    }

    #[test]
    fn sqlite_management_token_updates_patch_only_explicit_fields() {
        for clause in [
            "name = COALESCE(?, name)",
            "allowed_ips = CASE WHEN ? THEN NULL ELSE COALESCE(?, allowed_ips) END",
            "permissions = COALESCE(?, permissions)",
            "expires_at = CASE WHEN ? THEN NULL ELSE COALESCE(?, expires_at) END",
            "is_active = COALESCE(?, is_active)",
        ] {
            assert!(UPDATE_MANAGEMENT_TOKEN_SQL.contains(clause));
        }
    }

    #[tokio::test]
    async fn sqlite_repository_round_trips_management_tokens() {
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
INSERT INTO users (id, email, username, role, is_active, created_at, updated_at)
VALUES ('user-1', 'user-1@example.com', 'user-1', 'admin', 1, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("seed user should insert");

        let repository = SqliteManagementTokenRepository::new(pool);
        let user = StoredManagementTokenUserSummary::new(
            "user-1".to_string(),
            Some("user-1@example.com".to_string()),
            "user-1".to_string(),
            "admin".to_string(),
        )
        .expect("user summary should build");
        let created = repository
            .create_management_token(&CreateManagementTokenRecord {
                id: "token-1".to_string(),
                user_id: "user-1".to_string(),
                user,
                token_hash: "hash-1".to_string(),
                token_prefix: Some("ae_1234".to_string()),
                name: "primary".to_string(),
                description: Some("primary token".to_string()),
                allowed_ips: Some(serde_json::json!(["127.0.0.1"])),
                permissions: Some(serde_json::json!(["admin:usage:read"])),
                expires_at_unix_secs: Some(1_800_000_000),
                is_active: true,
            })
            .await
            .expect("token should create");
        assert_eq!(created.name, "primary");
        assert_eq!(
            created.permissions,
            Some(serde_json::json!(["admin:usage:read"]))
        );

        let page = repository
            .list_management_tokens(&ManagementTokenListQuery {
                user_id: Some("user-1".to_string()),
                is_active: Some(true),
                offset: 0,
                limit: 10,
            })
            .await
            .expect("tokens should list");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].token.id, "token-1");

        let by_hash = repository
            .get_management_token_with_user_by_hash("hash-1")
            .await
            .expect("hash lookup should succeed")
            .expect("token should exist");
        assert_eq!(by_hash.user.username, "user-1");

        let pending_install = repository
            .create_management_token(&CreateManagementTokenRecord {
                id: "token-install".to_string(),
                user_id: "user-1".to_string(),
                user: by_hash.user.clone(),
                token_hash: "hash-install".to_string(),
                token_prefix: Some("ae_install".to_string()),
                name: "pending install".to_string(),
                description: None,
                allowed_ips: Some(serde_json::json!(["127.0.0.1"])),
                permissions: Some(serde_json::json!(["admin:proxy_nodes:write"])),
                expires_at_unix_secs: Some(1_800_000_000),
                is_active: false,
            })
            .await
            .expect("pending install token should create");
        let activation = ActivateManagementTokenIfMatches {
            expected_token: pending_install,
            token_hash: "hash-install".to_string(),
            expected_user_security_version: 0,
            now_unix_secs: 1_700_000_000,
        };
        let mut wrong_secret = activation.clone();
        wrong_secret.token_hash = "hash-other".to_string();
        assert!(!repository
            .activate_management_token_if_matches(&wrong_secret)
            .await
            .expect("wrong-secret activation should execute"));
        let mut stale_snapshot = activation.clone();
        stale_snapshot.expected_token.description = Some("changed snapshot".to_string());
        assert!(!repository
            .activate_management_token_if_matches(&stale_snapshot)
            .await
            .expect("stale-snapshot activation should execute"));
        assert!(repository
            .activate_management_token_if_matches(&activation)
            .await
            .expect("matching activation should execute"));
        assert!(!repository
            .activate_management_token_if_matches(&activation)
            .await
            .expect("already-active token must not activate again"));

        let cross_owner_update = UpdateManagementTokenRecord {
            token_id: "token-1".to_string(),
            name: Some("hijacked".to_string()),
            description: None,
            clear_description: false,
            allowed_ips: None,
            clear_allowed_ips: false,
            permissions: None,
            expires_at_unix_secs: None,
            clear_expires_at: false,
            is_active: None,
        };
        assert!(repository
            .update_management_token_for_user(&cross_owner_update, "user-2")
            .await
            .expect("owner-scoped update should execute")
            .is_none());
        assert!(repository
            .set_management_token_active_for_user("token-1", "user-2", false)
            .await
            .expect("owner-scoped toggle should execute")
            .is_none());
        assert!(repository
            .regenerate_management_token_secret_for_user(
                &RegenerateManagementTokenSecret {
                    token_id: "token-1".to_string(),
                    token_hash: "hash-hijacked".to_string(),
                    token_prefix: Some("ae_hijacked".to_string()),
                },
                "user-2",
            )
            .await
            .expect("owner-scoped regeneration should execute")
            .is_none());
        assert!(!repository
            .delete_management_token_for_user("token-1", "user-2")
            .await
            .expect("owner-scoped delete should execute"));
        assert_eq!(
            repository
                .get_management_token_with_user_by_hash("hash-1")
                .await
                .expect("original hash lookup should succeed")
                .expect("token should remain")
                .token
                .name,
            "primary"
        );
        assert!(repository
            .get_management_token_with_user_by_hash("hash-hijacked")
            .await
            .expect("replacement hash lookup should succeed")
            .is_none());

        let updated = repository
            .update_management_token(&UpdateManagementTokenRecord {
                token_id: "token-1".to_string(),
                name: Some("renamed".to_string()),
                description: None,
                clear_description: true,
                allowed_ips: Some(serde_json::json!(["10.0.0.1"])),
                clear_allowed_ips: false,
                permissions: Some(serde_json::json!(["admin:usage:read", "admin:usage:write"])),
                expires_at_unix_secs: None,
                clear_expires_at: true,
                is_active: Some(false),
            })
            .await
            .expect("update should succeed")
            .expect("token should exist");
        assert_eq!(updated.name, "renamed");
        assert!(!updated.is_active);
        assert_eq!(updated.description, None);
        assert_eq!(
            updated.permissions,
            Some(serde_json::json!(["admin:usage:read", "admin:usage:write"]))
        );
        assert_eq!(updated.expires_at_unix_secs, None);

        let toggled = repository
            .set_management_token_active("token-1", true)
            .await
            .expect("toggle should succeed")
            .expect("token should exist");
        assert!(toggled.is_active);

        let regenerated = repository
            .regenerate_management_token_secret(&RegenerateManagementTokenSecret {
                token_id: "token-1".to_string(),
                token_hash: "hash-2".to_string(),
                token_prefix: Some("ae_5678".to_string()),
            })
            .await
            .expect("regenerate should succeed")
            .expect("token should exist");
        assert_eq!(regenerated.token_prefix.as_deref(), Some("ae_5678"));
        assert!(repository
            .get_management_token_with_user_by_hash("hash-1")
            .await
            .expect("old hash lookup should succeed")
            .is_none());

        let used = repository
            .record_management_token_usage("token-1", Some("127.0.0.1"))
            .await
            .expect("usage should record")
            .expect("token should exist");
        assert_eq!(used.usage_count, 1);
        assert_eq!(used.last_used_ip.as_deref(), Some("127.0.0.1"));

        assert!(repository
            .delete_management_token("token-1")
            .await
            .expect("delete should succeed"));
        assert!(repository
            .delete_management_token("token-install")
            .await
            .expect("install token delete should succeed"));
    }

    #[tokio::test]
    async fn sqlite_install_activation_rejects_changed_admin_identity_snapshot() {
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
  id, email, username, role, is_active, is_deleted, security_version, created_at, updated_at
)
VALUES ('admin-install', 'admin@example.com', 'admin-install', 'admin', 1, 0, 7, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("admin user should insert");

        let repository = SqliteManagementTokenRepository::new(pool.clone());
        let user = StoredManagementTokenUserSummary::new(
            "admin-install".to_string(),
            Some("admin@example.com".to_string()),
            "admin-install".to_string(),
            "admin".to_string(),
        )
        .expect("user summary should build");

        async fn create_pending(
            repository: &SqliteManagementTokenRepository,
            user: &StoredManagementTokenUserSummary,
            id: &str,
        ) -> aether_data_contracts::repository::management_tokens::StoredManagementToken {
            repository
                .create_management_token(&CreateManagementTokenRecord {
                    id: id.to_string(),
                    user_id: user.id.clone(),
                    user: user.clone(),
                    token_hash: format!("hash-{id}"),
                    token_prefix: Some("ae_install".to_string()),
                    name: id.to_string(),
                    description: Some("one-time tunnel install".to_string()),
                    allowed_ips: Some(serde_json::json!(["127.0.0.1"])),
                    permissions: Some(serde_json::json!(["admin:proxy_nodes:write"])),
                    expires_at_unix_secs: Some(1_800_000_000),
                    is_active: false,
                })
                .await
                .expect("pending install token should create")
        }

        fn activation(
            token: aether_data_contracts::repository::management_tokens::StoredManagementToken,
            security_version: i64,
        ) -> ActivateManagementTokenIfMatches {
            ActivateManagementTokenIfMatches {
                token_hash: format!("hash-{}", token.id),
                expected_token: token,
                expected_user_security_version: security_version,
                now_unix_secs: 1_700_000_000,
            }
        }

        let role_activation =
            activation(create_pending(&repository, &user, "install-role").await, 7);
        sqlx::query("UPDATE users SET role = 'user' WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("role downgrade should update");
        assert!(!repository
            .activate_management_token_if_matches(&role_activation)
            .await
            .expect("role-mismatched activation should execute"));
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("admin role should restore");

        let version_activation = activation(
            create_pending(&repository, &user, "install-version").await,
            7,
        );
        sqlx::query("UPDATE users SET security_version = 8 WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("security version should update");
        assert!(!repository
            .activate_management_token_if_matches(&version_activation)
            .await
            .expect("version-mismatched activation should execute"));

        let inactive_activation = activation(
            create_pending(&repository, &user, "install-inactive").await,
            8,
        );
        sqlx::query("UPDATE users SET is_active = 0 WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("administrator should deactivate");
        assert!(!repository
            .activate_management_token_if_matches(&inactive_activation)
            .await
            .expect("inactive-admin activation should execute"));
        sqlx::query("UPDATE users SET is_active = 1 WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("administrator should reactivate");

        let deleted_activation = activation(
            create_pending(&repository, &user, "install-deleted").await,
            8,
        );
        sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("administrator should be soft deleted");
        assert!(!repository
            .activate_management_token_if_matches(&deleted_activation)
            .await
            .expect("deleted-admin activation should execute"));
        sqlx::query("UPDATE users SET is_deleted = 0 WHERE id = 'admin-install'")
            .execute(&pool)
            .await
            .expect("administrator deletion flag should restore");

        let valid_activation =
            activation(create_pending(&repository, &user, "install-valid").await, 8);
        assert!(repository
            .activate_management_token_if_matches(&valid_activation)
            .await
            .expect("matching administrator activation should execute"));
    }
}
