use super::reads::{
    attach_admin_provider_pool_model_cooldowns, read_admin_provider_pool_runtime_state,
};
use crate::handlers::admin::provider::pool::config::admin_provider_pool_config;
use crate::handlers::admin::request::AdminAppState;
use serde_json::json;

pub(crate) async fn build_admin_provider_pool_status_payload(
    state: &AdminAppState<'_>,
    provider_id: &str,
) -> Option<serde_json::Value> {
    if !state.has_provider_catalog_data_reader() {
        return None;
    }

    let provider = state
        .read_provider_catalog_providers_by_ids(&[provider_id.to_string()])
        .await
        .ok()
        .and_then(|mut providers| providers.drain(..).next())?;
    let Some(pool_config) = admin_provider_pool_config(&provider) else {
        return Some(json!({
            "provider_id": provider.id,
            "provider_name": provider.name,
            "pool_enabled": false,
            "total_keys": 0,
            "total_sticky_sessions": 0,
            "provider_hot_count": 0,
            "provider_desired_hot": 0,
            "provider_in_flight": 0,
            "provider_ema_in_flight": 0.0,
            "provider_burst_pending": false,
            "keys": [],
        }));
    };

    let keys = state
        .list_provider_catalog_keys_by_provider_ids(std::slice::from_ref(&provider.id))
        .await
        .ok()
        .unwrap_or_default();
    let key_ids = keys.iter().map(|key| key.id.clone()).collect::<Vec<_>>();
    let now_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut runtime = read_admin_provider_pool_runtime_state(
        state.runtime_state(),
        &provider.id,
        &key_ids,
        &pool_config,
        None,
    )
    .await;
    attach_admin_provider_pool_model_cooldowns(
        state.runtime_state(),
        &provider.id,
        &key_ids,
        &mut runtime,
    )
    .await;
    let key_payloads = keys
        .into_iter()
        .map(|key| {
            let cooldown_reason = runtime.cooldown_reason_by_key.get(&key.id).cloned();
            let cooldown_ttl_seconds = cooldown_reason
                .as_ref()
                .and_then(|_| runtime.cooldown_ttl_by_key.get(&key.id).copied());
            json!({
                "key_id": key.id,
                "key_name": key.name,
                "is_active": key.is_active,
                "cooldown_reason": cooldown_reason,
                "cooldown_ttl_seconds": cooldown_ttl_seconds,
                "cooldown_until": cooldown_ttl_seconds.map(|ttl| now_unix_secs.saturating_add(ttl)),
                "cooldown_meta": runtime.cooldown_meta_by_key.get(&key.id).cloned(),
                "model_cooldowns": runtime
                    .model_cooldowns_by_key
                    .get(&key.id)
                    .map(|cooldowns| cooldowns
                        .iter()
                        .map(|cooldown| json!({
                            "model": cooldown.model,
                            "reason": cooldown.reason,
                            "ttl_seconds": cooldown.ttl_seconds,
                            "until": now_unix_secs.saturating_add(cooldown.ttl_seconds),
                            "meta": cooldown.meta,
                        }))
                        .collect::<Vec<_>>())
                    .unwrap_or_default(),
                "cost_window_usage": runtime.cost_window_usage_by_key.get(&key.id).copied().unwrap_or(0),
                "cost_limit": pool_config.cost_limit_per_key_tokens,
                "sticky_sessions": runtime.sticky_sessions_by_key.get(&key.id).copied().unwrap_or(0),
                "latency_avg_ms": runtime.latency_avg_ms_by_key.get(&key.id).copied(),
                "lru_score": runtime.lru_score_by_key.get(&key.id).copied(),
            })
        })
        .collect::<Vec<_>>();

    Some(json!({
        "provider_id": provider.id,
        "provider_name": provider.name,
        "pool_enabled": true,
        "total_keys": key_payloads.len(),
        "total_sticky_sessions": runtime.total_sticky_sessions,
        "provider_hot_count": runtime.active_probe_member_ids.len(),
        "provider_desired_hot": runtime.provider_desired_hot,
        "provider_in_flight": runtime.provider_in_flight,
        "provider_ema_in_flight": runtime.provider_ema_in_flight,
        "provider_burst_pending": runtime.provider_burst_pending,
        "keys": key_payloads,
    }))
}
