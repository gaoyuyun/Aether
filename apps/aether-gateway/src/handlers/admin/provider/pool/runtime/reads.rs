use super::keys::{
    parse_pool_cost_member, parse_pool_latency_member, pool_cooldown_index_key, pool_cooldown_key,
    pool_cooldown_keys, pool_cooldown_meta_keys, pool_cost_keys, pool_latency_keys, pool_lru_key,
    pool_model_cooldown_index_key, pool_model_cooldown_key, pool_model_cooldown_meta_key,
    pool_sticky_key, pool_sticky_pattern,
};
use crate::handlers::admin::provider::pool::config::admin_provider_pool_cache_affinity_enabled;
use crate::handlers::admin::provider::shared::support::{
    admin_provider_pool_quota_probe_active_members_key, AdminProviderPoolConfig,
    AdminProviderPoolModelCooldown, AdminProviderPoolRuntimeState,
};
use crate::maintenance::PoolQuotaProbeWorkerConfig;
use crate::provider_pool_demand::{
    provider_pool_burst_pending, read_provider_pool_demand_snapshot,
};
use aether_runtime_state::{DataLayerError, RuntimeState};
use futures_util::future::join_all;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const DEFAULT_POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT: usize = 512;
const MAX_POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT: usize = 10_000;
const POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT_ENV: &str =
    "AETHER_GATEWAY_ADMIN_POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT";

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn should_load_active_probe_members(pool_config: &AdminProviderPoolConfig) -> bool {
    pool_config.probing_enabled
}

fn pool_runtime_window_metric_key_limit() -> usize {
    std::env::var(POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT)
        .clamp(1, MAX_POOL_RUNTIME_WINDOW_METRIC_KEY_LIMIT)
}

fn bounded_runtime_window_metric_key_ids(key_ids: &[String], limit: usize) -> &[String] {
    let end = key_ids.len().min(limit.max(1));
    &key_ids[..end]
}

pub(crate) async fn read_admin_provider_pool_cooldown_counts(
    runtime: &RuntimeState,
    provider_ids: &[String],
) -> BTreeMap<String, usize> {
    join_all(provider_ids.iter().map(|provider_id| async move {
        let count = runtime
            .set_len(&pool_cooldown_index_key(provider_id))
            .await
            .unwrap_or(0);
        (provider_id.clone(), count)
    }))
    .await
    .into_iter()
    .collect()
}

