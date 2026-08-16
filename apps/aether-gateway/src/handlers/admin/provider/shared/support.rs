use crate::handlers::admin::request::AdminAppState;
use crate::LocalProviderDeleteTaskState;
use aether_pool_core::PoolMemberScoreRules;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const ADMIN_PROVIDER_MAPPING_PREVIEW_MAX_KEYS: usize = 200;
pub(crate) const ADMIN_PROVIDER_MAPPING_PREVIEW_MAX_MODELS: usize = 500;
pub(crate) const ADMIN_PROVIDER_MAPPING_PREVIEW_FETCH_LIMIT: usize = 10_000;
pub(crate) const ADMIN_PROVIDER_POOL_SCAN_BATCH: u64 = 200;
pub(crate) const ADMIN_PROVIDER_POOL_QUOTA_PROBE_ACTIVE_SET_PREFIX: &str =
    "ap:quota_probe:active_members";
pub(crate) const ADMIN_PROVIDER_OAUTH_DATA_UNAVAILABLE_DETAIL: &str =
    "Admin provider OAuth data unavailable";
pub(crate) const PROVIDER_MAX_TRANSFER_COUNT_CONFIG_KEY: &str = "max_transfer_count";
pub(crate) const PROVIDER_MAX_TRANSFER_TIMEOUT_SECONDS_CONFIG_KEY: &str =
    "max_transfer_timeout_seconds";
pub(crate) const PROVIDER_QUOTA_WINDOWS_CONFIG_KEY: &str = "quota_windows";
const PROVIDER_QUOTA_WINDOW_MIN_DURATION_SECS: u64 = 60;
const PROVIDER_QUOTA_WINDOW_MAX_DURATION_SECS: u64 = 30 * 24 * 60 * 60;
const PROVIDER_QUOTA_WINDOW_MAX_COUNT: usize = 8;

pub(crate) fn normalize_provider_quota_windows(value: Option<&Value>) -> Result<Value, String> {
    let Some(value) = value else {
        return Ok(Value::Array(Vec::new()));
    };
    let Some(entries) = value.as_array() else {
        return Err("quota_windows 必须是数组".to_string());
    };
    if entries.len() > PROVIDER_QUOTA_WINDOW_MAX_COUNT {
        return Err(format!(
            "quota_windows 最多支持 {} 个窗口",
            PROVIDER_QUOTA_WINDOW_MAX_COUNT
        ));
    }

    let mut normalized = Vec::with_capacity(entries.len());
    let mut durations = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            return Err(format!("quota_windows[{index}] 必须是对象"));
        };
        let duration_secs = object
            .get("duration_secs")
            .and_then(|value| {
                value.as_u64().or_else(|| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<u64>().ok())
                })
            })
            .ok_or_else(|| format!("quota_windows[{index}].duration_secs 必须是整数"))?;
        if !(PROVIDER_QUOTA_WINDOW_MIN_DURATION_SECS..=PROVIDER_QUOTA_WINDOW_MAX_DURATION_SECS)
            .contains(&duration_secs)
            || duration_secs % 60 != 0
        {
            return Err(format!(
                "quota_windows[{index}].duration_secs 必须是 60 的倍数，且在 {PROVIDER_QUOTA_WINDOW_MIN_DURATION_SECS} 到 {PROVIDER_QUOTA_WINDOW_MAX_DURATION_SECS} 秒之间"
            ));
        }
        if !durations.insert(duration_secs) {
            return Err(format!(
                "quota_windows[{index}].duration_secs 与其他窗口重复"
            ));
        }
        let limit_usd = object
            .get("limit_usd")
            .and_then(|value| {
                value.as_f64().or_else(|| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<f64>().ok())
                })
            })
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or_else(|| format!("quota_windows[{index}].limit_usd 必须是非负数"))?;
        normalized.push(json!({
            "duration_secs": duration_secs,
            "limit_usd": limit_usd,
        }));
    }
    Ok(Value::Array(normalized))
}

pub(crate) fn normalize_provider_transfer_limit(
    value: i64,
    field_name: &str,
) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{field_name} 必须是非负整数"))
}

pub(crate) fn normalize_provider_transfer_limit_json(
    value: &serde_json::Value,
    field_name: &str,
) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{field_name} 必须是非负整数"))
}

