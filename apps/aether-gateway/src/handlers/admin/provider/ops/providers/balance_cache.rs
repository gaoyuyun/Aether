//! Durable "last known balance" snapshots for provider ops.
//!
//! The admin page never waits for an upstream here: it reads the snapshot and
//! the background refresher (`balance_refresh.rs`) updates it. Snapshots live
//! in the catalog database when one is configured and fall back to the runtime
//! KV store otherwise.
use crate::handlers::admin::request::AdminAppState;
use crate::handlers::shared::unix_secs_to_rfc3339;
use aether_data_contracts::repository::provider_ops_balance::StoredProviderOpsBalanceSnapshot;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;
use tracing::warn;

const ADMIN_PROVIDER_OPS_BALANCE_CACHE_PREFIX: &str = "provider_ops:balance:";
/// Runtime KV fallback lifetime for deployments without a catalog database.
const ADMIN_PROVIDER_OPS_BALANCE_KV_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// Reported to clients as the nominal lifetime of a successful snapshot.
const ADMIN_PROVIDER_OPS_BALANCE_CACHE_TTL_SECS: u64 = 86_400;
/// A successful value older than this is flagged `stale` in API responses.
pub(crate) const ADMIN_PROVIDER_OPS_BALANCE_STALE_AFTER_SECS: u64 = 30 * 60;
const ADMIN_PROVIDER_OPS_BALANCE_BACKOFF_BASE_SECS: u64 = 60;
const ADMIN_PROVIDER_OPS_BALANCE_BACKOFF_MAX_SECS: u64 = 30 * 60;
/// Credentials do not fix themselves; wait for the checkin worker or an
/// operator before retrying.
const ADMIN_PROVIDER_OPS_BALANCE_AUTH_FAILED_BACKOFF_SECS: u64 = 15 * 60;
const ADMIN_PROVIDER_OPS_BALANCE_ERROR_MAX_CHARS: usize = 200;
const ADMIN_PROVIDER_OPS_BALANCE_PENDING_MESSAGE: &str = "余额数据加载中，请稍后刷新";

pub(crate) fn admin_provider_ops_balance_now_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

pub(crate) fn admin_provider_ops_balance_status_is_success(status: &str) -> bool {
    matches!(status, "success" | "auth_expired")
}

fn admin_provider_ops_balance_kv_key(provider_id: &str) -> String {
    format!("{ADMIN_PROVIDER_OPS_BALANCE_CACHE_PREFIX}{provider_id}")
}

pub(crate) async fn read_admin_provider_ops_balance_snapshots(
    state: &AdminAppState<'_>,
    provider_ids: &[String],
) -> HashMap<String, StoredProviderOpsBalanceSnapshot> {
    if provider_ids.is_empty() {
        return HashMap::new();
    }
    let data = &state.app().data;
    if data.has_provider_ops_balance_snapshot_reader() {
        return match data.list_provider_ops_balance_snapshots(provider_ids).await {
            Ok(snapshots) => snapshots
                .into_iter()
                .map(|snapshot| (snapshot.provider_id.clone(), snapshot))
                .collect(),
            Err(err) => {
                warn!(error = %err, "failed to read provider ops balance snapshots");
                HashMap::new()
            }
        };
    }
    let keys = provider_ids
        .iter()
        .map(|provider_id| admin_provider_ops_balance_kv_key(provider_id))
        .collect::<Vec<_>>();
    match state.runtime_state().kv_get_many(&keys).await {
        Ok(values) => values
            .into_iter()
            .zip(provider_ids)
            .filter_map(|(raw, provider_id)| {
                let raw = raw?;
                match serde_json::from_str::<StoredProviderOpsBalanceSnapshot>(&raw) {
                    Ok(snapshot) => Some((provider_id.clone(), snapshot)),
                    Err(err) => {
                        warn!(error = %err, provider_id, "failed to parse provider ops balance snapshot");
                        None
                    }
                }
            })
            .collect(),
        Err(err) => {
            warn!(error = %err, "failed to read provider ops balance runtime cache");
            HashMap::new()
        }
    }
}

