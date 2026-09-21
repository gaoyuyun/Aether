use super::keys::{
    pool_cooldown_backoff_key, pool_cooldown_index_key, pool_cooldown_key, pool_cooldown_meta_key,
    pool_cost_key, pool_latency_key, pool_lru_key, pool_model_cooldown_index_key,
    pool_model_cooldown_key, pool_model_cooldown_meta_key, pool_sticky_key,
    pool_stream_timeout_key,
};
use crate::handlers::admin::provider::pool::config::admin_provider_pool_cache_affinity_enabled;
use crate::handlers::admin::provider::pool::cooldown::{
    decide_provider_cooldown, CooldownAction, CooldownDecision, CooldownDecisionInput,
    ProviderCooldownConfig, RATE_LIMIT_BACKOFF_LEVEL_MEMORY_SECONDS,
};
use crate::handlers::admin::provider::shared::support::{
    admin_provider_pool_quota_probe_active_members_key, AdminProviderPoolConfig,
    AdminProviderPoolUnschedulableRule,
};
use crate::provider_transport::RetryHintScope;
use aether_runtime_state::RuntimeState;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;
use uuid::Uuid;

/// 池冷却 KV 的硬上限。上游明确给出的更长等待时长走 quota 元数据里的 `reset_at`，
/// 由调度层按重置时刻过期；KV 只负责「短期不要再试」。
pub(crate) const MAX_POOL_COOLDOWN_SECONDS: u64 = 32 * 60;

const ACCOUNT_DISABLE_PATTERNS: &[&str] = &[
    "organization has been disabled",
    "organization disabled",
    "organization_disabled",
    "account has been disabled",
    "account disabled",
    "account_disabled",
    "account has been deactivated",
    "account_deactivated",
    "account deactivated",
];

const WORKSPACE_DISABLE_PATTERNS: &[&str] = &[
    "deactivated_workspace",
    "workspace has been disabled",
    "workspace disabled",
    "workspace has been deactivated",
    "workspace deactivated",
    "workspace is disabled",
    "workspace is deactivated",
];

const FORBIDDEN_ACCOUNT_PATTERNS: &[&str] = &[
    "account suspended",
    "account suspend",
    "account banned",
    "account blocked",
    "account forbidden",
    "account deactivated",
    "account access denied",
    "subscription inactive",
    "suspended",
    "banned",
    "blocked",
    "deactivated",
    "access denied",
];

fn current_unix_secs_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn enabled_pool_presets(pool_config: &AdminProviderPoolConfig) -> impl Iterator<Item = &str> {
    pool_config
        .scheduling_presets
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.preset.as_str())
}

fn should_touch_lru(pool_config: &AdminProviderPoolConfig) -> bool {
    pool_config.lru_enabled || enabled_pool_presets(pool_config).next().is_some()
}

fn should_record_latency(pool_config: &AdminProviderPoolConfig) -> bool {
    enabled_pool_presets(pool_config).any(|preset| !preset.eq_ignore_ascii_case("lru"))
}

fn oauth_cache_key(key_id: &str) -> String {
    format!("provider_oauth_token_cache:{key_id}")
}

fn extract_error_message(error_body: Option<&str>) -> String {
    let Some(error_body) = error_body.map(str::trim).filter(|value| !value.is_empty()) else {
        return String::new();
    };

    serde_json::from_str::<serde_json::Value>(error_body)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .and_then(|object| object.get("error").or_else(|| object.get("message")))
                .and_then(|error| match error {
                    serde_json::Value::Object(object) => {
                        first_error_text(object, &["message", "detail", "reason", "code", "status"])
                    }
                    serde_json::Value::String(text) => Some(text.clone()),
                    _ => None,
                })
        })
        .unwrap_or_else(|| error_body.chars().take(500).collect())
}

fn first_error_text(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        let text = object.get(*key).and_then(|value| match value {
            serde_json::Value::String(text) => Some(text.trim().to_string()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        })?;
        (!text.is_empty()).then_some(text)
    })
}

pub(crate) fn admin_provider_pool_key_terminal_error_reason(
    status_code: u16,
    error_body: Option<&str>,
) -> Option<String> {
    let error_message = extract_error_message(error_body).to_ascii_lowercase();
    if let Some(pattern) = WORKSPACE_DISABLE_PATTERNS
        .iter()
        .find(|pattern| error_message.contains(**pattern))
    {
        return Some(format!("workspace_deactivated_{status_code}:{pattern}"));
    }

    match status_code {
        401 if ACCOUNT_DISABLE_PATTERNS
            .iter()
            .any(|pattern| error_message.contains(pattern)) =>
        {
            Some("account_deactivated_401".to_string())
        }
        402 => Some("payment_required_402".to_string()),
        403 if FORBIDDEN_ACCOUNT_PATTERNS
            .iter()
            .any(|pattern| error_message.contains(pattern)) =>
        {
            Some("forbidden_403".to_string())
        }
        400 => ACCOUNT_DISABLE_PATTERNS
            .iter()
            .find(|pattern| error_message.contains(**pattern))
            .map(|pattern| format!("account_disabled_400:{pattern}")),
        423 if FORBIDDEN_ACCOUNT_PATTERNS
            .iter()
            .any(|pattern| error_message.contains(pattern)) =>
        {
            Some("account_locked_423".to_string())
        }
        _ => None,
    }
}

/// 仍在生效的冷却剩余秒数；不存在或已过期返回 `None`。
async fn active_pool_cooldown_ttl_seconds(
    runtime: &RuntimeState,
    cooldown_key: &str,
) -> Option<u64> {
    match runtime.kv_ttl_seconds(cooldown_key).await {
        Ok(Some(ttl)) if ttl > 0 => u64::try_from(ttl).ok(),
        _ => None,
    }
}

/// 写 Key 级冷却。后来的失败只延长仍在生效的冷却，不缩短：已有更长的 TTL 时保留
/// 原值与原因，只刷新元数据里的最近一次决策。
async fn set_pool_cooldown(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    reason: &str,
    ttl_seconds: u64,
) {
    set_pool_cooldown_with_meta(runtime, provider_id, key_id, reason, ttl_seconds, None).await;
}

