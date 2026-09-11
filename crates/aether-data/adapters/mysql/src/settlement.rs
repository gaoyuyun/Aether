use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, Acquire, Row};

use aether_data_contracts::repository::settlement::{
    finite_wallet_available_usd, plan_finite_wallet_debit, settlement_billable_cost_usd,
    settlement_billing_status_for_usage_status, validate_wallet_settlement_values,
    ReconcileUsagePolicyCostInput, ReleaseUsagePolicyRequestAdmissionInput,
    ReserveUsagePolicyCostInput, ReserveUsagePolicyCostOutcome, ReserveUsagePolicyRequestInput,
    ReserveUsagePolicyRequestOutcome, SettlementWriteRepository, StoredUsagePolicyCostReservation,
    StoredUsagePolicyRequestAdmission, StoredUsageSettlement, UsagePolicyCostReservationState,
    UsagePolicyRequestAdmissionState, UsageSettlementInput, SETTLEMENT_EPSILON_USD,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const FIND_USAGE_FOR_SETTLEMENT_SQL: &str = r#"
SELECT
  usage_record.request_id,
  COALESCE(usage_settlement_snapshots.wallet_id, usage_record.wallet_id) AS wallet_id,
  COALESCE(usage_settlement_snapshots.billing_status, usage_record.billing_status) AS billing_status,
  COALESCE(
    usage_settlement_snapshots.wallet_balance_before,
    usage_record.wallet_balance_before
  ) AS wallet_balance_before,
  COALESCE(
    usage_settlement_snapshots.wallet_balance_after,
    usage_record.wallet_balance_after
  ) AS wallet_balance_after,
  COALESCE(
    usage_settlement_snapshots.wallet_recharge_balance_before,
    usage_record.wallet_recharge_balance_before
  ) AS wallet_recharge_balance_before,
  COALESCE(
    usage_settlement_snapshots.wallet_recharge_balance_after,
    usage_record.wallet_recharge_balance_after
  ) AS wallet_recharge_balance_after,
  COALESCE(
    usage_settlement_snapshots.wallet_gift_balance_before,
    usage_record.wallet_gift_balance_before
  ) AS wallet_gift_balance_before,
  COALESCE(
    usage_settlement_snapshots.wallet_gift_balance_after,
    usage_record.wallet_gift_balance_after
  ) AS wallet_gift_balance_after,
  usage_settlement_snapshots.provider_monthly_used_usd AS provider_monthly_used_usd,
  usage_record.provider_id,
  COALESCE(
    usage_routing_snapshots.candidate_id,
    (
      SELECT MAX(candidate.id)
      FROM request_candidates AS candidate
      WHERE candidate.request_id = usage_record.request_id
        AND candidate.provider_id = usage_record.provider_id
        AND candidate.status = 'success'
      HAVING COUNT(*) = 1
    )
  ) AS provider_attempt_id,
  usage_record.created_at_unix_ms AS usage_created_at_unix_secs,
  COALESCE(
    JSON_UNQUOTE(JSON_EXTRACT(usage_settlement_snapshots.settlement_snapshot, '$.pricing_snapshot.provider_billing_type')),
    CAST(provider.billing_type AS CHAR)
  ) AS provider_billing_type_at_usage,
  COALESCE(
    CAST(JSON_UNQUOTE(JSON_EXTRACT(usage_settlement_snapshots.settlement_snapshot, '$.pricing_snapshot.provider_quota_epoch_start_unix_secs')) AS SIGNED),
    provider.quota_last_reset_at
  ) AS quota_epoch_start_at_usage,
  CAST(JSON_UNQUOTE(JSON_EXTRACT(usage_settlement_snapshots.settlement_snapshot, '$.provider_quota_cost_usd')) AS DOUBLE) AS provider_quota_cost_usd,
  CASE
    WHEN JSON_UNQUOTE(JSON_EXTRACT(usage_settlement_snapshots.settlement_snapshot, '$.status')) = 'complete'
      OR LOWER(COALESCE(usage_record.endpoint_api_format, '')) = 'openai:search'
      THEN 1
    ELSE 0
  END AS provider_quota_cost_is_resolved,
  usage_settlement_snapshots.billing_rule_version AS pricing_rule_version_at_usage,
  JSON_EXTRACT(usage_settlement_snapshots.settlement_snapshot, '$.pricing_snapshot') AS provider_pricing_snapshot_at_usage,
  COALESCE(usage_settlement_snapshots.finalized_at, usage_record.finalized_at) AS finalized_at_unix_secs
FROM `usage` AS usage_record
LEFT JOIN usage_settlement_snapshots
  ON usage_settlement_snapshots.request_id = usage_record.request_id
LEFT JOIN providers AS provider
  ON provider.id = usage_record.provider_id
LEFT JOIN usage_routing_snapshots
  ON usage_routing_snapshots.request_id = usage_record.request_id
WHERE usage_record.request_id = ?
FOR UPDATE
"#;

const FINALIZE_USAGE_BILLING_SQL: &str = r#"
UPDATE `usage`
SET
  billing_status = ?,
  finalized_at = COALESCE(finalized_at, ?)
WHERE request_id = ?
"#;

const UPSERT_USAGE_SETTLEMENT_SNAPSHOT_SQL: &str = r#"
INSERT INTO usage_settlement_snapshots (
  request_id,
  billing_status,
  wallet_id,
  wallet_balance_before,
  wallet_balance_after,
  wallet_recharge_balance_before,
  wallet_recharge_balance_after,
  wallet_gift_balance_before,
  wallet_gift_balance_after,
  provider_monthly_used_usd,
  finalized_at,
  created_at,
  updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  billing_status = VALUES(billing_status),
  wallet_id = COALESCE(VALUES(wallet_id), wallet_id),
  wallet_balance_before = COALESCE(VALUES(wallet_balance_before), wallet_balance_before),
  wallet_balance_after = COALESCE(VALUES(wallet_balance_after), wallet_balance_after),
  wallet_recharge_balance_before = COALESCE(
    VALUES(wallet_recharge_balance_before),
    wallet_recharge_balance_before
  ),
  wallet_recharge_balance_after = COALESCE(
    VALUES(wallet_recharge_balance_after),
    wallet_recharge_balance_after
  ),
  wallet_gift_balance_before = COALESCE(VALUES(wallet_gift_balance_before), wallet_gift_balance_before),
  wallet_gift_balance_after = COALESCE(VALUES(wallet_gift_balance_after), wallet_gift_balance_after),
  provider_monthly_used_usd = COALESCE(VALUES(provider_monthly_used_usd), provider_monthly_used_usd),
  finalized_at = COALESCE(VALUES(finalized_at), finalized_at),
  updated_at = VALUES(updated_at)
"#;

const ENQUEUE_PROVIDER_MONTHLY_USAGE_DELTA_SQL: &str = r#"
INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, total_cost_usd_delta,
  usage_created_at_unix_secs, provider_billing_type_at_usage,
  quota_epoch_start_at_usage, provider_dispatch_at_unix_secs,
  provider_quota_cost_usd, pricing_rule_version_at_usage,
  provider_pricing_snapshot_at_usage, quota_accounting_status, created_at
)
VALUES (?, ?, 'provider_monthly', ?, ?, ?, 'monthly_quota', ?, ?, ?, ?, ?, ?, ?)
"#;

#[derive(Debug, Clone)]
pub struct MysqlSettlementRepository {
    pool: MysqlPool,
}

impl MysqlSettlementRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

fn usage_policy_cost_i64(value: u64, field: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::InvalidInput(format!("{field} exceeds the integer range")))
}

fn usage_policy_cost_u64(value: i64, field: &str) -> Result<u64, DataLayerError> {
    u64::try_from(value)
        .map_err(|_| DataLayerError::UnexpectedValue(format!("{field} must not be negative")))
}

fn usage_policy_request_admission_from_mysql_row(
    row: &MySqlRow,
) -> Result<StoredUsagePolicyRequestAdmission, DataLayerError> {
    let state: String = row.try_get("state").map_sql_err()?;
    Ok(StoredUsagePolicyRequestAdmission {
        request_id: row.try_get("request_id").map_sql_err()?,
        subject_id: row.try_get("subject_id").map_sql_err()?,
        event_token: row.try_get("event_token").map_sql_err()?,
        admitted_at_unix_secs: usage_policy_cost_u64(
            row.try_get("admitted_at_unix_secs").map_sql_err()?,
            "usage policy request admitted_at",
        )?,
        retain_until_unix_secs: usage_policy_cost_u64(
            row.try_get("retain_until_unix_secs").map_sql_err()?,
            "usage policy request retain_until",
        )?,
        state: UsagePolicyRequestAdmissionState::parse(&state).ok_or_else(|| {
            DataLayerError::UnexpectedValue(format!(
                "unknown usage policy request admission state {state}"
            ))
        })?,
        released_at_unix_secs: row
            .try_get::<Option<i64>, _>("released_at_unix_secs")
            .map_sql_err()?
            .map(|value| usage_policy_cost_u64(value, "usage policy request released_at"))
            .transpose()?,
    })
}

