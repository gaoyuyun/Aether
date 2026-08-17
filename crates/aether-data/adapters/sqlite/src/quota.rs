use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, Row, Sqlite};

use aether_data_contracts::repository::quota::{
    ProviderQuotaReadRepository, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
};
use aether_data_query::{DialectSql, SelectColumn, SelectQuery, SqlDialect};

use crate::error::SqlResultExt;
use crate::{sqlite_optional_real, sqlite_real, DataLayerError, SqlitePool};

fn current_unix_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

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
        SelectColumn::expr("pending_quota_reset_at").alias("pending_quota_reset_at_unix_secs"),
        SelectColumn::expr(DialectSql::dialect(
            "CAST(EXTRACT(EPOCH FROM quota_expires_at) AS BIGINT)",
            "quota_expires_at",
        ))
        .alias("quota_expires_at_unix_secs"),
        SelectColumn::expr(DialectSql::dialect(
            "CASE WHEN is_active AND NOT EXISTS (SELECT 1 FROM provider_quota_maintenance_state AS task WHERE task.provider_id = providers.id AND task.quota_epoch_start = (providers.quota_last_reset_at / 60) * 60 AND task.status IN ('pending', 'running', 'failed')) AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas AS delta WHERE delta.kind = 'provider_monthly' AND delta.target_id = providers.id AND delta.quota_epoch_start_at_usage = (providers.quota_last_reset_at / 60) * 60 AND delta.quota_accounting_status IN ('pending', 'failed')) THEN TRUE ELSE FALSE END",
            "CASE WHEN is_active = 1 AND NOT EXISTS (SELECT 1 FROM provider_quota_maintenance_state AS task WHERE task.provider_id = providers.id AND task.quota_epoch_start = (providers.quota_last_reset_at / 60) * 60 AND task.status IN ('pending', 'running', 'failed')) AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas AS delta WHERE delta.kind = 'provider_monthly' AND delta.target_id = providers.id AND delta.quota_epoch_start_at_usage = (providers.quota_last_reset_at / 60) * 60 AND delta.quota_accounting_status IN ('pending', 'failed')) THEN 1 ELSE 0 END",
        ))
        .alias("is_active"),
    ])
}

#[derive(Debug, Clone)]
pub struct SqliteProviderQuotaRepository {
    pool: SqlitePool,
}

impl SqliteProviderQuotaRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProviderQuotaReadRepository for SqliteProviderQuotaRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderQuotaSnapshot>, DataLayerError> {
        let mut statement = quota_snapshot_select().statement::<Sqlite>(SqlDialect::Sqlite);
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

        let mut statement = quota_snapshot_select().statement::<Sqlite>(SqlDialect::Sqlite);
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
impl ProviderQuotaWriteRepository for SqliteProviderQuotaRepository {
    async fn reset_due(&self, now_unix_secs: u64) -> Result<usize, DataLayerError> {
        let now = i64::try_from(now_unix_secs).map_err(|_| {
            DataLayerError::InvalidInput("provider quota reset timestamp overflow".to_string())
        })?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE providers SET updated_at = updated_at WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
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
            sqlx::query(
                "UPDATE providers SET monthly_used_usd = 0, quota_last_reset_at = ?, pending_quota_reset_at = NULL, updated_at = ? WHERE id = ?",
            )
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
        let result = sqlx::query(
            "UPDATE providers SET pending_quota_reset_at = ?, updated_at = ? WHERE id = ? AND billing_type = 'monthly_quota'",
        )
        .bind(effective_at)
        .bind(current_unix_secs())
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

fn map_row(row: &SqliteRow) -> Result<StoredProviderQuotaSnapshot, DataLayerError> {
    let mut snapshot = StoredProviderQuotaSnapshot::new(
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("billing_type").map_sql_err()?,
        sqlite_optional_real(row, "monthly_quota_usd")?,
        sqlite_real(row, "monthly_used_usd")?,
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
    use super::SqliteProviderQuotaRepository;
    use aether_data_contracts::repository::quota::{
        ProviderQuotaReadRepository, ProviderQuotaWriteRepository,
    };

    use crate::run_migrations;

    #[tokio::test]
    async fn sqlite_repository_reads_and_resets_provider_quotas() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_provider_quotas(&pool).await;

        let repository = SqliteProviderQuotaRepository::new(pool);
        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should load")
            .expect("quota should exist");
        assert_eq!(quota.monthly_used_usd, 5.0);

        let quota = repository
            .find_by_provider_id("provider-null-used")
            .await
            .expect("quota with null usage should load")
            .expect("quota with null usage should exist");
        assert_eq!(quota.monthly_used_usd, 0.0);

        let quotas = repository
            .find_by_provider_ids(&["provider-2".to_string(), "provider-1".to_string()])
            .await
            .expect("quotas should load");
        assert_eq!(
            quotas
                .iter()
                .map(|quota| quota.provider_id.as_str())
                .collect::<Vec<_>>(),
            vec!["provider-1", "provider-2"]
        );

        let reset = repository
            .reset_due(1_000 + 7 * 24 * 60 * 60)
            .await
            .expect("quota reset should run");
        assert_eq!(reset, 1);
        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should reload")
            .expect("quota should exist");
        assert_eq!(quota.monthly_used_usd, 0.0);
        assert_eq!(quota.quota_last_reset_at_unix_secs, Some(605_760));

        repository
            .clear_window_counters("provider-1")
            .await
            .expect("window counters should clear");
        let remaining_window_counters: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM provider_quota_window_counters WHERE provider_id = 'provider-1'",
        )
        .fetch_one(&repository.pool)
        .await
        .expect("window counter count should load");
        assert_eq!(remaining_window_counters, 0);
    }

    #[tokio::test]
    async fn sqlite_repository_fail_closes_on_unresolved_monthly_attempt() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_provider_quotas(&pool).await;
        sqlx::query(
            r#"
INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, total_cost_usd_delta,
  provider_billing_type_at_usage, quota_epoch_start_at_usage,
  provider_dispatch_at_unix_secs, provider_quota_cost_usd,
  quota_delta_sequence, quota_accounting_status, created_at
) VALUES (
  'pending-attempt', 'candidate-pending', 'provider_monthly', 'provider-1', 0,
  'monthly_quota', 960, 1020, 0, 1, 'pending', 1020
)
"#,
        )
        .execute(&pool)
        .await
        .expect("pending attempt should seed");
        let repository = SqliteProviderQuotaRepository::new(pool.clone());

        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should load")
            .expect("quota should exist");
        assert!(!quota.is_active);

