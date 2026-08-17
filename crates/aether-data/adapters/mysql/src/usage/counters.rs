use std::collections::BTreeMap;

use aether_data_contracts::repository::usage::{
    api_key_usage_contribution, model_usage_contribution, provider_api_key_usage_contribution,
    ApiKeyLastUsedDelta, ApiKeyUsageDelta, ManagementTokenCounterDelta, ModelUsageDelta,
    ProviderApiKeyUsageDelta, ProxyNodeCounterDelta, StoredRequestUsageAudit,
    UsageCounterFlushSummary, UsageCounterHealthSnapshot, UsageCounterPendingHealthSnapshot,
};
use aether_data_contracts::DataLayerError;
use aether_wallet::{quota_clock_minute, quota_window_start_unix_secs, quota_windows_from_config};
use sqlx::{MySql, MySqlPool, QueryBuilder, Row};

use crate::error::SqlResultExt;

const KIND_API_KEY: &str = "api_key";
const KIND_PROVIDER_API_KEY: &str = "provider_api_key";
const KIND_MODEL: &str = "model";
const KIND_PROVIDER_MONTHLY: &str = "provider_monthly";
const KIND_PROXY_NODE: &str = "proxy_node";
const KIND_MANAGEMENT_TOKEN: &str = "management_token";
const KIND_API_KEY_LAST_USED: &str = "api_key_last_used";

const CLAIM_SQL: &str = r#"
SELECT
  id,
  kind,
  target_id,
  request_count_delta,
  total_requests_delta,
  success_count_delta,
  error_count_delta,
  dns_failures_delta,
  stream_errors_delta,
  total_tokens_delta,
  total_cost_usd_delta,
  total_response_time_ms_delta,
  last_used_at_unix_secs,
  last_used_ip,
  candidate_last_used_at_unix_secs,
  removed_last_used_at_unix_secs,
  usage_created_at_unix_secs,
  provider_billing_type_at_usage,
  quota_epoch_start_at_usage,
  provider_dispatch_at_unix_secs,
  provider_quota_cost_usd,
  quota_delta_sequence,
  quota_accounting_status,
  created_at
FROM usage_counter_deltas
WHERE processed_at IS NULL
ORDER BY created_at ASC, id ASC
LIMIT ?
FOR UPDATE SKIP LOCKED
"#;

struct DeltaRow {
    id: String,
    kind: String,
    target_id: String,
    request_count_delta: i64,
    total_requests_delta: i64,
    success_count_delta: i64,
    error_count_delta: i64,
    dns_failures_delta: i64,
    stream_errors_delta: i64,
    total_tokens_delta: i64,
    total_cost_usd_delta: f64,
    total_response_time_ms_delta: i64,
    last_used_at_unix_secs: Option<u64>,
    last_used_ip: Option<String>,
    candidate_last_used_at_unix_secs: Option<u64>,
    removed_last_used_at_unix_secs: Option<u64>,
    usage_created_at_unix_secs: Option<u64>,
    provider_billing_type_at_usage: Option<String>,
    quota_epoch_start_at_usage: Option<u64>,
    provider_dispatch_at_unix_secs: Option<u64>,
    provider_quota_cost_usd: Option<f64>,
    quota_delta_sequence: Option<i64>,
    quota_accounting_status: Option<String>,
    created_at_unix_secs: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProviderMonthlyDelta {
    provider_quota_cost_usd: f64,
    quota_epoch_start_unix_secs: u64,
    provider_dispatch_at_unix_secs: u64,
    accounting_ready: bool,
}

#[derive(Default)]
struct Aggregates {
    api_keys: BTreeMap<String, ApiKeyUsageDelta>,
    provider_api_keys: BTreeMap<String, ProviderApiKeyUsageDelta>,
    models: BTreeMap<String, ModelUsageDelta>,
    provider_monthly: BTreeMap<String, Vec<ProviderMonthlyDelta>>,
    proxy_nodes: BTreeMap<String, ProxyNodeCounterDelta>,
    management_tokens: BTreeMap<String, ManagementTokenCounterDelta>,
    api_key_last_used: BTreeMap<String, ApiKeyLastUsedDelta>,
}

impl Aggregates {
    fn from_rows(rows: &[DeltaRow]) -> Result<Self, DataLayerError> {
        let mut aggregates = Self::default();
        for row in rows {
            if !row.total_cost_usd_delta.is_finite() {
                return Err(DataLayerError::UnexpectedValue(format!(
                    "usage_counter_deltas.total_cost_usd_delta is not finite for {}",
                    row.id
                )));
            }
            match row.kind.as_str() {
                KIND_API_KEY => {
                    let entry = aggregates
                        .api_keys
                        .entry(row.target_id.clone())
                        .or_default();
                    entry.total_requests += row.total_requests_delta;
                    entry.total_tokens += row.total_tokens_delta;
                    entry.total_cost_usd += row.total_cost_usd_delta;
                    merge_optional_max(
                        &mut entry.candidate_last_used_at_unix_secs,
                        row.candidate_last_used_at_unix_secs,
                    );
                    merge_optional_max(
                        &mut entry.removed_last_used_at_unix_secs,
                        row.removed_last_used_at_unix_secs,
                    );
                }
                KIND_PROVIDER_API_KEY => {
                    let entry = aggregates
                        .provider_api_keys
                        .entry(row.target_id.clone())
                        .or_default();
                    entry.request_count += row.request_count_delta;
                    entry.success_count += row.success_count_delta;
                    entry.error_count += row.error_count_delta;
                    entry.total_tokens += row.total_tokens_delta;
                    entry.total_cost_usd += row.total_cost_usd_delta;
                    entry.total_response_time_ms += row.total_response_time_ms_delta;
                    merge_optional_max(
                        &mut entry.candidate_last_used_at_unix_secs,
                        row.candidate_last_used_at_unix_secs,
                    );
                    merge_optional_max(
                        &mut entry.removed_last_used_at_unix_secs,
                        row.removed_last_used_at_unix_secs,
                    );
                    merge_optional_max(
                        &mut entry.usage_created_at_unix_secs,
                        row.usage_created_at_unix_secs,
                    );
                }
                KIND_MODEL => {
                    aggregates
                        .models
                        .entry(row.target_id.clone())
                        .or_default()
                        .request_count += row.request_count_delta;
                }
                KIND_PROVIDER_MONTHLY => {
                    if row
                        .provider_billing_type_at_usage
                        .as_deref()
                        .map_or(true, |value| !value.eq_ignore_ascii_case("monthly_quota"))
                    {
                        continue;
                    }
                    let Some(quota_epoch_start_unix_secs) = row.quota_epoch_start_at_usage else {
                        continue;
                    };
                    aggregates
                        .provider_monthly
                        .entry(row.target_id.clone())
                        .or_default()
                        .push(ProviderMonthlyDelta {
                            provider_quota_cost_usd: row
                                .provider_quota_cost_usd
                                .unwrap_or(row.total_cost_usd_delta),
                            quota_epoch_start_unix_secs,
                            provider_dispatch_at_unix_secs: row
                                .provider_dispatch_at_unix_secs
                                .or(row.usage_created_at_unix_secs)
                                .unwrap_or(row.created_at_unix_secs),
                            accounting_ready: row.quota_accounting_status.as_deref()
                                == Some("ready"),
                        });
                }
                KIND_PROXY_NODE => {
                    let entry = aggregates
                        .proxy_nodes
                        .entry(row.target_id.clone())
                        .or_insert(ProxyNodeCounterDelta {
                            node_id: row.target_id.clone(),
                            total_requests_delta: 0,
                            failed_requests_delta: 0,
                            dns_failures_delta: 0,
                            stream_errors_delta: 0,
                        });
                    entry.total_requests_delta += row.total_requests_delta;
                    entry.failed_requests_delta += row.error_count_delta;
                    entry.dns_failures_delta += row.dns_failures_delta;
                    entry.stream_errors_delta += row.stream_errors_delta;
                }
                KIND_MANAGEMENT_TOKEN => {
                    let entry = aggregates
                        .management_tokens
                        .entry(row.target_id.clone())
                        .or_insert(ManagementTokenCounterDelta {
                            token_id: row.target_id.clone(),
                            usage_count_delta: 0,
                            last_used_at_unix_secs: None,
                            last_used_ip: None,
                        });
                    entry.usage_count_delta += row.request_count_delta;
                    merge_latest_timestamp_with_value(
                        &mut entry.last_used_at_unix_secs,
                        &mut entry.last_used_ip,
                        row.last_used_at_unix_secs,
                        row.last_used_ip.clone(),
                    );
                }
                KIND_API_KEY_LAST_USED => {
                    let Some(last_used_at_unix_secs) = row.last_used_at_unix_secs else {
                        continue;
                    };
                    let entry = aggregates
                        .api_key_last_used
                        .entry(row.target_id.clone())
                        .or_insert(ApiKeyLastUsedDelta {
                            api_key_id: row.target_id.clone(),
                            last_used_at_unix_secs,
                        });
                    if last_used_at_unix_secs > entry.last_used_at_unix_secs {
                        entry.last_used_at_unix_secs = last_used_at_unix_secs;
                    }
                }
                other => {
                    return Err(DataLayerError::UnexpectedValue(format!(
                        "unknown usage counter delta kind: {other}"
                    )));
                }
            }
        }
        Ok(aggregates)
    }
}

pub(super) async fn flush(
    pool: &MySqlPool,
    batch_size: usize,
) -> Result<UsageCounterFlushSummary, DataLayerError> {
    if batch_size == 0 {
        return Ok(UsageCounterFlushSummary::default());
    }
    let limit = i64::try_from(batch_size).map_err(|_| {
        DataLayerError::InvalidInput(format!(
            "usage counter flush batch size is out of range: {batch_size}"
        ))
    })?;

    let mut tx = pool.begin().await.map_sql_err()?;
    let rows = sqlx::query(CLAIM_SQL)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?
        .iter()
        .map(map_row)
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        tx.rollback().await.map_sql_err()?;
        return Ok(UsageCounterFlushSummary::default());
    }