pub(crate) fn provider_transfer_limit_from_config(
    config: Option<&serde_json::Map<String, serde_json::Value>>,
    field_name: &str,
) -> u64 {
    config
        .and_then(|config| config.get(field_name))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

pub(crate) fn admin_provider_pool_quota_probe_active_members_key(provider_id: &str) -> String {
    format!("{ADMIN_PROVIDER_POOL_QUOTA_PROBE_ACTIVE_SET_PREFIX}:{provider_id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdminProviderPoolSchedulingPreset {
    pub(crate) preset: String,
    pub(crate) enabled: bool,
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdminProviderPoolUnschedulableRule {
    pub(crate) keyword: String,
    pub(crate) duration_minutes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct AdminProviderPoolConfig {
    pub(crate) scheduling_presets: Vec<AdminProviderPoolSchedulingPreset>,
    pub(crate) unschedulable_rules: Vec<AdminProviderPoolUnschedulableRule>,
    pub(crate) lru_enabled: bool,
    pub(crate) skip_exhausted_accounts: bool,
    pub(crate) sticky_session_ttl_seconds: u64,
    pub(crate) latency_window_seconds: u64,
    pub(crate) latency_sample_limit: u64,
    pub(crate) cost_window_seconds: u64,
    pub(crate) cost_limit_per_key_tokens: Option<u64>,
    pub(crate) rate_limit_cooldown_seconds: u64,
    pub(crate) overload_cooldown_seconds: u64,
    pub(crate) probing_enabled: bool,
    pub(crate) probing_target_percent: Option<f64>,
    pub(crate) probing_target_count: Option<u64>,
    pub(crate) probe_concurrency: u64,
    pub(crate) account_self_check_enabled: bool,
    pub(crate) account_self_check_interval_minutes: u64,
    pub(crate) account_self_check_concurrency: u64,
    pub(crate) score_top_n: u64,
    pub(crate) score_fallback_scan_limit: u64,
    pub(crate) score_rules: PoolMemberScoreRules,
    pub(crate) stream_timeout_threshold: u64,
    pub(crate) stream_timeout_window_seconds: u64,
    pub(crate) stream_timeout_cooldown_seconds: u64,
}

#[derive(Debug, Default)]
pub(crate) struct AdminProviderPoolRuntimeState {
    pub(crate) total_sticky_sessions: usize,
    pub(crate) sticky_sessions_by_key: BTreeMap<String, usize>,
    pub(crate) sticky_bound_key_id: Option<String>,
    pub(crate) active_probe_member_ids: BTreeSet<String>,
    pub(crate) provider_in_flight: usize,
    pub(crate) provider_ema_in_flight: f64,
    pub(crate) provider_desired_hot: usize,
    pub(crate) provider_burst_pending: bool,
    pub(crate) cooldown_reason_by_key: BTreeMap<String, String>,
    pub(crate) cooldown_ttl_by_key: BTreeMap<String, u64>,
    pub(crate) cost_window_usage_by_key: BTreeMap<String, u64>,
    pub(crate) latency_avg_ms_by_key: BTreeMap<String, f64>,
    pub(crate) lru_score_by_key: BTreeMap<String, f64>,
}

pub(crate) fn build_admin_provider_delete_task_payload(
    task: &LocalProviderDeleteTaskState,
) -> serde_json::Value {
    json!({
        "task_id": task.task_id,
        "provider_id": task.provider_id,
        "status": task.status,
        "stage": task.stage,
        "total_keys": task.total_keys,
        "deleted_keys": task.deleted_keys,
        "total_endpoints": task.total_endpoints,
        "deleted_endpoints": task.deleted_endpoints,
        "message": task.message,
    })
}

pub(crate) fn put_admin_provider_delete_task(
    state: &AdminAppState<'_>,
    task: &LocalProviderDeleteTaskState,
) {
    state.as_ref().put_provider_delete_task(task.clone());
}

pub(crate) fn normalize_provider_billing_type(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "monthly_quota" | "pay_as_you_go" | "free_tier" => Ok(normalized),
        _ => Err("billing_type 仅支持 monthly_quota / pay_as_you_go / free_tier".to_string()),
    }
}

pub(crate) fn parse_optional_rfc3339_unix_secs(
    value: &str,
    field_name: &str,
) -> Result<u64, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field_name} 不能为空"));
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(trimmed)
        .map_err(|_| format!("{field_name} 必须是合法的 RFC3339 时间"))?;
    u64::try_from(parsed.timestamp()).map_err(|_| format!("{field_name} 超出有效时间范围"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::normalize_provider_quota_windows;

    #[test]
    fn quota_window_validation_rejects_unsafe_values() {
        assert!(normalize_provider_quota_windows(Some(&json!([
            {"duration_secs": 59, "limit_usd": 1.0}
        ])))
        .is_err());
        assert!(normalize_provider_quota_windows(Some(&json!([
            {"duration_secs": 86_400, "limit_usd": -1.0}
        ])))
        .is_err());
    }

    #[test]
    fn quota_window_validation_rejects_duplicate_periods() {
        assert!(normalize_provider_quota_windows(Some(&json!([
            {"duration_secs": 86_400, "limit_usd": 1.0},
            {"duration_secs": 86_400, "limit_usd": 2.0}
        ])))
        .is_err());
    }
}
