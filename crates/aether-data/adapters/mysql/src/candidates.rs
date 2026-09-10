use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, MySqlConnection, QueryBuilder, Row};

use aether_data_contracts::repository::candidates::{
    provider_quota_dispatch_snapshot, request_candidate_lifecycle_would_regress,
    PublicHealthStatusCount, PublicHealthTimelineBucket, RequestCandidateReadRepository,
    RequestCandidateStatus, RequestCandidateWriteRepository, StoredRequestCandidate,
    UpsertRequestCandidateRecord,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const CANDIDATE_COLUMNS: &str = r#"
SELECT
  id,
  request_id,
  user_id,
  api_key_id,
  username,
  api_key_name,
  candidate_index,
  retry_index,
  provider_id,
  endpoint_id,
  key_id,
  status,
  skip_reason,
  is_cached,
  status_code,
  error_type,
  error_message,
  latency_ms,
  concurrent_requests,
  extra_data,
  required_capabilities,
  created_at AS created_at_unix_ms,
  started_at AS started_at_unix_ms,
  finished_at AS finished_at_unix_ms
FROM request_candidates
"#;

#[derive(Debug, Clone)]
pub struct MysqlRequestCandidateRepository {
    pool: MysqlPool,
}

impl MysqlRequestCandidateRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RequestCandidateReadRepository for MysqlRequestCandidateRepository {
    async fn list_by_request_id(
        &self,
        request_id: &str,
    ) -> Result<Vec<StoredRequestCandidate>, DataLayerError> {
        let rows = sqlx::query(&format!(
            "{CANDIDATE_COLUMNS} WHERE request_id = ? ORDER BY candidate_index ASC, retry_index ASC, created_at ASC"
        ))
        .bind(request_id)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_candidate_row).collect()
    }

    async fn list_attempted_by_request_id(
        &self,
        request_id: &str,
    ) -> Result<Vec<StoredRequestCandidate>, DataLayerError> {
        let rows = sqlx::query(&format!(
            "{CANDIDATE_COLUMNS} WHERE request_id = ? \
             AND (status IN ('streaming', 'success', 'failed', 'cancelled') \
             OR (status = 'pending' AND started_at IS NOT NULL)) \
             ORDER BY candidate_index ASC, retry_index ASC, created_at ASC"
        ))
        .bind(request_id)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_candidate_row).collect()
    }

    async fn list_recent(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredRequestCandidate>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "{CANDIDATE_COLUMNS} ORDER BY created_at DESC LIMIT ?"
        ))
        .bind(limit_i64(limit, "recent request candidate limit")?)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_candidate_row).collect()
    }

    async fn list_by_provider_id(
        &self,
        provider_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredRequestCandidate>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "{CANDIDATE_COLUMNS} WHERE provider_id = ? ORDER BY created_at DESC LIMIT ?"
        ))
        .bind(provider_id)
        .bind(limit_i64(limit, "provider request candidate limit")?)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_candidate_row).collect()
    }

    async fn list_finalized_by_endpoint_ids_since(
        &self,
        endpoint_ids: &[String],
        since_unix_secs: u64,
        limit: usize,
    ) -> Result<Vec<StoredRequestCandidate>, DataLayerError> {
        if endpoint_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<MySql>::new(CANDIDATE_COLUMNS);
        push_endpoint_in_clause(&mut builder, endpoint_ids);
        builder
            .push(" AND created_at >= ")
            .push_bind(unix_secs_to_ms_i64(since_unix_secs)?)
            .push(" AND status IN ('success', 'failed', 'skipped')")
            .push(" ORDER BY created_at DESC LIMIT ")
            .push_bind(limit_i64(limit, "finalized request candidate limit")?);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_candidate_row).collect()
    }

    async fn count_finalized_statuses_by_endpoint_ids_since(
        &self,
        endpoint_ids: &[String],
        since_unix_secs: u64,
    ) -> Result<Vec<PublicHealthStatusCount>, DataLayerError> {
        if endpoint_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT endpoint_id, status, COUNT(id) AS count FROM request_candidates",
        );
        push_endpoint_in_clause(&mut builder, endpoint_ids);
        builder
            .push(" AND created_at >= ")
            .push_bind(unix_secs_to_ms_i64(since_unix_secs)?)
            .push(" AND status IN ('success', 'failed', 'skipped')")
            .push(" GROUP BY endpoint_id, status");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter()
            .map(|row| {
                Ok(PublicHealthStatusCount {
                    endpoint_id: row.try_get("endpoint_id").map_sql_err()?,
                    status: RequestCandidateStatus::from_database(
                        row.try_get::<String, _>("status").map_sql_err()?.as_str(),
                    )?,
                    count: u64::try_from(row.try_get::<i64, _>("count").map_sql_err()?).map_err(
                        |_| {
                            DataLayerError::UnexpectedValue(
                                "public health status count out of range".to_string(),
                            )
                        },
                    )?,
                })
            })
            .collect()
    }

    async fn aggregate_finalized_timeline_by_endpoint_ids_since(
        &self,
        endpoint_ids: &[String],
        since_unix_secs: u64,
        until_unix_secs: u64,
        segments: u32,
    ) -> Result<Vec<PublicHealthTimelineBucket>, DataLayerError> {
        if endpoint_ids.is_empty() || segments == 0 || until_unix_secs < since_unix_secs {
            return Ok(Vec::new());
        }
        let since_ms = unix_secs_to_ms_i64(since_unix_secs)?;
        let until_ms = unix_secs_to_ms_i64(until_unix_secs)?;
        let mut builder = QueryBuilder::<MySql>::new(CANDIDATE_COLUMNS);
        push_endpoint_in_clause(&mut builder, endpoint_ids);
        builder
            .push(" AND created_at >= ")
            .push_bind(since_ms)
            .push(" AND created_at <= ")
            .push_bind(until_ms)
            .push(" AND status IN ('success', 'failed', 'skipped')");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        aggregate_timeline(
            rows.iter()
                .map(map_candidate_row)
                .collect::<Result<Vec<_>, _>>()?,
            since_unix_secs,
            until_unix_secs,
            segments,
        )
    }
}

