use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, Row};

use aether_data_contracts::repository::quota::{
    ProviderQuotaReadRepository, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
};
use aether_data_query::{DialectSql, SelectColumn, SelectQuery, SqlDialect};

use crate::error::SqlResultExt;
use crate::{DataLayerError, MysqlPool};

fn quota_snapshot_select() -> SelectQuery<'static> {
    SelectQuery::new("providers").select_columns([
        SelectColumn::expr("id").alias("provider_id"),
        SelectColumn::expr(
            DialectSql::common("billing_type").with_postgres("CAST(billing_type AS TEXT)"),
        )
        .alias("billing_type"),
        SelectColumn::expr(
            DialectSql::dialect(
                "CAST(monthly_quota_usd AS DOUBLE PRECISION)",
                "CAST(monthly_quota_usd AS REAL)",
            )
            .with_mysql("CAST(monthly_quota_usd AS DOUBLE)"),
        )
        .alias("monthly_quota_usd"),
        SelectColumn::expr(
            DialectSql::dialect(
                "CAST(COALESCE(monthly_used_usd, 0) AS DOUBLE PRECISION)",
                "CAST(COALESCE(monthly_used_usd, 0) AS REAL)",
            )
            .with_mysql("CAST(COALESCE(monthly_used_usd, 0) AS DOUBLE)"),
        )
        .alias("monthly_used_usd"),
        SelectColumn::expr("quota_reset_day"),
        SelectColumn::expr(
            DialectSql::dialect(
                "CAST(EXTRACT(EPOCH FROM quota_last_reset_at) AS BIGINT)",
                "quota_last_reset_at",
            )
            .with_mysql("quota_last_reset_at"),
        )
        .alias("quota_last_reset_at_unix_secs"),
        SelectColumn::expr("pending_quota_reset_at").alias("pending_quota_reset_at_unix_secs"),
        SelectColumn::expr(
            DialectSql::dialect(
                "CAST(EXTRACT(EPOCH FROM quota_expires_at) AS BIGINT)",
                "quota_expires_at",
            )
            .with_mysql("quota_expires_at"),
        )
        .alias("quota_expires_at_unix_secs"),
        SelectColumn::expr(
            DialectSql::dialect(
                "CASE WHEN is_active AND NOT EXISTS (SELECT 1 FROM provider_quota_maintenance_state AS task WHERE task.provider_id = providers.id AND task.quota_epoch_start = CAST(FLOOR(EXTRACT(EPOCH FROM providers.quota_last_reset_at) / 60) * 60 AS BIGINT) AND task.status IN ('pending', 'running', 'failed')) AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas AS delta WHERE delta.kind = 'provider_monthly' AND delta.target_id = providers.id AND delta.quota_epoch_start_at_usage = CAST(FLOOR(EXTRACT(EPOCH FROM providers.quota_last_reset_at) / 60) * 60 AS BIGINT) AND delta.quota_accounting_status IN ('pending', 'failed')) THEN TRUE ELSE FALSE END",
                "CASE WHEN is_active = 1 AND NOT EXISTS (SELECT 1 FROM provider_quota_maintenance_state AS task WHERE task.provider_id = providers.id AND task.quota_epoch_start = (providers.quota_last_reset_at / 60) * 60 AND task.status IN ('pending', 'running', 'failed')) AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas AS delta WHERE delta.kind = 'provider_monthly' AND delta.target_id = providers.id AND delta.quota_epoch_start_at_usage = (providers.quota_last_reset_at / 60) * 60 AND delta.quota_accounting_status IN ('pending', 'failed')) THEN 1 ELSE 0 END",
            )
            .with_mysql(
                "CASE WHEN is_active = 1 AND NOT EXISTS (SELECT 1 FROM provider_quota_maintenance_state AS task WHERE task.provider_id = providers.id AND task.quota_epoch_start = (providers.quota_last_reset_at DIV 60) * 60 AND task.status IN ('pending', 'running', 'failed')) AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas AS delta WHERE delta.kind = 'provider_monthly' AND delta.target_id = providers.id AND delta.quota_epoch_start_at_usage = (providers.quota_last_reset_at DIV 60) * 60 AND delta.quota_accounting_status IN ('pending', 'failed')) THEN 1 ELSE 0 END",
            ),
        )
        .alias("is_active"),
    ])
}

#[derive(Debug, Clone)]
pub struct MysqlProviderQuotaRepository {
    pool: MysqlPool,
}

