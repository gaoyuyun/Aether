use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Row};

use aether_data_contracts::repository::quota::{
    ProviderQuotaReadRepository, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
};
use aether_data_query::{DialectSql, SelectColumn, SelectQuery, SqlDialect};

use crate::{error::SqlxResultExt, DataLayerError};

fn quota_snapshot_select() -> SelectQuery<'static> {
    SelectQuery::new("providers").select_columns([
        SelectColumn::expr("id").alias("provider_id"),
        SelectColumn::expr(
            DialectSql::common("billing_type").with_postgres("CAST(billing_type AS TEXT)"),
        )
        .alias("billing_type"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(monthly_quota_usd AS DOUBLE PRECISION)",
            "CAST(monthly_quota_usd AS REAL)",
        ))
        .alias("monthly_quota_usd"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(COALESCE(monthly_used_usd, 0) AS DOUBLE PRECISION)",
            "CAST(COALESCE(monthly_used_usd, 0) AS REAL)",
        ))
        .alias("monthly_used_usd"),
        SelectColumn::expr("quota_reset_day"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(EXTRACT(EPOCH FROM quota_last_reset_at) AS BIGINT)",
            "quota_last_reset_at",
        ))
        .alias("quota_last_reset_at_unix_secs"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(EXTRACT(EPOCH FROM pending_quota_reset_at) AS BIGINT)",
            "pending_quota_reset_at",
        ))
        .alias("pending_quota_reset_at_unix_secs"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(EXTRACT(EPOCH FROM quota_expires_at) AS BIGINT)",
            "quota_expires_at",
        ))
        .alias("quota_expires_at_unix_secs"),
        SelectColumn::expr("is_active"),
    ])
}

#[derive(Debug, Clone)]
pub struct SqlxProviderQuotaRepository {
    pool: PgPool,
}

impl SqlxProviderQuotaRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderQuotaReadRepository for SqlxProviderQuotaRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderQuotaSnapshot>, DataLayerError> {
        let mut statement = quota_snapshot_select().statement::<Postgres>(SqlDialect::Postgres);
        statement.where_eq("id", provider_id.to_string()).limit(1);
        let row = statement
            .finish()
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_postgres_err()?;
        row.as_ref().map(map_row).transpose()
    }

    async fn find_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderQuotaSnapshot>, DataLayerError> {
        if provider_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = quota_snapshot_select().statement::<Postgres>(SqlDialect::Postgres);
        statement
            .where_in("id", provider_ids)
            .order_by_sql("id ASC");
        statement
            .finish()
            .build()
            .fetch_all(&self.pool)
            .await
            .map_postgres_err()?
            .iter()
            .map(map_row)
            .collect()
    }
}

#[async_trait]
impl ProviderQuotaWriteRepository for SqlxProviderQuotaRepository {
    async fn reset_due(&self, now_unix_secs: u64) -> Result<usize, DataLayerError> {
        let now = i64::try_from(now_unix_secs).map_err(|_| {
            DataLayerError::InvalidInput("provider quota reset timestamp overflow".to_string())
        })?;
        let mut tx = self.pool.begin().await.map_postgres_err()?;
        let due = sqlx::query(
            r#"
SELECT id, CAST(EXTRACT(EPOCH FROM pending_quota_reset_at) AS BIGINT) AS pending_quota_reset_at
FROM providers
WHERE is_active = TRUE
  AND (
    (pending_quota_reset_at IS NOT NULL AND pending_quota_reset_at <= TO_TIMESTAMP($1::double precision))
    OR (
      billing_type = 'monthly_quota'
      AND quota_reset_day BETWEEN 1 AND 30
      AND (
        quota_last_reset_at IS NULL
        OR ($1 - EXTRACT(EPOCH FROM quota_last_reset_at)) >= (quota_reset_day * 86400)
      )
    )
  )
FOR UPDATE
"#,
        )
            .bind(now as f64)
            .fetch_all(&mut *tx)
            .await
            .map_postgres_err()?;
        for row in &due {
            let provider_id: String = row.try_get("id").map_postgres_err()?;
            let effective_at = row
                .try_get::<Option<i64>, _>("pending_quota_reset_at")
                .map_postgres_err()?
                .filter(|value| *value <= now)
                .unwrap_or(now);
            sqlx::query("UPDATE providers SET monthly_used_usd = 0, quota_last_reset_at = TO_TIMESTAMP($1::double precision), pending_quota_reset_at = NULL, updated_at = NOW() WHERE id = $2")
                .bind((effective_at / 60 * 60) as f64)
                .bind(&provider_id)
                .execute(&mut *tx)
                .await
                .map_postgres_err()?;
            sqlx::query("DELETE FROM provider_quota_window_counters WHERE provider_id = $1")
                .bind(&provider_id)
                .execute(&mut *tx)
                .await
                .map_postgres_err()?;
        }
        tx.commit().await.map_postgres_err()?;
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
        let result = sqlx::query("UPDATE providers SET pending_quota_reset_at = TO_TIMESTAMP($1::double precision), updated_at = NOW() WHERE id = $2 AND billing_type = 'monthly_quota'")
            .bind(effective_at as f64)
            .bind(provider_id)
            .execute(&self.pool)
            .await
            .map_postgres_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn clear_window_counters(&self, provider_id: &str) -> Result<(), DataLayerError> {
        if provider_id.trim().is_empty() {
            return Err(DataLayerError::InvalidInput(
                "provider quota provider_id is empty".to_string(),
            ));
        }
        sqlx::query("DELETE FROM provider_quota_window_counters WHERE provider_id = $1")
            .bind(provider_id)
            .execute(&self.pool)
            .await
            .map_postgres_err()?;
        Ok(())
    }
}

fn map_row(row: &sqlx::postgres::PgRow) -> Result<StoredProviderQuotaSnapshot, DataLayerError> {
    let mut snapshot = StoredProviderQuotaSnapshot::new(
        row.try_get("provider_id").map_postgres_err()?,
        row.try_get("billing_type").map_postgres_err()?,
        row.try_get("monthly_quota_usd").map_postgres_err()?,
        row.try_get("monthly_used_usd").map_postgres_err()?,
        row.try_get("quota_reset_day").map_postgres_err()?,
        row.try_get("quota_last_reset_at_unix_secs")
            .map_postgres_err()?,
        row.try_get("quota_expires_at_unix_secs")
            .map_postgres_err()?,
        row.try_get("is_active").map_postgres_err()?,
    )?;
    snapshot.pending_quota_reset_at_unix_secs = row
        .try_get::<Option<i64>, _>("pending_quota_reset_at_unix_secs")
        .map_postgres_err()?
        .map(|value| value.max(0) as u64);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::SqlxProviderQuotaRepository;
    use crate::{PostgresPoolConfig, PostgresPoolFactory};

    #[tokio::test]
    async fn repository_constructs_from_lazy_pool() {
        let factory = PostgresPoolFactory::new(PostgresPoolConfig {
            database_url: "postgres://localhost/aether".to_string(),
            min_connections: 1,
            max_connections: 4,
            acquire_timeout_ms: 1_000,
            idle_timeout_ms: 5_000,
            max_lifetime_ms: 30_000,
            statement_cache_capacity: 64,
            require_ssl: false,
        })
        .expect("factory should build");

        let pool = factory.connect_lazy().expect("pool should build");
        let _repository = SqlxProviderQuotaRepository::new(pool);
    }
}