    let aggregates = Aggregates::from_rows(&rows)?;
    for (target_id, delta) in &aggregates.api_keys {
        apply_api_key(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.models {
        apply_model(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.provider_api_keys {
        apply_provider_api_key(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.provider_monthly {
        apply_provider_monthly(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.proxy_nodes {
        apply_proxy_node(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.management_tokens {
        apply_management_token(&mut tx, target_id, delta).await?;
    }
    for (target_id, delta) in &aggregates.api_key_last_used {
        apply_api_key_last_used(&mut tx, target_id, delta).await?;
    }

    let now = current_unix_secs();
    let mut quota_watermarks = BTreeMap::<(String, u64), i64>::new();
    for row in &rows {
        if row.kind == KIND_PROVIDER_MONTHLY {
            if let (Some(epoch), Some(sequence)) =
                (row.quota_epoch_start_at_usage, row.quota_delta_sequence)
            {
                quota_watermarks
                    .entry((row.target_id.clone(), epoch))
                    .and_modify(|value| *value = (*value).max(sequence))
                    .or_insert(sequence);
            }
        }
    }
    for ((provider_id, epoch), sequence) in quota_watermarks {
        sqlx::query(
            r#"
INSERT INTO provider_quota_applied_watermarks (
  provider_id, quota_epoch_start, applied_delta_sequence, updated_at
) VALUES (?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  applied_delta_sequence = GREATEST(applied_delta_sequence, VALUES(applied_delta_sequence)),
  updated_at = VALUES(updated_at)
"#,
        )
        .bind(provider_id)
        .bind(epoch as i64)
        .bind(sequence)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
    }
    let mut mark = QueryBuilder::<MySql>::new("UPDATE usage_counter_deltas SET processed_at = ");
    mark.push_bind(now).push(" WHERE id IN (");
    {
        let mut ids = mark.separated(", ");
        for row in &rows {
            ids.push_bind(&row.id);
        }
    }
    mark.push(")");
    mark.build().execute(&mut *tx).await.map_sql_err()?;
    tx.commit().await.map_sql_err()?;

    Ok(UsageCounterFlushSummary {
        rows_claimed: rows.len(),
        api_key_targets: aggregates.api_keys.len(),
        provider_api_key_targets: aggregates.provider_api_keys.len(),
        model_targets: aggregates.models.len(),
        provider_monthly_targets: aggregates.provider_monthly.len(),
        proxy_node_targets: aggregates.proxy_nodes.len(),
        management_token_targets: aggregates.management_tokens.len(),
        api_key_last_used_targets: aggregates.api_key_last_used.len(),
    })
}

pub(super) async fn maintain_provider_quota_windows(
    pool: &MySqlPool,
    now_unix_secs: u64,
) -> Result<usize, DataLayerError> {
    let clock_minute = quota_clock_minute(now_unix_secs);
    let providers = sqlx::query(
        "SELECT id, quota_last_reset_at, config FROM providers WHERE quota_last_reset_at IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_sql_err()?;
    let mut maintained = 0usize;
    for provider in providers {
        let provider_id: String = provider.try_get("id").map_sql_err()?;
        let epoch = quota_clock_minute(
            provider
                .try_get::<i64, _>("quota_last_reset_at")
                .map_sql_err()?
                .max(0) as u64,
        );
        let config = provider
            .try_get::<Option<String>, _>("config")
            .map_sql_err()?
            .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
        let windows = quota_windows_from_config(config.as_ref());
        let mut tx = pool.begin().await.map_sql_err()?;
        if !process_provider_quota_backfill_mysql(
            &mut tx,
            &provider_id,
            epoch,
            clock_minute,
            now_unix_secs,
            2_000,
        )
        .await?
        {
            tx.commit().await.map_sql_err()?;
            continue;
        }
        sqlx::query(
            r#"
UPDATE provider_quota_window_counters
SET status = 'rebuilding', rebuild_error = NULL, updated_at = ?
WHERE provider_id = ?
  AND status = 'failed'
  AND rebuild_error = 'quota cost is unavailable for a dispatched monthly request'
  AND NOT EXISTS (
    SELECT 1
    FROM usage_counter_deltas
    WHERE kind = 'provider_monthly'
      AND target_id = ?
      AND quota_epoch_start_at_usage = ?
      AND quota_accounting_status IN ('pending', 'failed')
  )
"#,
        )
        .bind(now_unix_secs as i64)
        .bind(&provider_id)
        .bind(&provider_id)
        .bind(epoch as i64)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        let durations = sqlx::query_scalar::<_, i64>(
            "SELECT duration_secs FROM provider_quota_window_counters WHERE provider_id = ? FOR UPDATE",
        )
        .bind(&provider_id)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        for duration in durations {
            if !windows
                .iter()
                .any(|window| window.duration_secs == duration.max(0) as u64)
            {
                sqlx::query(
                    "DELETE FROM provider_quota_window_counters WHERE provider_id = ? AND duration_secs = ?",
                )
                .bind(&provider_id)
                .bind(duration)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            }
        }
        for window in &windows {
            maintain_mysql_window(
                &mut tx,
                &provider_id,
                epoch,
                window.duration_secs,
                clock_minute,
                now_unix_secs,
            )
            .await?;
            maintained += 1;
        }
        tx.commit().await.map_sql_err()?;
    }
    Ok(maintained)
}

async fn process_provider_quota_backfill_mysql(
    tx: &mut sqlx::Transaction<'_, MySql>,
    provider_id: &str,
    epoch: u64,
    clock_minute: u64,
    now_unix_secs: u64,
    batch_size: usize,
) -> Result<bool, DataLayerError> {
    let task = sqlx::query(
        r#"
SELECT status, cursor_dispatch_at, cursor_request_id, cutover_delta_sequence, unknown_rows
FROM provider_quota_maintenance_state
WHERE provider_id = ? AND quota_epoch_start = ? AND task_kind = 'historical_backfill'
LIMIT 1 FOR UPDATE
"#,
    )
    .bind(provider_id)
    .bind(epoch as i64)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    let Some(task) = task else {
        return Ok(true);
    };
    let status: String = task.try_get("status").map_sql_err()?;
    if status == "complete" {
        return Ok(true);
    }
    if status == "failed" {
        return Ok(false);
    }
    let cursor_dispatch_at: i64 = task.try_get("cursor_dispatch_at").map_sql_err()?;
    let cursor_request_id: String = task.try_get("cursor_request_id").map_sql_err()?;
    let cutover_sequence = task
        .try_get::<Option<i64>, _>("cutover_delta_sequence")
        .map_sql_err()?;
    let lock_owner = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "UPDATE provider_quota_maintenance_state SET status = 'running', lock_owner = ?, lock_expires_at = ?, updated_at = ? WHERE provider_id = ? AND quota_epoch_start = ? AND task_kind = 'historical_backfill'",
    )
    .bind(&lock_owner)
    .bind(now_unix_secs.saturating_add(30) as i64)
    .bind(now_unix_secs as i64)
    .bind(provider_id)
    .bind(epoch as i64)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    if status == "pending" {
        sqlx::query(
            "DELETE FROM provider_quota_usage_buckets WHERE provider_id = ? AND quota_epoch_start = ?",
        )
        .bind(provider_id)
        .bind(epoch as i64)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
        sqlx::query(
            "UPDATE providers SET monthly_used_usd = 0, updated_at = ? WHERE id = ? AND quota_last_reset_at = ?",
        )
        .bind(now_unix_secs as i64)
        .bind(provider_id)
        .bind(epoch as i64)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    }
    let rows = sqlx::query(
        r#"
WITH candidate_rows AS (
  SELECT
    usage_record.request_id,
    usage_record.created_at_unix_ms,
    COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(candidate.extra_data, '$.provider_quota_dispatch_snapshot.provider_dispatch_at_unix_secs')) AS SIGNED),
      CAST(FLOOR(candidate.started_at / 1000) AS SIGNED)
    ) AS dispatch_at,
    CAST(COALESCE(
      JSON_UNQUOTE(JSON_EXTRACT(candidate.extra_data, '$.provider_quota_dispatch_snapshot.provider_billing_type_at_usage')),
      JSON_UNQUOTE(JSON_EXTRACT(snapshot.settlement_snapshot, '$.pricing_snapshot.provider_billing_type'))
    ) AS CHAR) AS billing_type_at_usage,
    COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(snapshot.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
      snapshot.billing_actual_total_cost_usd,
      usage_record.actual_total_cost_usd
    ) AS provider_quota_cost_usd,
    (
      SELECT MIN(delta.quota_delta_sequence)
      FROM usage_counter_deltas AS delta
      WHERE delta.kind = 'provider_monthly'
        AND delta.target_id = usage_record.provider_id
        AND delta.request_id = routing.candidate_id
    ) AS attempt_sequence
  FROM `usage` AS usage_record
  LEFT JOIN usage_routing_snapshots AS routing ON routing.request_id = usage_record.request_id
  LEFT JOIN request_candidates AS candidate ON candidate.id = routing.candidate_id
  LEFT JOIN usage_settlement_snapshots AS snapshot ON snapshot.request_id = usage_record.request_id
  WHERE usage_record.provider_id = ?
    AND usage_record.finalized_at IS NOT NULL
), backfill_rows AS (
  SELECT
    request_id,
    COALESCE(dispatch_at, created_at_unix_ms) AS scan_at,
    dispatch_at,
    billing_type_at_usage,
    provider_quota_cost_usd,
    attempt_sequence
  FROM candidate_rows
)
SELECT *
FROM backfill_rows
WHERE scan_at >= ?
  AND scan_at < ?
  AND (
    scan_at > ?
    OR (scan_at = ? AND request_id > ?)
  )
ORDER BY scan_at, request_id
LIMIT ?
"#,
    )
    .bind(provider_id)
    .bind(epoch as i64)
    .bind(clock_minute as i64)
    .bind(cursor_dispatch_at)
    .bind(cursor_dispatch_at)
    .bind(&cursor_request_id)
    .bind(batch_size as i64)
    .fetch_all(&mut **tx)
    .await
    .map_sql_err()?;
    if rows.is_empty() {
        let pending_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_counter_deltas WHERE kind = 'provider_monthly' AND target_id = ? AND quota_epoch_start_at_usage = ? AND quota_accounting_status IN ('pending', 'failed') AND (? IS NULL OR quota_delta_sequence <= ?)",
        )
        .bind(provider_id)
        .bind(epoch as i64)
        .bind(cutover_sequence)
        .bind(cutover_sequence)
        .fetch_one(&mut **tx)
        .await
        .map_sql_err()?;
        if pending_count > 0 {
            sqlx::query("UPDATE provider_quota_maintenance_state SET lock_owner = NULL, lock_expires_at = NULL, updated_at = ? WHERE provider_id = ? AND quota_epoch_start = ? AND task_kind = 'historical_backfill'")
                .bind(now_unix_secs as i64)
                .bind(provider_id)
                .bind(epoch as i64)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
            return Ok(false);
        }
        if let Some(cutover_sequence) = cutover_sequence {
            sqlx::query("UPDATE usage_counter_deltas SET processed_at = COALESCE(processed_at, ?), quota_accounting_status = 'backfill_absorbed' WHERE kind = 'provider_monthly' AND target_id = ? AND quota_epoch_start_at_usage = ? AND quota_delta_sequence <= ?")
                .bind(now_unix_secs as i64)
                .bind(provider_id)
                .bind(epoch as i64)
                .bind(cutover_sequence)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
            sqlx::query("INSERT INTO provider_quota_applied_watermarks (provider_id, quota_epoch_start, applied_delta_sequence, updated_at) VALUES (?, ?, ?, ?) ON DUPLICATE KEY UPDATE applied_delta_sequence = GREATEST(applied_delta_sequence, VALUES(applied_delta_sequence)), updated_at = VALUES(updated_at)")
                .bind(provider_id)
                .bind(epoch as i64)
                .bind(cutover_sequence)
                .bind(now_unix_secs as i64)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
        }
        let total_used = sqlx::query_scalar::<_, f64>(
            "SELECT COALESCE(SUM(used_usd), 0) FROM provider_quota_usage_buckets WHERE provider_id = ? AND quota_epoch_start = ?",
        )
        .bind(provider_id)
        .bind(epoch as i64)
        .fetch_one(&mut **tx)
        .await
        .map_sql_err()?;
        sqlx::query("UPDATE providers SET monthly_used_usd = ?, updated_at = ? WHERE id = ? AND quota_last_reset_at = ?")
            .bind(total_used)
            .bind(now_unix_secs as i64)
            .bind(provider_id)
            .bind(epoch as i64)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        let unknown_rows: i64 = task.try_get("unknown_rows").map_sql_err()?;
        let (final_status, last_error) = if unknown_rows == 0 {
            ("complete", None)
        } else {
            (
                "failed",
                Some("historical usage is missing an explicit dispatch billing or cost snapshot"),
            )
        };
        sqlx::query("UPDATE provider_quota_maintenance_state SET status = ?, absorbed_delta_sequence = COALESCE(cutover_delta_sequence, 0), lock_owner = NULL, lock_expires_at = NULL, last_error = ?, updated_at = ? WHERE provider_id = ? AND quota_epoch_start = ? AND task_kind = 'historical_backfill'")
            .bind(final_status)
            .bind(last_error)
            .bind(now_unix_secs as i64)
            .bind(provider_id)
            .bind(epoch as i64)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        if final_status == "failed" {
            sqlx::query("UPDATE provider_quota_window_counters SET status = 'failed', rebuild_error = ?, updated_at = ? WHERE provider_id = ? AND quota_epoch_start = ?")
                .bind(last_error)
                .bind(now_unix_secs as i64)
                .bind(provider_id)
                .bind(epoch as i64)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
            return Ok(false);
        }
        return Ok(true);
    }
    let mut buckets = BTreeMap::<u64, f64>::new();
    let mut included = 0i64;
    let mut excluded_payg = 0i64;
    let mut excluded_free = 0i64;
    let mut unknown = 0i64;
    let mut last_scan_at = cursor_dispatch_at;
    let mut last_request_id = cursor_request_id;
    for row in rows {
        let request_id: String = row.try_get("request_id").map_sql_err()?;
        let scan_at: i64 = row.try_get("scan_at").map_sql_err()?;
        last_scan_at = scan_at;
        last_request_id = request_id;
        let attempt_sequence = row
            .try_get::<Option<i64>, _>("attempt_sequence")
            .map_sql_err()?;
        if cutover_sequence
            .is_some_and(|cutover| attempt_sequence.is_some_and(|sequence| sequence > cutover))
        {
            continue;
        }
        let billing_type = row
            .try_get::<Option<String>, _>("billing_type_at_usage")
            .map_sql_err()?;
        match billing_type.as_deref() {
            Some("pay_as_you_go") => excluded_payg += 1,
            Some("free_tier") => excluded_free += 1,
            Some("monthly_quota") => {
                let dispatch_at = row.try_get::<Option<i64>, _>("dispatch_at").map_sql_err()?;
                let cost = row
                    .try_get::<Option<f64>, _>("provider_quota_cost_usd")
                    .map_sql_err()?;
                match (dispatch_at, cost) {
                    (Some(dispatch_at), Some(cost))
                        if dispatch_at >= epoch as i64
                            && dispatch_at < clock_minute as i64
                            && cost.is_finite()
                            && cost >= 0.0 =>
                    {
                        *buckets
                            .entry(quota_clock_minute(dispatch_at as u64))
                            .or_default() += cost;
                        included += 1;
                    }
                    _ => unknown += 1,
                }
            }
            _ => unknown += 1,
        }
    }
    for (bucket_start, used_usd) in buckets {
        sqlx::query("INSERT INTO provider_quota_usage_buckets (provider_id, quota_epoch_start, bucket_start, used_usd, updated_at) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE used_usd = used_usd + VALUES(used_usd), updated_at = VALUES(updated_at)")
            .bind(provider_id)
            .bind(epoch as i64)
            .bind(bucket_start as i64)
            .bind(used_usd)
            .bind(now_unix_secs as i64)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
    }
    sqlx::query("UPDATE provider_quota_maintenance_state SET cursor_dispatch_at = ?, cursor_request_id = ?, included_rows = included_rows + ?, excluded_payg_rows = excluded_payg_rows + ?, excluded_free_tier_rows = excluded_free_tier_rows + ?, unknown_rows = unknown_rows + ?, lock_owner = NULL, lock_expires_at = NULL, updated_at = ? WHERE provider_id = ? AND quota_epoch_start = ? AND task_kind = 'historical_backfill'")
        .bind(last_scan_at)
        .bind(last_request_id)
        .bind(included)
        .bind(excluded_payg)
        .bind(excluded_free)
        .bind(unknown)
        .bind(now_unix_secs as i64)
        .bind(provider_id)
        .bind(epoch as i64)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    Ok(false)
}

async fn maintain_mysql_window(
    tx: &mut sqlx::Transaction<'_, MySql>,
    provider_id: &str,
    epoch: u64,
    duration_secs: u64,
    clock_minute: u64,
    now_unix_secs: u64,
) -> Result<(), DataLayerError> {
    let desired_start = quota_window_start_unix_secs(clock_minute, Some(epoch), duration_secs);
    let existing = sqlx::query(
        "SELECT quota_epoch_start FROM provider_quota_window_counters WHERE provider_id = ? AND duration_secs = ? FOR UPDATE",
    )
    .bind(provider_id)
    .bind(duration_secs as i64)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    match existing {
        None => {
            sqlx::query(
                r#"
INSERT INTO provider_quota_window_counters (
  provider_id, duration_secs, quota_epoch_start, rolling_start,
  accounted_until, used_usd, status, rebuild_error, updated_at
) VALUES (?, ?, ?, ?, ?, 0, 'rebuilding', NULL, ?)
"#,
            )
            .bind(provider_id)
            .bind(duration_secs as i64)
            .bind(epoch as i64)
            .bind(desired_start as i64)
            .bind(clock_minute as i64)
            .bind(now_unix_secs as i64)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        }
        Some(row) if row.try_get::<i64, _>("quota_epoch_start").map_sql_err()? != epoch as i64 => {
            sqlx::query(
                r#"
UPDATE provider_quota_window_counters
SET quota_epoch_start = ?, rolling_start = ?, accounted_until = ?, used_usd = 0,
    status = 'rebuilding', rebuild_error = NULL, updated_at = ?
WHERE provider_id = ? AND duration_secs = ?
"#,
            )
            .bind(epoch as i64)
            .bind(desired_start as i64)
            .bind(clock_minute as i64)
            .bind(now_unix_secs as i64)
            .bind(provider_id)
            .bind(duration_secs as i64)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        }
        Some(_) => {}
    }

    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM provider_quota_window_counters WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ?",
    )
    .bind(provider_id)
    .bind(duration_secs as i64)
    .bind(epoch as i64)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    if status.as_deref() == Some("rebuilding") {
        let used: f64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(used_usd), 0) AS DOUBLE) FROM provider_quota_usage_buckets WHERE provider_id = ? AND quota_epoch_start = ? AND bucket_start >= ? AND bucket_start < ?",
        )
        .bind(provider_id)
        .bind(epoch as i64)
        .bind(desired_start as i64)
        .bind(clock_minute as i64)
        .fetch_one(&mut **tx)
        .await
        .map_sql_err()?;
        sqlx::query(
            "UPDATE provider_quota_window_counters SET rolling_start = ?, accounted_until = ?, used_usd = ?, status = 'ready', rebuild_error = NULL, updated_at = ? WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ? AND status = 'rebuilding'",
        )
        .bind(desired_start as i64)
        .bind(clock_minute as i64)
        .bind(used)
        .bind(now_unix_secs as i64)
        .bind(provider_id)
        .bind(duration_secs as i64)
        .bind(epoch as i64)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    }

    let row = sqlx::query(
        "SELECT rolling_start, accounted_until, used_usd FROM provider_quota_window_counters WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ? AND status = 'ready' FOR UPDATE",
    )
    .bind(provider_id)
    .bind(duration_secs as i64)
    .bind(epoch as i64)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    let Some(row) = row else {
        return Ok(());
    };
    let rolling_start = row.try_get::<i64, _>("rolling_start").map_sql_err()?.max(0) as u64;
    let accounted_until = row
        .try_get::<i64, _>("accounted_until")
        .map_sql_err()?
        .max(0) as u64;
    let mut used: f64 = row.try_get("used_usd").map_sql_err()?;
    if accounted_until < clock_minute {
        let added: f64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(used_usd), 0) AS DOUBLE) FROM provider_quota_usage_buckets WHERE provider_id = ? AND quota_epoch_start = ? AND bucket_start >= ? AND bucket_start < ?",
        )
        .bind(provider_id).bind(epoch as i64).bind(accounted_until as i64).bind(clock_minute as i64)
        .fetch_one(&mut **tx).await.map_sql_err()?;
        used += added;
        sqlx::query("UPDATE provider_quota_window_counters SET used_usd = ?, accounted_until = ?, updated_at = ? WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ? AND status = 'ready' AND accounted_until = ?")
            .bind(used).bind(clock_minute as i64).bind(now_unix_secs as i64).bind(provider_id)
            .bind(duration_secs as i64).bind(epoch as i64).bind(accounted_until as i64)
            .execute(&mut **tx).await.map_sql_err()?;
    }
    if desired_start > rolling_start && clock_minute >= desired_start {
        let removed: f64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(used_usd), 0) AS DOUBLE) FROM provider_quota_usage_buckets WHERE provider_id = ? AND quota_epoch_start = ? AND bucket_start >= ? AND bucket_start < ?",
        )
        .bind(provider_id).bind(epoch as i64).bind(rolling_start as i64).bind(desired_start as i64)
        .fetch_one(&mut **tx).await.map_sql_err()?;
        sqlx::query("UPDATE provider_quota_window_counters SET used_usd = GREATEST(? - ?, 0), rolling_start = ?, updated_at = ? WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ? AND status = 'ready' AND rolling_start = ?")
            .bind(used).bind(removed).bind(desired_start as i64).bind(now_unix_secs as i64)
            .bind(provider_id).bind(duration_secs as i64).bind(epoch as i64).bind(rolling_start as i64)
            .execute(&mut **tx).await.map_sql_err()?;
    }
    Ok(())
}