pub(crate) async fn read_admin_provider_ops_balance_snapshot(
    state: &AdminAppState<'_>,
    provider_id: &str,
) -> Option<StoredProviderOpsBalanceSnapshot> {
    read_admin_provider_ops_balance_snapshots(state, std::slice::from_ref(&provider_id.to_string()))
        .await
        .remove(provider_id)
}

pub(crate) async fn write_admin_provider_ops_balance_snapshot(
    state: &AdminAppState<'_>,
    snapshot: &StoredProviderOpsBalanceSnapshot,
) {
    let data = &state.app().data;
    if data.has_provider_ops_balance_snapshot_writer() {
        if let Err(err) = data.upsert_provider_ops_balance_snapshot(snapshot).await {
            warn!(
                error = %err,
                provider_id = %snapshot.provider_id,
                "failed to store provider ops balance snapshot"
            );
        }
        return;
    }
    let serialized = match serde_json::to_string(snapshot) {
        Ok(serialized) => serialized,
        Err(err) => {
            warn!(
                error = %err,
                provider_id = %snapshot.provider_id,
                "failed to serialize provider ops balance snapshot"
            );
            return;
        }
    };
    if let Err(err) = state
        .runtime_state()
        .kv_set(
            &admin_provider_ops_balance_kv_key(&snapshot.provider_id),
            serialized,
            Some(Duration::from_secs(ADMIN_PROVIDER_OPS_BALANCE_KV_TTL_SECS)),
        )
        .await
    {
        warn!(
            error = %err,
            provider_id = %snapshot.provider_id,
            "failed to store provider ops balance runtime cache"
        );
    }
}

pub(crate) async fn delete_admin_provider_ops_balance_snapshot(
    state: &AdminAppState<'_>,
    provider_id: &str,
) {
    let data = &state.app().data;
    if let Err(err) = data.delete_provider_ops_balance_snapshot(provider_id).await {
        warn!(error = %err, provider_id, "failed to delete provider ops balance snapshot");
    }
    if let Err(err) = state
        .runtime_state()
        .kv_delete(&admin_provider_ops_balance_kv_key(provider_id))
        .await
    {
        warn!(error = %err, provider_id, "failed to clear provider ops balance runtime cache");
    }
}

pub(crate) fn admin_provider_ops_pending_balance_response(message: &str) -> Value {
    json!({
        "status": "pending",
        "action_type": "query_balance",
        "data": Value::Null,
        "message": message,
        "executed_at": chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "response_time_ms": Value::Null,
        "cache_ttl_seconds": 0,
    })
}

/// Builds the API payload for one provider from its snapshot. The top-level
/// `status`/`data` describe the last successful query so operators keep seeing
/// a value while the upstream is unreachable; `last_error`, `stale` and
/// `next_retry_at` describe the most recent attempt.
pub(crate) fn build_admin_provider_ops_balance_response(
    snapshot: Option<&StoredProviderOpsBalanceSnapshot>,
    refresh_state: &str,
    now_unix_secs: u64,
) -> Value {
    let Some(snapshot) = snapshot else {
        let mut response =
            admin_provider_ops_pending_balance_response(ADMIN_PROVIDER_OPS_BALANCE_PENDING_MESSAGE);
        attach_admin_provider_ops_balance_meta(&mut response, None, refresh_state, now_unix_secs);
        return response;
    };
    let mut response = match snapshot.payload_json.as_ref().and_then(Value::as_object) {
        Some(payload) => {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("success");
            json!({
                "status": status,
                "action_type": "query_balance",
                "data": payload.get("data").cloned().unwrap_or(Value::Null),
                "message": if status == "auth_expired" {
                    Value::String("认证已过期".to_string())
                } else {
                    Value::Null
                },
                "executed_at": snapshot
                    .last_success_at_unix_secs
                    .and_then(unix_secs_to_rfc3339)
                    .map(Value::String)
                    .or_else(|| payload.get("executed_at").cloned())
                    .unwrap_or(Value::Null),
                "response_time_ms": payload.get("response_time_ms").cloned().unwrap_or(Value::Null),
                "cache_ttl_seconds": ADMIN_PROVIDER_OPS_BALANCE_CACHE_TTL_SECS,
            })
        }
        None => match snapshot.last_status.as_deref() {
            Some(status) if !admin_provider_ops_balance_status_is_success(status) => json!({
                "status": status,
                "action_type": "query_balance",
                "data": Value::Null,
                "message": snapshot
                    .last_error
                    .clone()
                    .unwrap_or_else(|| admin_provider_ops_balance_default_error(status).to_string()),
                "executed_at": snapshot
                    .last_attempt_at_unix_secs
                    .and_then(unix_secs_to_rfc3339),
                "response_time_ms": Value::Null,
                "cache_ttl_seconds": 0,
            }),
            _ => admin_provider_ops_pending_balance_response(
                ADMIN_PROVIDER_OPS_BALANCE_PENDING_MESSAGE,
            ),
        },
    };
    attach_admin_provider_ops_balance_meta(
        &mut response,
        Some(snapshot),
        refresh_state,
        now_unix_secs,
    );
    response
}

