use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite};

use aether_data_contracts::repository::provider_ops_balance::{
    ProviderOpsBalanceSnapshotReadRepository, ProviderOpsBalanceSnapshotWriteRepository,
    StoredProviderOpsBalanceSnapshot,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::SqlitePool;

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

/// Keep well under SQLite's bound-parameter limit for `IN (...)` lists.
const LIST_CHUNK_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub struct SqliteProviderOpsBalanceSnapshotRepository {
    pool: SqlitePool,
}

impl SqliteProviderOpsBalanceSnapshotRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderOpsBalanceSnapshotReadRepository for SqliteProviderOpsBalanceSnapshotRepository {
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
            let mut builder = QueryBuilder::<Sqlite>::new(SNAPSHOT_COLUMNS);
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
impl ProviderOpsBalanceSnapshotWriteRepository for SqliteProviderOpsBalanceSnapshotRepository {
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
ON CONFLICT(provider_id) DO UPDATE SET
  payload_json = excluded.payload_json,
  last_success_at = excluded.last_success_at,
  last_attempt_at = excluded.last_attempt_at,
  last_status = excluded.last_status,
  last_error = excluded.last_error,
  consecutive_failures = excluded.consecutive_failures,
  next_refresh_at = excluded.next_refresh_at,
  updated_at = excluded.updated_at
"#,
        )
        .bind(&snapshot.provider_id)
        .bind(
            snapshot
                .payload_json
                .as_ref()
                .map(serde_json::Value::to_string),
        )
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
        .bind(i64::from(snapshot.consecutive_failures))
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

fn map_row(row: &SqliteRow) -> Result<StoredProviderOpsBalanceSnapshot, DataLayerError> {
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
            row.try_get::<i64, _>("consecutive_failures")
                .map_sql_err()?,
        )
        .unwrap_or(0),
        next_refresh_at_unix_secs: optional_u64(row, "next_refresh_at")?,
        updated_at_unix_secs: u64::try_from(row.try_get::<i64, _>("updated_at").map_sql_err()?)
            .unwrap_or(0),
    })
}

fn optional_u64(row: &SqliteRow, field: &str) -> Result<Option<u64>, DataLayerError> {
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

#[cfg(test)]
mod tests {
    use super::SqliteProviderOpsBalanceSnapshotRepository;
    use aether_data_contracts::repository::provider_ops_balance::{
        ProviderOpsBalanceSnapshotReadRepository, ProviderOpsBalanceSnapshotWriteRepository,
        StoredProviderOpsBalanceSnapshot,
    };
    use serde_json::json;

    async fn seed_provider(pool: &crate::SqlitePool, provider_id: &str) {
        sqlx::query(
            "INSERT INTO providers (id, name, provider_type, is_active, created_at, updated_at) VALUES (?, ?, 'openai', 1, 1, 1)",
        )
        .bind(provider_id)
        .bind(provider_id)
        .execute(pool)
        .await
        .expect("provider should seed");
    }

    #[tokio::test]
    async fn sqlite_repository_round_trips_balance_snapshots() {
        let pool = crate::test_support::migrated_pool().await;
        seed_provider(&pool, "provider-1").await;
        seed_provider(&pool, "provider-2").await;
        let repository = SqliteProviderOpsBalanceSnapshotRepository::new(pool.clone());

        let snapshot = StoredProviderOpsBalanceSnapshot {
            provider_id: "provider-1".to_string(),
            payload_json: Some(json!({"status": "success", "data": {"total_available": 4.5}})),
            last_success_at_unix_secs: Some(100),
            last_attempt_at_unix_secs: Some(100),
            last_status: Some("success".to_string()),
            last_error: None,
            consecutive_failures: 0,
            next_refresh_at_unix_secs: Some(700),
            updated_at_unix_secs: 100,
        };
        repository
            .upsert(&snapshot)
            .await
            .expect("snapshot should insert");
        assert_eq!(
            repository
                .find_by_provider_id("provider-1")
                .await
                .expect("snapshot should read"),
            Some(snapshot.clone())
        );

        let failed = StoredProviderOpsBalanceSnapshot {
            last_attempt_at_unix_secs: Some(160),
            last_status: Some("network_error".to_string()),
            last_error: Some("请求超时".to_string()),
            consecutive_failures: 1,
            next_refresh_at_unix_secs: Some(220),
            updated_at_unix_secs: 160,
            ..snapshot.clone()
        };
        repository
            .upsert(&failed)
            .await
            .expect("snapshot should update");
        let listed = repository
            .list_by_provider_ids(&["provider-1".to_string(), "provider-2".to_string()])
            .await
            .expect("snapshots should list");
        assert_eq!(listed, vec![failed.clone()]);
        assert!(repository
            .list_by_provider_ids(&[])
            .await
            .expect("empty list should read")
            .is_empty());

        assert!(repository
            .delete_by_provider_id("provider-1")
            .await
            .expect("delete should run"));
        assert!(!repository
            .delete_by_provider_id("provider-1")
            .await
            .expect("second delete should run"));
        assert_eq!(
            repository
                .find_by_provider_id("provider-1")
                .await
                .expect("read should run"),
            None
        );
    }

    #[tokio::test]
    async fn sqlite_repository_drops_snapshots_with_their_provider() {
        let pool = crate::test_support::migrated_pool().await;
        seed_provider(&pool, "provider-cascade").await;
        let repository = SqliteProviderOpsBalanceSnapshotRepository::new(pool.clone());
        repository
            .upsert(&StoredProviderOpsBalanceSnapshot {
                provider_id: "provider-cascade".to_string(),
                payload_json: None,
                last_success_at_unix_secs: None,
                last_attempt_at_unix_secs: Some(5),
                last_status: Some("auth_failed".to_string()),
                last_error: Some("认证失败".to_string()),
                consecutive_failures: 3,
                next_refresh_at_unix_secs: Some(1805),
                updated_at_unix_secs: 5,
            })
            .await
            .expect("snapshot should insert");

        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind("provider-cascade")
            .execute(&pool)
            .await
            .expect("provider should delete");

        assert_eq!(
            repository
                .find_by_provider_id("provider-cascade")
                .await
                .expect("read should run"),
            None
        );
    }
}
