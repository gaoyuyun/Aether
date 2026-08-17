use crate::handlers::admin::provider::shared::support::{
    normalize_provider_billing_type, normalize_provider_quota_windows,
    parse_optional_rfc3339_unix_secs, PROVIDER_QUOTA_WINDOWS_CONFIG_KEY,
};
use crate::handlers::admin::request::AdminAppState;
use crate::handlers::admin::shared::unix_secs_to_rfc3339;
use crate::GatewayError;
use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
pub(crate) struct AdminProviderStrategyBillingRequest {
    pub(super) billing_type: String,
    #[serde(default)]
    pub(super) monthly_quota_usd: Option<f64>,
    #[serde(default = "default_provider_strategy_quota_reset_day")]
    pub(super) quota_reset_day: u64,
    #[serde(default)]
    pub(super) quota_last_reset_at: Option<String>,
    #[serde(default)]
    pub(super) quota_expires_at: Option<String>,
    #[serde(default)]
    pub(super) quota_windows: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) rpm_limit: Option<i32>,
    #[serde(default = "default_provider_strategy_provider_priority")]
    pub(super) provider_priority: i32,
}

fn default_provider_strategy_quota_reset_day() -> u64 {
    30
}

fn default_provider_strategy_provider_priority() -> i32 {
    100
}

pub(crate) fn build_provider_strategy_list_response() -> Response<Body> {
    Json(json!({
        "strategies": [{
            "name": "sticky_priority",
            "priority": 110,
            "version": "1.0.0",
            "description": "粘性优先级负载均衡策略，正常时始终使用同一提供商",
            "author": "System",
        }],
        "total": 1,
    }))
    .into_response()
}

pub(crate) async fn build_provider_strategy_update_billing_response(
    state: &AdminAppState<'_>,
    provider_id: String,
    payload: AdminProviderStrategyBillingRequest,
) -> Result<Response<Body>, GatewayError> {
    let Some(existing) = state
        .app()
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&provider_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(admin_provider_strategy_provider_not_found_response());
    };

    let billing_type = match normalize_provider_billing_type(&payload.billing_type) {
        Ok(value) => value,
        Err(message) => {
            return Ok((
                http::StatusCode::BAD_REQUEST,
                Json(json!({ "detail": message })),
            )
                .into_response());
        }
    };
    if payload
        .monthly_quota_usd
        .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Ok((
            http::StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "monthly_quota_usd 必须是非负数" })),
        )
            .into_response());
    }
    if !(1..=30).contains(&payload.quota_reset_day) {
        return Ok((
            http::StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "quota_reset_day 必须是 1 到 30 之间的整数" })),
        )
            .into_response());
    }
    if !(0..=10_000).contains(&payload.provider_priority) {
        return Ok((
            http::StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "provider_priority 必须在 0 到 10000 之间" })),
        )
            .into_response());
    }

    let mut quota_last_reset_at_unix_secs = match payload.quota_last_reset_at.as_deref() {
        Some(value) => match parse_optional_rfc3339_unix_secs(value, "quota_last_reset_at") {
            Ok(value) => Some(value),
            Err(message) => {
                return Ok((
                    http::StatusCode::BAD_REQUEST,
                    Json(json!({ "detail": message })),
                )
                    .into_response());
            }
        },
        None => existing.quota_last_reset_at_unix_secs,
    };
    let quota_expires_at_unix_secs = match payload.quota_expires_at.as_deref() {
        Some(value) => match parse_optional_rfc3339_unix_secs(value, "quota_expires_at") {
            Ok(value) => Some(value),
            Err(message) => {
                return Ok((
                    http::StatusCode::BAD_REQUEST,
                    Json(json!({ "detail": message })),
                )
                    .into_response());
            }
        },
        None => existing.quota_expires_at_unix_secs,
    };
    if existing
        .quota_last_reset_at_unix_secs
        .zip(quota_last_reset_at_unix_secs)
        .is_some_and(|(existing, updated)| existing / 60 == updated / 60)
    {
        quota_last_reset_at_unix_secs = existing.quota_last_reset_at_unix_secs;
    }
    let quota_start_changed =
        existing.quota_last_reset_at_unix_secs != quota_last_reset_at_unix_secs;

    let mut config_map = existing
        .config
        .clone()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(quota_windows) = payload.quota_windows.as_ref() {
        let value = match normalize_provider_quota_windows(Some(quota_windows)) {
            Ok(value) => value,
            Err(message) => {
                return Ok((
                    http::StatusCode::BAD_REQUEST,
                    Json(json!({ "detail": message })),
                )
                    .into_response());
            }
        };
        if value.as_array().is_some_and(|entries| entries.is_empty()) {
            config_map.remove(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY);
        } else {
            config_map.insert(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY.to_string(), value);
        }
    }

    let synced_monthly_used_usd = if quota_start_changed {
        Some(0.0)
    } else {
        existing.monthly_used_usd
    };

    let _ignored_rpm_limit = payload.rpm_limit;
    let updated = existing
        .clone()
        .with_billing_fields(
            Some(billing_type.clone()),
            payload.monthly_quota_usd,
            synced_monthly_used_usd,
            Some(payload.quota_reset_day),
            quota_last_reset_at_unix_secs,
            quota_expires_at_unix_secs,
        )
        .with_routing_fields(payload.provider_priority);
    let mut updated = updated;
    updated.config = (!config_map.is_empty()).then_some(serde_json::Value::Object(config_map));
    let Some(updated) = state
        .app()
        .update_provider_catalog_provider(&updated)
        .await?
    else {
        return Ok(admin_provider_strategy_provider_not_found_response());
    };
    if quota_start_changed {
        state
            .app()
            .clear_provider_quota_window_counters(&provider_id)
            .await?;
    }

    Ok(Json(json!({
        "message": "Provider billing config updated successfully",
        "provider": {
            "id": updated.id,
            "name": updated.name,
            "billing_type": billing_type,
            "provider_priority": updated.provider_priority,
        },
    }))
    .into_response())
}