#[async_trait]
impl RequestCandidateWriteRepository for MysqlRequestCandidateRepository {
    async fn upsert(
        &self,
        mut candidate: UpsertRequestCandidateRecord,
    ) -> Result<StoredRequestCandidate, DataLayerError> {
        candidate.sanitize_for_persistence();
        candidate.validate()?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        match upsert_candidate_in_transaction(&mut tx, candidate).await {
            Ok(candidate) => {
                tx.commit().await.map_sql_err()?;
                Ok(candidate)
            }
            Err(err) => {
                tx.rollback().await.map_sql_err()?;
                Err(err)
            }
        }
    }

    async fn upsert_many(
        &self,
        mut candidates: Vec<UpsertRequestCandidateRecord>,
    ) -> Result<usize, DataLayerError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        for candidate in &mut candidates {
            candidate.sanitize_for_persistence();
            candidate.validate()?;
        }

        let mut tx = self.pool.begin().await.map_sql_err()?;
        let result: Result<usize, DataLayerError> = async {
            let mut persisted = 0usize;
            for candidate in candidates {
                upsert_candidate_in_transaction(&mut tx, candidate).await?;
                persisted = persisted.saturating_add(1);
            }
            Ok(persisted)
        }
        .await;
        match result {
            Ok(persisted) => {
                tx.commit().await.map_sql_err()?;
                Ok(persisted)
            }
            Err(err) => {
                tx.rollback().await.map_sql_err()?;
                Err(err)
            }
        }
    }

    async fn delete_created_before(
        &self,
        created_before_unix_secs: u64,
        limit: usize,
    ) -> Result<usize, DataLayerError> {
        if limit == 0 {
            return Ok(0);
        }
        let rows_affected = sqlx::query(
            r#"
DELETE FROM request_candidates
WHERE id IN (
  SELECT id
  FROM (
    SELECT id
    FROM request_candidates
    WHERE created_at < ?
    ORDER BY created_at ASC, id ASC
    LIMIT ?
  ) AS old_request_candidates
)
"#,
        )
        .bind(unix_secs_to_ms_i64(created_before_unix_secs)?)
        .bind(limit_i64(limit, "request candidate delete limit")?)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(usize::try_from(rows_affected).unwrap_or_default())
    }
}

async fn upsert_candidate_in_transaction(
    tx: &mut sqlx::Transaction<'_, MySql>,
    candidate: UpsertRequestCandidateRecord,
) -> Result<StoredRequestCandidate, DataLayerError> {
    // Write first so both existing rows and previously empty unique keys are locked
    // before the Rust merge reads their latest committed state.
    let insert_candidate = merge_candidate(candidate.clone(), None)?;
    insert_candidate_if_absent(tx, &insert_candidate).await?;
    let existing = find_by_unique_for_update(
        tx,
        &candidate.request_id,
        candidate.candidate_index,
        candidate.retry_index,
    )
    .await?
    .ok_or_else(|| {
        DataLayerError::UnexpectedValue(
            "request candidate row was not locked after insert-if-absent".to_string(),
        )
    })?;
    let merged = merge_candidate(candidate, Some(existing))?;
    upsert_merged_candidate(tx, &merged).await?;
    upsert_provider_quota_attempt_delta(tx, &merged).await?;
    find_by_unique_for_update(
        tx,
        &merged.request_id,
        merged.candidate_index,
        merged.retry_index,
    )
    .await?
    .ok_or_else(|| {
        DataLayerError::UnexpectedValue(
            "request candidate row disappeared after atomic upsert".to_string(),
        )
    })
}