impl MysqlProviderQuotaRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderQuotaReadRepository for MysqlProviderQuotaRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderQuotaSnapshot>, DataLayerError> {
        let mut statement = quota_snapshot_select().statement::<MySql>(SqlDialect::MySql);
        statement.where_eq("id", provider_id.to_string()).limit(1);
        let row = statement
            .finish()
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_row).transpose()
    }

    async fn find_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderQuotaSnapshot>, DataLayerError> {
        if provider_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = quota_snapshot_select().statement::<MySql>(SqlDialect::MySql);
        statement
            .where_in("id", provider_ids)
            .order_by_sql("id ASC");
        let rows = statement
            .finish()
            .build()
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;
        rows.iter().map(map_row).collect()
    }
}

#[async_trait]
impl ProviderQuotaWriteRepository for MysqlProviderQuotaRepository {
    async fn reset_due(&self, now_unix_secs: u64) -> Result<usize, DataLayerError> {
        let now = i64::try_from(now_unix_secs).map_err(|_| {
            DataLayerError::InvalidInput("provider quota reset timestamp overflow".to_string())
        })?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let due = sqlx::query(
            r#"
SELECT id, pending_quota_reset_at
FROM providers
WHERE is_active = 1
  AND (
    (pending_quota_reset_at IS NOT NULL AND pending_quota_reset_at <= ?)
    OR (
      billing_type = 'monthly_quota'
      AND quota_reset_day BETWEEN 1 AND 30
      AND (
        quota_last_reset_at IS NULL
        OR (? - quota_last_reset_at) >= (quota_reset_day * 86400)
      )
    )
  )
FOR UPDATE
"#,
        )
        .bind(now)
        .bind(now)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        for row in &due {
            let provider_id: String = row.try_get("id").map_sql_err()?;
            let effective_at = row
                .try_get::<Option<i64>, _>("pending_quota_reset_at")
                .map_sql_err()?
                .filter(|value| *value <= now)
                .unwrap_or(now);
            sqlx::query("UPDATE providers SET monthly_used_usd = 0, quota_last_reset_at = ?, pending_quota_reset_at = NULL, updated_at = ? WHERE id = ?")
                .bind(effective_at / 60 * 60)
                .bind(now)
                .bind(&provider_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            sqlx::query("DELETE FROM provider_quota_window_counters WHERE provider_id = ?")
                .bind(&provider_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
        }
        tx.commit().await.map_sql_err()?;
        Ok(due.len())
    }

    async fn request_reset(
        &self,
        provider_id: &str,
        effective_at_unix_secs: u64,
    ) -> Result<bool, DataLayerError> {
        let effective_at = i64::try_from(effective_at_unix_secs / 60 * 60).map_err(|_| {
            DataLayerError::InvalidInput("provider quota reset timestamp overflow".to_string())
        })?;
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query("UPDATE providers SET pending_quota_reset_at = ?, updated_at = ? WHERE id = ? AND billing_type = 'monthly_quota'")
            .bind(effective_at)
            .bind(now)
            .bind(provider_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn clear_window_counters(&self, provider_id: &str) -> Result<(), DataLayerError> {
        if provider_id.trim().is_empty() {
            return Err(DataLayerError::InvalidInput(
                "provider quota provider_id is empty".to_string(),
            ));
        }
        sqlx::query("DELETE FROM provider_quota_window_counters WHERE provider_id = ?")
            .bind(provider_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(())
    }
}

fn map_row(row: &MySqlRow) -> Result<StoredProviderQuotaSnapshot, DataLayerError> {
    let mut snapshot = StoredProviderQuotaSnapshot::new(
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("billing_type").map_sql_err()?,
        row.try_get("monthly_quota_usd").map_sql_err()?,
        row.try_get("monthly_used_usd").map_sql_err()?,
        row.try_get("quota_reset_day").map_sql_err()?,
        row.try_get("quota_last_reset_at_unix_secs").map_sql_err()?,
        row.try_get("quota_expires_at_unix_secs").map_sql_err()?,
        row.try_get("is_active").map_sql_err()?,
    )?;
    snapshot.pending_quota_reset_at_unix_secs = row
        .try_get::<Option<i64>, _>("pending_quota_reset_at_unix_secs")
        .map_sql_err()?
        .map(|value| value.max(0) as u64);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::{quota_snapshot_select, MysqlProviderQuotaRepository};
    use aether_data_query::SqlDialect;

    #[test]
    fn quota_projection_renders_for_mysql() {
        let sql = quota_snapshot_select().render(SqlDialect::MySql);

        assert!(sql.contains("id AS `provider_id`"));
        assert!(sql.contains("CAST(monthly_quota_usd AS DOUBLE) AS `monthly_quota_usd`"));
        assert!(sql.contains("quota_last_reset_at AS `quota_last_reset_at_unix_secs`"));
    }

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlProviderQuotaRepository::new(pool);
    }
}