        sqlx::query(
            "UPDATE usage_counter_deltas SET quota_accounting_status = 'ready', provider_quota_cost_usd = 1, total_cost_usd_delta = 1 WHERE id = 'pending-attempt'",
        )
        .execute(&pool)
        .await
        .expect("attempt should reconcile");
        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should reload")
            .expect("quota should exist");
        assert!(quota.is_active);
    }

    #[tokio::test]
    async fn sqlite_repository_scopes_fail_closed_state_to_current_quota_epoch() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        seed_provider_quotas(&pool).await;
        sqlx::query(
            r#"
INSERT INTO provider_quota_maintenance_state (
  provider_id, quota_epoch_start, task_kind, status, created_at, updated_at
) VALUES ('provider-1', 900, 'historical_backfill', 'failed', 1, 1);

INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, total_cost_usd_delta,
  provider_billing_type_at_usage, quota_epoch_start_at_usage,
  provider_dispatch_at_unix_secs, provider_quota_cost_usd,
  quota_delta_sequence, quota_accounting_status, created_at
) VALUES (
  'old-failed-attempt', 'candidate-old-failed', 'provider_monthly', 'provider-1', 0,
  'monthly_quota', 900, 900, 0, 1, 'failed', 900
)
"#,
        )
        .execute(&pool)
        .await
        .expect("old failed state should seed");
        let repository = SqliteProviderQuotaRepository::new(pool.clone());

        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should load")
            .expect("quota should exist");
        assert!(
            quota.is_active,
            "an old epoch must not poison the current epoch"
        );

        sqlx::query(
            r#"
INSERT INTO provider_quota_maintenance_state (
  provider_id, quota_epoch_start, task_kind, status, created_at, updated_at
) VALUES ('provider-1', 960, 'historical_backfill', 'failed', 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("current failed task should seed");
        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("quota should reload")
            .expect("quota should exist");
        assert!(!quota.is_active, "a current-epoch failure must fail closed");

        assert!(repository
            .request_reset("provider-1", 1_080)
            .await
            .expect("reset should be requested"));
        assert_eq!(
            repository.reset_due(1_080).await.expect("reset should run"),
            1
        );
        let quota = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("reset quota should load")
            .expect("quota should exist");
        assert_eq!(quota.quota_last_reset_at_unix_secs, Some(1_080));
        assert!(quota.is_active, "reset must start a clean quota epoch");
    }

    async fn seed_provider_quotas(pool: &sqlx::SqlitePool) {
        sqlx::query(
            r#"
INSERT INTO providers (
  id, name, provider_type, billing_type, monthly_quota_usd, monthly_used_usd,
  quota_reset_day, quota_last_reset_at, is_active, created_at, updated_at
)
VALUES
  ('provider-1', 'Provider One', 'openai', 'monthly_quota', 20.0, 5.0, 7, 1000, 1, 1, 1),
  ('provider-2', 'Provider Two', 'openai', 'payg', NULL, 1.5, NULL, NULL, 1, 1, 1),
  ('provider-null-used', 'Provider Null Used', 'openai', 'payg', NULL, NULL, NULL, NULL, 1, 1, 1)
;

INSERT INTO provider_quota_window_counters (
  provider_id, duration_secs, quota_epoch_start, rolling_start,
  accounted_until, used_usd, status, updated_at
) VALUES ('provider-1', 86400, 960, 960, 1020, 2.5, 'ready', 1)
"#,
        )
        .execute(pool)
        .await
        .expect("providers should seed");
    }
}