async fn set_pool_cooldown_with_meta(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    reason: &str,
    ttl_seconds: u64,
    meta: Option<serde_json::Value>,
) {
    if ttl_seconds == 0 {
        return;
    }
    let requested_ttl_seconds = ttl_seconds.min(MAX_POOL_COOLDOWN_SECONDS);
    let cooldown_key = pool_cooldown_key(provider_id, key_id);
    let active_ttl_seconds = active_pool_cooldown_ttl_seconds(runtime, &cooldown_key).await;
    let extend_only_kept_existing =
        active_ttl_seconds.is_some_and(|active| active > requested_ttl_seconds);
    let effective_ttl_seconds = active_ttl_seconds.map_or(requested_ttl_seconds, |active| {
        active.max(requested_ttl_seconds)
    });

    if !extend_only_kept_existing {
        if let Err(err) = runtime
            .kv_set(
                &cooldown_key,
                reason.to_string(),
                Some(std::time::Duration::from_secs(effective_ttl_seconds)),
            )
            .await
        {
            warn!(
                "gateway admin provider pool: failed to set cooldown for provider {provider_id} key {key_id}: {:?}",
                err
            );
        }
    }
    let meta_key = pool_cooldown_meta_key(provider_id, key_id);
    match meta {
        Some(mut meta) => {
            if let Some(object) = meta.as_object_mut() {
                object.insert(
                    "effective_ttl_seconds".to_string(),
                    serde_json::json!(effective_ttl_seconds),
                );
                object.insert(
                    "extended_existing".to_string(),
                    serde_json::json!(extend_only_kept_existing),
                );
            }
            let _ = runtime
                .kv_set(
                    &meta_key,
                    meta.to_string(),
                    Some(std::time::Duration::from_secs(effective_ttl_seconds)),
                )
                .await;
        }
        None => {
            // 没有决策元数据的老路径（流超时、不可调度规则）：清掉过期的解释，避免误导。
            if !extend_only_kept_existing {
                let _ = runtime.kv_delete(&meta_key).await;
            }
        }
    }
    let _ = runtime
        .set_add(&pool_cooldown_index_key(provider_id), key_id)
        .await;
    extend_pool_index_expiry(
        runtime,
        &pool_cooldown_index_key(provider_id),
        effective_ttl_seconds,
    )
    .await;
    spawn_remove_pool_active_probe_member(runtime, provider_id, key_id);
}

/// 冷却索引集合的过期只往后推：索引里可能还有别的成员在更长的冷却里，
/// 一次较短的冷却不能把整个索引提前过期（否则管理端列表会漏掉仍在冷却的 Key）。
async fn extend_pool_index_expiry(runtime: &RuntimeState, index_key: &str, ttl_seconds: u64) {
    let requested_seconds = ttl_seconds.saturating_add(60);
    let current_seconds = match runtime.key_ttl_seconds(index_key).await {
        Ok(Some(ttl)) if ttl > 0 => u64::try_from(ttl).unwrap_or(0),
        _ => 0,
    };
    if current_seconds >= requested_seconds {
        return;
    }
    let _ = runtime
        .key_expire(index_key, std::time::Duration::from_secs(requested_seconds))
        .await;
}

/// 写 Key+模型 级冷却：只影响这把 Key 上的这个模型，其他模型继续可调度。
#[allow(clippy::too_many_arguments)]
async fn set_pool_model_cooldown(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    model: &str,
    reason: &str,
    ttl_seconds: u64,
    meta: serde_json::Value,
) {
    if ttl_seconds == 0 || model.trim().is_empty() {
        return;
    }
    let requested_ttl_seconds = ttl_seconds.min(MAX_POOL_COOLDOWN_SECONDS);
    let cooldown_key = pool_model_cooldown_key(provider_id, key_id, model);
    let active_ttl_seconds = active_pool_cooldown_ttl_seconds(runtime, &cooldown_key).await;
    let effective_ttl_seconds = active_ttl_seconds.map_or(requested_ttl_seconds, |active| {
        active.max(requested_ttl_seconds)
    });
    if !active_ttl_seconds.is_some_and(|active| active > requested_ttl_seconds) {
        if let Err(err) = runtime
            .kv_set(
                &cooldown_key,
                reason.to_string(),
                Some(std::time::Duration::from_secs(effective_ttl_seconds)),
            )
            .await
        {
            warn!(
                "gateway admin provider pool: failed to set model cooldown for provider {provider_id} key {key_id}: {:?}",
                err
            );
        }
    }
    let mut meta = meta;
    if let Some(object) = meta.as_object_mut() {
        object.insert("model".to_string(), serde_json::json!(model));
        object.insert(
            "effective_ttl_seconds".to_string(),
            serde_json::json!(effective_ttl_seconds),
        );
    }
    let _ = runtime
        .kv_set(
            &pool_model_cooldown_meta_key(provider_id, key_id, model),
            meta.to_string(),
            Some(std::time::Duration::from_secs(effective_ttl_seconds)),
        )
        .await;
    let index_key = pool_model_cooldown_index_key(provider_id, key_id);
    let _ = runtime.set_add(&index_key, model.trim()).await;
    extend_pool_index_expiry(runtime, &index_key, effective_ttl_seconds).await;
}

async fn read_pool_cooldown_backoff_level(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
) -> Option<u32> {
    runtime
        .kv_get(&pool_cooldown_backoff_key(provider_id, key_id))
        .await
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
}

async fn write_pool_cooldown_backoff_level(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    level: u32,
    cooldown_ttl_seconds: u64,
) {
    let _ = runtime
        .kv_set(
            &pool_cooldown_backoff_key(provider_id, key_id),
            level.to_string(),
            Some(std::time::Duration::from_secs(
                cooldown_ttl_seconds.saturating_add(RATE_LIMIT_BACKOFF_LEVEL_MEMORY_SECONDS),
            )),
        )
        .await;
}

/// 请求成功后清掉退避等级：下一次无提示 429 从 30s 重新开始。
async fn clear_pool_cooldown_backoff_level(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
) {
    let _ = runtime
        .kv_delete(&pool_cooldown_backoff_key(provider_id, key_id))
        .await;
}

fn spawn_remove_pool_active_probe_member(runtime: &RuntimeState, provider_id: &str, key_id: &str) {
    let runtime = runtime.clone();
    let provider_id = provider_id.to_string();
    let key_id = key_id.to_string();
    tokio::spawn(async move {
        if let Err(err) = runtime
            .set_remove(
                &admin_provider_pool_quota_probe_active_members_key(&provider_id),
                &key_id,
            )
            .await
        {
            warn!(
                "gateway admin provider pool: failed to remove active probe member for provider {provider_id} key {key_id}: {:?}",
                err
            );
        }
    });
}

async fn invalidate_pool_oauth_cache(runtime: &RuntimeState, key_id: &str) {
    if let Err(err) = runtime.kv_delete(&oauth_cache_key(key_id)).await {
        warn!(
            "gateway admin provider pool: failed to invalidate oauth cache for key {key_id}: {:?}",
            err
        );
    }
}

