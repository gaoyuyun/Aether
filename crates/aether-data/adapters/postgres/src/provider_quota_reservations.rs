//! Atomic provider admission; no network work occurs while holding this lock.
use crate::error::SqlxResultExt;
use aether_data_contracts::{
    repository::candidates::{
        provider_quota_dispatch_snapshot, RequestCandidateStatus, StoredRequestCandidate,
    },
    DataLayerError,
};
use sqlx::Row;

// Candidate inserts can already hold a foreign-key KEY SHARE lock. NO KEY
// UPDATE serializes reservations without an incompatible lock upgrade/deadlock.
const PROVIDER_SQL: &str = r#"SELECT p.is_active, CAST(p.billing_type AS TEXT) AS billing_type,
 (CAST(EXTRACT(EPOCH FROM p.quota_last_reset_at) AS BIGINT) / 60) * 60 AS epoch, CAST(EXTRACT(EPOCH FROM p.quota_expires_at) AS BIGINT) AS expires_at, p.quota_subscription_started_at AS subscription_start,
 CAST(p.quota_reset_day AS BIGINT) AS quota_reset_day, p.quota_cycle_start_at, CAST(EXTRACT(EPOCH FROM p.pending_quota_reset_at) AS BIGINT) AS pending_quota_reset_at,
 CAST(p.monthly_quota_usd AS DOUBLE PRECISION) AS limit_usd, CAST(p.config AS TEXT) AS config
 FROM providers p WHERE p.id = $1 FOR NO KEY UPDATE"#;
const CYCLE_SQL: &str = r#"SELECT CAST(COALESCE(q.monthly_used_usd, 0) + COALESCE((SELECT SUM(d.provider_quota_cost_usd) FROM usage_counter_deltas d
 WHERE d.kind = 'provider_monthly' AND d.target_id = q.provider_id
 AND d.quota_epoch_start_at_usage = q.epoch AND d.processed_at IS NULL AND d.quota_accounting_status = 'ready'
 ), 0) + COALESCE((SELECT CAST(SUM(r.reserved_cost_units) AS DOUBLE PRECISION) / 100000000.0 FROM provider_quota_reservations r
 WHERE r.provider_id = q.provider_id AND r.quota_epoch_start = q.epoch AND r.state IN ('reserved', 'uncertain')

 AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas d WHERE d.kind = 'provider_monthly'
 AND d.request_id = r.candidate_id AND d.quota_accounting_status IN ('ready', 'reconciled'))), 0) AS DOUBLE PRECISION) AS used_usd
 FROM (SELECT p.id AS provider_id, p.monthly_used_usd, (CAST(EXTRACT(EPOCH FROM p.quota_last_reset_at) AS BIGINT) / 60) * 60 AS epoch FROM providers p WHERE p.id = $1) q"#;
const WINDOW_SQL: &str = r#"
SELECT w.status, CAST(w.used_usd
 + COALESCE((SELECT SUM(b.used_usd) FROM provider_quota_usage_buckets b
 WHERE b.provider_id=w.provider_id AND b.quota_epoch_start=w.quota_epoch_start
 AND b.bucket_start >= GREATEST(w.accounted_until, TO_TIMESTAMP(GREATEST(w.epoch_secs,n.clock-w.duration_secs)::double precision))
 AND b.bucket_start < TO_TIMESTAMP((n.clock+60)::double precision)),0)
 - COALESCE((SELECT SUM(b.used_usd) FROM provider_quota_usage_buckets b
 WHERE b.provider_id=w.provider_id AND b.quota_epoch_start=w.quota_epoch_start
 AND b.bucket_start >= w.rolling_start
 AND b.bucket_start < TO_TIMESTAMP(GREATEST(w.epoch_secs,n.clock-w.duration_secs)::double precision)
 AND b.bucket_start < w.accounted_until),0)
 + COALESCE((SELECT SUM(d.provider_quota_cost_usd) FROM usage_counter_deltas d
 WHERE d.kind='provider_monthly' AND d.target_id=w.provider_id
 AND d.quota_epoch_start_at_usage=w.epoch_secs AND d.processed_at IS NULL AND d.quota_accounting_status='ready'
 AND d.provider_dispatch_at_unix_secs >= GREATEST(w.epoch_secs,n.clock-w.duration_secs)
 AND d.provider_dispatch_at_unix_secs < n.clock+60),0)
 + COALESCE((SELECT CAST(SUM(r.reserved_cost_units) AS double precision)/100000000.0 FROM provider_quota_reservations r
 WHERE r.provider_id=w.provider_id AND r.quota_epoch_start=w.epoch_secs AND r.state IN ('reserved','uncertain')
 AND r.dispatch_at >= GREATEST(w.epoch_secs,n.clock-w.duration_secs) AND r.dispatch_at < n.clock+60
 AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas d WHERE d.kind='provider_monthly'
 AND d.request_id=r.candidate_id AND d.quota_accounting_status IN ('ready','reconciled'))),0)
 AS double precision) AS used_usd