pub(super) async fn enqueue_proxy_node(
    pool: &MySqlPool,
    delta: ProxyNodeCounterDelta,
) -> Result<bool, DataLayerError> {
    if delta.is_noop() {
        return Ok(false);
    }
    let node_id = delta.node_id.trim().to_string();
    let request_id = format!("proxy_node:{node_id}:{}", uuid::Uuid::new_v4());
    let mut tx = pool.begin().await.map_sql_err()?;
    insert_delta(
        &mut tx,
        DeltaInsert {
            request_id: &request_id,
            kind: KIND_PROXY_NODE,
            target_id: &node_id,
            total_requests_delta: delta.total_requests_delta,
            error_count_delta: delta.failed_requests_delta,
            dns_failures_delta: delta.dns_failures_delta,
            stream_errors_delta: delta.stream_errors_delta,
            ..DeltaInsert::default()
        },
    )
    .await?;
    tx.commit().await.map_sql_err()?;
    Ok(true)
}

pub(super) async fn enqueue_management_token(
    pool: &MySqlPool,
    delta: ManagementTokenCounterDelta,
) -> Result<bool, DataLayerError> {
    if delta.is_noop() {
        return Ok(false);
    }
    let token_id = delta.token_id.trim().to_string();
    let last_used_ip = delta
        .last_used_ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let last_used_at = delta
        .last_used_at_unix_secs
        .unwrap_or_else(|| current_unix_secs().max(0) as u64);
    let request_id = format!("management_token:{token_id}:{}", uuid::Uuid::new_v4());
    let mut tx = pool.begin().await.map_sql_err()?;
    insert_delta(
        &mut tx,
        DeltaInsert {
            request_id: &request_id,
            kind: KIND_MANAGEMENT_TOKEN,
            target_id: &token_id,
            request_count_delta: delta.usage_count_delta,
            last_used_at_unix_secs: Some(last_used_at),
            last_used_ip: last_used_ip.as_deref(),
            ..DeltaInsert::default()
        },
    )
    .await?;
    tx.commit().await.map_sql_err()?;
    Ok(true)
}