pub(crate) async fn read_admin_provider_pool_runtime_state(
    runtime: &RuntimeState,
    provider_id: &str,
    key_ids: &[String],
    pool_config: &AdminProviderPoolConfig,
    sticky_session_token: Option<&str>,
) -> AdminProviderPoolRuntimeState {
    let mut state = AdminProviderPoolRuntimeState::default();
    let cooldown_keys = pool_cooldown_keys(provider_id, key_ids);
    let metric_key_limit = pool_runtime_window_metric_key_limit();
    let metric_key_ids = bounded_runtime_window_metric_key_ids(key_ids, metric_key_limit);
    if metric_key_ids.len() < key_ids.len() {
        info!(
            event_name = "admin_pool_runtime_window_metrics_truncated",
            log_type = "event",
            provider_id,
            total_key_count = key_ids.len(),
            scanned_key_count = metric_key_ids.len(),
            metric_key_limit,
            "gateway limited admin pool runtime cost/latency window reads"
        );
    }
    let cost_keys = pool_cost_keys(provider_id, metric_key_ids);
    let latency_keys = pool_latency_keys(provider_id, metric_key_ids);
    let sticky_sessions_enabled = pool_config.sticky_session_ttl_seconds > 0
        && admin_provider_pool_cache_affinity_enabled(pool_config);

    if let Some(sticky_session_token) = sticky_session_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| sticky_sessions_enabled)
    {
        let sticky_key = pool_sticky_key(provider_id, sticky_session_token);
        if let Ok(Some(bound_key_id)) = runtime.kv_get(&sticky_key).await {
            let cooldown_key = pool_cooldown_key(provider_id, &bound_key_id);
            match runtime.kv_exists(&cooldown_key).await {
                Ok(false) => {
                    let _ = runtime
                        .key_expire(
                            &sticky_key,
                            std::time::Duration::from_secs(pool_config.sticky_session_ttl_seconds),
                        )
                        .await;
                    state.sticky_bound_key_id = Some(bound_key_id);
                }
                Ok(true) => {
                    let _ = runtime.kv_delete(&sticky_key).await;
                }
                Err(err) => {
                    warn!(
                        "gateway admin provider pool: failed to validate sticky cooldown for provider {provider_id}: {:?}",
                        err
                    );
                    state.sticky_bound_key_id = Some(bound_key_id);
                }
            }
        }
    }

    if sticky_sessions_enabled {
        let sticky_keys = runtime
            .scan_keys(&pool_sticky_pattern(provider_id), 200)
            .await
            .unwrap_or_default();
        state.total_sticky_sessions = sticky_keys.len();
        if !sticky_keys.is_empty() {
            let raw_keys = sticky_keys
                .iter()
                .map(|key| runtime.strip_namespace(key).to_string())
                .collect::<Vec<_>>();
            if let Ok(values) = runtime.kv_get_many(&raw_keys).await {
                for bound_key_id in values.into_iter().flatten() {
                    *state
                        .sticky_sessions_by_key
                        .entry(bound_key_id)
                        .or_insert(0) += 1;
                }
            }
        }
    }

    if should_load_active_probe_members(pool_config) {
        state.active_probe_member_ids = runtime
            .set_members(&admin_provider_pool_quota_probe_active_members_key(
                provider_id,
            ))
            .await
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
    }

    let probe_config = PoolQuotaProbeWorkerConfig::from_env();
    let demand_snapshot = read_provider_pool_demand_snapshot(
        runtime,
        provider_id,
        key_ids.len(),
        probe_config.max_keys_per_provider,
    )
    .await;
    state.provider_in_flight = demand_snapshot.in_flight;
    state.provider_ema_in_flight = demand_snapshot.ema_in_flight;
    state.provider_desired_hot = if pool_config.probing_enabled {
        demand_snapshot.desired_hot
    } else {
        0
    };
    state.provider_burst_pending =
        pool_config.probing_enabled && provider_pool_burst_pending(runtime, provider_id).await;

    if !cooldown_keys.is_empty() {
        let cooldown_reasons = runtime
            .kv_get_many(&cooldown_keys)
            .await
            .unwrap_or_else(|_| vec![None; cooldown_keys.len()]);
        let meta_keys = pool_cooldown_meta_keys(provider_id, key_ids);
        let cooldown_metas = runtime
            .kv_get_many(&meta_keys)
            .await
            .unwrap_or_else(|_| vec![None; meta_keys.len()]);
        for (key_id, (cooldown_key, (reason, meta))) in key_ids.iter().zip(
            cooldown_keys
                .iter()
                .zip(cooldown_reasons.into_iter().zip(cooldown_metas)),
        ) {
            if let Some(reason) = reason {
                state.cooldown_reason_by_key.insert(key_id.clone(), reason);
                if let Ok(Some(ttl)) = runtime.kv_ttl_seconds(cooldown_key).await {
                    if let Ok(ttl_seconds) = u64::try_from(ttl) {
                        if ttl_seconds > 0 {
                            state
                                .cooldown_ttl_by_key
                                .insert(key_id.clone(), ttl_seconds);
                        }
                    }
                }
                if let Some(meta) = meta.and_then(|raw| serde_json::from_str(&raw).ok()) {
                    state.cooldown_meta_by_key.insert(key_id.clone(), meta);
                }
            }
        }
    }

    let now = current_unix_secs();
    let cost_window_start = now.saturating_sub(pool_config.cost_window_seconds) as f64;
    let cost_results = join_all(
        cost_keys
            .iter()
            .map(|cost_key| runtime.score_range_by_min(cost_key, cost_window_start)),
    )
    .await;
    for (key_id, members) in metric_key_ids.iter().zip(cost_results) {
        let total = members
            .unwrap_or_default()
            .iter()
            .map(|member| parse_pool_cost_member(member))
            .sum::<u64>();
        if total > 0 {
            state.cost_window_usage_by_key.insert(key_id.clone(), total);
        }
    }

    let latency_window_start = now.saturating_sub(pool_config.latency_window_seconds) as f64;
    let latency_results = join_all(
        latency_keys
            .iter()
            .map(|latency_key| runtime.score_range_by_min(latency_key, latency_window_start)),
    )
    .await;
    for (key_id, members) in metric_key_ids.iter().zip(latency_results) {
        let samples = members
            .unwrap_or_default()
            .iter()
            .map(|member| parse_pool_latency_member(member))
            .filter(|value| *value > 0)
            .collect::<Vec<_>>();
        if samples.is_empty() {
            continue;
        }
        let total = samples.iter().sum::<u64>() as f64;
        let average = total / samples.len() as f64;
        if average.is_finite() && average >= 0.0 {
            state.latency_avg_ms_by_key.insert(key_id.clone(), average);
        }
    }

    if (pool_config.lru_enabled
        || pool_config
            .scheduling_presets
            .iter()
            .any(|item| item.enabled))
        && !key_ids.is_empty()
    {
        if let Ok(scores) = runtime
            .score_many(&pool_lru_key(provider_id), key_ids)
            .await
        {
            for (key_id, score) in key_ids.iter().zip(scores) {
                if let Some(score) = score {
                    state.lru_score_by_key.insert(key_id.clone(), score);
                }
            }
        }
    }

    state
}