fn matching_unschedulable_rule<'a>(
    rules: &'a [AdminProviderPoolUnschedulableRule],
    error_message: &str,
) -> Option<&'a AdminProviderPoolUnschedulableRule> {
    rules.iter().find(|rule| {
        let keyword = rule.keyword.trim().to_ascii_lowercase();
        !keyword.is_empty() && error_message.contains(keyword.as_str())
    })
}

pub(crate) async fn record_admin_provider_pool_success(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    pool_config: &AdminProviderPoolConfig,
    sticky_session_token: Option<&str>,
    tokens_used: u64,
    ttfb_ms: Option<u64>,
) {
    let now = current_unix_secs_f64();
    clear_pool_cooldown_backoff_level(runtime, provider_id, key_id).await;

    if let Some(sticky_session_token) = sticky_session_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| pool_config.sticky_session_ttl_seconds > 0)
        .filter(|_| admin_provider_pool_cache_affinity_enabled(pool_config))
    {
        let _ = runtime
            .kv_set(
                &pool_sticky_key(provider_id, sticky_session_token),
                key_id.to_string(),
                Some(std::time::Duration::from_secs(
                    pool_config.sticky_session_ttl_seconds,
                )),
            )
            .await;
    }

    if should_touch_lru(pool_config) {
        let _ = runtime
            .score_set(&pool_lru_key(provider_id), key_id, now)
            .await;
    }

    if tokens_used > 0 && pool_config.cost_limit_per_key_tokens.is_some() {
        let cost_key = pool_cost_key(provider_id, key_id);
        let window_seconds = pool_config.cost_window_seconds.max(1);
        let member = format!("{}:{tokens_used}", Uuid::new_v4().simple());
        let _ = runtime.score_set(&cost_key, &member, now).await;
        let _ = runtime
            .score_remove_by_score(&cost_key, now - window_seconds as f64)
            .await;
        let _ = runtime
            .key_expire(
                &cost_key,
                std::time::Duration::from_secs(window_seconds.saturating_add(600)),
            )
            .await;
    }

    if let Some(ttfb_ms) = ttfb_ms
        .filter(|value| should_record_latency(pool_config))
        .filter(|_| pool_config.latency_window_seconds > 0)
    {
        let latency_key = pool_latency_key(provider_id, key_id);
        let window_seconds = pool_config.latency_window_seconds.max(1);
        let sample_limit = pool_config.latency_sample_limit.max(1);
        let member = format!("{}:{ttfb_ms}", Uuid::new_v4().simple());
        let _ = runtime.score_set(&latency_key, &member, now).await;
        let _ = runtime
            .score_remove_by_score(&latency_key, now - window_seconds as f64)
            .await;
        let _ = runtime
            .score_remove_by_rank(&latency_key, 0, -((sample_limit as i64) + 1))
            .await;
        let _ = runtime
            .key_expire(
                &latency_key,
                std::time::Duration::from_secs(window_seconds.saturating_add(600)),
            )
            .await;
    }
}

/// 供应商/Key 冷却策略与请求模型；调用方从传输快照解析后传入。
#[derive(Debug, Clone, Default)]
pub(crate) struct AdminProviderPoolErrorContext {
    pub(crate) provider_type: String,
    pub(crate) cooldown: ProviderCooldownConfig,
    /// 本次请求的上游模型名；模型级冷却与 KeyModel 作用域提示按它落键。
    pub(crate) provider_model_name: Option<String>,
}

pub(crate) async fn record_admin_provider_pool_error(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    pool_config: &AdminProviderPoolConfig,
    status_code: u16,
    error_body: Option<&str>,
    response_headers: Option<&BTreeMap<String, String>>,
) -> Option<CooldownDecision> {
    record_admin_provider_pool_error_with_context(
        runtime,
        provider_id,
        key_id,
        pool_config,
        status_code,
        error_body,
        response_headers,
        &AdminProviderPoolErrorContext::default(),
    )
    .await
}

/// 记录一次上游失败对池 Key 的影响，返回本次的冷却决策（调用方据此决定是否同 Key
/// 立即重试、是否把 `reset_at` 写入 quota 元数据）。`None` 表示这次失败没有产生决策
/// （401/402/403 账号级终态、400 客户端错误、不可调度规则命中）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_admin_provider_pool_error_with_context(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    pool_config: &AdminProviderPoolConfig,
    status_code: u16,
    error_body: Option<&str>,
    response_headers: Option<&BTreeMap<String, String>>,
    context: &AdminProviderPoolErrorContext,
) -> Option<CooldownDecision> {
    let error_message = extract_error_message(error_body).to_ascii_lowercase();

    if status_code == 401 {
        invalidate_pool_oauth_cache(runtime, key_id).await;
        spawn_remove_pool_active_probe_member(runtime, provider_id, key_id);
        return None;
    }

    if status_code == 402 {
        spawn_remove_pool_active_probe_member(runtime, provider_id, key_id);
        return None;
    }

    if status_code == 403
        && FORBIDDEN_ACCOUNT_PATTERNS
            .iter()
            .any(|pattern| error_message.contains(pattern))
    {
        spawn_remove_pool_active_probe_member(runtime, provider_id, key_id);
        return None;
    }

    if status_code == 400 {
        // Bad Request is usually attributable to the caller payload, not key health.
        // Account-level 400s are handled by orchestration pool-score feedback.
        return None;
    }

    if let Some(rule) =
        matching_unschedulable_rule(&pool_config.unschedulable_rules, &error_message)
    {
        let ttl_seconds = (rule.duration_minutes.max(1)).saturating_mul(60).max(60);
        set_pool_cooldown(
            runtime,
            provider_id,
            key_id,
            &format!("rule:{}", rule.keyword),
            ttl_seconds,
        )
        .await;
        return None;
    }

    let now_unix_secs = current_unix_secs();
    let model_level = context.cooldown.model_level
        && context
            .provider_model_name
            .as_deref()
            .is_some_and(|model| !model.trim().is_empty());
    let key_cooldown_key = pool_cooldown_key(provider_id, key_id);
    let active_cooldown_ttl_seconds = if model_level {
        let model = context.provider_model_name.as_deref().unwrap_or_default();
        active_pool_cooldown_ttl_seconds(
            runtime,
            &pool_model_cooldown_key(provider_id, key_id, model),
        )
        .await
    } else {
        active_pool_cooldown_ttl_seconds(runtime, &key_cooldown_key).await
    };
    let previous_backoff_level = if status_code == 429 {
        read_pool_cooldown_backoff_level(runtime, provider_id, key_id).await
    } else {
        None
    };
    let mut config = context.cooldown;
    if !matches!(status_code, 429 | 529) && pool_config.overload_cooldown_seconds == 0 {
        // 号池「529 冷却 = 0」历史上也关闭了其他瞬时错误的冷却；保持这个语义。
        config.transient_error_seconds = 0;
    }
    let decision = decide_provider_cooldown(CooldownDecisionInput {
        provider_type: context.provider_type.as_str(),
        status_code,
        headers: response_headers,
        error_body,
        now_unix_secs,
        config,
        rate_limit_cooldown_enabled: pool_config.rate_limit_cooldown_seconds > 0,
        overload_cooldown_seconds: pool_config.overload_cooldown_seconds,
        previous_backoff_level,
        active_cooldown_ttl_seconds,
    });

    if !decision.writes_cooldown() {
        if decision.action == CooldownAction::ImmediateRetry {
            // 立即重试不冷却，但要让探针集合知道这把 Key 刚刚失败过。
            spawn_remove_pool_active_probe_member(runtime, provider_id, key_id);
        }
        return Some(decision);
    }

    let reason = pool_cooldown_reason_for_decision(status_code, &decision, error_body);
    let meta = decision.to_meta_json(now_unix_secs);
    let scoped_to_model = model_level || decision.scope == RetryHintScope::KeyModel;
    match context
        .provider_model_name
        .as_deref()
        .filter(|_| scoped_to_model)
    {
        Some(model) => {
            set_pool_model_cooldown(
                runtime,
                provider_id,
                key_id,
                model,
                &reason,
                decision.ttl_seconds,
                meta,
            )
            .await;
        }
        None => {
            set_pool_cooldown_with_meta(
                runtime,
                provider_id,
                key_id,
                &reason,
                decision.ttl_seconds,
                Some(meta),
            )
            .await;
        }
    }
    if let Some(next_level) = decision.next_backoff_level {
        write_pool_cooldown_backoff_level(
            runtime,
            provider_id,
            key_id,
            next_level,
            decision.ttl_seconds.min(MAX_POOL_COOLDOWN_SECONDS),
        )
        .await;
    }
    Some(decision)
}