const FIND_USAGE_POLICY_REQUEST_ADMISSION_MYSQL_SQL: &str = r#"
SELECT request_id, subject_id, event_token,
       admitted_at AS admitted_at_unix_secs,
       retain_until AS retain_until_unix_secs,
       state, released_at AS released_at_unix_secs
FROM usage_request_admissions
WHERE event_token = ?
FOR UPDATE
"#;

const USAGE_POLICY_REQUEST_TRANSACTION_ISOLATION_MYSQL_SQL: &str =
    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED";

fn usage_policy_cost_reservation_from_mysql_row(
    row: &MySqlRow,
) -> Result<StoredUsagePolicyCostReservation, DataLayerError> {
    let state: String = row.try_get("state").map_sql_err()?;
    Ok(StoredUsagePolicyCostReservation {
        request_id: row.try_get("request_id").map_sql_err()?,
        subject_id: row.try_get("subject_id").map_sql_err()?,
        reservation_token: row.try_get("reservation_token").map_sql_err()?,
        admitted_at_unix_secs: usage_policy_cost_u64(
            row.try_get("admitted_at_unix_secs").map_sql_err()?,
            "usage policy admitted_at",
        )?,
        reserved_cost_units: usage_policy_cost_u64(
            row.try_get("reserved_cost_units").map_sql_err()?,
            "usage policy reserved_cost_units",
        )?,
        actual_cost_units: row
            .try_get::<Option<i64>, _>("actual_cost_units")
            .map_sql_err()?
            .map(|value| usage_policy_cost_u64(value, "usage policy actual_cost_units"))
            .transpose()?,
        state: UsagePolicyCostReservationState::parse(&state).ok_or_else(|| {
            DataLayerError::UnexpectedValue(format!(
                "unknown usage policy reservation state {state}"
            ))
        })?,
        reservation_expires_at_unix_secs: usage_policy_cost_u64(
            row.try_get("reservation_expires_at_unix_secs")
                .map_sql_err()?,
            "usage policy reservation_expires_at",
        )?,
        retain_until_unix_secs: usage_policy_cost_u64(
            row.try_get("retain_until_unix_secs").map_sql_err()?,
            "usage policy retain_until",
        )?,
        finalized_at_unix_secs: row
            .try_get::<Option<i64>, _>("finalized_at_unix_secs")
            .map_sql_err()?
            .map(|value| usage_policy_cost_u64(value, "usage policy finalized_at"))
            .transpose()?,
    })
}

const FIND_USAGE_POLICY_COST_RESERVATION_MYSQL_SQL: &str = r#"
SELECT
  request_id,
  subject_id,
  reservation_token,
  admitted_at AS admitted_at_unix_secs,
  reserved_cost_units,
  actual_cost_units,
  state,
  reservation_expires_at AS reservation_expires_at_unix_secs,
  retain_until AS retain_until_unix_secs,
  finalized_at AS finalized_at_unix_secs
FROM usage_cost_reservations
WHERE reservation_token = ?
FOR UPDATE
"#;

async fn lock_usage_policy_subject_mysql(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    subject_id: &str,
) -> Result<bool, DataLayerError> {
    let exists = sqlx::query_scalar::<_, String>(
        r#"
SELECT id
FROM users
WHERE id = ?
FOR UPDATE
        "#,
    )
    .bind(subject_id)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?
    .is_some();
    Ok(exists)
}

fn usage_policy_subject_missing() -> DataLayerError {
    DataLayerError::InvalidInput("usage policy subject does not exist".to_string())
}

fn settlement_from_row(row: &MySqlRow) -> Result<StoredUsageSettlement, DataLayerError> {
    Ok(StoredUsageSettlement {
        request_id: row.try_get("request_id").map_sql_err()?,
        wallet_id: row.try_get("wallet_id").map_sql_err()?,
        billing_status: row.try_get("billing_status").map_sql_err()?,
        wallet_balance_before: row.try_get("wallet_balance_before").map_sql_err()?,
        wallet_balance_after: row.try_get("wallet_balance_after").map_sql_err()?,
        wallet_recharge_balance_before: row
            .try_get("wallet_recharge_balance_before")
            .map_sql_err()?,
        wallet_recharge_balance_after: row
            .try_get("wallet_recharge_balance_after")
            .map_sql_err()?,
        wallet_gift_balance_before: row.try_get("wallet_gift_balance_before").map_sql_err()?,
        wallet_gift_balance_after: row.try_get("wallet_gift_balance_after").map_sql_err()?,
        provider_monthly_used_usd: row.try_get("provider_monthly_used_usd").map_sql_err()?,
        finalized_at_unix_secs: row
            .try_get::<Option<i64>, _>("finalized_at_unix_secs")
            .map_sql_err()?
            .map(|value| value as u64),
    })
}

fn now_unix_secs() -> Result<i64, DataLayerError> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .map_err(|_| DataLayerError::InvalidInput("timestamp overflow".to_string()))
}

async fn enqueue_provider_monthly_usage_delta_mysql(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    request_id: &str,
    provider_id: &str,
    total_cost_usd_delta: f64,
    usage_created_at_unix_secs: i64,
    quota_epoch_start_at_usage: i64,
    pricing_rule_version_at_usage: Option<&str>,
    provider_pricing_snapshot_at_usage: Option<&str>,
    cost_is_resolved: bool,
    created_at: i64,
) -> Result<(), DataLayerError> {
    let request_id = request_id.trim();
    let provider_id = provider_id.trim();
    if request_id.is_empty() || provider_id.is_empty() {
        return Ok(());
    }
    if !total_cost_usd_delta.is_finite() {
        return Err(DataLayerError::UnexpectedValue(format!(
            "provider monthly usage delta is not finite for {provider_id}"
        )));
    }

    sqlx::query(ENQUEUE_PROVIDER_MONTHLY_USAGE_DELTA_SQL)
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(request_id)
        .bind(provider_id)
        .bind(total_cost_usd_delta)
        .bind(usage_created_at_unix_secs)
        .bind(quota_epoch_start_at_usage)
        .bind(usage_created_at_unix_secs)
        .bind(total_cost_usd_delta)
        .bind(pricing_rule_version_at_usage)
        .bind(provider_pricing_snapshot_at_usage)
        .bind(
            if total_cost_usd_delta > SETTLEMENT_EPSILON_USD || cost_is_resolved {
                "ready"
            } else {
                "failed"
            },
        )
        .bind(created_at)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    Ok(())
}

pub(crate) async fn reconcile_provider_monthly_attempt_mysql(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    candidate_id: &str,
    actual_cost_usd: f64,
    cost_is_resolved: bool,
    updated_at: i64,
) -> Result<bool, DataLayerError> {
    let reconciled = reconcile_provider_monthly_attempt_mysql_inner(
        tx,
        candidate_id,
        actual_cost_usd,
        cost_is_resolved,
        updated_at,
    )
    .await?;
    if reconciled {
        crate::provider_quota_reservations::settle_if_ready(tx, candidate_id, updated_at).await?;
    }
    Ok(reconciled)
}

