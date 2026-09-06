use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, Row};

use aether_data_contracts::repository::quota::{
    ProviderQuotaAdjustment, ProviderQuotaReadRepository, ProviderQuotaRecovery,
    ProviderQuotaResetMode, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
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
        SelectColumn::expr("quota_subscription_started_at"),
        SelectColumn::expr("quota_cycle_start_at"),
        SelectColumn::expr("pending_quota_reset_mode"),
        SelectColumn::expr("pending_quota_reset_days"),
        SelectColumn::expr("pending_quota_reset_usage"),
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
        let now = i64::try_from(now_unix_secs / 60 * 60)
            .map_err(|_| DataLayerError::InvalidInput("quota timestamp overflow".to_string()))?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let mut statement = quota_snapshot_select().statement::<MySql>(SqlDialect::MySql);
        statement.where_raw(&format!("is_active = 1 AND billing_type = 'monthly_quota' AND quota_reset_day BETWEEN 1 AND 30 AND ((pending_quota_reset_at IS NOT NULL AND pending_quota_reset_at <= {now}) OR quota_last_reset_at IS NULL OR ({now} - (FLOOR(COALESCE(quota_cycle_start_at, quota_last_reset_at) / 60) * 60)) >= quota_reset_day * 86400)"));
        let mut query = statement.finish();
        query.push(" FOR UPDATE");
        let due = query.build().fetch_all(&mut *tx).await.map_sql_err()?;
        let mut count = 0;
        for row in due {
            let snapshot = map_row(&row)?;
            let Some(change) = snapshot.due_transition(now as u64) else {
                continue;
            };
            sqlx::query(
                r#"UPDATE providers SET
  quota_subscription_started_at = COALESCE(quota_subscription_started_at, quota_last_reset_at, ?),
  quota_cycle_start_at = ?, quota_reset_day = ?,
  monthly_used_usd = CASE WHEN ? THEN 0 ELSE monthly_used_usd END,
  quota_last_reset_at = ?,
  pending_quota_reset_at = CASE WHEN ? THEN NULL ELSE pending_quota_reset_at END,
  pending_quota_reset_mode = CASE WHEN ? THEN NULL ELSE pending_quota_reset_mode END,
  pending_quota_reset_days = CASE WHEN ? THEN NULL ELSE pending_quota_reset_days END,
  pending_quota_reset_usage = CASE WHEN ? THEN NULL ELSE pending_quota_reset_usage END,
  updated_at = ? WHERE id = ?"#,
            )
            .bind(change.cycle_start as i64)
            .bind(change.cycle_start as i64)
            .bind(change.cycle_days as i32)
            .bind(change.reset_usage)
            .bind(change.epoch_start as i64)
            .bind(change.applied_pending)
            .bind(change.applied_pending)
            .bind(change.applied_pending)
            .bind(change.applied_pending)
            .bind(now)
            .bind(&snapshot.provider_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            if change.reset_usage {
                sqlx::query("DELETE FROM provider_quota_window_counters WHERE provider_id = ?")
                    .bind(&snapshot.provider_id)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
            }
            count += 1;
        }
        tx.commit().await.map_sql_err()?;
        Ok(count)
    }

    async fn request_reset(
        &self,
        provider_id: &str,
        effective_at_unix_secs: u64,
    ) -> Result<bool, DataLayerError> {
        self.request_adjustment(
            provider_id,
            &ProviderQuotaAdjustment::cycle(effective_at_unix_secs),
        )
        .await
    }

    async fn request_adjustment(
        &self,
        provider_id: &str,
        adjustment: &ProviderQuotaAdjustment,
    ) -> Result<bool, DataLayerError> {
        adjustment.validate()?;
        let effective = i64::try_from(adjustment.effective_at_unix_secs)
            .map_err(|_| DataLayerError::InvalidInput("quota timestamp overflow".to_string()))?;
        let result = sqlx::query(
            r#"UPDATE providers SET pending_quota_reset_at = ?,
  pending_quota_reset_mode = ?, pending_quota_reset_days = ?, pending_quota_reset_usage = ?,
  updated_at = ? WHERE id = ? AND billing_type = 'monthly_quota' "#,
        )
        .bind(effective)
        .bind(match adjustment.mode {
            ProviderQuotaResetMode::Cycle => "cycle",
            ProviderQuotaResetMode::UsageOnly => "usage_only",
        })
        .bind(adjustment.cycle_days.map(|v| v as i32))
        .bind(adjustment.reset_usage)
        .bind(chrono::Utc::now().timestamp())
        .bind(provider_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() > 0)
    }

    async fn recover_attempts(
        &self,
        provider_id: Option<&str>,
        limit: usize,
        now_unix_secs: u64,
        dry_run: bool,
    ) -> Result<Vec<ProviderQuotaRecovery>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await.map_sql_err()?;
        // A timeout alone is not proof of termination. Active candidates are recovered only
        // after durable request finalization, with five minutes for terminal queue settlement.
        let rows = sqlx::query(r#"SELECT delta.id AS delta_id, delta.request_id AS candidate_id, delta.target_id AS provider_id,
  delta.quota_epoch_start_at_usage AS epoch, delta.provider_quota_cost_usd AS cost,
  c.extra_data, c.status AS candidate_status,
  (SELECT CAST(JSON_UNQUOTE(JSON_EXTRACT(ss.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE) FROM usage_settlement_snapshots ss
    JOIN usage_routing_snapshots routing ON routing.request_id = ss.request_id
    WHERE routing.candidate_id = c.id AND ss.finalized_at IS NOT NULL LIMIT 1) AS settled_cost
FROM usage_counter_deltas delta
JOIN request_candidates c ON c.id = delta.request_id AND c.provider_id = delta.target_id
JOIN providers p ON p.id = delta.target_id
WHERE delta.kind = 'provider_monthly' AND delta.quota_epoch_start_at_usage = (p.quota_last_reset_at DIV 60) * 60
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND (? IS NULL OR delta.target_id = ?)
  AND (c.status IN ('failed', 'cancelled') OR EXISTS (
    SELECT 1 FROM `usage` u WHERE u.request_id = c.request_id
      AND u.finalized_at IS NOT NULL AND u.finalized_at <= ?))
ORDER BY delta.created_at, delta.id LIMIT ? FOR UPDATE"#)
            .bind(provider_id).bind(provider_id).bind(now_unix_secs.saturating_sub(300) as i64)
            .bind(limit.min(1000) as i64).fetch_all(&mut *tx).await.map_sql_err()?;
        let mut recovered = Vec::new();
        for row in rows {
            let extra = row
                .try_get::<Option<String>, _>("extra_data")
                .map_sql_err()?
                .map(|v| serde_json::from_str::<serde_json::Value>(&v))
                .transpose()
                .map_err(|err| DataLayerError::UnexpectedValue(err.to_string()))?;
            let Some(snapshot) =
                aether_data_contracts::repository::candidates::provider_quota_dispatch_snapshot(
                    extra.as_ref(),
                )?
                .filter(|s| s.is_monthly_quota())
            else {
                continue;
            };
            let delta_id: String = row.try_get("delta_id").map_sql_err()?;
            let candidate_id: String = row.try_get("candidate_id").map_sql_err()?;
            let known = extra
                .as_ref()
                .and_then(|v| v.pointer("/provider_quota_attempt_accounting/cost_usd"))
                .and_then(serde_json::Value::as_f64);
            let settled: Option<f64> = row.try_get("settled_cost").map_sql_err()?;
            let cost = row
                .try_get::<Option<f64>, _>("cost")
                .map_sql_err()?
                .unwrap_or(0.0)
                .max(snapshot.provider_quota_cost_usd.unwrap_or(0.0))
                .max(known.unwrap_or(0.0))
                .max(settled.unwrap_or(0.0));
            let reason = if known.is_some() || settled.is_some() {
                "known_usage"
            } else {
                "availability_first_provisional_unknown"
            };
            if !dry_run {
                crate::settlement::reconcile_provider_monthly_attempt_mysql(
                    &mut tx,
                    &candidate_id,
                    cost,
                    true,
                    now_unix_secs as i64,
                )
                .await?;
                let audit = serde_json::json!({ "delta_id": delta_id, "reason": reason, "cost_usd": cost, "recovered_at_unix_secs": now_unix_secs });
                sqlx::query("UPDATE request_candidates SET extra_data = JSON_SET(COALESCE(extra_data, '{}'), '$.provider_quota_recovery', CAST(? AS JSON)) WHERE id = ?")
                    .bind(audit.to_string()).bind(&candidate_id).execute(&mut *tx).await.map_sql_err()?;
            }
            recovered.push(ProviderQuotaRecovery {
                provider_id: row.try_get("provider_id").map_sql_err()?,
                candidate_id,
                delta_id,
                quota_epoch_start: row.try_get::<i64, _>("epoch").map_sql_err()? as u64,
                known_cost_usd: cost,
                reason: reason.to_string(),
                applied: !dry_run,
            });
        }
        tx.commit().await.map_sql_err()?;
        Ok(recovered)
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
    snapshot.quota_subscription_started_at_unix_secs = row
        .try_get::<Option<i64>, _>("quota_subscription_started_at")
        .map_sql_err()?
        .map(|v| v.max(0) as u64)
        .or(snapshot.quota_last_reset_at_unix_secs);
    snapshot.quota_cycle_start_at_unix_secs = row
        .try_get::<Option<i64>, _>("quota_cycle_start_at")
        .map_sql_err()?
        .map(|v| v.max(0) as u64)
        .or(snapshot.quota_last_reset_at_unix_secs);
    if let Some(effective) = snapshot.pending_quota_reset_at_unix_secs {
        snapshot.pending_adjustment = Some(ProviderQuotaAdjustment {
            effective_at_unix_secs: effective,
            mode: if row
                .try_get::<Option<String>, _>("pending_quota_reset_mode")
                .map_sql_err()?
                .as_deref()
                == Some("usage_only")
            {
                ProviderQuotaResetMode::UsageOnly
            } else {
                ProviderQuotaResetMode::Cycle
            },
            reset_usage: row
                .try_get::<Option<bool>, _>("pending_quota_reset_usage")
                .map_sql_err()?
                .unwrap_or(true),
            cycle_days: row
                .try_get::<Option<i32>, _>("pending_quota_reset_days")
                .map_sql_err()?
                .map(|v| v as u64),
        });
    }
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
