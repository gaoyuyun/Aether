use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, QueryBuilder, Row};

use aether_data_contracts::repository::provider_ops_balance::{
    ProviderOpsBalanceSnapshotReadRepository, ProviderOpsBalanceSnapshotWriteRepository,
    StoredProviderOpsBalanceSnapshot,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const SNAPSHOT_COLUMNS: &str = r#"
SELECT
  provider_id,
  payload_json,
  last_success_at,
  last_attempt_at,
  last_status,
  last_error,
  consecutive_failures,
  next_refresh_at,
  updated_at
FROM provider_ops_balance_snapshots
"#;

const LIST_CHUNK_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub struct MysqlProviderOpsBalanceSnapshotRepository {
    pool: MysqlPool,
}

impl MysqlProviderOpsBalanceSnapshotRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderOpsBalanceSnapshotReadRepository for MysqlProviderOpsBalanceSnapshotRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        let row = sqlx::query(&format!("{SNAPSHOT_COLUMNS} WHERE provider_id = ? LIMIT 1"))
            .bind(provider_id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_row).transpose()
    }

    async fn list_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        let mut snapshots = Vec::with_capacity(provider_ids.len());
        for chunk in provider_ids.chunks(LIST_CHUNK_SIZE) {
            let mut builder = QueryBuilder::<MySql>::new(SNAPSHOT_COLUMNS);
            builder.push(" WHERE provider_id IN (");
            let mut separated = builder.separated(", ");
            for provider_id in chunk {
                separated.push_bind(provider_id.as_str());
            }
            separated.push_unseparated(")");
            let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
            for row in &rows {
                snapshots.push(map_row(row)?);
            }
        }
        Ok(snapshots)
    }
}

#[async_trait]
impl ProviderOpsBalanceSnapshotWriteRepository for MysqlProviderOpsBalanceSnapshotRepository {
    async fn upsert(
        &self,
        snapshot: &StoredProviderOpsBalanceSnapshot,
    ) -> Result<(), DataLayerError> {
        sqlx::query(
            r#"
INSERT INTO provider_ops_balance_snapshots (
  provider_id, payload_json, last_success_at, last_attempt_at, last_status, last_error,
  consecutive_failures, next_refresh_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  payload_json = VALUES(payload_json),
  last_success_at = VALUES(last_success_at),
  last_attempt_at = VALUES(last_attempt_at),
  last_status = VALUES(last_status),
  last_error = VALUES(last_error),
  consecutive_failures = VALUES(consecutive_failures),
  next_refresh_at = VALUES(next_refresh_at),
  updated_at = VALUES(updated_at)
"#,
        )
        .bind(&snapshot.provider_id)
        .bind(json_to_string(&snapshot.payload_json)?)
        .bind(optional_i64(
            snapshot.last_success_at_unix_secs,
            "last_success_at",
        )?)
        .bind(optional_i64(
            snapshot.last_attempt_at_unix_secs,
            "last_attempt_at",
        )?)
        .bind(&snapshot.last_status)
        .bind(&snapshot.last_error)
        .bind(i32::try_from(snapshot.consecutive_failures).unwrap_or(i32::MAX))
        .bind(optional_i64(
            snapshot.next_refresh_at_unix_secs,
            "next_refresh_at",
        )?)
        .bind(i64_from_u64(snapshot.updated_at_unix_secs, "updated_at")?)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(())
    }

    async fn delete_by_provider_id(&self, provider_id: &str) -> Result<bool, DataLayerError> {
        let rows_affected =
            sqlx::query("DELETE FROM provider_ops_balance_snapshots WHERE provider_id = ?")
                .bind(provider_id)
                .execute(&self.pool)
                .await
                .map_sql_err()?
                .rows_affected();
        Ok(rows_affected > 0)
    }
}

fn map_row(row: &MySqlRow) -> Result<StoredProviderOpsBalanceSnapshot, DataLayerError> {
    Ok(StoredProviderOpsBalanceSnapshot {
        provider_id: row.try_get("provider_id").map_sql_err()?,
        payload_json: parse_optional_json(
            row.try_get::<Option<String>, _>("payload_json")
                .map_sql_err()?,
        )?,
        last_success_at_unix_secs: optional_u64(row, "last_success_at")?,
        last_attempt_at_unix_secs: optional_u64(row, "last_attempt_at")?,
        last_status: row
            .try_get::<Option<String>, _>("last_status")
            .map_sql_err()?,
        last_error: row
            .try_get::<Option<String>, _>("last_error")
            .map_sql_err()?,
        consecutive_failures: u32::try_from(
            row.try_get::<i32, _>("consecutive_failures")
                .map_sql_err()?,
        )
        .unwrap_or(0),
        next_refresh_at_unix_secs: optional_u64(row, "next_refresh_at")?,
        updated_at_unix_secs: u64::try_from(row.try_get::<i64, _>("updated_at").map_sql_err()?)
            .unwrap_or(0),
    })
}

fn optional_u64(row: &MySqlRow, field: &str) -> Result<Option<u64>, DataLayerError> {
    Ok(row
        .try_get::<Option<i64>, _>(field)
        .map_sql_err()?
        .and_then(|value| u64::try_from(value).ok()))
}

fn optional_i64(value: Option<u64>, field_name: &str) -> Result<Option<i64>, DataLayerError> {
    value
        .map(|value| i64_from_u64(value, field_name))
        .transpose()
}

fn i64_from_u64(value: u64, field_name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value).map_err(|_| {
        DataLayerError::InvalidInput(format!(
            "provider_ops_balance_snapshots.{field_name} exceeds i64: {value}"
        ))
    })
}

fn json_to_string(value: &Option<serde_json::Value>) -> Result<Option<String>, DataLayerError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "provider_ops_balance_snapshots.payload_json is unserializable: {err}"
                ))
            })
        })
        .transpose()
}

fn parse_optional_json(value: Option<String>) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|raw| {
            serde_json::from_str::<serde_json::Value>(&raw).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "invalid provider_ops_balance_snapshots.payload_json: {err}"
                ))
            })
        })
        .transpose()
}