async fn upsert_provider_quota_attempt_delta(
    tx: &mut sqlx::Transaction<'_, MySql>,
    candidate: &StoredRequestCandidate,
) -> Result<(), DataLayerError> {
    let Some(snapshot) = provider_quota_dispatch_snapshot(candidate.extra_data.as_ref())? else {
        return Ok(());
    };
    if !snapshot.is_monthly_quota() {
        return Ok(());
    }
    let provider_id = candidate
        .provider_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DataLayerError::InvalidInput(
                "monthly provider quota attempt is missing provider_id".to_string(),
            )
        })?;
    let quota_epoch = snapshot.quota_epoch_start_at_usage.ok_or_else(|| {
        DataLayerError::InvalidInput(
            "monthly provider quota attempt is missing quota epoch".to_string(),
        )
    })?;
    let delta_id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("provider-quota-attempt:{}", candidate.id).as_bytes(),
    )
    .to_string();
    let pricing_snapshot = snapshot
        .provider_pricing_snapshot_at_usage
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| DataLayerError::UnexpectedValue(err.to_string()))?;
    let cost = snapshot.provider_quota_cost_usd.unwrap_or(0.0);
    sqlx::query(
        r#"
INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, total_cost_usd_delta,
  usage_created_at_unix_secs, provider_billing_type_at_usage,
  quota_epoch_start_at_usage, provider_dispatch_at_unix_secs,
  provider_quota_cost_usd, pricing_rule_version_at_usage,
  provider_pricing_snapshot_at_usage, quota_accounting_status, created_at
)
VALUES (?, ?, 'provider_monthly', ?, ?, ?, 'monthly_quota', ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  provider_quota_cost_usd = IF(
    processed_at IS NULL AND COALESCE(quota_accounting_status, 'pending') IN ('pending', 'failed'),
    VALUES(provider_quota_cost_usd), provider_quota_cost_usd
  ),
  total_cost_usd_delta = IF(
    processed_at IS NULL AND COALESCE(quota_accounting_status, 'pending') IN ('pending', 'failed'),
    VALUES(total_cost_usd_delta), total_cost_usd_delta
  ),
  quota_accounting_status = IF(
    processed_at IS NULL AND COALESCE(quota_accounting_status, 'pending') IN ('pending', 'failed'),
    VALUES(quota_accounting_status), quota_accounting_status
  )
"#,
    )
    .bind(&delta_id)
    .bind(&candidate.id)
    .bind(provider_id)
    .bind(cost)
    .bind(snapshot.provider_dispatch_at_unix_secs as i64)
    .bind(quota_epoch as i64)
    .bind(snapshot.provider_dispatch_at_unix_secs as i64)
    .bind(cost)
    .bind(snapshot.pricing_rule_version_at_usage.as_deref())
    .bind(pricing_snapshot)
    .bind(snapshot.quota_accounting_status)
    .bind(snapshot.provider_dispatch_at_unix_secs as i64)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    reconcile_settled_provider_quota_attempt(tx, &delta_id, candidate).await?;
    if matches!(
        candidate.status,
        RequestCandidateStatus::Failed | RequestCandidateStatus::Cancelled
    ) {
        let actual = candidate
            .extra_data
            .as_ref()
            .and_then(|v| v.pointer("/provider_quota_attempt_accounting/cost_usd"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(cost)
            .max(cost);
        // Unknown consumption uses the approved provisional minimum, while the immutable
        // dispatch snapshot and candidate accounting annotation preserve the uncertainty.
        crate::settlement::reconcile_provider_monthly_attempt_mysql(
            tx,
            &candidate.id,
            actual,
            true,
            (candidate
                .finished_at_unix_ms
                .unwrap_or(candidate.created_at_unix_ms)
                / 1000) as i64,
        )
        .await?;
    }
    Ok(())
}

async fn reconcile_settled_provider_quota_attempt(
    tx: &mut sqlx::Transaction<'_, MySql>,
    delta_id: &str,
    candidate: &StoredRequestCandidate,
) -> Result<(), DataLayerError> {
    sqlx::query(
        r#"
UPDATE usage_counter_deltas AS delta
JOIN `usage` AS usage_record ON usage_record.request_id = ?
JOIN usage_settlement_snapshots AS settlement ON settlement.request_id = usage_record.request_id
LEFT JOIN usage_routing_snapshots AS routing ON routing.request_id = usage_record.request_id
SET delta.provider_quota_cost_usd = COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
      settlement.billing_actual_total_cost_usd,
      usage_record.actual_total_cost_usd,
      0
    ),
    delta.total_cost_usd_delta = COALESCE(
      CAST(JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE),
      settlement.billing_actual_total_cost_usd,
      usage_record.actual_total_cost_usd,
      0
    ),
    delta.quota_accounting_status = 'ready'
WHERE delta.id = ? AND delta.kind = 'provider_monthly'
  AND delta.processed_at IS NULL
  AND delta.quota_accounting_status IN ('pending', 'failed')
  AND (
    routing.candidate_id = ?
    OR (routing.candidate_id IS NULL AND usage_record.provider_id = ? AND ?)
  )
  AND settlement.billing_status = 'settled'
  AND COALESCE(usage_record.finalized_at, settlement.finalized_at) IS NOT NULL
  AND (
    JSON_UNQUOTE(JSON_EXTRACT(settlement.settlement_snapshot, '$.status')) = 'complete'
    OR LOWER(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
  )
"#,
    )
    .bind(&candidate.request_id)
    .bind(delta_id)
    .bind(&candidate.id)
    .bind(candidate.provider_id.as_deref())
    .bind(candidate.status == RequestCandidateStatus::Success)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn insert_candidate_if_absent(
    connection: &mut MySqlConnection,
    candidate: &StoredRequestCandidate,
) -> Result<(), DataLayerError> {
    sqlx::query(
        r#"
INSERT INTO request_candidates (
  id, request_id, candidate_index, retry_index, status, created_at
)
VALUES (?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE id = id
"#,
    )
    .bind(&candidate.id)
    .bind(&candidate.request_id)
    .bind(to_i32(candidate.candidate_index)?)
    .bind(to_i32(candidate.retry_index)?)
    .bind(status_to_database(candidate.status))
    .bind(u64_to_i64(
        candidate.created_at_unix_ms,
        "request candidate created_at",
    )?)
    .execute(connection)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn find_by_unique_for_update(
    connection: &mut MySqlConnection,
    request_id: &str,
    candidate_index: u32,
    retry_index: u32,
) -> Result<Option<StoredRequestCandidate>, DataLayerError> {
    let row = sqlx::query(&format!(
        "{CANDIDATE_COLUMNS} WHERE request_id = ? AND candidate_index = ? AND retry_index = ? LIMIT 1 FOR UPDATE"
    ))
    .bind(request_id)
    .bind(to_i32(candidate_index)?)
    .bind(to_i32(retry_index)?)
    .fetch_optional(connection)
    .await
    .map_sql_err()?;
    row.as_ref().map(map_candidate_row).transpose()
}

async fn upsert_merged_candidate(
    connection: &mut MySqlConnection,
    candidate: &StoredRequestCandidate,
) -> Result<(), DataLayerError> {
    sqlx::query(
        r#"
INSERT INTO request_candidates (
  id, request_id, user_id, api_key_id, username, api_key_name,
  candidate_index, retry_index, provider_id, endpoint_id, key_id, status,
  skip_reason, is_cached, status_code, error_type, error_message, latency_ms,
  concurrent_requests, extra_data, required_capabilities, created_at, started_at, finished_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  user_id = VALUES(user_id),
  api_key_id = VALUES(api_key_id),
  username = VALUES(username),
  api_key_name = VALUES(api_key_name),
  provider_id = VALUES(provider_id),
  endpoint_id = VALUES(endpoint_id),
  key_id = VALUES(key_id),
  status = CASE
    WHEN status IN ('success', 'failed', 'cancelled', 'skipped')
      AND VALUES(status) IN ('available', 'unused', 'pending', 'streaming')
      THEN status
    WHEN status = 'pending' AND VALUES(status) IN ('available', 'unused')
      THEN status
    WHEN status = 'streaming' AND VALUES(status) IN ('available', 'unused', 'pending')
      THEN status
    ELSE VALUES(status)
  END,
  skip_reason = VALUES(skip_reason),
  is_cached = VALUES(is_cached),
  status_code = CASE
    WHEN status IN ('success', 'failed', 'cancelled', 'skipped')
      AND VALUES(status) IN ('available', 'unused', 'pending', 'streaming')
      THEN status_code
    WHEN status = 'pending' AND VALUES(status) IN ('available', 'unused')
      THEN status_code
    WHEN status = 'streaming' AND VALUES(status) IN ('available', 'unused', 'pending')
      THEN status_code
    ELSE COALESCE(VALUES(status_code), status_code)
  END,
  error_type = VALUES(error_type),
  error_message = VALUES(error_message),
  latency_ms = CASE
    WHEN status IN ('success', 'failed', 'cancelled', 'skipped')
      AND VALUES(status) IN ('available', 'unused', 'pending', 'streaming')
      THEN latency_ms
    WHEN status = 'pending' AND VALUES(status) IN ('available', 'unused')
      THEN latency_ms
    WHEN status = 'streaming' AND VALUES(status) IN ('available', 'unused', 'pending')
      THEN latency_ms
    ELSE COALESCE(VALUES(latency_ms), latency_ms)
  END,
  concurrent_requests = VALUES(concurrent_requests),
  extra_data = VALUES(extra_data),
  required_capabilities = VALUES(required_capabilities),
  created_at = VALUES(created_at),
  started_at = VALUES(started_at),
  finished_at = CASE
    WHEN status IN ('success', 'failed', 'cancelled', 'skipped')
      AND VALUES(status) IN ('available', 'unused', 'pending', 'streaming')
      THEN finished_at
    WHEN status = 'pending' AND VALUES(status) IN ('available', 'unused')
      THEN finished_at
    WHEN status = 'streaming' AND VALUES(status) IN ('available', 'unused', 'pending')
      THEN finished_at
    ELSE COALESCE(VALUES(finished_at), finished_at)
  END
"#,
    )
    .bind(&candidate.id)
    .bind(&candidate.request_id)
    .bind(&candidate.user_id)
    .bind(&candidate.api_key_id)
    .bind(&candidate.username)
    .bind(&candidate.api_key_name)
    .bind(to_i32(candidate.candidate_index)?)
    .bind(to_i32(candidate.retry_index)?)
    .bind(&candidate.provider_id)
    .bind(&candidate.endpoint_id)
    .bind(&candidate.key_id)
    .bind(status_to_database(candidate.status))
    .bind(&candidate.skip_reason)
    .bind(candidate.is_cached)
    .bind(candidate.status_code.map(i32::from))
    .bind(&candidate.error_type)
    .bind(&candidate.error_message)
    .bind(candidate.latency_ms.map(to_i32_u64).transpose()?)
    .bind(candidate.concurrent_requests.map(to_i32).transpose()?)
    .bind(json_to_string(&candidate.extra_data)?)
    .bind(json_to_string(&candidate.required_capabilities)?)
    .bind(u64_to_i64(
        candidate.created_at_unix_ms,
        "request candidate created_at",
    )?)
    .bind(optional_u64_to_i64(
        candidate.started_at_unix_ms,
        "request candidate started_at",
    )?)
    .bind(optional_u64_to_i64(
        candidate.finished_at_unix_ms,
        "request candidate finished_at",
    )?)
    .execute(connection)
    .await
    .map_sql_err()?;
    Ok(())
}

fn push_endpoint_in_clause<'args>(
    builder: &mut QueryBuilder<'args, MySql>,
    endpoint_ids: &'args [String],
) {
    builder.push(" WHERE endpoint_id IN (");
    {
        let mut separated = builder.separated(", ");
        for endpoint_id in endpoint_ids {
            separated.push_bind(endpoint_id);
        }
    }
    builder.push(")");
}

fn merge_candidate(
    mut candidate: UpsertRequestCandidateRecord,
    existing: Option<StoredRequestCandidate>,
) -> Result<StoredRequestCandidate, DataLayerError> {
    candidate.sanitize_for_persistence();
    let preserve_existing_lifecycle = existing.as_ref().is_some_and(|value| {
        request_candidate_lifecycle_would_regress(value.status, candidate.status)
    });
    let merged_status = if preserve_existing_lifecycle {
        existing
            .as_ref()
            .map(|value| value.status)
            .unwrap_or(candidate.status)
    } else {
        candidate.status
    };
    let created_at_unix_ms = existing
        .as_ref()
        .map(|value| value.created_at_unix_ms)
        .filter(|value| *value > 1000)
        .or_else(|| candidate.created_at_unix_ms.filter(|value| *value > 1000))
        .or(candidate.started_at_unix_ms)
        .or(candidate.finished_at_unix_ms)
        .unwrap_or_else(current_unix_ms);
    let id = existing
        .as_ref()
        .map(|value| value.id.clone())
        .unwrap_or(candidate.id);
    let extra_data = merge_json_objects(
        existing.as_ref().and_then(|value| value.extra_data.clone()),
        candidate.extra_data,
        preserve_existing_lifecycle,
    );
    StoredRequestCandidate::new(
        id,
        candidate.request_id,
        existing
            .as_ref()
            .and_then(|value| value.user_id.clone())
            .or(candidate.user_id),
        existing
            .as_ref()
            .and_then(|value| value.api_key_id.clone())
            .or(candidate.api_key_id),
        existing
            .as_ref()
            .and_then(|value| value.username.clone())
            .or(candidate.username),
        existing
            .as_ref()
            .and_then(|value| value.api_key_name.clone())
            .or(candidate.api_key_name),
        to_i32(candidate.candidate_index)?,
        to_i32(candidate.retry_index)?,
        existing
            .as_ref()
            .and_then(|value| value.provider_id.clone())
            .or(candidate.provider_id),
        existing
            .as_ref()
            .and_then(|value| value.endpoint_id.clone())
            .or(candidate.endpoint_id),
        existing
            .as_ref()
            .and_then(|value| value.key_id.clone())
            .or(candidate.key_id),
        merged_status,
        candidate.skip_reason.or_else(|| {
            existing
                .as_ref()
                .and_then(|value| value.skip_reason.clone())
        }),
        candidate
            .is_cached
            .unwrap_or_else(|| existing.as_ref().is_some_and(|value| value.is_cached)),
        if preserve_existing_lifecycle {
            existing
                .as_ref()
                .and_then(|value| value.status_code.map(i32::from))
        } else {
            candidate.status_code.map(i32::from).or_else(|| {
                existing
                    .as_ref()
                    .and_then(|value| value.status_code.map(i32::from))
            })
        },
        if preserve_existing_lifecycle {
            existing.as_ref().and_then(|value| value.error_type.clone())
        } else {
            candidate
                .error_type
                .or_else(|| existing.as_ref().and_then(|value| value.error_type.clone()))
        },
        if preserve_existing_lifecycle {
            existing
                .as_ref()
                .and_then(|value| value.error_message.clone())
        } else {
            candidate.error_message.or_else(|| {
                existing
                    .as_ref()
                    .and_then(|value| value.error_message.clone())
            })
        },
        if preserve_existing_lifecycle {
            match existing.as_ref().and_then(|value| value.latency_ms) {
                Some(value) => Some(to_i32_u64(value)?),
                None => None,
            }
        } else {
            candidate.latency_ms.map(to_i32_u64).transpose()?.or(
                match existing.as_ref().and_then(|value| value.latency_ms) {
                    Some(value) => Some(to_i32_u64(value)?),
                    None => None,
                },
            )
        },
        candidate.concurrent_requests.map(to_i32).transpose()?.or(
            match existing
                .as_ref()
                .and_then(|value| value.concurrent_requests)
            {
                Some(value) => Some(to_i32(value)?),
                None => None,
            },
        ),
        extra_data,
        candidate.required_capabilities.or_else(|| {
            existing
                .as_ref()
                .and_then(|value| value.required_capabilities.clone())
        }),
        u64_to_i64(created_at_unix_ms, "request candidate created_at")?,
        existing
            .as_ref()
            .and_then(|value| value.started_at_unix_ms)
            .or(candidate.started_at_unix_ms)
            .map(|value| u64_to_i64(value, "request candidate started_at"))
            .transpose()?,
        if preserve_existing_lifecycle {
            existing
                .as_ref()
                .and_then(|value| value.finished_at_unix_ms)
        } else {
            candidate.finished_at_unix_ms.or_else(|| {
                existing
                    .as_ref()
                    .and_then(|value| value.finished_at_unix_ms)
            })
        }
        .map(|value| u64_to_i64(value, "request candidate finished_at"))
        .transpose()?,
    )
}

fn aggregate_timeline(
    candidates: Vec<StoredRequestCandidate>,
    since_unix_secs: u64,
    until_unix_secs: u64,
    segments: u32,
) -> Result<Vec<PublicHealthTimelineBucket>, DataLayerError> {
    let endpoint_ids = candidates
        .iter()
        .filter_map(|candidate| candidate.endpoint_id.clone())
        .collect::<BTreeSet<_>>();
    let span_ms = until_unix_secs
        .saturating_sub(since_unix_secs)
        .saturating_mul(1000)
        .max(1);
    let since_ms = since_unix_secs.saturating_mul(1000);
    let mut buckets = BTreeMap::<(String, u32), PublicHealthTimelineBucket>::new();
    for candidate in candidates {
        let Some(endpoint_id) = candidate.endpoint_id.clone() else {
            continue;
        };
        let offset = candidate.created_at_unix_ms.saturating_sub(since_ms);
        let segment_idx = ((offset.saturating_mul(u64::from(segments))) / span_ms)
            .min(u64::from(segments.saturating_sub(1))) as u32;
        let bucket = buckets.entry((endpoint_id.clone(), segment_idx)).or_insert(
            PublicHealthTimelineBucket {
                endpoint_id,
                segment_idx,
                total_count: 0,
                success_count: 0,
                failed_count: 0,
                min_created_at_unix_ms: Some(candidate.created_at_unix_ms),
                max_created_at_unix_ms: Some(candidate.created_at_unix_ms),
            },
        );
        bucket.total_count += 1;
        if candidate.status == RequestCandidateStatus::Success {
            bucket.success_count += 1;
        }
        if candidate.status == RequestCandidateStatus::Failed {
            bucket.failed_count += 1;
        }
        bucket.min_created_at_unix_ms = bucket
            .min_created_at_unix_ms
            .map(|value| value.min(candidate.created_at_unix_ms));
        bucket.max_created_at_unix_ms = bucket
            .max_created_at_unix_ms
            .map(|value| value.max(candidate.created_at_unix_ms));
    }
    for endpoint_id in endpoint_ids {
        for segment_idx in 0..segments {
            buckets.entry((endpoint_id.clone(), segment_idx)).or_insert(
                PublicHealthTimelineBucket {
                    endpoint_id: endpoint_id.clone(),
                    segment_idx,
                    total_count: 0,
                    success_count: 0,
                    failed_count: 0,
                    min_created_at_unix_ms: None,
                    max_created_at_unix_ms: None,
                },
            );
        }
    }
    Ok(buckets.into_values().collect())
}

fn map_candidate_row(row: &MySqlRow) -> Result<StoredRequestCandidate, DataLayerError> {
    StoredRequestCandidate::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("request_id").map_sql_err()?,
        row.try_get("user_id").map_sql_err()?,
        row.try_get("api_key_id").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("api_key_name").map_sql_err()?,
        row.try_get("candidate_index").map_sql_err()?,
        row.try_get("retry_index").map_sql_err()?,
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("endpoint_id").map_sql_err()?,
        row.try_get("key_id").map_sql_err()?,
        RequestCandidateStatus::from_database(
            row.try_get::<String, _>("status").map_sql_err()?.as_str(),
        )?,
        row.try_get("skip_reason").map_sql_err()?,
        row.try_get("is_cached").map_sql_err()?,
        row.try_get("status_code").map_sql_err()?,
        row.try_get("error_type").map_sql_err()?,
        row.try_get("error_message").map_sql_err()?,
        row.try_get("latency_ms").map_sql_err()?,
        row.try_get("concurrent_requests").map_sql_err()?,
        parse_json(row.try_get("extra_data").ok().flatten())?,
        parse_json(row.try_get("required_capabilities").ok().flatten())?,
        row.try_get("created_at_unix_ms").map_sql_err()?,
        row.try_get("started_at_unix_ms").map_sql_err()?,
        row.try_get("finished_at_unix_ms").map_sql_err()?,
    )
}

fn parse_json(value: Option<String>) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "request_candidates JSON field is invalid: {err}"
                ))
            })
        })
        .transpose()
}