pub(crate) fn attach_admin_provider_ops_balance_meta(
    response: &mut Value,
    snapshot: Option<&StoredProviderOpsBalanceSnapshot>,
    refresh_state: &str,
    now_unix_secs: u64,
) {
    let Some(object) = response.as_object_mut() else {
        return;
    };
    let fetched_at = snapshot.and_then(|snapshot| snapshot.last_success_at_unix_secs);
    let stale = fetched_at.is_none_or(|fetched_at| {
        now_unix_secs.saturating_sub(fetched_at) > ADMIN_PROVIDER_OPS_BALANCE_STALE_AFTER_SECS
    });
    let last_error = snapshot.and_then(|snapshot| {
        let status = snapshot.last_status.as_deref()?;
        if admin_provider_ops_balance_status_is_success(status) {
            return None;
        }
        Some(json!({
            "status": status,
            "message": snapshot
                .last_error
                .clone()
                .unwrap_or_else(|| admin_provider_ops_balance_default_error(status).to_string()),
            "at": snapshot
                .last_attempt_at_unix_secs
                .and_then(unix_secs_to_rfc3339),
        }))
    });
    let next_retry_at = snapshot
        .and_then(|snapshot| snapshot.next_refresh_at_unix_secs)
        .filter(|next_refresh_at| *next_refresh_at > now_unix_secs)
        .and_then(unix_secs_to_rfc3339);
    object.insert(
        "fetched_at".to_string(),
        fetched_at
            .and_then(unix_secs_to_rfc3339)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    object.insert("stale".to_string(), Value::Bool(stale));
    object.insert(
        "refresh_state".to_string(),
        Value::String(refresh_state.to_string()),
    );
    object.insert(
        "next_retry_at".to_string(),
        next_retry_at.map(Value::String).unwrap_or(Value::Null),
    );
    object.insert(
        "consecutive_failures".to_string(),
        Value::from(snapshot.map_or(0, |snapshot| snapshot.consecutive_failures)),
    );
    object.insert("last_error".to_string(), last_error.unwrap_or(Value::Null));
}

/// Folds one query result into the snapshot. Successful payloads replace the
/// stored value; failures keep the previous value and schedule a backoff.
pub(crate) fn apply_admin_provider_ops_balance_attempt(
    previous: Option<&StoredProviderOpsBalanceSnapshot>,
    provider_id: &str,
    payload: &Value,
    now_unix_secs: u64,
) -> StoredProviderOpsBalanceSnapshot {
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .unwrap_or("unknown_error");
    let mut next = previous
        .cloned()
        .unwrap_or_else(|| StoredProviderOpsBalanceSnapshot {
            provider_id: provider_id.to_string(),
            payload_json: None,
            last_success_at_unix_secs: None,
            last_attempt_at_unix_secs: None,
            last_status: None,
            last_error: None,
            consecutive_failures: 0,
            next_refresh_at_unix_secs: None,
            updated_at_unix_secs: now_unix_secs,
        });
    next.provider_id = provider_id.to_string();
    next.last_attempt_at_unix_secs = Some(now_unix_secs);
    next.updated_at_unix_secs = now_unix_secs;

    if admin_provider_ops_balance_status_is_success(status) {
        if let Some(projected) = project_admin_provider_ops_balance_cache_payload(payload) {
            next.payload_json = Some(projected);
            next.last_success_at_unix_secs = Some(now_unix_secs);
            next.last_status = Some(status.to_string());
            next.last_error = None;
            next.consecutive_failures = 0;
            next.next_refresh_at_unix_secs = None;
            return next;
        }
        record_admin_provider_ops_balance_failure(
            &mut next,
            "parse_error",
            Some("响应格式无效"),
            now_unix_secs,
        );
        return next;
    }

    record_admin_provider_ops_balance_failure(
        &mut next,
        status,
        payload.get("message").and_then(Value::as_str),
        now_unix_secs,
    );
    next
}

fn record_admin_provider_ops_balance_failure(
    snapshot: &mut StoredProviderOpsBalanceSnapshot,
    status: &str,
    message: Option<&str>,
    now_unix_secs: u64,
) {
    snapshot.consecutive_failures = snapshot.consecutive_failures.saturating_add(1);
    snapshot.last_status = Some(status.to_string());
    snapshot.last_error = Some(admin_provider_ops_balance_error_message(status, message));
    snapshot.next_refresh_at_unix_secs = Some(now_unix_secs.saturating_add(
        admin_provider_ops_balance_backoff_secs(status, snapshot.consecutive_failures),
    ));
}

pub(crate) fn admin_provider_ops_balance_backoff_secs(
    status: &str,
    consecutive_failures: u32,
) -> u64 {
    match status {
        "auth_failed" => ADMIN_PROVIDER_OPS_BALANCE_AUTH_FAILED_BACKOFF_SECS,
        "not_configured" | "not_supported" | "parse_error" => {
            ADMIN_PROVIDER_OPS_BALANCE_BACKOFF_MAX_SECS
        }
        _ => {
            let exponent = consecutive_failures.saturating_sub(1).min(16);
            ADMIN_PROVIDER_OPS_BALANCE_BACKOFF_BASE_SECS
                .saturating_mul(1u64 << exponent)
                .min(ADMIN_PROVIDER_OPS_BALANCE_BACKOFF_MAX_SECS)
        }
    }
}

fn admin_provider_ops_balance_default_error(status: &str) -> &'static str {
    match status {
        "auth_failed" => "认证失败",
        "network_error" => "网络错误",
        "rate_limited" => "请求频率限制",
        "parse_error" => "响应解析失败",
        "not_configured" => "未配置操作设置",
        "not_supported" => "功能未开放",
        _ => "查询失败",
    }
}