async fn reconcile_provider_monthly_attempt_mysql_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    candidate_id: &str,
    actual_cost_usd: f64,
    cost_is_resolved: bool,
    updated_at: i64,
) -> Result<bool, DataLayerError> {
    let delta_id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("provider-quota-attempt:{}", candidate_id).as_bytes(),
    )
    .to_string();
    let row = sqlx::query(
        "SELECT provider_quota_cost_usd, quota_accounting_status, processed_at, provider_pricing_snapshot_at_usage, pricing_rule_version_at_usage, provider_dispatch_at_unix_secs, quota_epoch_start_at_usage, target_id FROM usage_counter_deltas WHERE id = ? AND kind = 'provider_monthly' LIMIT 1 FOR UPDATE",
    )
    .bind(&delta_id)
    .fetch_optional(&mut **tx)
    .await
    .map_sql_err()?;
    let Some(row) = row else {
        return Ok(false);
    };
    let base_cost = row
        .try_get::<Option<f64>, _>("provider_quota_cost_usd")
        .map_sql_err()?
        .unwrap_or(0.0);
    let base_status = row
        .try_get::<Option<String>, _>("quota_accounting_status")
        .map_sql_err()?;
    let processed_at = row
        .try_get::<Option<i64>, _>("processed_at")
        .map_sql_err()?;
    let reconciled_cost = actual_cost_usd.max(base_cost);
    if !reconciled_cost.is_finite() || reconciled_cost < 0.0 {
        return Err(DataLayerError::InvalidInput(
            "provider quota attempt settlement cost is invalid".to_string(),
        ));
    }
    if base_status.as_deref() != Some("ready") && reconciled_cost <= SETTLEMENT_EPSILON_USD {
        sqlx::query(
            "UPDATE usage_counter_deltas SET provider_quota_cost_usd = 0, total_cost_usd_delta = 0, quota_accounting_status = ? WHERE id = ?",
        )
        .bind(if cost_is_resolved { "ready" } else { "failed" })
        .bind(&delta_id)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
        return Ok(true);
    }
    if processed_at.is_none() {
        sqlx::query(
            "UPDATE usage_counter_deltas SET provider_quota_cost_usd = ?, total_cost_usd_delta = ?, quota_accounting_status = 'ready' WHERE id = ? AND processed_at IS NULL",
        )
        .bind(reconciled_cost)
        .bind(reconciled_cost)
        .bind(&delta_id)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
        return Ok(true);
    }
    let applied: f64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(provider_quota_cost_usd), 0) AS DOUBLE) FROM usage_counter_deltas WHERE kind = 'provider_monthly' AND request_id = ? AND id <> ?")
        .bind(candidate_id).bind(&delta_id).fetch_one(&mut **tx).await.map_sql_err()?;
    let adjustment = (reconciled_cost - base_cost - applied).max(0.0);
    if adjustment.abs() <= SETTLEMENT_EPSILON_USD {
        return Ok(true);
    }
    let adjustment_id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!(
            "provider-quota-attempt-actual:{}:{:.12}",
            candidate_id, reconciled_cost
        )
        .as_bytes(),
    )
    .to_string();
    let pricing_snapshot: Option<String> = row
        .try_get("provider_pricing_snapshot_at_usage")
        .map_sql_err()?;
    let pricing_rule_version: Option<String> =
        row.try_get("pricing_rule_version_at_usage").map_sql_err()?;
    let dispatch_at: i64 = row
        .try_get("provider_dispatch_at_unix_secs")
        .map_sql_err()?;
    let epoch: i64 = row.try_get("quota_epoch_start_at_usage").map_sql_err()?;
    let target_id: String = row.try_get("target_id").map_sql_err()?;
    sqlx::query(
        r#"
INSERT INTO usage_counter_deltas (
  id, request_id, kind, target_id, total_cost_usd_delta,
  usage_created_at_unix_secs, provider_billing_type_at_usage,
  quota_epoch_start_at_usage, provider_dispatch_at_unix_secs,
  provider_quota_cost_usd, pricing_rule_version_at_usage,
  provider_pricing_snapshot_at_usage, quota_accounting_status, created_at
)
VALUES (?, ?, 'provider_monthly', ?, ?, ?, 'monthly_quota', ?, ?, ?, ?, ?, 'ready', ?)
ON DUPLICATE KEY UPDATE id = id
"#,
    )
    .bind(adjustment_id)
    .bind(candidate_id)
    .bind(target_id)
    .bind(adjustment)
    .bind(dispatch_at)
    .bind(epoch)
    .bind(dispatch_at)
    .bind(adjustment)
    .bind(pricing_rule_version)
    .bind(pricing_snapshot)
    .bind(updated_at)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    if base_status.as_deref() != Some("ready") {
        sqlx::query(
            "UPDATE usage_counter_deltas SET quota_accounting_status = 'reconciled' WHERE id = ?",
        )
        .bind(delta_id)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    }
    Ok(true)
}

#[derive(Debug, Default)]
struct DailyQuotaDebitResult {
    debited_usd: f64,
    insufficient: bool,
}

#[derive(Debug)]
struct DailyQuotaGrant {
    entitlement_id: String,
    daily_quota_usd: f64,
    usage_date: String,
    allow_wallet_overage: bool,
}

fn daily_quota_usage_date(
    reset_timezone: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<String, DataLayerError> {
    let timezone = reset_timezone
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Asia/Shanghai")
        .parse::<chrono_tz::Tz>()
        .map_err(|err| DataLayerError::InvalidInput(format!("invalid reset_timezone: {err}")))?;
    Ok(now.with_timezone(&timezone).date_naive().to_string())
}

fn daily_quota_grants_from_entitlement(
    entitlement_id: &str,
    entitlements: &serde_json::Value,
    current_allow_wallet_overage: Option<bool>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DailyQuotaGrant>, DataLayerError> {
    let mut grants = Vec::new();
    let Some(items) = entitlements.as_array() else {
        return Ok(grants);
    };
    for item in items {
        if item.get("type").and_then(serde_json::Value::as_str) != Some("daily_quota") {
            continue;
        }
        let daily_quota_usd = item
            .get("daily_quota_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if !daily_quota_usd.is_finite() || daily_quota_usd <= 0.0 {
            continue;
        }
        grants.push(DailyQuotaGrant {
            entitlement_id: entitlement_id.to_string(),
            daily_quota_usd,
            usage_date: daily_quota_usage_date(
                item.get("reset_timezone")
                    .and_then(serde_json::Value::as_str),
                now,
            )?,
            allow_wallet_overage: current_allow_wallet_overage.unwrap_or_else(|| {
                item.get("allow_wallet_overage")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            }),
        });
    }
    Ok(grants)
}

fn daily_quota_wallet_overage_policy(entitlements: &serde_json::Value) -> Option<bool> {
    entitlements.as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(serde_json::Value::as_str) == Some("daily_quota"))
            .then(|| {
                item.get("allow_wallet_overage")
                    .and_then(serde_json::Value::as_bool)
            })
            .flatten()
    })
}

async fn consume_daily_quota_mysql(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    request_id: &str,
    total_cost_usd: f64,
    wallet_available_usd: Option<f64>,
    wallet_can_overdraft: bool,
    now_unix_secs: i64,
) -> Result<DailyQuotaDebitResult, DataLayerError> {
    if !total_cost_usd.is_finite() || total_cost_usd < 0.0 {
        return Err(DataLayerError::InvalidInput(
            "daily quota settlement cost must be finite and non-negative".to_string(),
        ));
    }
    if total_cost_usd == 0.0 {
        return Ok(DailyQuotaDebitResult::default());
    }
    let rows = sqlx::query(
        r#"
SELECT
    user_plan_entitlements.id,
    user_plan_entitlements.entitlements_snapshot,
    billing_plans.entitlements_json AS plan_entitlements_json
FROM user_plan_entitlements
JOIN billing_plans ON billing_plans.id = user_plan_entitlements.plan_id
WHERE user_plan_entitlements.user_id = ?
    AND user_plan_entitlements.status = 'active'
    AND user_plan_entitlements.starts_at <= ?
    AND user_plan_entitlements.expires_at > ?
ORDER BY user_plan_entitlements.expires_at ASC,
                 user_plan_entitlements.created_at ASC,
                 user_plan_entitlements.id ASC
FOR UPDATE
"#,
    )
    .bind(user_id)
    .bind(now_unix_secs)
    .bind(now_unix_secs)
    .fetch_all(&mut **tx)
    .await
    .map_sql_err()?;
    let now = chrono::Utc::now();
    let mut grants = Vec::new();
    for row in rows {
        let entitlement_id: String = row.try_get("id").map_sql_err()?;
        let entitlements_raw: String = row.try_get("entitlements_snapshot").map_sql_err()?;
        let entitlements =
            serde_json::from_str::<serde_json::Value>(&entitlements_raw).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "user_plan_entitlements.entitlements_snapshot invalid json: {err}"
                ))
            })?;
        let plan_entitlements_raw: String = row.try_get("plan_entitlements_json").map_sql_err()?;
        let plan_entitlements = serde_json::from_str::<serde_json::Value>(&plan_entitlements_raw)
            .map_err(|err| {
            DataLayerError::UnexpectedValue(format!(
                "billing_plans.entitlements_json invalid json: {err}"
            ))
        })?;
        grants.extend(daily_quota_grants_from_entitlement(
            &entitlement_id,
            &entitlements,
            daily_quota_wallet_overage_policy(&plan_entitlements),
            now,
        )?);
    }
    if grants.is_empty() {
        return Ok(DailyQuotaDebitResult::default());
    }

    let mut grants_with_remaining = Vec::new();
    let mut total_remaining = 0.0;
    let mut allow_wallet_overage = true;
    for grant in grants {
        allow_wallet_overage &= grant.allow_wallet_overage;
        let used = sqlx::query_scalar::<_, f64>(
            r#"
SELECT COALESCE(SUM(amount_usd), 0)
FROM entitlement_usage_ledgers
WHERE user_entitlement_id = ?
  AND usage_date = ?
"#,
        )
        .bind(&grant.entitlement_id)
        .bind(&grant.usage_date)
        .fetch_one(&mut **tx)
        .await
        .map_sql_err()?;
        if !used.is_finite() || used < 0.0 {
            return Err(DataLayerError::UnexpectedValue(
                "daily quota usage ledger total is invalid".to_string(),
            ));
        }
        let remaining = (grant.daily_quota_usd - used).max(0.0);
        total_remaining += remaining;
        if !total_remaining.is_finite() {
            return Err(DataLayerError::UnexpectedValue(
                "daily quota remaining total overflowed".to_string(),
            ));
        }
        grants_with_remaining.push((grant, remaining));
    }
    let insufficient = (!allow_wallet_overage && total_remaining + 0.000_000_01 < total_cost_usd)
        || (allow_wallet_overage
            && !wallet_can_overdraft
            && wallet_available_usd.is_some_and(|available| {
                total_remaining + available + SETTLEMENT_EPSILON_USD < total_cost_usd
            }));

    let mut remaining_cost = total_cost_usd;
    let mut debited = 0.0;
    for (grant, balance_before) in grants_with_remaining {
        if remaining_cost <= 0.000_000_01 || balance_before <= 0.0 {
            continue;
        }
        let amount = remaining_cost.min(balance_before);
        let balance_after = balance_before - amount;
        sqlx::query(
            r#"
INSERT IGNORE INTO entitlement_usage_ledgers (
  id, user_entitlement_id, user_id, request_id, amount_usd,
  balance_before, balance_after, usage_date, created_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&grant.entitlement_id)
        .bind(user_id)
        .bind(request_id)
        .bind(amount)
        .bind(balance_before)
        .bind(balance_after)
        .bind(&grant.usage_date)
        .bind(now_unix_secs)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
        remaining_cost -= amount;
        debited += amount;
    }
    Ok(DailyQuotaDebitResult {
        debited_usd: debited,
        insufficient,
    })
}