fn json_to_string(value: &Option<serde_json::Value>) -> Result<Option<String>, DataLayerError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "request_candidates JSON field is unserializable: {err}"
                ))
            })
        })
        .transpose()
}

fn merge_json_objects(
    existing: Option<serde_json::Value>,
    overlay: Option<serde_json::Value>,
    preserve_error_details: bool,
) -> Option<serde_json::Value> {
    match (existing, overlay) {
        (
            Some(serde_json::Value::Object(mut existing_object)),
            Some(serde_json::Value::Object(mut overlay_object)),
        ) => {
            if preserve_error_details {
                for key in [
                    "upstream_response",
                    "error_flow",
                    "failure_diagnostic",
                    "request_conversion_error",
                    "request_body_build_error",
                ] {
                    overlay_object.remove(key);
                }
            }
            existing_object.extend(overlay_object);
            Some(serde_json::Value::Object(existing_object))
        }
        (_existing, Some(overlay)) => Some(overlay),
        (existing, None) => existing,
    }
}

fn status_to_database(status: RequestCandidateStatus) -> &'static str {
    match status {
        RequestCandidateStatus::Available => "available",
        RequestCandidateStatus::Unused => "unused",
        RequestCandidateStatus::Pending => "pending",
        RequestCandidateStatus::Streaming => "streaming",
        RequestCandidateStatus::Success => "success",
        RequestCandidateStatus::Failed => "failed",
        RequestCandidateStatus::Cancelled => "cancelled",
        RequestCandidateStatus::Skipped => "skipped",
    }
}