pub(super) async fn enqueue_api_key_last_used(
    pool: &MySqlPool,
    delta: ApiKeyLastUsedDelta,
) -> Result<bool, DataLayerError> {
    if delta.is_noop() {
        return Ok(false);
    }
    let api_key_id = delta.api_key_id.trim().to_string();
    let request_id = format!("api_key_last_used:{api_key_id}:{}", uuid::Uuid::new_v4());
    let mut tx = pool.begin().await.map_sql_err()?;
    insert_delta(
        &mut tx,
        DeltaInsert {
            request_id: &request_id,
            kind: KIND_API_KEY_LAST_USED,
            target_id: &api_key_id,
            last_used_at_unix_secs: Some(delta.last_used_at_unix_secs),
            ..DeltaInsert::default()
        },
    )
    .await?;
    tx.commit().await.map_sql_err()?;
    Ok(true)
}

pub(super) async fn cleanup_processed(
    pool: &MySqlPool,
    cutoff_unix_secs: u64,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    if batch_size == 0 {
        return Ok(0);
    }
    let cutoff = to_i64(cutoff_unix_secs, "usage counter cleanup cutoff")?;
    let limit = i64::try_from(batch_size).map_err(|_| {
        DataLayerError::InvalidInput(format!(
            "usage counter cleanup batch size is out of range: {batch_size}"
        ))
    })?;
    let deleted = sqlx::query(
        r#"
DELETE FROM usage_counter_deltas
WHERE id IN (
  SELECT id FROM (
    SELECT id
    FROM usage_counter_deltas
    WHERE processed_at IS NOT NULL AND processed_at < ?
      AND NOT (
        kind = 'provider_monthly'
        AND quota_accounting_status IN ('pending', 'failed')
        AND EXISTS (
          SELECT 1 FROM providers AS current_provider
          WHERE current_provider.id = usage_counter_deltas.target_id
            AND (current_provider.quota_last_reset_at DIV 60) * 60
              = usage_counter_deltas.quota_epoch_start_at_usage
        )
      )
      AND (
        kind <> 'provider_monthly'
        OR EXISTS (
          SELECT 1 FROM provider_quota_applied_watermarks AS watermark
          WHERE watermark.provider_id = usage_counter_deltas.target_id
            AND watermark.quota_epoch_start = usage_counter_deltas.quota_epoch_start_at_usage
            AND watermark.applied_delta_sequence >= usage_counter_deltas.quota_delta_sequence
        )
      )
    ORDER BY processed_at ASC, created_at ASC, id ASC
    LIMIT ?
  ) AS doomed
)
"#,
    )
    .bind(cutoff)
    .bind(limit)
    .execute(pool)
    .await
    .map_sql_err()?
    .rows_affected();
    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

pub(super) async fn read_health(
    pool: &MySqlPool,
) -> Result<UsageCounterHealthSnapshot, DataLayerError> {
    let row = sqlx::query(
        r#"
SELECT
  (SELECT CAST(COUNT(*) AS SIGNED) FROM usage_counter_deltas WHERE processed_at IS NULL)
    AS pending_rows,
  (SELECT CAST(COUNT(*) AS SIGNED) FROM usage_counter_deltas WHERE processed_at IS NOT NULL)
    AS processed_rows,
  (SELECT MIN(created_at) FROM usage_counter_deltas WHERE processed_at IS NULL)
    AS oldest_pending_created_at_unix_secs,
  (SELECT MAX(processed_at) FROM usage_counter_deltas WHERE processed_at IS NOT NULL)
    AS latest_processed_at_unix_secs
"#,
    )
    .fetch_one(pool)
    .await
    .map_sql_err()?;
    let mut snapshot = UsageCounterHealthSnapshot {
        pending_rows: nonnegative_u64(row.try_get("pending_rows").map_sql_err()?),
        processed_rows: nonnegative_u64(row.try_get("processed_rows").map_sql_err()?),
        oldest_pending_created_at_unix_secs: optional_nonnegative_u64(
            row.try_get("oldest_pending_created_at_unix_secs")
                .map_sql_err()?,
        ),
        latest_processed_at_unix_secs: optional_nonnegative_u64(
            row.try_get("latest_processed_at_unix_secs").map_sql_err()?,
        ),
        pending_by_kind: BTreeMap::new(),
    };
    for row in pending_health_rows(pool).await? {
        snapshot.pending_by_kind.insert(row.0, row.1);
    }
    Ok(snapshot)
}

pub(super) async fn read_pending_health(
    pool: &MySqlPool,
) -> Result<UsageCounterPendingHealthSnapshot, DataLayerError> {
    let mut snapshot = UsageCounterPendingHealthSnapshot::default();
    for (kind, pending_rows, oldest) in pending_health_rows(pool).await? {
        snapshot.pending_rows = snapshot.pending_rows.saturating_add(pending_rows);
        if let Some(oldest) = oldest {
            snapshot.oldest_pending_created_at_unix_secs = Some(
                snapshot
                    .oldest_pending_created_at_unix_secs
                    .map_or(oldest, |current| current.min(oldest)),
            );
        }
        snapshot.pending_by_kind.insert(kind, pending_rows);
    }
    Ok(snapshot)
}

async fn pending_health_rows(
    pool: &MySqlPool,
) -> Result<Vec<(String, u64, Option<u64>)>, DataLayerError> {
    let rows = sqlx::query(
        r#"
SELECT
  kind,
  CAST(COUNT(*) AS SIGNED) AS pending_rows,
  MIN(created_at) AS oldest_pending_created_at_unix_secs
FROM usage_counter_deltas
WHERE processed_at IS NULL
GROUP BY kind
ORDER BY kind ASC
"#,
    )
    .fetch_all(pool)
    .await
    .map_sql_err()?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("kind").map_sql_err()?,
                nonnegative_u64(row.try_get("pending_rows").map_sql_err()?),
                optional_nonnegative_u64(
                    row.try_get("oldest_pending_created_at_unix_secs")
                        .map_sql_err()?,
                ),
            ))
        })
        .collect()
}