FROM (SELECT counter.*, CAST(EXTRACT(EPOCH FROM quota_epoch_start) AS BIGINT) AS epoch_secs
 FROM provider_quota_window_counters counter) w
CROSS JOIN (SELECT CAST($1 AS BIGINT) AS clock) n
WHERE w.provider_id=$2 AND w.duration_secs=$3
AND w.quota_epoch_start=TO_TIMESTAMP(CAST($4 AS BIGINT)::double precision)
"#;

pub(crate) async fn reserve(
    tx: &mut crate::PostgresTransaction,
    candidate: &StoredRequestCandidate,
) -> Result<(), DataLayerError> {
    if !matches!(
        candidate.status,
        RequestCandidateStatus::Pending | RequestCandidateStatus::Streaming
    ) {
        return Ok(());
    }
    let Some(snapshot) = provider_quota_dispatch_snapshot(candidate.extra_data.as_ref())?
        .filter(|s| s.is_monthly_quota())
    else {
        return Ok(());
    };
    let Some(estimate) = snapshot.reserved_cost_usd else {
        return Ok(());
    };
    let provider_id = candidate.provider_id.as_deref().ok_or_else(|| {
        DataLayerError::InvalidInput("quota reservation is missing provider".into())
    })?;
    let epoch = snapshot
        .quota_epoch_start_at_usage
        .ok_or_else(|| DataLayerError::InvalidInput("quota reservation is missing epoch".into()))?
        as i64;
    let units = (estimate * 100_000_000.0).ceil() as i64;
    let rejected = |reason: &str| DataLayerError::ProviderQuotaUnavailable {
        provider_id: provider_id.to_owned(),
        reason: reason.to_owned(),
    };
    if let Some(row) = sqlx::query(r#"SELECT provider_id, quota_epoch_start, reserved_cost_units FROM provider_quota_reservations WHERE candidate_id = $1"#).bind(&candidate.id).fetch_optional(&mut **tx).await.map_postgres_err()? {
        if row.try_get::<String,_>("provider_id").map_postgres_err()? != provider_id || row.try_get::<i64,_>("quota_epoch_start").map_postgres_err()? != epoch {
            return Err(DataLayerError::InvalidInput("quota reservation identity conflict".into()));
        }
        return Ok(());
    }
    let provider = sqlx::query(PROVIDER_SQL)
        .bind(provider_id)
        .fetch_optional(&mut **tx)
        .await
        .map_postgres_err()?
        .ok_or_else(|| rejected("provider_missing"))?;
    let dispatch_at = snapshot.provider_dispatch_at_unix_secs as i64;
    // A waiter can cross a minute boundary while another admission commits.
    // Validate against the current clock, retaining dispatch-time attribution.
    let now = dispatch_at.max(chrono::Utc::now().timestamp());
    if !provider
        .try_get::<bool, _>("is_active")
        .map_postgres_err()?
        || provider
            .try_get::<String, _>("billing_type")
            .map_postgres_err()?
            != "monthly_quota"
        || provider
            .try_get::<Option<i64>, _>("epoch")
            .map_postgres_err()?
            != Some(epoch)
        || epoch > now
        || provider
            .try_get::<Option<i64>, _>("subscription_start")
            .map_postgres_err()?
            .is_some_and(|s| s > now)
        || provider
            .try_get::<Option<i64>, _>("expires_at")
            .map_postgres_err()?
            .is_some_and(|s| s <= now)
        || provider
            .try_get::<Option<i64>, _>("pending_quota_reset_at")
            .map_postgres_err()?
            .is_some_and(|s| s <= now)
    {
        return Err(rejected("subscription_unavailable"));
    }
    let days: Option<i64> = provider.try_get("quota_reset_day").map_postgres_err()?;
    let cycle: Option<i64> = provider
        .try_get("quota_cycle_start_at")
        .map_postgres_err()?;
    if days.is_some_and(|days| {
        !(1..=30).contains(&days)
            || cycle
                .unwrap_or(epoch)
                .saturating_add(days.saturating_mul(86400))
                <= now
    }) {
        return Err(rejected("quota_reset_due"));
    }
    // Explicit zero-cost routes still obey activation/expiry, but not spending limits.
    if units > 0 {
        let tasks: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM provider_quota_maintenance_state WHERE provider_id = $1 AND quota_epoch_start = $2 AND status IN ('pending', 'running', 'failed')"#).bind(provider_id).bind(epoch).fetch_one(&mut **tx).await.map_postgres_err()?;
        if tasks != 0 {
            return Err(rejected("quota_accounting_unavailable"));
        }
        let unresolved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_counter_deltas d WHERE d.kind='provider_monthly' AND d.target_id=$1 AND d.quota_epoch_start_at_usage=$2 AND d.quota_accounting_status IN ('pending','failed') AND NOT EXISTS (SELECT 1 FROM provider_quota_reservations r WHERE r.candidate_id=d.request_id AND r.provider_id=d.target_id AND r.quota_epoch_start=d.quota_epoch_start_at_usage AND r.state IN ('reserved','uncertain'))").bind(provider_id).bind(epoch).fetch_one(&mut **tx).await.map_postgres_err()?;
        if unresolved != 0 {
            return Err(rejected("quota_accounting_unavailable"));
        }
        let used: f64 = sqlx::query_scalar(CYCLE_SQL)
            .bind(provider_id)
            .fetch_one(&mut **tx)
            .await
            .map_postgres_err()?;
        let limit: Option<f64> = provider.try_get("limit_usd").map_postgres_err()?;
        if !used.is_finite()
            || limit.is_some_and(|limit| used.max(0.0) + estimate > limit + 0.000000001)
        {
            return Err(rejected("cycle_reservation_insufficient"));
        }
        let config = provider
            .try_get::<Option<String>, _>("config")
            .map_postgres_err()?
            .map(|v| serde_json::from_str::<serde_json::Value>(&v))
            .transpose()
            .map_err(|e| DataLayerError::UnexpectedValue(e.to_string()))?;
        if !aether_wallet::quota_windows_config_is_valid(config.as_ref()) {
            return Err(rejected("quota_windows_invalid"));
        }
        for window in aether_wallet::quota_windows_from_config(config.as_ref()) {
            let row = sqlx::query(WINDOW_SQL)
                .bind(now / 60 * 60)
                .bind(provider_id)
                .bind(window.duration_secs as i64)
                .bind(epoch)
                .fetch_optional(&mut **tx)
                .await
                .map_postgres_err()?
                .ok_or_else(|| rejected("quota_window_unavailable"))?;
            let used: f64 = row.try_get("used_usd").map_postgres_err()?;
            if row.try_get::<String, _>("status").map_postgres_err()? != "ready"
                || !used.is_finite()
            {
                return Err(rejected("quota_window_unavailable"));
            }
            if used.max(0.0) + estimate > window.limit_usd + 0.000000001 {
                return Err(rejected("window_reservation_insufficient"));
            }
        }
    }
    sqlx::query(r#"INSERT INTO provider_quota_reservations (candidate_id, provider_id, quota_epoch_start, dispatch_at, reserved_cost_units, state, created_at) VALUES ($1, $2, $3, $4, $5, 'reserved', $6)"#).bind(&candidate.id).bind(provider_id).bind(epoch).bind(dispatch_at).bind(units).bind(now).execute(&mut **tx).await.map_postgres_err()?;
    Ok(())
}

/// Called in the same transaction that makes the actual-cost outbox visible.
/// Admission also joins the outbox, so a late candidate update cannot recreate
/// an occupied reservation or expose a gap before the minute flusher runs.
pub(crate) async fn settle_if_ready(
    tx: &mut crate::PostgresTransaction,
    candidate_id: &str,
    now: i64,
) -> Result<(), DataLayerError> {
    sqlx::query(r#"UPDATE provider_quota_reservations SET state = 'settled', finalized_at = $1 WHERE candidate_id = $2
 AND state IN ('reserved', 'uncertain') AND EXISTS (SELECT 1 FROM usage_counter_deltas d
 WHERE d.request_id = provider_quota_reservations.candidate_id AND d.kind = 'provider_monthly'
 AND d.quota_accounting_status IN ('ready', 'reconciled'))"#).bind(now).bind(candidate_id).execute(&mut **tx).await.map_postgres_err()?;
    Ok(())
}

/// Keep only this attempt's estimate when the upstream may have consumed tokens
/// but did not provide a measurable bill. A lease/timeout is not proof of zero cost.
pub(crate) async fn retain_uncertain(
    tx: &mut crate::PostgresTransaction,
    candidate_id: &str,
) -> Result<bool, DataLayerError> {
    let result = sqlx::query("UPDATE provider_quota_reservations SET state = 'uncertain' WHERE candidate_id = $1 AND state = 'reserved' AND NOT EXISTS (SELECT 1 FROM usage_counter_deltas d WHERE d.request_id = provider_quota_reservations.candidate_id AND d.kind = 'provider_monthly' AND d.quota_accounting_status IN ('ready','reconciled'))")
        .bind(candidate_id).execute(&mut **tx).await.map_postgres_err()?;
    Ok(result.rows_affected() > 0)
}