fn current_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn unix_secs_to_ms_i64(value: u64) -> Result<i64, DataLayerError> {
    let value = value.checked_mul(1000).ok_or_else(|| {
        DataLayerError::UnexpectedValue("request candidate timestamp overflow".to_string())
    })?;
    i64::try_from(value).map_err(|_| {
        DataLayerError::UnexpectedValue("request candidate timestamp overflow".to_string())
    })
}

fn limit_i64(value: usize, name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::UnexpectedValue(format!("invalid {name}: {value}")))
}

fn to_i32(value: u32) -> Result<i32, DataLayerError> {
    i32::try_from(value).map_err(|_| {
        DataLayerError::UnexpectedValue(format!("request candidate value out of range: {value}"))
    })
}

fn to_i32_u64(value: u64) -> Result<i32, DataLayerError> {
    i32::try_from(value).map_err(|_| {
        DataLayerError::UnexpectedValue(format!("request candidate value out of range: {value}"))
    })
}

fn u64_to_i64(value: u64, name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value).map_err(|_| DataLayerError::UnexpectedValue(format!("{name} overflow")))
}

fn optional_u64_to_i64(value: Option<u64>, name: &str) -> Result<Option<i64>, DataLayerError> {
    value.map(|value| u64_to_i64(value, name)).transpose()
}

