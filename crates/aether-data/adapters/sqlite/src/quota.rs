use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, Row, Sqlite};

use aether_data_contracts::repository::quota::{
    ProviderQuotaAdjustment, ProviderQuotaReadRepository, ProviderQuotaRecovery,
    ProviderQuotaResetMode, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
};
use aether_data_query::{DialectSql, SelectColumn, SelectQuery, SqlDialect};

use crate::error::SqlResultExt;
use crate::{sqlite_optional_real, sqlite_real, DataLayerError, SqlitePool};

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
        SelectColumn::expr("quota_subscription_started_at"),
        SelectColumn::expr("quota_cycle_start_at"),
        SelectColumn::expr("pending_quota_reset_mode"),
        SelectColumn::expr("pending_quota_reset_days"),
        SelectColumn::expr("pending_quota_reset_usage"),
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
        let now = i64::try_from(now_unix_secs / 60 * 60)
            .map_err(|_| DataLayerError::InvalidInput("quota timestamp overflow".to_string()))?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE providers SET updated_at = updated_at WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let mut statement = quota_snapshot_select().statement::<Sqlite>(SqlDialect::Sqlite);
        statement.where_raw(&format!("is_active = 1 AND billing_type = 'monthly_quota' AND quota_reset_day BETWEEN 1 AND 30 AND ((pending_quota_reset_at IS NOT NULL AND pending_quota_reset_at <= {now}) OR quota_last_reset_at IS NULL OR ({now} - ((COALESCE(quota_cycle_start_at, quota_last_reset_at) / 60) * 60)) >= quota_reset_day * 86400)"));
        let mut query = statement.finish();
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
        sqlx::query("UPDATE providers SET updated_at = updated_at WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        // A timeout alone is not proof of termination. Active candidates are recovered only
        // after durable request finalization, with five minutes for terminal queue settlement.
        let rows = sqlx::query(r#"SELECT delta.id AS delta_id, delta.request_id AS candidate_id, delta.target_id AS provider_id,
  delta.quota_epoch_start_at_usage AS epoch, delta.provider_quota_cost_usd AS cost,
  c.extra_data, c.status AS candidate_status,
  (SELECT CAST(json_extract(ss.settlement_snapshot, '$.provider_quota_cost_usd') AS REAL) FROM usage_settlement_snapshots ss
    JOIN usage_routing_snapshots routing ON routing.request_id = ss.request_id
    WHERE routing.candidate_id = c.id AND ss.finalized_at IS NOT NULL LIMIT 1) AS settled_cost
FROM usage_counter_deltas delta
JOIN request_candidates c ON c.id = delta.request_id AND c.provider_id = delta.target_id
JOIN providers p ON p.id = delta.target_id
WHERE delta.kind = 'provider_monthly' AND delta.quota_epoch_start_at_usage = (p.quota_last_reset_at / 60) * 60
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND (? IS NULL OR delta.target_id = ?)
  AND (c.status IN ('failed', 'cancelled') OR EXISTS (
    SELECT 1 FROM "usage" u WHERE u.request_id = c.request_id
      AND u.finalized_at IS NOT NULL AND u.finalized_at <= ?))
ORDER BY delta.created_at, delta.id LIMIT ?"#)
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
                crate::settlement::reconcile_provider_monthly_attempt_sqlite(
                    &mut tx,
                    &candidate_id,
                    cost,
                    true,
                    now_unix_secs as i64,
                )
                .await?;
                let audit = serde_json::json!({ "delta_id": delta_id, "reason": reason, "cost_usd": cost, "recovered_at_unix_secs": now_unix_secs });
                sqlx::query("UPDATE request_candidates SET extra_data = json_set(COALESCE(extra_data, '{}'), '$.provider_quota_recovery', json(?)) WHERE id = ?")
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

    #[tokio::test]
    async fn sqlite_quota_reset_modes_preserve_history_expiry_and_natural_grid() {
        use aether_data_contracts::repository::quota::{
            ProviderQuotaAdjustment, ProviderQuotaResetMode,
        };
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();
        seed_provider_quotas(&pool).await;
        sqlx::query("UPDATE providers SET quota_last_reset_at=960, quota_cycle_start_at=960, quota_subscription_started_at=960, quota_expires_at=9999999 WHERE id='provider-1'").execute(&pool).await.unwrap();
        let repository = SqliteProviderQuotaRepository::new(pool.clone());
        repository
            .request_adjustment(
                "provider-1",
                &ProviderQuotaAdjustment {
                    effective_at_unix_secs: 1200,
                    mode: ProviderQuotaResetMode::UsageOnly,
                    reset_usage: true,
                    cycle_days: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(repository.reset_due(1199).await.unwrap(), 0);
        repository.reset_due(1200).await.unwrap();
        let row = repository
            .find_by_provider_id("provider-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.quota_cycle_start_at_unix_secs, Some(960));
        assert_eq!(row.quota_last_reset_at_unix_secs, Some(1200));
        assert_eq!(row.monthly_used_usd, 0.0);
        // Worker delay across several periods cannot move the natural time grid.
        repository
            .reset_due(960 + 3 * 7 * 86400 + 180)
            .await
            .unwrap();
        let row = repository
            .find_by_provider_id("provider-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.quota_cycle_start_at_unix_secs,
            Some(960 + 3 * 7 * 86400)
        );
        assert_eq!(row.quota_subscription_started_at_unix_secs, Some(960));
        assert_eq!(row.quota_expires_at_unix_secs, Some(9999999));
        sqlx::query("UPDATE providers SET monthly_used_usd=4 WHERE id='provider-1'")
            .execute(&pool)
            .await
            .unwrap();
        let effective = 960 + 3 * 7 * 86400 + 360;
        repository
            .request_adjustment(
                "provider-1",
                &ProviderQuotaAdjustment {
                    effective_at_unix_secs: effective,
                    mode: ProviderQuotaResetMode::Cycle,
                    reset_usage: false,
                    cycle_days: Some(2),
                },
            )
            .await
            .unwrap();
        repository.reset_due(effective).await.unwrap();
        let row = repository
            .find_by_provider_id("provider-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.monthly_used_usd, 4.0);
        assert_eq!(row.quota_cycle_start_at_unix_secs, Some(effective));
        assert_eq!(row.quota_last_reset_at_unix_secs, Some(960 + 3 * 7 * 86400));
        assert_eq!(row.quota_reset_day, Some(2));
        assert_eq!(row.quota_subscription_started_at_unix_secs, Some(960));
        assert_eq!(row.quota_expires_at_unix_secs, Some(9999999));
        assert_eq!(repository.reset_due(effective).await.unwrap(), 0);
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