/// Messages come from the gateway's own classification of an attempt, never
/// from upstream bodies. Still refuse anything that looks like a credential
/// echo and cap the length before it reaches storage or the UI.
fn admin_provider_ops_balance_error_message(status: &str, message: Option<&str>) -> String {
    let fallback = admin_provider_ops_balance_default_error(status);
    let Some(message) = message.map(str::trim).filter(|message| !message.is_empty()) else {
        return fallback.to_string();
    };
    if message.chars().any(char::is_control) {
        return fallback.to_string();
    }
    let lower = message.to_ascii_lowercase();
    if [
        "authorization",
        "bearer ",
        "token=",
        "api_key=",
        "password=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return fallback.to_string();
    }
    message
        .chars()
        .take(ADMIN_PROVIDER_OPS_BALANCE_ERROR_MAX_CHARS)
        .collect()
}

/// `total_available` of the last successful query, used by the quota alert.
pub(crate) fn admin_provider_ops_balance_snapshot_total_available(
    snapshot: &StoredProviderOpsBalanceSnapshot,
) -> Option<f64> {
    let payload = snapshot.payload_json.as_ref()?;
    if payload.get("status").and_then(Value::as_str) != Some("success") {
        return None;
    }
    payload
        .get("data")
        .and_then(|data| data.get("total_available"))
        .and_then(|value| {
            value.as_f64().or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<f64>().ok())
            })
        })
        .filter(|value| value.is_finite())
}

const BALANCE_CACHE_EXTRA_NUMERIC_FIELDS: &[&str] = &[
    "balance",
    "points",
    "active_subscriptions",
    "total_used_usd",
    "normal_balance",
    "subscription_balance",
    "charity_balance",
    "pay_as_you_go_balance",
    "daily_limit",
    "weekly_limit",
    "weekly_spent",
    "daily_spent",
    "daily_used_quota",
    "daily_quota_limit",
    "daily_remaining_quota",
];