#[async_trait]
impl SettlementWriteRepository for MysqlSettlementRepository {
    async fn reserve_usage_policy_request(
        &self,
        input: ReserveUsagePolicyRequestInput,
    ) -> Result<ReserveUsagePolicyRequestOutcome, DataLayerError> {
        input.validate()?;
        let admitted_at = usage_policy_cost_i64(
            input.admitted_at_unix_secs,
            "usage policy request admitted_at",
        )?;
        let retain_until = usage_policy_cost_i64(
            input.retain_until_unix_secs,
            "usage policy request retain_until",
        )?;
        let created_at = now_unix_secs()?;
        // Different subjects lock different `users` rows. Under InnoDB's default REPEATABLE READ,
        // two missing-token locking reads can retain compatible gap locks and then deadlock when
        // both transactions try to insert the same unique event token. READ COMMITTED removes
        // that gap-lock cycle while the subject row still serializes each subject's window count.
        let mut connection = self.pool.acquire().await.map_sql_err()?;
        sqlx::query(USAGE_POLICY_REQUEST_TRANSACTION_ISOLATION_MYSQL_SQL)
            .execute(&mut *connection)
            .await
            .map_sql_err()?;
        let mut tx = connection.begin().await.map_sql_err()?;
        if !lock_usage_policy_subject_mysql(&mut tx, &input.subject_id).await? {
            return Err(usage_policy_subject_missing());
        }

        let existing_row = sqlx::query(FIND_USAGE_POLICY_REQUEST_ADMISSION_MYSQL_SQL)
            .bind(&input.event_token)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        if let Some(row) = existing_row.as_ref() {
            let existing = usage_policy_request_admission_from_mysql_row(row)?;
            if existing.request_id != input.request_id || existing.subject_id != input.subject_id {
                tx.commit().await.map_sql_err()?;
                return Ok(ReserveUsagePolicyRequestOutcome::Conflict);
            }
            if existing.admitted_at_unix_secs != input.admitted_at_unix_secs {
                return Err(DataLayerError::InvalidInput(
                    "usage policy event_token must keep its original admitted_at".to_string(),
                ));
            }
            sqlx::query(
                "UPDATE usage_request_admissions SET retain_until = GREATEST(retain_until, ?) WHERE event_token = ?",
            )
            .bind(retain_until)
            .bind(&input.event_token)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            let outcome = match existing.state {
                UsagePolicyRequestAdmissionState::Active => {
                    ReserveUsagePolicyRequestOutcome::Allowed
                }
                UsagePolicyRequestAdmissionState::Released => {
                    ReserveUsagePolicyRequestOutcome::AlreadyReleased
                }
            };
            tx.commit().await.map_sql_err()?;
            return Ok(outcome);
        }

        for (window_index, window) in input.windows.iter().enumerate() {
            let used_requests = sqlx::query_scalar::<_, i64>(
                r#"
SELECT CAST(COUNT(*) AS SIGNED)
FROM usage_request_admissions
WHERE subject_id = ?
  AND state = 'active'
  AND admitted_at >= ?
  AND admitted_at < ?
                "#,
            )
            .bind(&input.subject_id)
            .bind(usage_policy_cost_i64(
                window.starts_at_unix_secs,
                "usage policy request window start",
            )?)
            .bind(usage_policy_cost_i64(
                window.ends_at_unix_secs,
                "usage policy request window end",
            )?)
            .fetch_one(&mut *tx)
            .await
            .map_sql_err()?;
            let used_requests =
                usage_policy_cost_u64(used_requests, "usage policy request used_requests")?;
            if used_requests >= window.limit_requests {
                let outcome = ReserveUsagePolicyRequestOutcome::Rejected {
                    window_index,
                    limit_requests: window.limit_requests,
                    used_requests,
                };
                tx.commit().await.map_sql_err()?;
                return Ok(outcome);
            }
        }

        sqlx::query(
            r#"
INSERT INTO usage_request_admissions (
  request_id, subject_id, event_token, admitted_at, retain_until,
  state, released_at, created_at
) VALUES (?, ?, ?, ?, ?, 'active', NULL, ?)
ON DUPLICATE KEY UPDATE event_token = VALUES(event_token)
            "#,
        )
        .bind(&input.request_id)
        .bind(&input.subject_id)
        .bind(&input.event_token)
        .bind(admitted_at)
        .bind(retain_until)
        .bind(created_at)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;

        let row = sqlx::query(FIND_USAGE_POLICY_REQUEST_ADMISSION_MYSQL_SQL)
            .bind(&input.event_token)
            .fetch_one(&mut *tx)
            .await
            .map_sql_err()?;
        let existing = usage_policy_request_admission_from_mysql_row(&row)?;
        if existing.request_id != input.request_id || existing.subject_id != input.subject_id {
            tx.commit().await.map_sql_err()?;
            return Ok(ReserveUsagePolicyRequestOutcome::Conflict);
        }
        if existing.admitted_at_unix_secs != input.admitted_at_unix_secs {
            return Err(DataLayerError::InvalidInput(
                "usage policy event_token must keep its original admitted_at".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE usage_request_admissions SET retain_until = GREATEST(retain_until, ?) WHERE event_token = ?",
        )
        .bind(retain_until)
        .bind(&input.event_token)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        let outcome = match existing.state {
            UsagePolicyRequestAdmissionState::Active => ReserveUsagePolicyRequestOutcome::Allowed,
            UsagePolicyRequestAdmissionState::Released => {
                ReserveUsagePolicyRequestOutcome::AlreadyReleased
            }
        };
        tx.commit().await.map_sql_err()?;
        Ok(outcome)
    }

    async fn release_usage_policy_request_admission(
        &self,
        input: ReleaseUsagePolicyRequestAdmissionInput,
    ) -> Result<Option<StoredUsagePolicyRequestAdmission>, DataLayerError> {
        input.validate()?;
        let released_at = usage_policy_cost_i64(
            input.released_at_unix_secs,
            "usage policy request released_at",
        )?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        if !lock_usage_policy_subject_mysql(&mut tx, &input.subject_id).await? {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        }
        let row = sqlx::query(FIND_USAGE_POLICY_REQUEST_ADMISSION_MYSQL_SQL)
            .bind(&input.event_token)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(row) = row else {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        };
        let mut admission = usage_policy_request_admission_from_mysql_row(&row)?;
        if admission.request_id != input.request_id || admission.subject_id != input.subject_id {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        }
        if input.released_at_unix_secs < admission.admitted_at_unix_secs {
            return Err(DataLayerError::InvalidInput(
                "usage policy released_at must not precede admitted_at".to_string(),
            ));
        }
        if admission.state == UsagePolicyRequestAdmissionState::Active {
            sqlx::query(
                "UPDATE usage_request_admissions SET state = 'released', released_at = ? WHERE event_token = ? AND state = 'active'",
            )
            .bind(released_at)
            .bind(&input.event_token)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            admission.state = UsagePolicyRequestAdmissionState::Released;
            admission.released_at_unix_secs = Some(input.released_at_unix_secs);
        }
        tx.commit().await.map_sql_err()?;
        Ok(Some(admission))
    }

    async fn cleanup_usage_policy_request_admissions(
        &self,
        now_unix_secs: u64,
        batch_size: usize,
    ) -> Result<usize, DataLayerError> {
        if batch_size == 0 {
            return Ok(0);
        }
        let now = usage_policy_cost_i64(now_unix_secs, "usage policy request cleanup timestamp")?;
        let limit = i64::try_from(batch_size).unwrap_or(i64::MAX);
        let result = sqlx::query(
            r#"
DELETE FROM usage_request_admissions
WHERE retain_until <= ?
ORDER BY retain_until, event_token
LIMIT ?
            "#,
        )
        .bind(now)
        .bind(limit)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() as usize)
    }

    async fn reserve_usage_policy_cost(
        &self,
        input: ReserveUsagePolicyCostInput,
    ) -> Result<ReserveUsagePolicyCostOutcome, DataLayerError> {
        input.validate()?;
        let admitted_at =
            usage_policy_cost_i64(input.admitted_at_unix_secs, "usage policy admitted_at")?;
        let reservation_expires_at = usage_policy_cost_i64(
            input.reservation_expires_at_unix_secs,
            "usage policy reservation_expires_at",
        )?;
        let updated_at = now_unix_secs()?;

        let mut tx = self.pool.begin().await.map_sql_err()?;
        if !lock_usage_policy_subject_mysql(&mut tx, &input.subject_id).await? {
            return Err(usage_policy_subject_missing());
        }
        let existing_row = sqlx::query(FIND_USAGE_POLICY_COST_RESERVATION_MYSQL_SQL)
            .bind(&input.reservation_token)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let existing = existing_row
            .as_ref()
            .map(usage_policy_cost_reservation_from_mysql_row)
            .transpose()?;
        if let Some(existing) = existing.as_ref() {
            if existing.request_id != input.request_id || existing.subject_id != input.subject_id {
                tx.commit().await.map_sql_err()?;
                return Ok(ReserveUsagePolicyCostOutcome::Conflict);
            }
            if existing.state != UsagePolicyCostReservationState::Reserved {
                let outcome = ReserveUsagePolicyCostOutcome::AlreadyTerminal {
                    state: existing.state,
                };
                tx.commit().await.map_sql_err()?;
                return Ok(outcome);
            }
            if existing.admitted_at_unix_secs != input.admitted_at_unix_secs {
                return Err(DataLayerError::InvalidInput(
                    "usage policy reservation_token must keep its original admitted_at".to_string(),
                ));
            }
        }

        let previous_reserved_cost_units = existing
            .as_ref()
            .map(|reservation| reservation.reserved_cost_units)
            .unwrap_or(0);
        let target_reserved_cost_units =
            previous_reserved_cost_units.max(input.reserved_cost_units);
        for (window_index, window) in input.windows.iter().enumerate() {
            let window_start =
                usage_policy_cost_i64(window.starts_at_unix_secs, "usage policy window start")?;
            let window_end =
                usage_policy_cost_i64(window.ends_at_unix_secs, "usage policy window end")?;
            let used_cost_units = sqlx::query_scalar::<_, i64>(
                r#"
SELECT CAST(COALESCE(SUM(
  CASE
    WHEN state = 'finalized' THEN COALESCE(actual_cost_units, 0)
    WHEN state = 'reserved' AND reservation_expires_at > ? THEN reserved_cost_units
    ELSE 0
  END
), 0) AS SIGNED)
FROM usage_cost_reservations
WHERE subject_id = ?
  AND admitted_at >= ?
  AND admitted_at < ?
  AND reservation_token <> ?
                "#,
            )
            .bind(admitted_at)
            .bind(&input.subject_id)
            .bind(window_start)
            .bind(window_end)
            .bind(&input.reservation_token)
            .fetch_one(&mut *tx)
            .await
            .map_sql_err()?;
            let used_cost_units =
                usage_policy_cost_u64(used_cost_units, "usage policy used_cost_units")?;
            if used_cost_units
                .checked_add(target_reserved_cost_units)
                .is_none_or(|total| total > window.limit_cost_units)
            {
                let outcome = ReserveUsagePolicyCostOutcome::Rejected {
                    window_index,
                    limit_cost_units: window.limit_cost_units,
                    used_cost_units,
                };
                tx.commit().await.map_sql_err()?;
                return Ok(outcome);
            }
        }

        let admitted_at = existing
            .as_ref()
            .map(|reservation| {
                usage_policy_cost_i64(
                    reservation.admitted_at_unix_secs,
                    "usage policy admitted_at",
                )
            })
            .transpose()?
            .unwrap_or(admitted_at);
        sqlx::query(
            r#"
INSERT INTO usage_cost_reservations (
  request_id, subject_id, reservation_token, admitted_at,
  reserved_cost_units, actual_cost_units,
  state, reservation_expires_at, retain_until, finalized_at, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, NULL, 'reserved', ?, ?, NULL, ?, ?)
ON DUPLICATE KEY UPDATE
  reserved_cost_units = GREATEST(reserved_cost_units, VALUES(reserved_cost_units)),
  reservation_expires_at = GREATEST(
    reservation_expires_at,
    VALUES(reservation_expires_at)
  ),
  retain_until = GREATEST(retain_until, VALUES(retain_until)),
  updated_at = VALUES(updated_at)
            "#,
        )
        .bind(&input.request_id)
        .bind(&input.subject_id)
        .bind(&input.reservation_token)
        .bind(admitted_at)
        .bind(usage_policy_cost_i64(
            target_reserved_cost_units,
            "usage policy reserved_cost_units",
        )?)
        .bind(reservation_expires_at)
        .bind(usage_policy_cost_i64(
            input.retain_until_unix_secs,
            "usage policy retain_until",
        )?)
        .bind(updated_at)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;

        tx.commit().await.map_sql_err()?;
        Ok(ReserveUsagePolicyCostOutcome::Allowed {
            reserved_cost_units: target_reserved_cost_units,
            additional_reserved_cost_units: target_reserved_cost_units
                .saturating_sub(previous_reserved_cost_units),
        })
    }

    async fn reconcile_usage_policy_cost(
        &self,
        input: ReconcileUsagePolicyCostInput,
    ) -> Result<Option<StoredUsagePolicyCostReservation>, DataLayerError> {
        input.validate()?;
        let actual_cost_units =
            usage_policy_cost_i64(input.actual_cost_units, "usage policy actual_cost_units")?;
        let finalized_at =
            usage_policy_cost_i64(input.finalized_at_unix_secs, "usage policy finalized_at")?;
        let updated_at = now_unix_secs()?;

        let mut tx = self.pool.begin().await.map_sql_err()?;
        if !lock_usage_policy_subject_mysql(&mut tx, &input.subject_id).await? {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        }
        let row = sqlx::query(FIND_USAGE_POLICY_COST_RESERVATION_MYSQL_SQL)
            .bind(&input.reservation_token)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(row) = row else {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        };
        let mut reservation = usage_policy_cost_reservation_from_mysql_row(&row)?;
        if reservation.request_id != input.request_id || reservation.subject_id != input.subject_id
        {
            // The token selects the row; audit identity must still match before the reservation
            // can be finalized.
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        }
        if reservation.state == UsagePolicyCostReservationState::Reserved {
            sqlx::query(
                r#"
UPDATE usage_cost_reservations
SET state = ?,
    actual_cost_units = ?,
    finalized_at = ?,
    updated_at = ?
WHERE reservation_token = ?
  AND request_id = ?
  AND subject_id = ?
  AND state = 'reserved'
                "#,
            )
            .bind(input.terminal_state.as_str())
            .bind(actual_cost_units)
            .bind(finalized_at)
            .bind(updated_at)
            .bind(&input.reservation_token)
            .bind(&input.request_id)
            .bind(&input.subject_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            reservation.state = input.terminal_state;
            reservation.actual_cost_units = Some(input.actual_cost_units);
            reservation.finalized_at_unix_secs = Some(input.finalized_at_unix_secs);
        }

        tx.commit().await.map_sql_err()?;
        Ok(Some(reservation))
    }

    async fn cleanup_usage_policy_cost_reservations(
        &self,
        now_unix_secs: u64,
        batch_size: usize,
    ) -> Result<usize, DataLayerError> {
        if batch_size == 0 {
            return Ok(0);
        }
        let now = usage_policy_cost_i64(now_unix_secs, "usage policy cleanup timestamp")?;
        let limit = i64::try_from(batch_size).unwrap_or(i64::MAX);
        let result = sqlx::query(
            r#"
DELETE FROM usage_cost_reservations
WHERE retain_until <= ?
ORDER BY retain_until, reservation_token
LIMIT ?
            "#,
        )
        .bind(now)
        .bind(limit)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() as usize)
    }

    async fn settle_usage(
        &self,
        input: UsageSettlementInput,
    ) -> Result<Option<StoredUsageSettlement>, DataLayerError> {
        input.validate()?;
        let finalized_at = i64::try_from(
            input
                .finalized_at_unix_secs
                .unwrap_or(now_unix_secs()? as u64),
        )
        .map_err(|_| DataLayerError::InvalidInput("finalized_at overflow".to_string()))?;
        let updated_at = now_unix_secs()?;

        let mut tx = self.pool.begin().await.map_sql_err()?;
        let row = sqlx::query(FIND_USAGE_FOR_SETTLEMENT_SQL)
            .bind(&input.request_id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;

        let Some(usage_row) = row else {
            tx.commit().await.map_sql_err()?;
            return Ok(None);
        };

        let current_billing_status: String = usage_row.try_get("billing_status").map_sql_err()?;
        if matches!(
            current_billing_status.as_str(),
            "settled" | "void" | "insufficient_quota"
        ) {
            let settlement = settlement_from_row(&usage_row)?;
            tx.commit().await.map_sql_err()?;
            return Ok(Some(settlement));
        }

        let provider_billing_type_at_usage = usage_row
            .try_get::<Option<String>, _>("provider_billing_type_at_usage")
            .map_sql_err()?
            .unwrap_or_default();
        let quota_epoch_start_at_usage = usage_row
            .try_get::<Option<i64>, _>("quota_epoch_start_at_usage")
            .map_sql_err()?;
        let provider_attempt_id = usage_row
            .try_get::<Option<String>, _>("provider_attempt_id")
            .map_sql_err()?;
        let provider_quota_cost_is_resolved = usage_row
            .try_get::<i64, _>("provider_quota_cost_is_resolved")
            .map_sql_err()?
            != 0;
        let attempt_reconciled = if let Some(candidate_id) = provider_attempt_id.as_deref() {
            reconcile_provider_monthly_attempt_mysql(
                &mut tx,
                candidate_id,
                usage_row
                    .try_get::<Option<f64>, _>("provider_quota_cost_usd")
                    .map_sql_err()?
                    .unwrap_or(input.actual_total_cost_usd),
                provider_quota_cost_is_resolved,
                updated_at,
            )
            .await?
        } else {
            false
        };
        if !attempt_reconciled
            && provider_billing_type_at_usage.eq_ignore_ascii_case("monthly_quota")
        {
            if let (Some(provider_id), Some(quota_epoch_start_at_usage)) = (
                input
                    .provider_id
                    .as_deref()
                    .filter(|value| !value.is_empty()),
                quota_epoch_start_at_usage,
            ) {
                let provider_quota_cost_usd = usage_row
                    .try_get::<Option<f64>, _>("provider_quota_cost_usd")
                    .map_sql_err()?
                    .unwrap_or(input.actual_total_cost_usd);
                let pricing_rule_version = usage_row
                    .try_get::<Option<String>, _>("pricing_rule_version_at_usage")
                    .map_sql_err()?;
                let provider_pricing_snapshot = usage_row
                    .try_get::<Option<String>, _>("provider_pricing_snapshot_at_usage")
                    .map_sql_err()?;
                enqueue_provider_monthly_usage_delta_mysql(
                    &mut tx,
                    &input.request_id,
                    provider_id,
                    provider_quota_cost_usd,
                    usage_row
                        .try_get("usage_created_at_unix_secs")
                        .map_sql_err()?,
                    quota_epoch_start_at_usage / 60 * 60,
                    pricing_rule_version.as_deref(),
                    provider_pricing_snapshot.as_deref(),
                    provider_quota_cost_is_resolved,
                    updated_at,
                )
                .await?;
            }
        }

        let mut final_billing_status =
            settlement_billing_status_for_usage_status(&input.status).to_string();
        let mut settlement = StoredUsageSettlement {
            request_id: input.request_id.clone(),
            wallet_id: None,
            billing_status: final_billing_status.clone(),
            wallet_balance_before: None,
            wallet_balance_after: None,
            wallet_recharge_balance_before: None,
            wallet_recharge_balance_after: None,
            wallet_gift_balance_before: None,
            wallet_gift_balance_after: None,
            provider_monthly_used_usd: None,
            finalized_at_unix_secs: Some(finalized_at as u64),
        };
        let skip_user_billing = input.skip_user_billing.unwrap_or(false);
        let skip_plan_billing = input.skip_plan_billing.unwrap_or(false);

        if final_billing_status == "settled" {
            let api_key_id = (!skip_user_billing)
                .then_some(input.api_key_id.as_deref())
                .flatten()
                .filter(|value| !value.is_empty());
            let api_key_is_standalone = if input.api_key_is_standalone {
                true
            } else if let Some(api_key_id) = api_key_id {
                sqlx::query_scalar::<_, bool>(
                    r#"
SELECT is_standalone
FROM api_keys
WHERE id = ?
LIMIT 1
"#,
                )
                .bind(api_key_id)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?
                .unwrap_or(false)
            } else {
                false
            };

            let wallet_row = if let Some(api_key_id) = api_key_id {
                sqlx::query(
                    r#"
SELECT id, balance, gift_balance, total_consumed, limit_mode
FROM wallets
WHERE api_key_id = ?
LIMIT 1
FOR UPDATE
"#,
                )
                .bind(api_key_id)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?
            } else {
                None
            };

            let wallet_row = if wallet_row.is_some() {
                wallet_row
            } else if !skip_user_billing && !api_key_is_standalone {
                if let Some(user_id) = input.user_id.as_deref().filter(|value| !value.is_empty()) {
                    sqlx::query(
                        r#"
SELECT id, balance, gift_balance, total_consumed, limit_mode
FROM wallets
WHERE user_id = ?
LIMIT 1
FOR UPDATE
"#,
                    )
                    .bind(user_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_sql_err()?
                } else {
                    None
                }
            } else {
                None
            };

            let wallet_can_overdraft = wallet_row.is_some();
            let wallet_available_usd = match wallet_row.as_ref() {
                Some(row) => {
                    let recharge_balance: f64 = row.try_get("balance").map_sql_err()?;
                    let gift_balance: f64 = row.try_get("gift_balance").map_sql_err()?;
                    let total_consumed: f64 = row.try_get("total_consumed").map_sql_err()?;
                    validate_wallet_settlement_values(
                        recharge_balance,
                        gift_balance,
                        total_consumed,
                        0.0,
                    )?;
                    let limit_mode: String = row.try_get("limit_mode").map_sql_err()?;
                    if limit_mode.eq_ignore_ascii_case("unlimited") {
                        None
                    } else {
                        Some(finite_wallet_available_usd(recharge_balance, gift_balance))
                    }
                }
                None => Some(0.0),
            };
            if let Some(row) = wallet_row.as_ref() {
                let wallet_id: String = row.try_get("id").map_sql_err()?;
                let before_recharge: f64 = row.try_get("balance").map_sql_err()?;
                let before_gift: f64 = row.try_get("gift_balance").map_sql_err()?;
                let before_total = before_recharge + before_gift;
                settlement.wallet_id = Some(wallet_id);
                settlement.wallet_balance_before = Some(before_total);
                settlement.wallet_balance_after = Some(before_total);
                settlement.wallet_recharge_balance_before = Some(before_recharge);
                settlement.wallet_recharge_balance_after = Some(before_recharge);
                settlement.wallet_gift_balance_before = Some(before_gift);
                settlement.wallet_gift_balance_after = Some(before_gift);
            }

            let billable_cost_usd = settlement_billable_cost_usd(&input);
            let wallet_debit_cost_usd = if skip_user_billing {
                0.0
            } else if !api_key_is_standalone && !skip_plan_billing {
                if let Some(user_id) = input.user_id.as_deref().filter(|value| !value.is_empty()) {
                    let quota = consume_daily_quota_mysql(
                        &mut tx,
                        user_id,
                        &input.request_id,
                        billable_cost_usd,
                        wallet_available_usd,
                        wallet_can_overdraft,
                        updated_at,
                    )
                    .await?;
                    if quota.insufficient {
                        final_billing_status = "insufficient_quota".to_string();
                        settlement.billing_status = final_billing_status.clone();
                        0.0
                    } else {
                        (billable_cost_usd - quota.debited_usd).max(0.0)
                    }
                } else {
                    billable_cost_usd
                }
            } else {
                billable_cost_usd
            };
            if final_billing_status != "settled" {
                sqlx::query(UPSERT_USAGE_SETTLEMENT_SNAPSHOT_SQL)
                    .bind(&settlement.request_id)
                    .bind(&settlement.billing_status)
                    .bind(settlement.wallet_id.as_deref())
                    .bind(settlement.wallet_balance_before)
                    .bind(settlement.wallet_balance_after)
                    .bind(settlement.wallet_recharge_balance_before)
                    .bind(settlement.wallet_recharge_balance_after)
                    .bind(settlement.wallet_gift_balance_before)
                    .bind(settlement.wallet_gift_balance_after)
                    .bind(settlement.provider_monthly_used_usd)
                    .bind(settlement.finalized_at_unix_secs.map(|value| value as i64))
                    .bind(updated_at)
                    .bind(updated_at)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
                sqlx::query(FINALIZE_USAGE_BILLING_SQL)
                    .bind(&final_billing_status)
                    .bind(finalized_at)
                    .bind(&input.request_id)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
                tx.commit().await.map_sql_err()?;
                return Ok(Some(settlement));
            }

            if wallet_debit_cost_usd > SETTLEMENT_EPSILON_USD {
                if let Some(wallet_row) = wallet_row {
                    let wallet_id: String = wallet_row.try_get("id").map_sql_err()?;
                    let before_recharge: f64 = wallet_row.try_get("balance").map_sql_err()?;
                    let before_gift: f64 = wallet_row.try_get("gift_balance").map_sql_err()?;
                    let total_consumed: f64 = wallet_row.try_get("total_consumed").map_sql_err()?;
                    let limit_mode: String = wallet_row.try_get("limit_mode").map_sql_err()?;
                    let before_total = before_recharge + before_gift;
                    let mut after_recharge = before_recharge;
                    let mut after_gift = before_gift;
                    if !limit_mode.eq_ignore_ascii_case("unlimited") {
                        let debit_plan = plan_finite_wallet_debit(
                            before_recharge,
                            before_gift,
                            wallet_debit_cost_usd,
                        );
                        (after_recharge, after_gift) =
                            debit_plan.after_balances(before_recharge, before_gift);
                    }
                    let total_consumed_after = total_consumed + wallet_debit_cost_usd;
                    validate_wallet_settlement_values(
                        after_recharge,
                        after_gift,
                        total_consumed_after,
                        0.0,
                    )?;
                    if final_billing_status == "settled" {
                        sqlx::query(
                            r#"
UPDATE wallets
SET
  balance = ?,
  gift_balance = ?,
  total_consumed = ?,
  updated_at = ?
WHERE id = ?
"#,
                        )
                        .bind(after_recharge)
                        .bind(after_gift)
                        .bind(total_consumed_after)
                        .bind(updated_at)
                        .bind(&wallet_id)
                        .execute(&mut *tx)
                        .await
                        .map_sql_err()?;
                    }

                    settlement.wallet_id = Some(wallet_id);
                    settlement.wallet_balance_before = Some(before_total);
                    settlement.wallet_balance_after = Some(after_recharge + after_gift);
                    settlement.wallet_recharge_balance_before = Some(before_recharge);
                    settlement.wallet_recharge_balance_after = Some(after_recharge);
                    settlement.wallet_gift_balance_before = Some(before_gift);
                    settlement.wallet_gift_balance_after = Some(after_gift);
                } else {
                    final_billing_status = "insufficient_quota".to_string();
                    settlement.billing_status = final_billing_status.clone();
                }
            }

            if final_billing_status != "settled" {
                sqlx::query(UPSERT_USAGE_SETTLEMENT_SNAPSHOT_SQL)
                    .bind(&settlement.request_id)
                    .bind(&settlement.billing_status)
                    .bind(settlement.wallet_id.as_deref())
                    .bind(settlement.wallet_balance_before)
                    .bind(settlement.wallet_balance_after)
                    .bind(settlement.wallet_recharge_balance_before)
                    .bind(settlement.wallet_recharge_balance_after)
                    .bind(settlement.wallet_gift_balance_before)
                    .bind(settlement.wallet_gift_balance_after)
                    .bind(settlement.provider_monthly_used_usd)
                    .bind(settlement.finalized_at_unix_secs.map(|value| value as i64))
                    .bind(updated_at)
                    .bind(updated_at)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
                sqlx::query(FINALIZE_USAGE_BILLING_SQL)
                    .bind(&final_billing_status)
                    .bind(finalized_at)
                    .bind(&input.request_id)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?;
                tx.commit().await.map_sql_err()?;
                return Ok(Some(settlement));
            }
        }

        sqlx::query(UPSERT_USAGE_SETTLEMENT_SNAPSHOT_SQL)
            .bind(&settlement.request_id)
            .bind(&settlement.billing_status)
            .bind(settlement.wallet_id.as_deref())
            .bind(settlement.wallet_balance_before)
            .bind(settlement.wallet_balance_after)
            .bind(settlement.wallet_recharge_balance_before)
            .bind(settlement.wallet_recharge_balance_after)
            .bind(settlement.wallet_gift_balance_before)
            .bind(settlement.wallet_gift_balance_after)
            .bind(settlement.provider_monthly_used_usd)
            .bind(settlement.finalized_at_unix_secs.map(|value| value as i64))
            .bind(updated_at)
            .bind(updated_at)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;

        sqlx::query(FINALIZE_USAGE_BILLING_SQL)
            .bind(&final_billing_status)
            .bind(finalized_at)
            .bind(&input.request_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;

        tx.commit().await.map_sql_err()?;
        Ok(Some(settlement))
    }
}

#[cfg(test)]
mod tests {
    use super::{MysqlSettlementRepository, USAGE_POLICY_REQUEST_TRANSACTION_ISOLATION_MYSQL_SQL};
    use crate::run_migrations;
    use aether_data_contracts::repository::settlement::{
        ReserveUsagePolicyRequestInput, ReserveUsagePolicyRequestOutcome,
        SettlementWriteRepository, UsagePolicyRequestWindow, UsageSettlementInput,
    };

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlSettlementRepository::new(pool);
    }

    #[test]
    fn request_admission_transactions_use_read_committed() {
        assert_eq!(
            USAGE_POLICY_REQUEST_TRANSACTION_ISOLATION_MYSQL_SQL,
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cross_subject_same_token_is_allowed_once_without_deadlock_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql request admission race test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("mysql pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");
        cleanup_request_admission_rows(&pool).await;
        sqlx::query(
            r#"
INSERT INTO users (id, username, auth_source, created_at, updated_at)
VALUES
  ('admission-race-user-1', 'admission-race-user-1', 'local', 1, 1),
  ('admission-race-user-2', 'admission-race-user-2', 'local', 1, 1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("race users should seed");

        let repository = MysqlSettlementRepository::new(pool.clone());
        let reserve = |request_id: &str, subject_id: &str| ReserveUsagePolicyRequestInput {
            request_id: request_id.to_string(),
            subject_id: subject_id.to_string(),
            event_token: "admission-race-token".to_string(),
            admitted_at_unix_secs: 100,
            retain_until_unix_secs: 1_000,
            windows: vec![UsagePolicyRequestWindow {
                starts_at_unix_secs: 0,
                ends_at_unix_secs: 1_000,
                limit_requests: 10,
            }],
        };
        let (first, second) = tokio::join!(
            repository.reserve_usage_policy_request(reserve(
                "admission-race-request-1",
                "admission-race-user-1"
            )),
            repository.reserve_usage_policy_request(reserve(
                "admission-race-request-2",
                "admission-race-user-2"
            ))
        );
        let mut outcomes = vec![
            first.expect("first reserve should not deadlock"),
            second.expect("second reserve should not deadlock"),
        ];
        outcomes.sort_by_key(|outcome| match outcome {
            ReserveUsagePolicyRequestOutcome::Allowed => 0,
            ReserveUsagePolicyRequestOutcome::Conflict => 1,
            _ => 2,
        });
        assert_eq!(
            outcomes,
            vec![
                ReserveUsagePolicyRequestOutcome::Allowed,
                ReserveUsagePolicyRequestOutcome::Conflict,
            ]
        );

        cleanup_request_admission_rows(&pool).await;
    }

    #[tokio::test]
    async fn mysql_repository_settles_once_and_enqueues_provider_delta_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql settlement parity test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("mysql pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");
        cleanup_settlement_rows(&pool).await;

        sqlx::query(
            r#"
INSERT INTO providers (
  id, name, provider_type, billing_type, monthly_used_usd,
  quota_last_reset_at, created_at, updated_at
)
VALUES (
  'settlement-provider-1', 'Settlement Provider', 'openai', 'monthly_quota',
  5.0, 1200, 1, 1
)
"#,
        )
        .execute(&pool)
        .await
        .expect("provider should seed");
        sqlx::query(
            r#"
INSERT INTO wallets (id, user_id, balance, gift_balance, limit_mode, created_at, updated_at)
VALUES ('settlement-wallet-1', 'settlement-user-1', 10.0, 2.0, 'finite', 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("wallet should seed");
        sqlx::query(
            r#"
INSERT INTO `usage` (
  request_id, user_id, provider_id, status, billing_status,
  total_cost_usd, actual_total_cost_usd
)
VALUES (
  'settlement-request-1', 'settlement-user-1', 'settlement-provider-1',
  'completed', 'pending', 3.0, 6.0
)
"#,
        )
        .execute(&pool)
        .await
        .expect("usage should seed");

        let repository = MysqlSettlementRepository::new(pool.clone());
        let input = UsageSettlementInput {
            request_id: "settlement-request-1".to_string(),
            user_id: Some("settlement-user-1".to_string()),
            api_key_id: None,
            api_key_is_standalone: false,
            skip_user_billing: None,
            skip_plan_billing: None,
            provider_id: Some("settlement-provider-1".to_string()),
            status: "completed".to_string(),
            billing_status: "pending".to_string(),
            total_cost_usd: 3.0,
            actual_total_cost_usd: 6.0,
            finalized_at_unix_secs: Some(1_234),
        };
        let first = repository
            .settle_usage(input.clone())
            .await
            .expect("settlement should run")
            .expect("usage should exist");
        let second = repository
            .settle_usage(input)
            .await
            .expect("second settlement should run")
            .expect("usage should exist");
        assert_eq!(first.billing_status, "settled");
        assert_eq!(first.provider_monthly_used_usd, None);
        assert_eq!(second.finalized_at_unix_secs, Some(1_234));

        let wallet: (f64, f64, f64) = sqlx::query_as(
            "SELECT balance, gift_balance, total_consumed FROM wallets WHERE id = 'settlement-wallet-1'",
        )
        .fetch_one(&pool)
        .await
        .expect("wallet should load");
        assert_eq!(wallet, (4.0, 2.0, 6.0));
        let provider_used: f64 = sqlx::query_scalar(
            "SELECT monthly_used_usd FROM providers WHERE id = 'settlement-provider-1'",
        )
        .fetch_one(&pool)
        .await
        .expect("provider should load");
        assert_eq!(provider_used, 5.0);
        let provider_delta: (i64, f64) = sqlx::query_as(
            r#"
SELECT CAST(COUNT(*) AS SIGNED), COALESCE(SUM(total_cost_usd_delta), 0)
FROM usage_counter_deltas
WHERE request_id = 'settlement-request-1'
  AND kind = 'provider_monthly'
  AND target_id = 'settlement-provider-1'
"#,
        )
        .fetch_one(&pool)
        .await
        .expect("provider delta should load");
        assert_eq!(provider_delta, (1, 6.0));

        cleanup_settlement_rows(&pool).await;
    }

    async fn cleanup_settlement_rows(pool: &sqlx::MySqlPool) {
        for sql in [
            "DELETE FROM usage_counter_deltas WHERE request_id = 'settlement-request-1'",
            "DELETE FROM usage_settlement_snapshots WHERE request_id = 'settlement-request-1'",
            "DELETE FROM `usage` WHERE request_id = 'settlement-request-1'",
            "DELETE FROM wallets WHERE id = 'settlement-wallet-1'",
            "DELETE FROM providers WHERE id = 'settlement-provider-1'",
        ] {
            sqlx::query(sql)
                .execute(pool)
                .await
                .expect("settlement cleanup should succeed");
        }
    }

    #[tokio::test]
    async fn mysql_settlement_respects_commerce_billing_flags_when_database_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql settlement commerce test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        crate::run_migrations(&pool)
            .await
            .expect("mysql migrations should run");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let short = &suffix[..8];
        let user_id = format!("commerce-user-{short}");
        let user_key_id = format!("commerce-key-{short}");
        let standalone_key_id = format!("commerce-standalone-{short}");
        let provider_id = format!("commerce-provider-{short}");
        let wallet_id = format!("commerce-wallet-{short}");
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO users (id, username, role, auth_source, created_at, updated_at) VALUES (?, ?, 'user', 'local', ?, ?)",
        )
        .bind(&user_id)
        .bind(format!("commerce-{short}"))
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .expect("mysql commerce user should insert");
        for (key_id, standalone) in [(&user_key_id, false), (&standalone_key_id, true)] {
            sqlx::query(
                "INSERT INTO api_keys (id, user_id, key_hash, is_standalone, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(key_id)
            .bind(&user_id)
            .bind(format!("hash-{key_id}"))
            .bind(standalone)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await
            .expect("mysql commerce api key should insert");
        }
        sqlx::query(
            "INSERT INTO providers (id, name, provider_type, billing_type, monthly_used_usd, quota_last_reset_at, created_at, updated_at) VALUES (?, ?, 'custom', 'monthly_quota', 0, 1980, ?, ?)",
        )
        .bind(&provider_id)
        .bind(format!("Commerce Provider {short}"))
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .expect("mysql commerce provider should insert");
        sqlx::query(
            "INSERT INTO wallets (id, user_id, balance, gift_balance, created_at, updated_at) VALUES (?, ?, 10, 0, ?, ?)",
        )
        .bind(&wallet_id)
        .bind(&user_id)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .expect("mysql commerce wallet should insert");

        let skip_user_request = format!("commerce-skip-user-{short}");
        let skip_plan_request = format!("commerce-skip-plan-{short}");
        let standalone_request = format!("commerce-standalone-{short}");
        sqlx::query(
            r#"
INSERT INTO `usage` (
  id, request_id, user_id, api_key_id, provider_name, model, provider_id,
  status, billing_status, total_cost_usd, actual_total_cost_usd
)
VALUES
  (?, ?, ?, ?, 'Commerce', 'model', ?, 'completed', 'pending', 3, 6),
  (?, ?, ?, ?, 'Commerce', 'model', ?, 'completed', 'pending', 3, 6),
  (?, ?, ?, ?, 'Commerce', 'model', ?, 'completed', 'pending', 3, 6)
"#,
        )
        .bind(format!("usage-{skip_user_request}"))
        .bind(&skip_user_request)
        .bind(&user_id)
        .bind(&user_key_id)
        .bind(&provider_id)
        .bind(format!("usage-{skip_plan_request}"))
        .bind(&skip_plan_request)
        .bind(&user_id)
        .bind(&user_key_id)
        .bind(&provider_id)
        .bind(format!("usage-{standalone_request}"))
        .bind(&standalone_request)
        .bind(&user_id)
        .bind(&standalone_key_id)
        .bind(&provider_id)
        .execute(&pool)
        .await
        .expect("mysql commerce usage should insert");

        let repository = MysqlSettlementRepository::new(pool.clone());
        let settled = repository
            .settle_usage(UsageSettlementInput {
                request_id: skip_user_request,
                user_id: Some(user_id.clone()),
                api_key_id: Some(user_key_id.clone()),
                api_key_is_standalone: false,
                skip_user_billing: Some(true),
                skip_plan_billing: Some(true),
                provider_id: Some(provider_id.clone()),
                status: "completed".to_string(),
                billing_status: "pending".to_string(),
                total_cost_usd: 3.0,
                actual_total_cost_usd: 6.0,
                finalized_at_unix_secs: Some(2_000),
            })
            .await
            .expect("skip-user settlement should run")
            .expect("skip-user usage should exist");
        assert_eq!(settled.wallet_id, None);

        let plan_settled = repository
            .settle_usage(UsageSettlementInput {
                request_id: skip_plan_request.clone(),
                user_id: Some(user_id.clone()),
                api_key_id: Some(user_key_id.clone()),
                api_key_is_standalone: false,
                skip_user_billing: Some(false),
                skip_plan_billing: Some(true),
                provider_id: Some(provider_id.clone()),
                status: "completed".to_string(),
                billing_status: "pending".to_string(),
                total_cost_usd: 3.0,
                actual_total_cost_usd: 6.0,
                finalized_at_unix_secs: Some(2_001),
            })
            .await
            .expect("skip-plan settlement should run")
            .expect("skip-plan usage should exist");
        assert_eq!(plan_settled.wallet_balance_after, Some(4.0));
        let replay = repository
            .settle_usage(UsageSettlementInput {
                request_id: skip_plan_request,
                user_id: Some(user_id.clone()),
                api_key_id: Some(user_key_id),
                api_key_is_standalone: false,
                skip_user_billing: Some(false),
                skip_plan_billing: Some(true),
                provider_id: Some(provider_id.clone()),
                status: "completed".to_string(),
                billing_status: "pending".to_string(),
                total_cost_usd: 3.0,
                actual_total_cost_usd: 6.0,
                finalized_at_unix_secs: Some(9_999),
            })
            .await
            .expect("replayed settlement should run")
            .expect("replayed usage should exist");
        assert_eq!(replay.finalized_at_unix_secs, Some(2_001));

        let standalone = repository
            .settle_usage(UsageSettlementInput {
                request_id: standalone_request,
                user_id: Some(user_id.clone()),
                api_key_id: Some(standalone_key_id),
                api_key_is_standalone: true,
                skip_user_billing: Some(false),
                skip_plan_billing: Some(false),
                provider_id: Some(provider_id.clone()),
                status: "completed".to_string(),
                billing_status: "pending".to_string(),
                total_cost_usd: 3.0,
                actual_total_cost_usd: 6.0,
                finalized_at_unix_secs: Some(2_002),
            })
            .await
            .expect("standalone settlement should run")
            .expect("standalone usage should exist");
        assert_eq!(standalone.wallet_id, None);
        assert_eq!(standalone.billing_status, "insufficient_quota");

        let wallet_balance: f64 = sqlx::query_scalar("SELECT balance FROM wallets WHERE id = ?")
            .bind(&wallet_id)
            .fetch_one(&pool)
            .await
            .expect("mysql commerce wallet should load");
        assert_eq!(wallet_balance, 4.0);
        let provider_cost: f64 =
            sqlx::query_scalar("SELECT monthly_used_usd FROM providers WHERE id = ?")
                .bind(&provider_id)
                .fetch_one(&pool)
                .await
                .expect("mysql commerce provider should load");
        assert_eq!(provider_cost, 0.0);
        let provider_delta: (i64, f64) = sqlx::query_as(
            r#"
SELECT CAST(COUNT(*) AS SIGNED), COALESCE(SUM(total_cost_usd_delta), 0)
FROM usage_counter_deltas
WHERE kind = 'provider_monthly'
  AND target_id = ?
"#,
        )
        .bind(&provider_id)
        .fetch_one(&pool)
        .await
        .expect("mysql commerce provider delta should load");
        assert_eq!(provider_delta, (3, 18.0));
    }

    async fn cleanup_request_admission_rows(pool: &sqlx::MySqlPool) {
        sqlx::query(
            "DELETE FROM usage_request_admissions WHERE event_token = 'admission-race-token'",
        )
        .execute(pool)
        .await
        .expect("admission race row cleanup should succeed");
        sqlx::query(
            "DELETE FROM users WHERE id IN ('admission-race-user-1', 'admission-race-user-2')",
        )
        .execute(pool)
        .await
        .expect("admission race user cleanup should succeed");
    }
}
