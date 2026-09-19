use async_trait::async_trait;
use sqlx::{postgres::PgRow, PgPool, Postgres, QueryBuilder, Row};

use aether_data_contracts::repository::provider_ops_balance::{
    ProviderOpsBalanceSnapshotReadRepository, ProviderOpsBalanceSnapshotWriteRepository,
    StoredProviderOpsBalanceSnapshot,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlxResultExt;

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
pub struct SqlxProviderOpsBalanceSnapshotRepository {
    pool: PgPool,
}

impl SqlxProviderOpsBalanceSnapshotRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderOpsBalanceSnapshotReadRepository for SqlxProviderOpsBalanceSnapshotRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        let row = sqlx::query(&format!("{SNAPSHOT_COLUMNS} WHERE provider_id = $1"))
            .bind(provider_id)
            .fetch_optional(&self.pool)
            .await
            .map_postgres_err()?;
        row.as_ref().map(map_row).transpose()
    }

    async fn list_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        let mut snapshots = Vec::with_capacity(provider_ids.len());
        for chunk in provider_ids.chunks(LIST_CHUNK_SIZE) {
            let mut builder = QueryBuilder::<Postgres>::new(SNAPSHOT_COLUMNS);
            builder.push(" WHERE provider_id IN (");
            let mut separated = builder.separated(", ");
            for provider_id in chunk {
                separated.push_bind(provider_id.as_str());
            }
            separated.push_unseparated(")");
            let rows = builder
                .build()
                .fetch_all(&self.pool)
                .await
                .map_postgres_err()?;
            for row in &rows {
                snapshots.push(map_row(row)?);
            }
        }
        Ok(snapshots)
    }
}

#[async_trait]
impl ProviderOpsBalanceSnapshotWriteRepository for SqlxProviderOpsBalanceSnapshotRepository {
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
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
ON CONFLICT (provider_id) DO UPDATE SET
  payload_json = EXCLUDED.payload_json,
  last_success_at = EXCLUDED.last_success_at,
  last_attempt_at = EXCLUDED.last_attempt_at,
  last_status = EXCLUDED.last_status,
  last_error = EXCLUDED.last_error,
  consecutive_failures = EXCLUDED.consecutive_failures,
  next_refresh_at = EXCLUDED.next_refresh_at,
  updated_at = EXCLUDED.updated_at
"#,
        )
        .bind(&snapshot.provider_id)
        .bind(snapshot.payload_json.clone())
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
        .map_postgres_err()?;
        Ok(())
    }

    async fn delete_by_provider_id(&self, provider_id: &str) -> Result<bool, DataLayerError> {
        let rows_affected =
            sqlx::query("DELETE FROM provider_ops_balance_snapshots WHERE provider_id = $1")
                .bind(provider_id)
                .execute(&self.pool)
                .await
                .map_postgres_err()?
                .rows_affected();
        Ok(rows_affected > 0)
    }
}

fn map_row(row: &PgRow) -> Result<StoredProviderOpsBalanceSnapshot, DataLayerError> {
    Ok(StoredProviderOpsBalanceSnapshot {
        provider_id: row.try_get("provider_id").map_postgres_err()?,
        payload_json: row
            .try_get::<Option<serde_json::Value>, _>("payload_json")
            .map_postgres_err()?,
        last_success_at_unix_secs: optional_u64(row, "last_success_at")?,
        last_attempt_at_unix_secs: optional_u64(row, "last_attempt_at")?,
        last_status: row
            .try_get::<Option<String>, _>("last_status")
            .map_postgres_err()?,
        last_error: row
            .try_get::<Option<String>, _>("last_error")
            .map_postgres_err()?,
        consecutive_failures: u32::try_from(
            row.try_get::<i32, _>("consecutive_failures")
                .map_postgres_err()?,
        )
        .unwrap_or(0),
        next_refresh_at_unix_secs: optional_u64(row, "next_refresh_at")?,
        updated_at_unix_secs: u64::try_from(
            row.try_get::<i64, _>("updated_at").map_postgres_err()?,
        )
        .unwrap_or(0),
    })
}

fn optional_u64(row: &PgRow, field: &str) -> Result<Option<u64>, DataLayerError> {
    Ok(row
        .try_get::<Option<i64>, _>(field)
        .map_postgres_err()?
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