const BALANCE_CACHE_EXTRA_STRING_FIELDS: &[&str] = &[
    "plan_name",
    "subscription_status",
    "status",
    "group_name",
    "effective_start_date",
    "effective_end_date",
];

const BALANCE_CACHE_EXTRA_BOOL_FIELDS: &[&str] = &["checkin_success", "cookie_expired"];

const BALANCE_CACHE_EXTRA_NESTED_FIELDS: &[&str] = &[
    "five_hour_limit",
    "weekly_limit",
    "month_stats",
    "subscriptions",
];
const BALANCE_CACHE_LIMIT_FIELDS: &[&str] = &["limit", "used", "remaining", "resets_at"];
const BALANCE_CACHE_MONTH_STATS_FIELDS: &[&str] = &[
    "total_input_tokens",
    "total_output_tokens",
    "total_quota",
    "total_requests",
];

/// Projects a successful query result onto the allowlisted, secret-free shape
/// that is safe to persist and return to every admin session.
pub(crate) fn project_admin_provider_ops_balance_cache_payload(payload: &Value) -> Option<Value> {
    let source = payload.as_object()?;
    let status = source.get("status").and_then(Value::as_str)?.trim();
    if !matches!(status, "success" | "auth_expired" | "auth_failed") {
        return None;
    }
    if source.get("action_type").and_then(Value::as_str) != Some("query_balance") {
        return None;
    }

    let mut projected = Map::new();
    projected.insert("status".to_string(), Value::String(status.to_string()));
    projected.insert(
        "action_type".to_string(),
        Value::String("query_balance".to_string()),
    );

    let data = match source.get("data") {
        Some(Value::Null) | None => Value::Null,
        Some(value) => project_admin_provider_ops_balance_data(value)?,
    };
    projected.insert("data".to_string(), data);
    projected.insert(
        "message".to_string(),
        match status {
            "auth_failed" => Value::String("认证失败".to_string()),
            "auth_expired" => Value::String("认证已过期".to_string()),
            _ => Value::Null,
        },
    );
    if let Some(value) = source
        .get("executed_at")
        .and_then(project_admin_provider_ops_safe_string)
    {
        projected.insert("executed_at".to_string(), Value::String(value));
    }
    if let Some(value) = source
        .get("response_time_ms")
        .and_then(project_admin_provider_ops_finite_number)
    {
        projected.insert("response_time_ms".to_string(), value);
    }
    projected.insert(
        "cache_ttl_seconds".to_string(),
        Value::from(ADMIN_PROVIDER_OPS_BALANCE_CACHE_TTL_SECS),
    );
    Some(Value::Object(projected))
}

fn project_admin_provider_ops_balance_data(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let mut projected = Map::new();
    for field in ["total_granted", "total_used", "total_available"] {
        if let Some(value) = source.get(field) {
            projected.insert(
                field.to_string(),
                project_admin_provider_ops_finite_number_or_null(value)?,
            );
        }
    }
    if let Some(value) = source.get("expires_at") {
        projected.insert(
            "expires_at".to_string(),
            project_admin_provider_ops_finite_number_or_null(value)?,
        );
    }
    if let Some(value) = source.get("currency") {
        let currency = project_admin_provider_ops_safe_string(value)?;
        if currency.len() > 32
            || !currency
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
        {
            return None;
        }
        projected.insert("currency".to_string(), Value::String(currency));
    }
    if let Some(extra) = source.get("extra") {
        projected.insert(
            "extra".to_string(),
            project_admin_provider_ops_balance_extra(extra)?,
        );
    }
    Some(Value::Object(projected))
}