pub(crate) async fn build_provider_strategy_stats_response(
    state: &AdminAppState<'_>,
    provider_id: String,
    hours: u64,
) -> Result<Response<Body>, GatewayError> {
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let since_unix_secs = now_unix_secs.saturating_sub(hours.saturating_mul(3600));

    let Some(provider) = state
        .app()
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&provider_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(admin_provider_strategy_provider_not_found_response());
    };

    let summary = state
        .app()
        .summarize_provider_usage_since(&provider_id, since_unix_secs)
        .await?;
    let actual_total_cost_usd = state
        .app()
        .summarize_provider_actual_usage_since(&provider_id, since_unix_secs)
        .await?;
    let monthly_used_usd = provider.monthly_used_usd.unwrap_or(0.0);
    let quota_remaining_usd = provider
        .monthly_quota_usd
        .map(|value| value - monthly_used_usd);
    let success_rate = if summary.total_requests > 0 {
        summary.successful_requests as f64 / summary.total_requests as f64
    } else {
        0.0
    };
    let configured_windows = aether_wallet::quota_windows_from_config(provider.config.as_ref());
    let quota_epoch = provider
        .quota_last_reset_at_unix_secs
        .map(aether_wallet::quota_clock_minute);
    let window_requests = quota_epoch
        .map(|quota_epoch_start_unix_secs| {
            configured_windows
                .iter()
                .map(|window| {
                    aether_data_contracts::repository::usage::ProviderQuotaWindowUsageRequest {
                        provider_id: provider_id.clone(),
                        duration_secs: window.duration_secs,
                        quota_epoch_start_unix_secs,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let window_usage = state
        .app()
        .read_provider_quota_window_usage(&window_requests)
        .await?
        .into_iter()
        .map(|usage| (usage.duration_secs, usage))
        .collect::<BTreeMap<_, _>>();
    let quota_windows = configured_windows
        .iter()
        .map(|window| {
            let usage = window_usage.get(&window.duration_secs);
            json!({
                "duration_secs": window.duration_secs,
                "limit_usd": window.limit_usd,
                "used_usd": usage.map(|usage| usage.used_usd),
                "rolling_start": usage
                    .and_then(|usage| unix_secs_to_rfc3339(usage.rolling_start_unix_secs)),
                "accounted_until": usage
                    .and_then(|usage| unix_secs_to_rfc3339(usage.accounted_until_unix_secs)),
                "quota_epoch_start": usage
                    .and_then(|usage| unix_secs_to_rfc3339(usage.quota_epoch_start_unix_secs)),
                "status": usage.map(|usage| usage.status.as_str()).unwrap_or("rebuilding"),
                "rebuild_error": usage.and_then(|usage| usage.rebuild_error.as_deref()),
            })
        })
        .collect::<Vec<_>>();
    let pending_reset = state
        .app()
        .read_provider_quota_snapshot(&provider_id)
        .await?
        .and_then(|quota| quota.pending_quota_reset_at_unix_secs)
        .and_then(unix_secs_to_rfc3339);

    Ok(Json(json!({
        "provider_id": provider_id,
        "provider_name": provider.name,
        "period_hours": hours,
        "billing_info": {
            "billing_type": provider.billing_type,
            "monthly_quota_usd": provider.monthly_quota_usd,
            "monthly_used_usd": monthly_used_usd,
            "quota_remaining_usd": quota_remaining_usd,
            "quota_windows": quota_windows,
            "pending_quota_reset_at": pending_reset,
            "quota_expires_at": provider.quota_expires_at_unix_secs.and_then(unix_secs_to_rfc3339),
        },
        "usage_stats": {
            "total_requests": summary.total_requests,
            "successful_requests": summary.successful_requests,
            "failed_requests": summary.failed_requests,
            "success_rate": success_rate,
            "avg_response_time_ms": (summary.avg_response_time_ms * 100.0).round() / 100.0,
            "total_cost_usd": (summary.total_cost_usd * 10_000.0).round() / 10_000.0,
            "actual_total_cost_usd": (actual_total_cost_usd * 10_000.0).round() / 10_000.0,
        },
    }))
    .into_response())
}

pub(crate) async fn build_provider_strategy_reset_quota_response(
    state: &AdminAppState<'_>,
    provider_id: String,
) -> Result<Response<Body>, GatewayError> {
    let Some(provider) = state
        .app()
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&provider_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(admin_provider_strategy_provider_not_found_response());
    };

    if provider.billing_type.as_deref() != Some("monthly_quota") {
        return Ok((
            http::StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "Only monthly quota providers can be reset" })),
        )
            .into_response());
    }

    let previous_used = provider.monthly_used_usd.unwrap_or(0.0);
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let effective_at = (now_unix_secs / 60 + 1) * 60;
    if !state.app().has_provider_quota_data_writer() {
        return Ok((
            http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "detail": "Provider quota writer is unavailable" })),
        )
            .into_response());
    }
    let requested = state
        .app()
        .request_provider_quota_reset(&provider_id, effective_at)
        .await?;
    if !requested {
        return Ok(admin_provider_strategy_provider_not_found_response());
    }

    Ok(Json(json!({
        "message": "Provider quota reset scheduled",
        "provider_name": provider.name,
        "previous_used": previous_used,
        "current_used": previous_used,
        "pending": true,
        "effective_at": unix_secs_to_rfc3339(effective_at),
        "quota_epoch_start": provider
            .quota_last_reset_at_unix_secs
            .and_then(unix_secs_to_rfc3339),
    }))
    .into_response())
}

fn admin_provider_strategy_provider_not_found_response() -> Response<Body> {
    (
        http::StatusCode::NOT_FOUND,
        Json(json!({ "detail": "Provider not found" })),
    )
        .into_response()
}