pub(super) async fn enqueue_usage_transition(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
    before: Option<&StoredRequestUsageAudit>,
    after: &StoredRequestUsageAudit,
) -> Result<(), DataLayerError> {
    let before_api_key = before.and_then(api_key_usage_contribution);
    let after_api_key = api_key_usage_contribution(after);
    match (before_api_key.as_ref(), after_api_key.as_ref()) {
        (Some(before), Some(after)) if before.api_key_id == after.api_key_id => {
            enqueue_api_key_delta(
                tx,
                request_id,
                &before.api_key_id,
                &ApiKeyUsageDelta::between(before, after),
            )
            .await?;
        }
        _ => {
            if let Some(before) = before_api_key.as_ref() {
                enqueue_api_key_delta(
                    tx,
                    request_id,
                    &before.api_key_id,
                    &ApiKeyUsageDelta::removal(before),
                )
                .await?;
            }
            if let Some(after) = after_api_key.as_ref() {
                enqueue_api_key_delta(
                    tx,
                    request_id,
                    &after.api_key_id,
                    &ApiKeyUsageDelta::addition(after),
                )
                .await?;
            }
        }
    }

    let before_model = before.and_then(model_usage_contribution);
    let after_model = model_usage_contribution(after);
    match (before_model.as_ref(), after_model.as_ref()) {
        (Some(before), Some(after)) if before.model == after.model => {
            enqueue_model_delta(
                tx,
                request_id,
                &before.model,
                &ModelUsageDelta::between(before, after),
            )
            .await?;
        }
        _ => {
            if let Some(before) = before_model.as_ref() {
                enqueue_model_delta(
                    tx,
                    request_id,
                    &before.model,
                    &ModelUsageDelta::removal(before),
                )
                .await?;
            }
            if let Some(after) = after_model.as_ref() {
                enqueue_model_delta(
                    tx,
                    request_id,
                    &after.model,
                    &ModelUsageDelta::addition(after),
                )
                .await?;
            }
        }
    }

    let before_provider = before.and_then(provider_api_key_usage_contribution);
    let after_provider = provider_api_key_usage_contribution(after);
    match (before_provider.as_ref(), after_provider.as_ref()) {
        (Some(before), Some(after)) if before.key_id == after.key_id => {
            enqueue_provider_api_key_delta(
                tx,
                request_id,
                &before.key_id,
                &ProviderApiKeyUsageDelta::between(before, after),
            )
            .await?;
        }
        _ => {
            if let Some(before) = before_provider.as_ref() {
                enqueue_provider_api_key_delta(
                    tx,
                    request_id,
                    &before.key_id,
                    &ProviderApiKeyUsageDelta::removal(before),
                )
                .await?;
            }
            if let Some(after) = after_provider.as_ref() {
                enqueue_provider_api_key_delta(
                    tx,
                    request_id,
                    &after.key_id,
                    &ProviderApiKeyUsageDelta::addition(after),
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub(super) async fn enqueue_usage_transition_for_request(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
    before: Option<&StoredRequestUsageAudit>,
) -> Result<(), DataLayerError> {
    let row = sqlx::query(&format!(
        "{} WHERE `usage`.request_id = ? LIMIT 1",
        super::USAGE_COLUMNS
    ))
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?
    .ok_or_else(|| {
        DataLayerError::UnexpectedValue(format!(
            "usage row missing while preparing counter delta: {request_id}"
        ))
    })?;
    let after = super::map_usage_row(&row, false)?;
    enqueue_usage_transition(tx, request_id, before, &after).await
}

pub(super) async fn lock_and_load_usage(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
) -> Result<Option<StoredRequestUsageAudit>, DataLayerError> {
    let exists = sqlx::query_scalar::<_, String>(
        "SELECT request_id FROM `usage` WHERE request_id = ? FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    if exists.is_none() {
        return Ok(None);
    }
    let row = sqlx::query(&format!(
        "{} WHERE `usage`.request_id = ? LIMIT 1",
        super::USAGE_COLUMNS
    ))
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    row.as_ref()
        .map(|row| super::map_usage_row(row, false))
        .transpose()
}

async fn enqueue_api_key_delta(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
    target_id: &str,
    delta: &ApiKeyUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    insert_delta(
        tx,
        DeltaInsert {
            request_id,
            kind: KIND_API_KEY,
            target_id,
            total_requests_delta: delta.total_requests,
            total_tokens_delta: delta.total_tokens,
            total_cost_usd_delta: finite_or_zero(delta.total_cost_usd),
            candidate_last_used_at_unix_secs: delta.candidate_last_used_at_unix_secs,
            removed_last_used_at_unix_secs: delta.removed_last_used_at_unix_secs,
            ..DeltaInsert::default()
        },
    )
    .await
}

async fn enqueue_model_delta(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
    target_id: &str,
    delta: &ModelUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    insert_delta(
        tx,
        DeltaInsert {
            request_id,
            kind: KIND_MODEL,
            target_id,
            request_count_delta: delta.request_count,
            ..DeltaInsert::default()
        },
    )
    .await
}

async fn enqueue_provider_api_key_delta(
    tx: &mut sqlx::Transaction<'_, MySql>,
    request_id: &str,
    target_id: &str,
    delta: &ProviderApiKeyUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    insert_delta(
        tx,
        DeltaInsert {
            request_id,
            kind: KIND_PROVIDER_API_KEY,
            target_id,
            request_count_delta: delta.request_count,
            success_count_delta: delta.success_count,
            error_count_delta: delta.error_count,
            total_tokens_delta: delta.total_tokens,
            total_cost_usd_delta: finite_or_zero(delta.total_cost_usd),
            total_response_time_ms_delta: delta.total_response_time_ms,
            candidate_last_used_at_unix_secs: delta.candidate_last_used_at_unix_secs,
            removed_last_used_at_unix_secs: delta.removed_last_used_at_unix_secs,
            usage_created_at_unix_secs: delta.usage_created_at_unix_secs,
            ..DeltaInsert::default()
        },
    )
    .await
}

#[derive(Default)]
struct DeltaInsert<'a> {
    request_id: &'a str,
    kind: &'a str,
    target_id: &'a str,
    request_count_delta: i64,
    total_requests_delta: i64,
    success_count_delta: i64,
    error_count_delta: i64,
    dns_failures_delta: i64,
    stream_errors_delta: i64,
    total_tokens_delta: i64,
    total_cost_usd_delta: f64,
    total_response_time_ms_delta: i64,
    last_used_at_unix_secs: Option<u64>,
    last_used_ip: Option<&'a str>,
    candidate_last_used_at_unix_secs: Option<u64>,
    removed_last_used_at_unix_secs: Option<u64>,
    usage_created_at_unix_secs: Option<u64>,
}

async fn insert_delta(
    tx: &mut sqlx::Transaction<'_, MySql>,
    input: DeltaInsert<'_>,
) -> Result<(), DataLayerError> {
    let request_id = input.request_id.trim();
    let target_id = input.target_id.trim();
    if request_id.is_empty() || target_id.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r#"
INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, request_count_delta, total_requests_delta,
  success_count_delta, error_count_delta, dns_failures_delta, stream_errors_delta,
  total_tokens_delta, total_cost_usd_delta, total_response_time_ms_delta,
  last_used_at_unix_secs, last_used_ip, candidate_last_used_at_unix_secs,
  removed_last_used_at_unix_secs, usage_created_at_unix_secs, created_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(request_id)
    .bind(input.kind)
    .bind(target_id)
    .bind(input.request_count_delta)
    .bind(input.total_requests_delta)
    .bind(input.success_count_delta)
    .bind(input.error_count_delta)
    .bind(input.dns_failures_delta)
    .bind(input.stream_errors_delta)
    .bind(input.total_tokens_delta)
    .bind(finite_or_zero(input.total_cost_usd_delta))
    .bind(input.total_response_time_ms_delta)
    .bind(optional_to_i64(
        input.last_used_at_unix_secs,
        "usage counter last_used_at_unix_secs",
    )?)
    .bind(
        input
            .last_used_ip
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .bind(optional_to_i64(
        input.candidate_last_used_at_unix_secs,
        "usage counter candidate_last_used_at_unix_secs",
    )?)
    .bind(optional_to_i64(
        input.removed_last_used_at_unix_secs,
        "usage counter removed_last_used_at_unix_secs",
    )?)
    .bind(optional_to_i64(
        input.usage_created_at_unix_secs,
        "usage counter usage_created_at_unix_secs",
    )?)
    .bind(current_unix_secs())
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

fn map_row(row: &sqlx::mysql::MySqlRow) -> Result<DeltaRow, DataLayerError> {
    Ok(DeltaRow {
        id: row.try_get("id").map_sql_err()?,
        kind: row.try_get("kind").map_sql_err()?,
        target_id: row.try_get("target_id").map_sql_err()?,
        request_count_delta: row.try_get("request_count_delta").map_sql_err()?,
        total_requests_delta: row.try_get("total_requests_delta").map_sql_err()?,
        success_count_delta: row.try_get("success_count_delta").map_sql_err()?,
        error_count_delta: row.try_get("error_count_delta").map_sql_err()?,
        dns_failures_delta: row.try_get("dns_failures_delta").map_sql_err()?,
        stream_errors_delta: row.try_get("stream_errors_delta").map_sql_err()?,
        total_tokens_delta: row.try_get("total_tokens_delta").map_sql_err()?,
        total_cost_usd_delta: row.try_get("total_cost_usd_delta").map_sql_err()?,
        total_response_time_ms_delta: row.try_get("total_response_time_ms_delta").map_sql_err()?,
        last_used_at_unix_secs: optional_u64(
            "usage_counter_deltas.last_used_at_unix_secs",
            row.try_get("last_used_at_unix_secs").map_sql_err()?,
        )?,
        last_used_ip: row.try_get("last_used_ip").map_sql_err()?,
        candidate_last_used_at_unix_secs: optional_u64(
            "usage_counter_deltas.candidate_last_used_at_unix_secs",
            row.try_get("candidate_last_used_at_unix_secs")
                .map_sql_err()?,
        )?,
        removed_last_used_at_unix_secs: optional_u64(
            "usage_counter_deltas.removed_last_used_at_unix_secs",
            row.try_get("removed_last_used_at_unix_secs")
                .map_sql_err()?,
        )?,
        usage_created_at_unix_secs: optional_u64(
            "usage_counter_deltas.usage_created_at_unix_secs",
            row.try_get("usage_created_at_unix_secs").map_sql_err()?,
        )?,
        provider_billing_type_at_usage: row
            .try_get("provider_billing_type_at_usage")
            .map_sql_err()?,
        quota_epoch_start_at_usage: optional_u64(
            "usage_counter_deltas.quota_epoch_start_at_usage",
            row.try_get("quota_epoch_start_at_usage").map_sql_err()?,
        )?,
        provider_dispatch_at_unix_secs: optional_u64(
            "usage_counter_deltas.provider_dispatch_at_unix_secs",
            row.try_get("provider_dispatch_at_unix_secs")
                .map_sql_err()?,
        )?,
        provider_quota_cost_usd: row
            .try_get::<Option<f64>, _>("provider_quota_cost_usd")
            .map_sql_err()?,
        quota_delta_sequence: row.try_get("quota_delta_sequence").map_sql_err()?,
        quota_accounting_status: row.try_get("quota_accounting_status").map_sql_err()?,
        created_at_unix_secs: row.try_get::<i64, _>("created_at").map_sql_err()?.max(0) as u64,
    })
}

async fn apply_api_key(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ApiKeyUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    let candidate = optional_to_i64(
        delta.candidate_last_used_at_unix_secs,
        "api key candidate last used at",
    )?;
    let removed = optional_to_i64(
        delta.removed_last_used_at_unix_secs,
        "api key removed last used at",
    )?;
    sqlx::query(
        r#"
UPDATE api_keys
SET total_requests = GREATEST(COALESCE(total_requests, 0) + ?, 0),
    total_tokens = GREATEST(COALESCE(total_tokens, 0) + ?, 0),
    total_cost_usd = GREATEST(COALESCE(total_cost_usd, 0) + ?, 0),
    last_used_at = CASE
      WHEN ? IS NOT NULL THEN GREATEST(COALESCE(last_used_at, 0), ?)
      WHEN ? IS NOT NULL AND last_used_at = ? THEN (
        SELECT MAX(created_at_unix_ms)
        FROM `usage`
        WHERE api_key_id = ? AND status NOT IN ('pending', 'streaming')
      )
      ELSE last_used_at
    END
WHERE id = ?
"#,
    )
    .bind(delta.total_requests)
    .bind(delta.total_tokens)
    .bind(finite_or_zero(delta.total_cost_usd))
    .bind(candidate)
    .bind(candidate)
    .bind(removed)
    .bind(removed)
    .bind(target_id)
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn apply_model(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ModelUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE global_models SET usage_count = GREATEST(COALESCE(usage_count, 0) + ?, 0), updated_at = ? WHERE name = ?",
    )
    .bind(delta.request_count)
    .bind(current_unix_secs())
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn apply_provider_api_key(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ProviderApiKeyUsageDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    let candidate = optional_to_i64(
        delta.candidate_last_used_at_unix_secs,
        "provider api key candidate last used at",
    )?;
    let removed = optional_to_i64(
        delta.removed_last_used_at_unix_secs,
        "provider api key removed last used at",
    )?;
    sqlx::query(
        r#"
UPDATE provider_api_keys
SET request_count = GREATEST(COALESCE(request_count, 0) + ?, 0),
    success_count = GREATEST(COALESCE(success_count, 0) + ?, 0),
    error_count = GREATEST(COALESCE(error_count, 0) + ?, 0),
    total_tokens = GREATEST(COALESCE(total_tokens, 0) + ?, 0),
    total_cost_usd = GREATEST(COALESCE(total_cost_usd, 0) + ?, 0),
    total_response_time_ms = GREATEST(COALESCE(total_response_time_ms, 0) + ?, 0),
    last_used_at = CASE
      WHEN ? IS NOT NULL THEN GREATEST(COALESCE(last_used_at, 0), ?)
      WHEN ? IS NOT NULL AND last_used_at = ? THEN (
        SELECT MAX(created_at_unix_ms)
        FROM `usage`
        WHERE provider_api_key_id = ? AND status NOT IN ('pending', 'streaming')
      )
      ELSE last_used_at
    END
WHERE id = ?
"#,
    )
    .bind(delta.request_count)
    .bind(delta.success_count)
    .bind(delta.error_count)
    .bind(delta.total_tokens)
    .bind(finite_or_zero(delta.total_cost_usd))
    .bind(delta.total_response_time_ms)
    .bind(candidate)
    .bind(candidate)
    .bind(removed)
    .bind(removed)
    .bind(target_id)
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn apply_provider_monthly(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    deltas: &[ProviderMonthlyDelta],
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || deltas.is_empty() {
        return Ok(());
    }
    let provider =
        sqlx::query("SELECT quota_last_reset_at, config FROM providers WHERE id = ? LIMIT 1")
            .bind(target_id)
            .fetch_optional(&mut **tx)
            .await
            .map_sql_err()?;
    let Some(provider) = provider else {
        return Ok(());
    };
    let current_epoch = provider
        .try_get::<Option<i64>, _>("quota_last_reset_at")
        .map_sql_err()?
        .map(|value| quota_clock_minute(value.max(0) as u64));
    let now_db = current_unix_secs();
    let now = now_db.max(0) as u64;
    let accepted = deltas
        .iter()
        .copied()
        .filter(|delta| {
            delta.provider_dispatch_at_unix_secs <= now
                && delta.provider_dispatch_at_unix_secs >= delta.quota_epoch_start_unix_secs
        })
        .collect::<Vec<_>>();
    let current_epoch_delta = accepted
        .iter()
        .filter(|delta| Some(delta.quota_epoch_start_unix_secs) == current_epoch)
        .filter(|delta| delta.accounting_ready)
        .map(|delta| delta.provider_quota_cost_usd)
        .sum::<f64>();
    if !current_epoch_delta.is_finite() {
        return Err(DataLayerError::UnexpectedValue(format!(
            "providers.monthly_used_usd delta is not finite for {target_id}"
        )));
    }
    if current_epoch_delta != 0.0 {
        sqlx::query(
            "UPDATE providers SET monthly_used_usd = COALESCE(monthly_used_usd, 0) + ?, updated_at = ? WHERE id = ? AND quota_last_reset_at = ?",
        )
        .bind(current_epoch_delta)
        .bind(now_db)
        .bind(target_id)
        .bind(current_epoch.map(|value| value as i64))
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    }

    let config = provider
        .try_get::<Option<String>, _>("config")
        .map_sql_err()?
        .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
    let windows = quota_windows_from_config(config.as_ref());
    for delta in &accepted {
        let bucket_start = quota_clock_minute(delta.provider_dispatch_at_unix_secs);
        if delta.accounting_ready {
            sqlx::query(
                r#"
INSERT INTO provider_quota_usage_buckets (
  provider_id, quota_epoch_start, bucket_start, used_usd, updated_at
) VALUES (?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  used_usd = used_usd + VALUES(used_usd),
  updated_at = VALUES(updated_at)
"#,
            )
            .bind(target_id)
            .bind(delta.quota_epoch_start_unix_secs as i64)
            .bind(bucket_start as i64)
            .bind(delta.provider_quota_cost_usd)
            .bind(now_db)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        }
        if Some(delta.quota_epoch_start_unix_secs) != current_epoch {
            continue;
        }
        for window in &windows {
            let rolling_start =
                quota_window_start_unix_secs(now, current_epoch, window.duration_secs);
            sqlx::query(
                r#"
INSERT IGNORE INTO provider_quota_window_counters (
  provider_id, duration_secs, quota_epoch_start, rolling_start,
  accounted_until, used_usd, status, rebuild_error, updated_at
) VALUES (?, ?, ?, ?, ?, 0, 'rebuilding', NULL, ?)
"#,
            )
            .bind(target_id)
            .bind(window.duration_secs as i64)
            .bind(delta.quota_epoch_start_unix_secs as i64)
            .bind(rolling_start as i64)
            .bind(quota_clock_minute(now) as i64)
            .bind(now_db)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
            if delta.accounting_ready {
                sqlx::query(
                    r#"
UPDATE provider_quota_window_counters
SET used_usd = used_usd + ?, updated_at = ?
WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ?
  AND status = 'ready' AND ? >= rolling_start AND ? < accounted_until
"#,
                )
                .bind(delta.provider_quota_cost_usd)
                .bind(now_db)
                .bind(target_id)
                .bind(window.duration_secs as i64)
                .bind(delta.quota_epoch_start_unix_secs as i64)
                .bind(bucket_start as i64)
                .bind(bucket_start as i64)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
            } else {
                sqlx::query(
                    r#"
UPDATE provider_quota_window_counters
SET status = 'failed',
    rebuild_error = 'quota cost is unavailable for a dispatched monthly request',
    updated_at = ?
WHERE provider_id = ? AND duration_secs = ? AND quota_epoch_start = ?
"#,
                )
                .bind(now_db)
                .bind(target_id)
                .bind(window.duration_secs as i64)
                .bind(delta.quota_epoch_start_unix_secs as i64)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
            }
        }
    }
    Ok(())
}

async fn apply_proxy_node(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ProxyNodeCounterDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    sqlx::query(
        r#"
UPDATE proxy_nodes
SET total_requests = total_requests + GREATEST(?, 0),
    failed_requests = failed_requests + GREATEST(?, 0),
    dns_failures = dns_failures + GREATEST(?, 0),
    stream_errors = stream_errors + GREATEST(?, 0),
    updated_at = ?
WHERE id = ?
"#,
    )
    .bind(delta.total_requests_delta)
    .bind(delta.failed_requests_delta)
    .bind(delta.dns_failures_delta)
    .bind(delta.stream_errors_delta)
    .bind(current_unix_secs())
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn apply_management_token(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ManagementTokenCounterDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    let last_used_at = optional_to_i64(
        delta.last_used_at_unix_secs,
        "management token last used at",
    )?;
    sqlx::query(
        r#"
UPDATE management_tokens
SET usage_count = COALESCE(usage_count, 0) + GREATEST(?, 0),
    last_used_at = CASE
      WHEN ? IS NULL THEN last_used_at
      ELSE GREATEST(COALESCE(last_used_at, 0), ?)
    END,
    last_used_ip = COALESCE(?, last_used_ip),
    updated_at = ?
WHERE id = ?
"#,
    )
    .bind(delta.usage_count_delta)
    .bind(last_used_at)
    .bind(last_used_at)
    .bind(delta.last_used_ip.as_deref())
    .bind(current_unix_secs())
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn apply_api_key_last_used(
    tx: &mut sqlx::Transaction<'_, MySql>,
    target_id: &str,
    delta: &ApiKeyLastUsedDelta,
) -> Result<(), DataLayerError> {
    if target_id.trim().is_empty() || delta.is_noop() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE api_keys SET last_used_at = GREATEST(COALESCE(last_used_at, 0), ?) WHERE id = ?",
    )
    .bind(to_i64(
        delta.last_used_at_unix_secs,
        "api key last used at",
    )?)
    .bind(target_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

fn merge_optional_max(target: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        if target.is_none_or(|current| value > current) {
            *target = Some(value);
        }
    }
}

fn merge_latest_timestamp_with_value(
    target_timestamp: &mut Option<u64>,
    target_value: &mut Option<String>,
    timestamp: Option<u64>,
    value: Option<String>,
) {
    let Some(timestamp) = timestamp else {
        return;
    };
    if target_timestamp.is_none_or(|current| timestamp >= current) {
        *target_timestamp = Some(timestamp);
        if value
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| !v.is_empty())
        {
            *target_value = value;
        }
    }
}

fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn current_unix_secs() -> i64 {
    chrono::Utc::now().timestamp().max(0)
}

fn to_i64(value: u64, field: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::InvalidInput(format!("{field} exceeds i64: {value}")))
}

fn optional_to_i64(value: Option<u64>, field: &str) -> Result<Option<i64>, DataLayerError> {
    value.map(|value| to_i64(value, field)).transpose()
}

fn optional_u64(field: &str, value: Option<i64>) -> Result<Option<u64>, DataLayerError> {
    value
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                DataLayerError::UnexpectedValue(format!("{field} is negative: {value}"))
            })
        })
        .transpose()
}

fn nonnegative_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn optional_nonnegative_u64(value: Option<i64>) -> Option<u64> {
    value.map(nonnegative_u64)
}