fn project_admin_provider_ops_balance_extra(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let mut projected = Map::new();
    for (field, value) in source {
        let projected_value = if BALANCE_CACHE_EXTRA_NUMERIC_FIELDS.contains(&field.as_str())
            && project_admin_provider_ops_finite_number(value).is_some()
        {
            project_admin_provider_ops_finite_number(value)
        } else if BALANCE_CACHE_EXTRA_STRING_FIELDS.contains(&field.as_str()) {
            project_admin_provider_ops_safe_string(value).map(Value::String)
        } else if BALANCE_CACHE_EXTRA_BOOL_FIELDS.contains(&field.as_str()) {
            value.as_bool().map(Value::Bool)
        } else if BALANCE_CACHE_EXTRA_NESTED_FIELDS.contains(&field.as_str()) {
            project_admin_provider_ops_balance_extra_nested(field, value)
        } else if matches!(
            field.as_str(),
            "weekly_resets_at" | "daily_resets_at" | "resets_at"
        ) {
            project_admin_provider_ops_finite_number_or_safe_string(value)
        } else if matches!(field.as_str(), "checkin_message" | "cookie_expired_message") {
            project_admin_provider_ops_safe_string(value).map(Value::String)
        } else {
            None
        };
        if let Some(projected_value) = projected_value {
            projected.insert(field.clone(), projected_value);
        }
    }
    Some(Value::Object(projected))
}

fn project_admin_provider_ops_balance_extra_nested(field: &str, value: &Value) -> Option<Value> {
    if field == "subscriptions" {
        let items = value.as_array()?;
        return Some(Value::Array(
            items
                .iter()
                .take(128)
                .filter_map(project_admin_provider_ops_subscription)
                .collect(),
        ));
    }
    let source = value.as_object()?;
    let mut projected = Map::new();
    let allowed = if field == "month_stats" {
        BALANCE_CACHE_MONTH_STATS_FIELDS
    } else {
        BALANCE_CACHE_LIMIT_FIELDS
    };
    for key in allowed {
        if let Some(value) = source.get(*key) {
            let projected_value = if *key == "resets_at" {
                project_admin_provider_ops_finite_number_or_safe_string(value)
            } else {
                project_admin_provider_ops_finite_number(value)
            };
            if let Some(projected_value) = projected_value {
                projected.insert((*key).to_string(), projected_value);
            }
        }
    }
    Some(Value::Object(projected))
}

fn project_admin_provider_ops_subscription(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let mut projected = Map::new();
    for field in ["group_name", "status"] {
        if let Some(value) = source.get(field) {
            projected.insert(
                field.to_string(),
                Value::String(project_admin_provider_ops_safe_string(value)?),
            );
        }
    }
    for field in [
        "daily_used_usd",
        "daily_limit_usd",
        "weekly_used_usd",
        "weekly_limit_usd",
        "monthly_used_usd",
        "monthly_limit_usd",
    ] {
        if let Some(value) = source.get(field) {
            if let Some(value) = project_admin_provider_ops_finite_number(value) {
                projected.insert(field.to_string(), value);
            }
        }
    }
    if let Some(value) = source.get("expires_at") {
        if let Some(value) = project_admin_provider_ops_finite_number_or_safe_string(value) {
            projected.insert("expires_at".to_string(), value);
        }
    }
    Some(Value::Object(projected))
}

fn project_admin_provider_ops_finite_number(value: &Value) -> Option<Value> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then(|| value.clone());
    }
    let number = value.as_str()?.trim().parse::<f64>().ok()?;
    number.is_finite().then(|| Value::from(number))
}

fn project_admin_provider_ops_finite_number_or_null(value: &Value) -> Option<Value> {
    if value.is_null() {
        Some(Value::Null)
    } else {
        project_admin_provider_ops_finite_number(value)
    }
}

fn project_admin_provider_ops_finite_number_or_safe_string(value: &Value) -> Option<Value> {
    project_admin_provider_ops_finite_number(value)
        .or_else(|| project_admin_provider_ops_safe_string(value).map(Value::String))
}