/// 冷却 KV 里的原因码。429 保留历史上的 `rate_limited_429` / `quota_exhausted_429`
/// 语义（管理端过滤依赖它），无提示退避则写 `backoff_level_N`；其余状态码直接用决策原因。
fn pool_cooldown_reason_for_decision(
    status_code: u16,
    decision: &CooldownDecision,
    error_body: Option<&str>,
) -> String {
    if status_code == 429 {
        if decision.action == CooldownAction::QuotaExhausted
            || decision.hint.quota_exhausted
            || error_body_indicates_quota_exhaustion(error_body)
        {
            return "quota_exhausted_429".to_string();
        }
        if decision.backoff_level.is_some() {
            return decision.reason.clone();
        }
        return "rate_limited_429".to_string();
    }
    decision.reason.clone()
}

fn error_body_indicates_quota_exhaustion(error_body: Option<&str>) -> bool {
    let body = error_body.unwrap_or_default().to_ascii_lowercase();
    [
        "quota exhausted",
        "quota_exhausted",
        "quota exceeded",
        "quota_exceeded",
        "insufficient_quota",
        "resource exhausted",
        "resource has been exhausted",
        "resource_exhausted",
        "usage_limit_reached",
        "limit_reached",
        "quota limit reached",
        "credits exhausted",
        "insufficient credits",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

pub(crate) async fn record_admin_provider_pool_stream_timeout(
    runtime: &RuntimeState,
    provider_id: &str,
    key_id: &str,
    pool_config: &AdminProviderPoolConfig,
) {
    if pool_config.stream_timeout_threshold == 0 {
        return;
    }

    let timeout_key = pool_stream_timeout_key(provider_id, key_id);
    let now = current_unix_secs_f64();
    let window_seconds = pool_config.stream_timeout_window_seconds.max(1);
    let member = Uuid::new_v4().simple().to_string();
    let _ = runtime
        .score_remove_by_score(&timeout_key, now - window_seconds as f64)
        .await;
    let _ = runtime.score_set(&timeout_key, &member, now).await;
    let count = runtime.score_len(&timeout_key).await.unwrap_or(0) as u64;
    let _ = runtime
        .key_expire(
            &timeout_key,
            std::time::Duration::from_secs(window_seconds.saturating_add(60)),
        )
        .await;

    if count >= pool_config.stream_timeout_threshold {
        set_pool_cooldown(
            runtime,
            provider_id,
            key_id,
            &format!("stream_timeout_x{count}"),
            pool_config.stream_timeout_cooldown_seconds.max(1),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        admin_provider_pool_key_terminal_error_reason, error_body_indicates_quota_exhaustion,
        record_admin_provider_pool_error, record_admin_provider_pool_error_with_context,
        record_admin_provider_pool_stream_timeout, record_admin_provider_pool_success,
        AdminProviderPoolErrorContext, MAX_POOL_COOLDOWN_SECONDS,
    };
    use crate::handlers::admin::provider::pool::cooldown::{
        CooldownAction, ProviderCooldownConfig,
    };
    use crate::handlers::admin::provider::pool::runtime::keys::{
        pool_cooldown_backoff_key, pool_cooldown_meta_key, pool_model_cooldown_index_key,
        pool_model_cooldown_key, pool_model_cooldown_meta_key,
    };
    use crate::handlers::admin::provider::pool::runtime::reads::{
        read_admin_provider_pool_key_cooldown_reason, read_admin_provider_pool_runtime_state,
    };
    use crate::handlers::admin::provider::shared::support::{
        admin_provider_pool_quota_probe_active_members_key, AdminProviderPoolConfig,
        AdminProviderPoolSchedulingPreset, AdminProviderPoolUnschedulableRule,
    };
    use aether_runtime_state::{MemoryRuntimeStateConfig, RuntimeState};
    use std::collections::BTreeMap;

    fn sample_pool_config() -> AdminProviderPoolConfig {
        AdminProviderPoolConfig {
            scheduling_presets: vec![
                AdminProviderPoolSchedulingPreset {
                    preset: "cache_affinity".to_string(),
                    enabled: true,
                    mode: None,
                },
                AdminProviderPoolSchedulingPreset {
                    preset: "latency_first".to_string(),
                    enabled: true,
                    mode: None,
                },
            ],
            unschedulable_rules: Vec::new(),
            lru_enabled: true,
            skip_exhausted_accounts: false,
            sticky_session_ttl_seconds: 120,
            latency_window_seconds: 600,
            latency_sample_limit: 10,
            cost_window_seconds: 600,
            cost_limit_per_key_tokens: Some(10_000),
            rate_limit_cooldown_seconds: 300,
            overload_cooldown_seconds: 30,
            probing_enabled: false,
            probing_target_percent: None,
            probing_target_count: None,
            probe_concurrency: 4,
            account_self_check_enabled: false,
            account_self_check_interval_minutes: 60,
            account_self_check_concurrency: 4,
            score_top_n: 128,
            score_fallback_scan_limit: 4096,
            score_rules: aether_pool_core::PoolMemberScoreRules::default(),
            stream_timeout_threshold: 3,
            stream_timeout_window_seconds: 1800,
            stream_timeout_cooldown_seconds: 300,
        }
    }

    fn memory_runtime() -> RuntimeState {
        RuntimeState::memory(MemoryRuntimeStateConfig::default())
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    async fn cooldown_state(
        runtime: &RuntimeState,
        pool_config: &AdminProviderPoolConfig,
        key_id: &str,
    ) -> (Option<String>, Option<u64>) {
        let state = read_admin_provider_pool_runtime_state(
            runtime,
            "provider-1",
            &[key_id.to_string()],
            pool_config,
            None,
        )
        .await;
        (
            state.cooldown_reason_by_key.get(key_id).cloned(),
            state.cooldown_ttl_by_key.get(key_id).copied(),
        )
    }

    async fn cooldown_meta(runtime: &RuntimeState, key_id: &str) -> Option<serde_json::Value> {
        runtime
            .kv_get(&pool_cooldown_meta_key("provider-1", key_id))
            .await
            .expect("meta should read")
            .and_then(|raw| serde_json::from_str(&raw).ok())
    }

    async fn wait_for_active_probe_members_empty(runtime: &RuntimeState, set_key: &str) {
        for _ in 0..20 {
            let members = runtime
                .set_members(set_key)
                .await
                .expect("active members should read");
            if members.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let members = runtime
            .set_members(set_key)
            .await
            .expect("active members should read");
        assert!(members.is_empty());
    }

    #[test]
    fn terminal_error_reason_detects_workspace_deactivated_errors() {
        assert_eq!(
            admin_provider_pool_key_terminal_error_reason(
                400,
                Some(r#"{"error":{"message":"deactivated_workspace"}}"#),
            )
            .as_deref(),
            Some("workspace_deactivated_400:deactivated_workspace")
        );
        assert_eq!(
            admin_provider_pool_key_terminal_error_reason(
                403,
                Some(r#"{"error":{"message":"Workspace has been disabled"}}"#),
            )
            .as_deref(),
            Some("workspace_deactivated_403:workspace has been disabled")
        );
    }

    #[test]
    fn terminal_error_reason_detects_account_ban_errors() {
        assert_eq!(
            admin_provider_pool_key_terminal_error_reason(
                401,
                Some(r#"{"error":{"message":"account has been deactivated"}}"#),
            )
            .as_deref(),
            Some("account_deactivated_401")
        );
        assert_eq!(
            admin_provider_pool_key_terminal_error_reason(
                403,
                Some(r#"{"error":{"message":"account suspended"}}"#),
            )
            .as_deref(),
            Some("forbidden_403")
        );
        assert_eq!(
            admin_provider_pool_key_terminal_error_reason(429, Some("rate limited")),
            None
        );
    }

    #[test]
    fn detects_quota_exhaustion_markers_in_429_bodies() {
        assert!(error_body_indicates_quota_exhaustion(Some(
            r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded"}}"#
        )));
        assert!(error_body_indicates_quota_exhaustion(Some(
            r#"{"error":{"type":"usage_limit_reached"}}"#
        )));
        assert!(!error_body_indicates_quota_exhaustion(Some(
            r#"{"error":{"message":"rate limited"}}"#
        )));
    }

    #[tokio::test]
    async fn success_feedback_writes_sticky_lru_cost_and_latency() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let key_ids = vec!["key-1".to_string()];

        record_admin_provider_pool_success(
            &runtime,
            "provider-1",
            "key-1",
            &pool_config,
            Some("session-1"),
            120,
            Some(80),
        )
        .await;

        let state = read_admin_provider_pool_runtime_state(
            &runtime,
            "provider-1",
            &key_ids,
            &pool_config,
            Some("session-1"),
        )
        .await;

        assert_eq!(state.total_sticky_sessions, 1);
        assert_eq!(state.sticky_bound_key_id.as_deref(), Some("key-1"));
        assert_eq!(state.sticky_sessions_by_key.get("key-1"), Some(&1));
        assert_eq!(state.cost_window_usage_by_key.get("key-1"), Some(&120));
        assert_eq!(state.latency_avg_ms_by_key.get("key-1"), Some(&80.0));
        assert!(state.lru_score_by_key.contains_key("key-1"));
    }

    #[tokio::test]
    async fn success_feedback_does_not_write_sticky_when_ttl_is_zero() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.sticky_session_ttl_seconds = 0;

        record_admin_provider_pool_success(
            &runtime,
            "provider-1",
            "key-1",
            &pool_config,
            Some("session-1"),
            120,
            Some(80),
        )
        .await;

        let state = read_admin_provider_pool_runtime_state(
            &runtime,
            "provider-1",
            &["key-1".to_string()],
            &pool_config,
            Some("session-1"),
        )
        .await;
        assert_eq!(state.total_sticky_sessions, 0);
        assert_eq!(state.sticky_bound_key_id, None);
        assert_eq!(state.cost_window_usage_by_key.get("key-1"), Some(&120));
    }

    #[tokio::test]
    async fn success_feedback_does_not_write_sticky_without_cache_affinity() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.scheduling_presets = vec![AdminProviderPoolSchedulingPreset {
            preset: "latency_first".to_string(),
            enabled: true,
            mode: None,
        }];

        record_admin_provider_pool_success(
            &runtime,
            "provider-1",
            "key-1",
            &pool_config,
            Some("session-1"),
            120,
            Some(80),
        )
        .await;

        let state = read_admin_provider_pool_runtime_state(
            &runtime,
            "provider-1",
            &["key-1".to_string()],
            &pool_config,
            Some("session-1"),
        )
        .await;
        assert_eq!(state.total_sticky_sessions, 0);
        assert_eq!(state.sticky_bound_key_id, None);
        assert_eq!(state.sticky_sessions_by_key.get("key-1"), None);
        assert_eq!(state.cost_window_usage_by_key.get("key-1"), Some(&120));
        assert_eq!(state.latency_avg_ms_by_key.get("key-1"), Some(&80.0));
        assert!(state.lru_score_by_key.contains_key("key-1"));
    }

    /// 验收：Retry-After=120 → 冷却到提示时刻，原因保持 `rate_limited_429`，元数据说明来源。
    #[tokio::test]
    async fn error_feedback_respects_retry_after_for_rate_limits() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-2",
            &pool_config,
            429,
            Some(r#"{"error":{"message":"rate limited"}}"#),
            Some(&headers(&[("Retry-After", "120")])),
        )
        .await
        .expect("a 429 produces a decision");
        assert_eq!(decision.action, CooldownAction::Cooldown);

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-2").await;
        assert_eq!(reason.as_deref(), Some("rate_limited_429"));
        assert!(ttl.is_some_and(|ttl| ttl <= 120 && ttl >= 100));
        let meta = cooldown_meta(&runtime, "key-2")
            .await
            .expect("meta written");
        assert_eq!(meta["source"], "retry_after_header");
        assert_eq!(meta["reason"], "retry_after_header");
        assert_eq!(meta["ttl_seconds"], 120);
        assert!(meta["until"].as_u64().is_some());
    }

    /// 验收：Retry-After=2 → 不冷却，同 Key 立即重试。
    #[tokio::test]
    async fn short_retry_after_does_not_cool_down_and_asks_for_immediate_retry() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-fast",
            &pool_config,
            429,
            Some(r#"{"error":{"message":"slow down"}}"#),
            Some(&headers(&[("Retry-After", "2")])),
        )
        .await
        .expect("decision");
        assert_eq!(decision.action, CooldownAction::ImmediateRetry);
        assert_eq!(
            read_admin_provider_pool_key_cooldown_reason(&runtime, "provider-1", "key-fast")
                .await
                .expect("read"),
            None
        );
    }

    /// 验收：连续三次无提示 429 → 30s、60s、120s。
    #[tokio::test]
    async fn no_hint_429s_walk_the_backoff_ladder() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let mut observed = Vec::new();
        for _ in 0..3 {
            let decision = record_admin_provider_pool_error(
                &runtime,
                "provider-1",
                "key-backoff",
                &pool_config,
                429,
                Some(r#"{"error":{"message":"rate limited"}}"#),
                None,
            )
            .await
            .expect("decision");
            observed.push((decision.reason.clone(), decision.ttl_seconds));
            // 模拟窗口结束：清掉冷却本身，但保留退避等级。
            let _ = runtime
                .kv_delete(&super::pool_cooldown_key("provider-1", "key-backoff"))
                .await;
        }
        assert_eq!(
            observed,
            vec![
                ("backoff_level_0".to_string(), 30),
                ("backoff_level_1".to_string(), 60),
                ("backoff_level_2".to_string(), 120),
            ]
        );
        assert_eq!(
            runtime
                .kv_get(&pool_cooldown_backoff_key("provider-1", "key-backoff"))
                .await
                .expect("read")
                .as_deref(),
            Some("3")
        );

        // 成功一次后阶梯归零。
        record_admin_provider_pool_success(
            &runtime,
            "provider-1",
            "key-backoff",
            &pool_config,
            None,
            0,
            None,
        )
        .await;
        assert_eq!(
            runtime
                .kv_get(&pool_cooldown_backoff_key("provider-1", "key-backoff"))
                .await
                .expect("read"),
            None
        );
    }

    /// 同一冷却窗口内并发失败只升一级：第二次失败不改 TTL、不推进等级。
    #[tokio::test]
    async fn concurrent_failures_inside_a_window_escalate_once() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        for _ in 0..2 {
            record_admin_provider_pool_error(
                &runtime,
                "provider-1",
                "key-burst",
                &pool_config,
                429,
                None,
                None,
            )
            .await;
        }
        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-burst").await;
        assert_eq!(reason.as_deref(), Some("backoff_level_0"));
        assert!(ttl.is_some_and(|ttl| ttl <= 30 && ttl >= 25));
        assert_eq!(
            runtime
                .kv_get(&pool_cooldown_backoff_key("provider-1", "key-burst"))
                .await
                .expect("read")
                .as_deref(),
            Some("1")
        );
    }

    /// 后来的失败只延长仍在生效的冷却，不缩短。
    #[tokio::test]
    async fn later_failures_only_extend_an_active_cooldown() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-extend",
            &pool_config,
            429,
            None,
            Some(&headers(&[("Retry-After", "240")])),
        )
        .await;
        let (_, first_ttl) = cooldown_state(&runtime, &pool_config, "key-extend").await;
        assert!(first_ttl.is_some_and(|ttl| ttl >= 230));

        // 60s 的 5xx 冷却不能把 240s 的冷却缩短。
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-extend",
            &pool_config,
            502,
            None,
            None,
        )
        .await;
        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-extend").await;
        assert_eq!(reason.as_deref(), Some("rate_limited_429"));
        assert!(ttl.is_some_and(|ttl| ttl >= 230));
        let meta = cooldown_meta(&runtime, "key-extend").await.expect("meta");
        assert_eq!(meta["extended_existing"], true);

        // 更长的提示可以延长（≥5 分钟的提示同时升级为配额耗尽）。
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-extend",
            &pool_config,
            429,
            None,
            Some(&headers(&[("Retry-After", "1500")])),
        )
        .await;
        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-extend").await;
        assert_eq!(reason.as_deref(), Some("quota_exhausted_429"));
        assert!(ttl.is_some_and(|ttl| ttl >= 1490));
    }

    #[tokio::test]
    async fn error_feedback_does_not_write_cooldown_when_429_or_529_cooldown_is_zero() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.rate_limit_cooldown_seconds = 0;
        pool_config.overload_cooldown_seconds = 0;

        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-429",
            &pool_config,
            429,
            Some(r#"{"error":{"message":"rate limited"}}"#),
            Some(&headers(&[("Retry-After", "120")])),
        )
        .await;
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-529",
            &pool_config,
            529,
            Some(r#"{"error":{"message":"overloaded"}}"#),
            None,
        )
        .await;
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-503",
            &pool_config,
            503,
            None,
            None,
        )
        .await;

        let state = read_admin_provider_pool_runtime_state(
            &runtime,
            "provider-1",
            &[
                "key-429".to_string(),
                "key-529".to_string(),
                "key-503".to_string(),
            ],
            &pool_config,
            None,
        )
        .await;
        assert!(state.cooldown_reason_by_key.is_empty());
        assert!(state.cooldown_ttl_by_key.is_empty());
    }

    #[tokio::test]
    async fn error_feedback_removes_active_probe_member_when_key_becomes_unschedulable() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let set_key = admin_provider_pool_quota_probe_active_members_key("provider-1");

        runtime
            .set_add(&set_key, "key-401")
            .await
            .expect("active member should insert");
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-401",
            &pool_config,
            401,
            Some(r#"{"error":{"message":"invalid token"}}"#),
            None,
        )
        .await;
        wait_for_active_probe_members_empty(&runtime, &set_key).await;

        runtime
            .set_add(&set_key, "key-402")
            .await
            .expect("active member should insert");
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-402",
            &pool_config,
            402,
            Some(r#"{"error":{"message":"quota exhausted"}}"#),
            None,
        )
        .await;
        wait_for_active_probe_members_empty(&runtime, &set_key).await;
    }

    #[tokio::test]
    async fn error_feedback_uses_google_quota_cooldown_when_retry_after_missing() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-google-429",
            &pool_config,
            429,
            Some(
                r#"{
                    "error": {
                        "message": "Quota exhausted. reset after 45s.",
                        "status": "RESOURCE_EXHAUSTED",
                        "details": [{
                            "metadata": {
                                "quotaResetDelay": "45s"
                            }
                        }]
                    }
                }"#,
            ),
            None,
        )
        .await
        .expect("decision");
        assert_eq!(decision.hint.source.as_str(), "google_retry_info");

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-google-429").await;
        assert_eq!(reason.as_deref(), Some("quota_exhausted_429"));
        assert!(ttl.is_some_and(|ttl| ttl <= 45 && ttl >= 30));
    }

    /// ≥5 分钟的提示按配额耗尽处理：KV 冷却封顶 32 分钟，决策里保留完整 reset_at。
    #[tokio::test]
    async fn long_hints_are_quota_exhaustion_and_kv_ttl_is_capped() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-long-cooldown",
            &pool_config,
            429,
            Some(r#"{"error":{"message":"rate limited"}}"#),
            Some(&headers(&[("Retry-After", "3600")])),
        )
        .await
        .expect("decision");
        assert_eq!(decision.action, CooldownAction::QuotaExhausted);
        assert_eq!(decision.ttl_seconds, 3600);

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-long-cooldown").await;
        assert_eq!(reason.as_deref(), Some("quota_exhausted_429"));
        assert!(ttl.is_some_and(|ttl| ttl <= MAX_POOL_COOLDOWN_SECONDS && ttl >= 31 * 60));
        let meta = cooldown_meta(&runtime, "key-long-cooldown")
            .await
            .expect("meta");
        assert_eq!(meta["action"], "quota_exhausted");
        assert_eq!(meta["ttl_seconds"], 3600);
    }

    /// Anthropic 5h 窗口在未来 3 小时重置 → 配额耗尽、reset_at 落到决策里。
    #[tokio::test]
    async fn anthropic_window_rejection_is_quota_exhaustion_with_reset_at() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let reset_at = super::current_unix_secs() + 3 * 3600;
        let context = AdminProviderPoolErrorContext {
            provider_type: "claude_code".to_string(),
            cooldown: ProviderCooldownConfig::default(),
            provider_model_name: Some("claude-sonnet-4-5".to_string()),
        };
        let decision = record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-claude",
            &pool_config,
            429,
            Some(r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#),
            Some(&headers(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &reset_at.to_string(),
                ),
            ])),
            &context,
        )
        .await
        .expect("decision");
        assert_eq!(decision.action, CooldownAction::QuotaExhausted);
        assert_eq!(decision.reason, "ratelimit_window_5h");
        assert!(decision
            .hint
            .reset_at_unix_secs
            .is_some_and(|value| value >= reset_at && value <= reset_at + 30));

        let (reason, _) = cooldown_state(&runtime, &pool_config, "key-claude").await;
        assert_eq!(reason.as_deref(), Some("quota_exhausted_429"));
        let meta = cooldown_meta(&runtime, "key-claude").await.expect("meta");
        assert_eq!(meta["source"], "ratelimit_window_5h");
        assert_eq!(meta["scope"], "key");
    }

    /// 模型级冷却：只有请求的模型被冷却，Key 本身仍可调度。
    #[tokio::test]
    async fn model_level_cooldown_only_blocks_the_requested_model() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let context = AdminProviderPoolErrorContext {
            provider_type: "codex".to_string(),
            cooldown: ProviderCooldownConfig {
                model_level: true,
                ..ProviderCooldownConfig::default()
            },
            provider_model_name: Some("gpt-5.4".to_string()),
        };
        record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-model",
            &pool_config,
            429,
            None,
            Some(&headers(&[("Retry-After", "90")])),
            &context,
        )
        .await;

        let (reason, _) = cooldown_state(&runtime, &pool_config, "key-model").await;
        assert_eq!(reason, None, "key-level cooldown must stay untouched");
        assert_eq!(
            runtime
                .kv_get(&pool_model_cooldown_key(
                    "provider-1",
                    "key-model",
                    "gpt-5.4"
                ))
                .await
                .expect("read")
                .as_deref(),
            Some("rate_limited_429")
        );
        assert_eq!(
            runtime
                .kv_get(&pool_model_cooldown_key(
                    "provider-1",
                    "key-model",
                    "gpt-5.1"
                ))
                .await
                .expect("read"),
            None
        );
        let members = runtime
            .set_members(&pool_model_cooldown_index_key("provider-1", "key-model"))
            .await
            .expect("index");
        assert_eq!(members, vec!["gpt-5.4".to_string()]);
        let meta: serde_json::Value = runtime
            .kv_get(&pool_model_cooldown_meta_key(
                "provider-1",
                "key-model",
                "gpt-5.4",
            ))
            .await
            .expect("read")
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .expect("meta");
        assert_eq!(meta["model"], "gpt-5.4");
        assert_eq!(meta["ttl_seconds"], 90);
    }

    /// 404 带模型名时只冷却 Key+模型，整把 Key 仍可调度。
    #[tokio::test]
    async fn not_found_with_a_model_name_only_cools_that_model() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let context = AdminProviderPoolErrorContext {
            provider_type: "custom".to_string(),
            cooldown: ProviderCooldownConfig::default(),
            provider_model_name: Some("gpt-5.4-typo".to_string()),
        };
        let decision = record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-404-model",
            &pool_config,
            404,
            Some(r#"{"error":{"message":"model not found"}}"#),
            None,
            &context,
        )
        .await
        .expect("404 decision");
        assert_eq!(decision.reason, "not_found_404");
        let (reason, _) = cooldown_state(&runtime, &pool_config, "key-404-model").await;
        assert_eq!(reason, None, "the key itself stays schedulable");
        assert_eq!(
            runtime
                .kv_get(&pool_model_cooldown_key(
                    "provider-1",
                    "key-404-model",
                    "gpt-5.4-typo"
                ))
                .await
                .expect("model cooldown should read")
                .as_deref(),
            Some("not_found_404")
        );
    }

    /// 索引集合的过期只延长不缩短：先冷却 30 分钟的 Key，再来一个 30 秒的，索引仍要活到前者结束。
    #[tokio::test]
    async fn cooldown_index_expiry_is_never_shortened_by_a_later_short_cooldown() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-long",
            &pool_config,
            403,
            Some(r#"{"error":{"message":"temporarily forbidden"}}"#),
            None,
        )
        .await
        .expect("soft 403 decision");
        let index_key =
            crate::handlers::admin::provider::pool::runtime::keys::pool_cooldown_index_key(
                "provider-1",
            );
        let long_ttl = runtime
            .key_ttl_seconds(&index_key)
            .await
            .expect("ttl should read")
            .expect("index should exist");
        assert!(long_ttl >= 30 * 60);

        let mut short = AdminProviderPoolErrorContext::default();
        short.cooldown.transient_error_seconds = 30;
        record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-short",
            &pool_config,
            502,
            None,
            None,
            &short,
        )
        .await
        .expect("502 decision");
        let after = runtime
            .key_ttl_seconds(&index_key)
            .await
            .expect("ttl should read")
            .expect("index should still exist");
        assert!(
            after >= long_ttl - 5,
            "index ttl {after} was shortened below {long_ttl}"
        );
        let members = runtime
            .set_members(&index_key)
            .await
            .expect("index members");
        assert!(members.contains(&"key-long".to_string()));
        assert!(members.contains(&"key-short".to_string()));
    }

    /// 状态码表：401/402/403 是账号级终态，不写池冷却；404 写 12 小时（KV 封顶）；5xx 写瞬时冷却。
    #[tokio::test]
    async fn status_code_table_writes_expected_cooldowns() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        for (key_id, status, body) in [
            (
                "key-401",
                401,
                Some(r#"{"error":{"message":"account has been deactivated"}}"#),
            ),
            ("key-402", 402, None),
            (
                "key-403",
                403,
                Some(r#"{"error":{"message":"account suspended"}}"#),
            ),
        ] {
            let decision = record_admin_provider_pool_error(
                &runtime,
                "provider-1",
                key_id,
                &pool_config,
                status,
                body,
                None,
            )
            .await;
            assert!(decision.is_none(), "{status} is a terminal account error");
            let (reason, _) = cooldown_state(&runtime, &pool_config, key_id).await;
            assert_eq!(reason, None, "{status} must not use pool cooldown");
        }

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-403-soft",
            &pool_config,
            403,
            Some(r#"{"error":{"message":"temporarily forbidden"}}"#),
            None,
        )
        .await
        .expect("soft 403 decision");
        assert_eq!(decision.reason, "forbidden_403");
        assert_eq!(decision.ttl_seconds, 30 * 60);

        let decision = record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-404",
            &pool_config,
            404,
            None,
            None,
        )
        .await
        .expect("404 decision");
        assert_eq!(decision.reason, "not_found_404");
        assert_eq!(decision.ttl_seconds, 12 * 60 * 60);
        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-404").await;
        assert_eq!(reason.as_deref(), Some("not_found_404"));
        assert!(ttl.is_some_and(|ttl| ttl <= MAX_POOL_COOLDOWN_SECONDS && ttl >= 31 * 60));

        for (key_id, status, reason) in [
            ("key-500", 500, "server_error_500"),
            ("key-408", 408, "request_timeout_408"),
            ("key-522", 522, "transient_upstream_522"),
        ] {
            record_admin_provider_pool_error(
                &runtime,
                "provider-1",
                key_id,
                &pool_config,
                status,
                None,
                None,
            )
            .await;
            let (actual, ttl) = cooldown_state(&runtime, &pool_config, key_id).await;
            assert_eq!(actual.as_deref(), Some(reason));
            assert!(ttl.is_some_and(|ttl| ttl <= 60 && ttl >= 50), "{status}");
        }
    }

    #[tokio::test]
    async fn transient_cooldown_seconds_comes_from_provider_config() {
        let runtime = memory_runtime();
        let pool_config = sample_pool_config();
        let context = AdminProviderPoolErrorContext {
            provider_type: "custom".to_string(),
            cooldown: ProviderCooldownConfig {
                transient_error_seconds: 15,
                ..ProviderCooldownConfig::default()
            },
            provider_model_name: None,
        };
        record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-transient",
            &pool_config,
            503,
            None,
            None,
            &context,
        )
        .await;
        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-transient").await;
        assert_eq!(reason.as_deref(), Some("service_unavailable_503"));
        assert!(ttl.is_some_and(|ttl| ttl <= 15 && ttl >= 10));

        let disabled = AdminProviderPoolErrorContext {
            cooldown: ProviderCooldownConfig {
                disable: true,
                ..ProviderCooldownConfig::default()
            },
            ..context
        };
        let decision = record_admin_provider_pool_error_with_context(
            &runtime,
            "provider-1",
            "key-disabled",
            &pool_config,
            429,
            None,
            None,
            &disabled,
        )
        .await
        .expect("decision");
        assert_eq!(decision.action, CooldownAction::None);
        let (reason, _) = cooldown_state(&runtime, &pool_config, "key-disabled").await;
        assert_eq!(reason, None);
    }

    #[tokio::test]
    async fn error_feedback_applies_unschedulable_rule_cooldown() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.unschedulable_rules = vec![AdminProviderPoolUnschedulableRule {
            keyword: "review required".to_string(),
            duration_minutes: 7,
        }];

        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-rule",
            &pool_config,
            403,
            Some(r#"{"error":{"message":"manual review required before reuse"}}"#),
            None,
        )
        .await;

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-rule").await;
        assert_eq!(reason.as_deref(), Some("rule:review required"));
        assert!(ttl.is_some_and(|ttl| ttl <= 420 && ttl >= 400));
    }

    #[tokio::test]
    async fn error_feedback_ignores_client_bad_request_for_cooldown() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.unschedulable_rules = vec![AdminProviderPoolUnschedulableRule {
            keyword: "review required".to_string(),
            duration_minutes: 7,
        }];

        record_admin_provider_pool_error(
            &runtime,
            "provider-1",
            "key-client-400",
            &pool_config,
            400,
            Some(r#"{"error":{"message":"manual review required before reuse"}}"#),
            None,
        )
        .await;

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-client-400").await;
        assert_eq!(reason, None);
        assert_eq!(ttl, None);
    }

    #[tokio::test]
    async fn stream_timeout_policy_cools_down_after_threshold() {
        let runtime = memory_runtime();
        let mut pool_config = sample_pool_config();
        pool_config.stream_timeout_threshold = 2;
        pool_config.stream_timeout_window_seconds = 300;
        pool_config.stream_timeout_cooldown_seconds = 90;

        for _ in 0..2 {
            record_admin_provider_pool_stream_timeout(
                &runtime,
                "provider-1",
                "key-4",
                &pool_config,
            )
            .await;
        }

        let (reason, ttl) = cooldown_state(&runtime, &pool_config, "key-4").await;
        assert_eq!(reason.as_deref(), Some("stream_timeout_x2"));
        assert!(ttl.is_some_and(|ttl| ttl <= 90 && ttl >= 70));
    }
}