/// 管理端展示用：把每把 Key 的模型级冷却列表补进运行时状态。调度热路径不要调它——
/// 每把 Key 至少一次 `set_members`，命中时再逐模型读 KV，Redis 下是成倍的往返；
/// 真正挡请求的是 `read_admin_provider_pool_key_model_cooldown_reason` 的单次点查。
pub(crate) async fn attach_admin_provider_pool_model_cooldowns(
    runtime: &RuntimeState,
    provider_id: &str,
    key_ids: &[String],
    state: &mut AdminProviderPoolRuntimeState,
) {
    let model_cooldowns = join_all(key_ids.iter().map(|key_id| async move {
        (
            key_id.clone(),
            read_admin_provider_pool_key_model_cooldowns(runtime, provider_id, key_id).await,
        )
    }))
    .await;
    for (key_id, cooldowns) in model_cooldowns {
        if !cooldowns.is_empty() {
            state.model_cooldowns_by_key.insert(key_id, cooldowns);
        }
    }
}

pub(crate) async fn read_admin_provider_pool_cooldown_count(
    runtime: &RuntimeState,
    provider_id: &str,
) -> usize {
    runtime
        .set_len(&pool_cooldown_index_key(provider_id))
        .await
        .unwrap_or(0)
}

pub(crate) async fn read_admin_provider_pool_cooldown_key_ids(
    runtime: &RuntimeState,
    provider_id: &str,
) -> Vec<String> {
    runtime
        .set_members(&pool_cooldown_index_key(provider_id))
        .await
        .unwrap_or_default()
}

pub(crate) async fn read_admin_provider_pool_key_cooldown_reason(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
) -> Result<Option<String>, DataLayerError> {
    runtime
        .kv_get(&pool_cooldown_key(provider_id, key_id))
        .await
}

/// 这把 Key 上某个模型的冷却原因；模型级冷却没开或没命中时为 `None`。
pub(crate) async fn read_admin_provider_pool_key_model_cooldown_reason(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    model: &str,
) -> Result<Option<String>, DataLayerError> {
    if model.trim().is_empty() {
        return Ok(None);
    }
    runtime
        .kv_get(&pool_model_cooldown_key(provider_id, key_id, model))
        .await
}

/// 这把 Key 当前所有仍在生效的模型级冷却，供管理端展示。
pub(crate) async fn read_admin_provider_pool_key_model_cooldowns(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
) -> Vec<AdminProviderPoolModelCooldown> {
    let models = runtime
        .set_members(&pool_model_cooldown_index_key(provider_id, key_id))
        .await
        .unwrap_or_default();
    let mut cooldowns = Vec::new();
    for model in models {
        let cooldown_key = pool_model_cooldown_key(provider_id, key_id, &model);
        let Ok(Some(reason)) = runtime.kv_get(&cooldown_key).await else {
            // 冷却已过期：顺手把索引里的模型名清掉。
            let _ = runtime
                .set_remove(&pool_model_cooldown_index_key(provider_id, key_id), &model)
                .await;
            continue;
        };
        let ttl_seconds = match runtime.kv_ttl_seconds(&cooldown_key).await {
            Ok(Some(ttl)) if ttl > 0 => u64::try_from(ttl).unwrap_or(0),
            _ => 0,
        };
        let meta = runtime
            .kv_get(&pool_model_cooldown_meta_key(provider_id, key_id, &model))
            .await
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        cooldowns.push(AdminProviderPoolModelCooldown {
            model,
            reason,
            ttl_seconds,
            meta,
        });
    }
    cooldowns.sort_by(|left, right| left.model.cmp(&right.model));
    cooldowns
}

#[cfg(test)]
mod tests {
    use super::bounded_runtime_window_metric_key_ids;

    #[test]
    fn runtime_window_metric_key_ids_are_bounded() {
        let key_ids = vec![
            "key-1".to_string(),
            "key-2".to_string(),
            "key-3".to_string(),
        ];

        let bounded = bounded_runtime_window_metric_key_ids(&key_ids, 2);

        assert_eq!(bounded, &key_ids[..2]);
    }

    #[test]
    fn runtime_window_metric_key_ids_keep_at_least_one_key() {
        let key_ids = vec!["key-1".to_string(), "key-2".to_string()];

        let bounded = bounded_runtime_window_metric_key_ids(&key_ids, 0);

        assert_eq!(bounded, &key_ids[..1]);
    }
}