fn project_admin_provider_ops_safe_string(value: &Value) -> Option<String> {
    let value = value.as_str()?.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if [
        "authorization",
        "bearer ",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "cookie",
        "session",
        "secret",
        "token=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        admin_provider_ops_balance_backoff_secs,
        admin_provider_ops_balance_snapshot_total_available,
        admin_provider_ops_pending_balance_response, apply_admin_provider_ops_balance_attempt,
        build_admin_provider_ops_balance_response,
        project_admin_provider_ops_balance_cache_payload,
        ADMIN_PROVIDER_OPS_BALANCE_STALE_AFTER_SECS,
    };
    use serde_json::json;

    fn success_payload(total_available: f64) -> serde_json::Value {
        json!({
            "status": "success",
            "action_type": "query_balance",
            "data": {
                "total_available": total_available,
                "currency": "USD",
                "extra": {"balance": total_available}
            },
            "message": null,
            "executed_at": "2026-09-19T00:00:00Z",
            "response_time_ms": 120,
            "cache_ttl_seconds": 86400
        })
    }

    #[test]
    fn pending_balance_response_uses_pending_status() {
        let payload = admin_provider_ops_pending_balance_response("余额数据加载中，请稍后刷新");
        assert_eq!(payload["status"], json!("pending"));
        assert_eq!(payload["action_type"], json!("query_balance"));
    }

    #[test]
    fn successful_attempt_replaces_value_and_clears_failures() {
        let first = apply_admin_provider_ops_balance_attempt(
            None,
            "provider-1",
            &json!({"status": "network_error", "message": "请求超时"}),
            1_000,
        );
        assert_eq!(first.consecutive_failures, 1);
        assert_eq!(first.last_status.as_deref(), Some("network_error"));
        assert_eq!(first.last_error.as_deref(), Some("请求超时"));
        assert_eq!(first.next_refresh_at_unix_secs, Some(1_060));
        assert!(first.payload_json.is_none());

        let second = apply_admin_provider_ops_balance_attempt(
            Some(&first),
            "provider-1",
            &success_payload(4.5),
            1_100,
        );
        assert_eq!(second.consecutive_failures, 0);
        assert_eq!(second.last_success_at_unix_secs, Some(1_100));
        assert_eq!(second.next_refresh_at_unix_secs, None);
        assert_eq!(second.last_error, None);
        assert_eq!(
            second.payload_json.as_ref().unwrap()["data"]["total_available"],
            json!(4.5)
        );
        assert_eq!(
            admin_provider_ops_balance_snapshot_total_available(&second),
            Some(4.5)
        );
    }

    #[test]
    fn failed_attempt_keeps_last_value_and_backs_off_exponentially() {
        let good = apply_admin_provider_ops_balance_attempt(
            None,
            "provider-1",
            &success_payload(9.0),
            1_000,
        );
        let mut snapshot = good.clone();
        for (attempt, expected_backoff) in [(1u32, 60u64), (2, 120), (3, 240)] {
            let now = 2_000 + u64::from(attempt) * 1_000;
            snapshot = apply_admin_provider_ops_balance_attempt(
                Some(&snapshot),
                "provider-1",
                &json!({"status": "network_error", "message": "请求超时"}),
                now,
            );
            assert_eq!(snapshot.consecutive_failures, attempt);
            assert_eq!(
                snapshot.next_refresh_at_unix_secs,
                Some(now + expected_backoff)
            );
            assert_eq!(snapshot.payload_json, good.payload_json);
            assert_eq!(snapshot.last_success_at_unix_secs, Some(1_000));
        }

        // 25 minutes after the last success: still fresh, but the backoff from
        // the third failure (until 5_240) is pending.
        let response = build_admin_provider_ops_balance_response(Some(&snapshot), "idle", 2_500);
        assert_eq!(response["status"], json!("success"));
        assert_eq!(response["data"]["total_available"], json!(9.0));
        assert_eq!(response["last_error"]["status"], json!("network_error"));
        assert_eq!(response["last_error"]["message"], json!("请求超时"));
        assert_eq!(response["consecutive_failures"], json!(3));
        assert_eq!(response["stale"], json!(false));
        assert!(response["next_retry_at"].is_string());
        assert_eq!(response["fetched_at"], json!("1970-01-01T00:16:40Z"));

        // Past the freshness window and past the backoff.
        let past_backoff = snapshot.next_refresh_at_unix_secs.unwrap_or_default() + 1;
        let much_later = past_backoff.max(1_000 + ADMIN_PROVIDER_OPS_BALANCE_STALE_AFTER_SECS + 1);
        let stale =
            build_admin_provider_ops_balance_response(Some(&snapshot), "queued", much_later);
        assert_eq!(stale["stale"], json!(true));
        assert_eq!(stale["refresh_state"], json!("queued"));
        assert_eq!(stale["next_retry_at"], json!(null));
    }

    #[test]
    fn failure_without_previous_value_is_reported_as_error_not_pending() {
        let snapshot = apply_admin_provider_ops_balance_attempt(
            None,
            "provider-1",
            &json!({"status": "auth_failed", "message": "认证失败，请检查凭据配置"}),
            1_000,
        );
        assert_eq!(snapshot.next_refresh_at_unix_secs, Some(1_000 + 15 * 60));
        let response = build_admin_provider_ops_balance_response(Some(&snapshot), "idle", 1_001);
        assert_eq!(response["status"], json!("auth_failed"));
        assert_eq!(response["message"], json!("认证失败，请检查凭据配置"));
        assert_eq!(response["data"], json!(null));
        assert_eq!(response["fetched_at"], json!(null));
        assert_eq!(response["stale"], json!(true));

        let missing = build_admin_provider_ops_balance_response(None, "queued", 1_001);
        assert_eq!(missing["status"], json!("pending"));
        assert_eq!(missing["refresh_state"], json!("queued"));
    }

    #[test]
    fn error_messages_never_echo_credentials() {
        let snapshot = apply_admin_provider_ops_balance_attempt(
            None,
            "provider-1",
            &json!({"status": "unknown_error", "message": "HTTP 401 for Authorization: Bearer sk-secret"}),
            1_000,
        );
        assert_eq!(snapshot.last_error.as_deref(), Some("查询失败"));
        let long = "x".repeat(500);
        let snapshot = apply_admin_provider_ops_balance_attempt(
            None,
            "provider-1",
            &json!({"status": "network_error", "message": long}),
            1_000,
        );
        assert_eq!(snapshot.last_error.as_ref().map(String::len), Some(200));
    }

    #[test]
    fn backoff_schedule_matches_status_contract() {
        assert_eq!(
            admin_provider_ops_balance_backoff_secs("network_error", 1),
            60
        );
        assert_eq!(
            admin_provider_ops_balance_backoff_secs("network_error", 5),
            960
        );
        assert_eq!(
            admin_provider_ops_balance_backoff_secs("network_error", 12),
            1_800
        );
        assert_eq!(
            admin_provider_ops_balance_backoff_secs("auth_failed", 1),
            900
        );
        assert_eq!(
            admin_provider_ops_balance_backoff_secs("not_configured", 1),
            1_800
        );
    }

    #[test]
    fn balance_cache_projection_drops_untrusted_messages_and_fields() {
        let payload = json!({
            "status": "auth_failed",
            "action_type": "query_balance",
            "message": "authorization=Bearer upstream-secret",
            "data": {
                "total_available": 1.25,
                "currency": "USD",
                "extra": {
                    "balance": 1.0,
                    "access_token": "upstream-secret",
                    "today_stats": {"private_note": "upstream-secret"},
                    "checkin_message": "签到失败"
                }
            },
            "cache_ttl_seconds": 999999
        });
        let projected = project_admin_provider_ops_balance_cache_payload(&payload)
            .expect("known balance payload should project");
        assert_eq!(projected["message"], json!("认证失败"));
        assert_eq!(projected["data"]["extra"]["balance"], json!(1.0));
        assert!(projected.to_string().find("upstream-secret").is_none());
        assert!(projected["data"]["extra"].get("access_token").is_none());
        assert!(projected["data"]["extra"].get("today_stats").is_none());
    }

    #[test]
    fn balance_cache_projection_keeps_sub2api_subscription_allowlist() {
        let payload = json!({
            "status": "success",
            "action_type": "query_balance",
            "data": {
                "total_available": 8.5,
                "currency": "USD",
                "extra": {
                    "subscriptions": [{
                        "group_name": "default",
                        "status": "active",
                        "monthly_used_usd": 1.2,
                        "private_token": "must-drop"
                    }]
                }
            }
        });
        let projected = project_admin_provider_ops_balance_cache_payload(&payload)
            .expect("known balance payload should project");
        assert_eq!(
            projected["data"]["extra"]["subscriptions"][0]["group_name"],
            json!("default")
        );
        assert_eq!(
            projected["data"]["extra"]["subscriptions"][0]["monthly_used_usd"],
            json!(1.2)
        );
        assert!(projected["data"]["extra"]["subscriptions"][0]
            .get("private_token")
            .is_none());
    }
}