#[cfg(test)]
mod tests {
    use super::MysqlRequestCandidateRepository;
    use crate::run_migrations;
    use aether_data_contracts::repository::candidates::{
        RequestCandidateReadRepository, RequestCandidateStatus, RequestCandidateWriteRepository,
        StoredRequestCandidate, UpsertRequestCandidateRecord,
    };
    use serde_json::json;

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlRequestCandidateRepository::new(pool);
    }

    #[tokio::test]
    async fn mysql_concurrent_and_batch_upserts_are_atomic_when_configured() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql candidate lifecycle test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(12)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");
        let repository = MysqlRequestCandidateRepository::new(pool.clone());
        let request_id = format!("candidate-concurrency-{}", uuid::Uuid::new_v4());
        let mut initial = sample_upsert(
            &request_id,
            "initial",
            RequestCandidateStatus::Pending,
            Some(json!({"gateway_execution_runtime": true})),
            3_000_000,
        );
        initial.is_cached = Some(false);
        repository
            .upsert(initial)
            .await
            .expect("initial candidate should insert");
        sqlx::query(
            "UPDATE request_candidates SET skip_reason = ?, error_type = ?, error_message = ?, extra_data = ?, required_capabilities = ? WHERE request_id = ?",
        )
        .bind("legacy skip reason with tenant-secret")
        .bind("legacy_error_type_with_token")
        .bind("Bearer legacy-secret")
        .bind(r#"{"gateway_execution_runtime":true,"request_body":{"password":"secret"}}"#)
        .bind(r#"{"streaming":true,"internal_capability":"secret"}"#)
        .bind(&request_id)
        .execute(&pool)
        .await
        .expect("legacy diagnostics should be injected for the conflict test");

        const WRITERS: usize = 8;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(WRITERS));
        let mut tasks = Vec::new();
        for writer in 0..WRITERS {
            let repository = repository.clone();
            let request_id = request_id.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                let status = if writer == 0 {
                    RequestCandidateStatus::Success
                } else {
                    RequestCandidateStatus::Streaming
                };
                let extra_data = match writer {
                    0 => json!({"stream_completed": true}),
                    1 => json!({"cache_1h": true}),
                    2 => json!({"first_byte_time_ms": 2}),
                    3 => json!({"pool_key_index": 3}),
                    4 => json!({"priority_slot": 4}),
                    5 => json!({"ranking_index": 5}),
                    6 => json!({"phase": "provider_request"}),
                    7 => json!({"provider_api_format": "openai:responses"}),
                    _ => unreachable!("writer index is bounded by WRITERS"),
                };
                let mut candidate = sample_upsert(
                    &request_id,
                    format!("writer-{writer}").as_str(),
                    status,
                    Some(extra_data),
                    3_100_000 + u64::try_from(writer).expect("writer index should fit") * 10,
                );
                if writer != 0 {
                    candidate.latency_ms = Some(9_000 + writer as u64);
                    candidate.finished_at_unix_ms = Some(9_000_000 + writer as u64);
                }
                barrier.wait().await;
                repository.upsert(candidate).await
            }));
        }
        for task in tasks {
            task.await
                .expect("candidate writer should join")
                .expect("candidate writer should persist");
        }

        let candidates = repository
            .list_by_request_id(&request_id)
            .await
            .expect("mysql request candidates should load");
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.id, "initial");
        assert_eq!(candidate.status, RequestCandidateStatus::Success);
        assert_eq!(candidate.latency_ms, Some(123));
        assert_eq!(candidate.finished_at_unix_ms, Some(3_100_002));
        assert_eq!(
            candidate.extra_data,
            Some(json!({
                "cache_1h": true,
                "first_byte_time_ms": 2,
                "gateway_execution_runtime": true,
                "phase": "provider_request",
                "pool_key_index": 3,
                "priority_slot": 4,
                "provider_api_format": "openai:responses",
                "ranking_index": 5,
                "stream_completed": true
            }))
        );
        let raw = sqlx::query(
            "SELECT skip_reason, error_type, error_message, extra_data, required_capabilities FROM request_candidates WHERE request_id = ?",
        )
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("raw candidate diagnostics should load");
        assert_eq!(
            sqlx::Row::try_get::<Option<String>, _>(&raw, "error_message")
                .expect("error_message should decode")
                .as_deref(),
            Some("Bearer legacy-secret")
        );
        assert_eq!(
            sqlx::Row::try_get::<Option<String>, _>(&raw, "skip_reason")
                .expect("skip_reason should decode")
                .as_deref(),
            Some("unclassified_skip")
        );
        assert_eq!(
            sqlx::Row::try_get::<Option<String>, _>(&raw, "error_type")
                .expect("error_type should decode")
                .as_deref(),
            Some("unclassified_error")
        );
        let raw_extra = sqlx::Row::try_get::<Option<String>, _>(&raw, "extra_data")
            .expect("extra_data should decode")
            .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
        assert_eq!(raw_extra, candidate.extra_data);
        let raw_capabilities =
            sqlx::Row::try_get::<Option<String>, _>(&raw, "required_capabilities")
                .expect("required_capabilities should decode")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
        assert_eq!(raw_capabilities, Some(json!({"streaming": true})));

        let batch_request_id = format!("candidate-batch-{}", uuid::Uuid::new_v4());
        let mut pending = sample_upsert(
            &batch_request_id,
            "batch-first",
            RequestCandidateStatus::Pending,
            Some(json!({"gateway_execution_runtime": true})),
            4_000_000,
        );
        pending.is_cached = Some(false);
        let mut streaming = sample_upsert(
            &batch_request_id,
            "batch-second",
            RequestCandidateStatus::Streaming,
            Some(json!({"stream_completed": true})),
            4_000_100,
        );
        streaming.is_cached = None;
        let mut success = sample_upsert(
            &batch_request_id,
            "batch-third",
            RequestCandidateStatus::Success,
            Some(json!({"cache_1h": true})),
            4_000_200,
        );
        success.is_cached = Some(true);
        let mut late_pending = sample_upsert(
            &batch_request_id,
            "batch-fourth",
            RequestCandidateStatus::Pending,
            Some(json!({"first_byte_time_ms": 42})),
            4_000_300,
        );
        late_pending.is_cached = None;
        late_pending.latency_ms = Some(9_999);
        late_pending.finished_at_unix_ms = Some(9_999_999);
        assert_eq!(
            repository
                .upsert_many(vec![pending, streaming, success, late_pending])
                .await
                .expect("ordered batch should persist"),
            4
        );
        let batch_candidates = repository
            .list_by_request_id(&batch_request_id)
            .await
            .expect("batch candidate should load");
        assert_eq!(batch_candidates.len(), 1);
        assert_eq!(batch_candidates[0].id, "batch-first");
        assert_eq!(batch_candidates[0].status, RequestCandidateStatus::Success);
        assert!(batch_candidates[0].is_cached);
        assert_eq!(batch_candidates[0].latency_ms, Some(123));
        assert_eq!(batch_candidates[0].finished_at_unix_ms, Some(4_000_202));
        assert_eq!(
            batch_candidates[0].extra_data,
            Some(json!({
                "cache_1h": true,
                "first_byte_time_ms": 42,
                "gateway_execution_runtime": true,
                "stream_completed": true
            }))
        );

        let rollback_request_id = format!("candidate-rollback-{}", uuid::Uuid::new_v4());
        let valid = sample_upsert(
            &rollback_request_id,
            "rollback-valid",
            RequestCandidateStatus::Pending,
            None,
            5_000_000,
        );
        let mut invalid = sample_upsert(
            &rollback_request_id,
            "rollback-invalid",
            RequestCandidateStatus::Success,
            None,
            5_000_100,
        );
        invalid.candidate_index = 1;
        invalid.latency_ms = Some(u64::MAX);
        repository
            .upsert_many(vec![valid, invalid])
            .await
            .expect_err("invalid later row should roll back the batch");
        assert!(repository
            .list_by_request_id(&rollback_request_id)
            .await
            .expect("rolled-back batch should be readable")
            .is_empty());

        sqlx::query("DELETE FROM request_candidates WHERE request_id IN (?, ?, ?)")
            .bind(&request_id)
            .bind(&batch_request_id)
            .bind(&rollback_request_id)
            .execute(&pool)
            .await
            .expect("mysql candidate test rows should clean up");
    }

    #[test]
    fn merge_candidate_preserves_first_identity_and_terminal_fact() {
        let existing = StoredRequestCandidate::new(
            "candidate-1".to_string(),
            "request-1".to_string(),
            Some("user-1".to_string()),
            Some("key-1".to_string()),
            None,
            None,
            0,
            0,
            Some("provider-1".to_string()),
            Some("endpoint-1".to_string()),
            Some("provider-key-1".to_string()),
            RequestCandidateStatus::Success,
            None,
            false,
            Some(200),
            None,
            None,
            Some(123),
            None,
            Some(serde_json::json!({"stream_completed": true})),
            None,
            1_000,
            Some(1_001),
            Some(1_123),
        )
        .expect("existing candidate should build");

        let merged = super::merge_candidate(
            UpsertRequestCandidateRecord {
                id: "candidate-late".to_string(),
                request_id: "request-1".to_string(),
                user_id: Some("attacker-user".to_string()),
                api_key_id: Some("attacker-api-key".to_string()),
                username: Some("mallory".to_string()),
                api_key_name: Some("attacker-key".to_string()),
                candidate_index: 0,
                retry_index: 0,
                provider_id: Some("attacker-provider".to_string()),
                endpoint_id: Some("attacker-endpoint".to_string()),
                key_id: Some("attacker-provider-key".to_string()),
                status: RequestCandidateStatus::Failed,
                skip_reason: None,
                is_cached: Some(false),
                status_code: Some(200),
                error_type: None,
                error_message: Some("Bearer secret-token".to_string()),
                latency_ms: Some(9_999),
                concurrent_requests: None,
                extra_data: Some(serde_json::json!({"gateway_execution_runtime": true})),
                required_capabilities: None,
                created_at_unix_ms: Some(1_050),
                started_at_unix_ms: Some(1_051),
                finished_at_unix_ms: None,
            },
            Some(existing),
        )
        .expect("candidate should merge");

        assert_eq!(merged.id, "candidate-1");
        assert_eq!(merged.status, RequestCandidateStatus::Success);
        assert_eq!(merged.user_id.as_deref(), Some("user-1"));
        assert_eq!(merged.api_key_id.as_deref(), Some("key-1"));
        assert_eq!(merged.provider_id.as_deref(), Some("provider-1"));
        assert_eq!(merged.endpoint_id.as_deref(), Some("endpoint-1"));
        assert_eq!(merged.key_id.as_deref(), Some("provider-key-1"));
        assert!(merged.error_message.is_none());
        assert_eq!(merged.latency_ms, Some(123));
        assert_eq!(merged.finished_at_unix_ms, Some(1_123));
        assert_eq!(
            merged.extra_data,
            Some(serde_json::json!({
                "gateway_execution_runtime": true,
                "stream_completed": true
            }))
        );
    }

    #[tokio::test]
    async fn mysql_failed_candidate_diagnostics_survive_late_success_and_streaming() {
        let Ok(database_url) = std::env::var("AETHER_TEST_MYSQL_URL") else {
            eprintln!("skipping MySQL diagnostic replay test: AETHER_TEST_MYSQL_URL is unset");
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();
        let repository = MysqlRequestCandidateRepository::new(pool.clone());
        let request_id = format!("diagnostic-replay-{}", uuid::Uuid::new_v4());
        let candidate_id = uuid::Uuid::new_v4().to_string();
        let original_extra = json!({
            "upstream_response": {
                "status_code": 503,
                "headers": {"retry-after": "30"},
                "body": {"error": {"message": "original upstream failure"}}
            },
            "failure_diagnostic": {"path": "$.input", "message": "original diagnostic"}
        });
        let mut failed = sample_upsert(
            &request_id,
            &candidate_id,
            RequestCandidateStatus::Failed,
            Some(original_extra.clone()),
            5_000_000,
        );
        failed.request_id = request_id.clone();
        failed.status_code = Some(503);
        failed.error_type = Some("upstream_error".to_string());
        failed.error_message = Some("original upstream failure".to_string());
        repository.upsert(failed).await.unwrap();

        for status in [
            RequestCandidateStatus::Success,
            RequestCandidateStatus::Streaming,
        ] {
            let mut late = sample_upsert(
                &request_id,
                &uuid::Uuid::new_v4().to_string(),
                status,
                Some(json!({
                    "gateway_execution_runtime": true,
                    "upstream_response": {"status_code": 200, "body": "late unrelated body"},
                    "failure_diagnostic": {"message": "late unrelated diagnostic"}
                })),
                5_000_500,
            );
            late.request_id = request_id.clone();
            late.error_message = Some("late unrelated error".to_string());
            late.latency_ms = Some(9_999);
            repository.upsert_many(vec![late]).await.unwrap();

            let stored = repository.list_by_request_id(&request_id).await.unwrap();
            assert_eq!(stored.len(), 1);
            let candidate = &stored[0];
            assert_eq!(candidate.id, candidate_id);
            assert_eq!(candidate.status, RequestCandidateStatus::Failed);
            assert_eq!(candidate.status_code, Some(503));
            assert_eq!(candidate.error_type.as_deref(), Some("upstream_error"));
            assert_eq!(
                candidate.error_message.as_deref(),
                Some("original upstream failure")
            );
            assert_eq!(candidate.latency_ms, Some(123));
            assert_eq!(candidate.finished_at_unix_ms, Some(5_000_002));
            let extra = candidate.extra_data.as_ref().unwrap();
            assert_eq!(
                extra["upstream_response"],
                original_extra["upstream_response"]
            );
            assert_eq!(
                extra["failure_diagnostic"],
                original_extra["failure_diagnostic"]
            );
            assert_eq!(extra["gateway_execution_runtime"], true);
        }
        sqlx::query("DELETE FROM request_candidates WHERE request_id = ?")
            .bind(request_id)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    fn sample_upsert(
        request_id: &str,
        id: &str,
        status: RequestCandidateStatus,
        extra_data: Option<serde_json::Value>,
        created_at_unix_ms: u64,
    ) -> UpsertRequestCandidateRecord {
        UpsertRequestCandidateRecord {
            id: id.to_string(),
            request_id: request_id.to_string(),
            user_id: Some("user-1".to_string()),
            api_key_id: Some("key-1".to_string()),
            username: Some("user".to_string()),
            api_key_name: Some("Key".to_string()),
            candidate_index: 0,
            retry_index: 0,
            provider_id: Some("provider-1".to_string()),
            endpoint_id: Some("endpoint-1".to_string()),
            key_id: Some("provider-key-1".to_string()),
            status,
            skip_reason: None,
            is_cached: Some(false),
            status_code: Some(200),
            error_type: None,
            error_message: None,
            latency_ms: Some(123),
            concurrent_requests: Some(2),
            extra_data,
            required_capabilities: Some(json!({"streaming": true})),
            created_at_unix_ms: Some(created_at_unix_ms),
            started_at_unix_ms: Some(created_at_unix_ms + 1),
            finished_at_unix_ms: Some(created_at_unix_ms + 2),
        }
    }
}
