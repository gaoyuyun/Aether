//! 上游限流信号驱动的冷却决策。
//!
//! 输入是一次失败响应（状态码、响应头、错误体）与供应商/Key 的冷却配置，输出是
//! 「冷却多久、为什么、作用域是整把 Key 还是 Key+模型」。这里只做纯决策，不碰
//! KV 与数据库；写入由 `runtime/writes.rs` 负责，池分硬状态由 orchestration 负责。
//!
//! 规则（见 docs/operations/provider-quality-improvement-plan.md §1.2）：
//! - 上游给出的等待时长 < 3s：不冷却，同 Key 立即重试一次。
//! - 429 且提示 < 5min：冷却到提示时刻，换 Key。
//! - 429 且提示 ≥ 5min：按配额耗尽处理，`reset_at` 写入池成员 quota 元数据。
//! - 429 且无提示：指数退避阶梯，基数 30s、上限 30min；同一冷却窗口内的并发失败只升一级。
//! - 401 / 402 / 403：30 分钟；404：12 小时；5xx / 408 / 520–526：`transient_error_seconds`。
//! - 后来的失败只延长仍在生效的冷却，不缩短（由写入层按 TTL 比较执行）。

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::provider_transport::{
    extract_upstream_retry_hint, RetryHintScope, RetryHintSource, UpstreamRetryHint,
    RETRY_HINT_QUOTA_EXHAUSTED_THRESHOLD,
};

/// 供应商 `config.cooldown` 与 Key `auth_config.cooldown` 共用的键名。
pub(crate) const PROVIDER_COOLDOWN_CONFIG_KEY: &str = "cooldown";
pub(crate) const DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS: u64 = 60;
pub(crate) const MAX_TRANSIENT_ERROR_COOLDOWN_SECONDS: u64 = 24 * 60 * 60;
pub(crate) const RATE_LIMIT_BACKOFF_BASE_SECONDS: u64 = 30;
pub(crate) const RATE_LIMIT_BACKOFF_MAX_SECONDS: u64 = 30 * 60;
pub(crate) const AUTH_FAILURE_COOLDOWN_SECONDS: u64 = 30 * 60;
pub(crate) const NOT_FOUND_COOLDOWN_SECONDS: u64 = 12 * 60 * 60;
/// 非 429 的失败带了 Retry-After 时，最多按这个上限冷却；再长就不是瞬时错误了。
pub(crate) const TRANSIENT_HINT_COOLDOWN_MAX_SECONDS: u64 = 30 * 60;
/// 退避等级在最后一次冷却结束后还保留多久；窗口内再失败继续升级，窗口外从头开始。
pub(crate) const RATE_LIMIT_BACKOFF_LEVEL_MEMORY_SECONDS: u64 = 30 * 60;

pub(crate) const COOLDOWN_REASON_DISABLED: &str = "cooldown_disabled";
pub(crate) const COOLDOWN_REASON_RATE_LIMIT_DISABLED: &str = "rate_limit_cooldown_disabled";
pub(crate) const COOLDOWN_REASON_AUTH_FAILED_401: &str = "auth_failed_401";
pub(crate) const COOLDOWN_REASON_PAYMENT_REQUIRED_402: &str = "payment_required_402";
pub(crate) const COOLDOWN_REASON_FORBIDDEN_403: &str = "forbidden_403";
pub(crate) const COOLDOWN_REASON_NOT_FOUND_404: &str = "not_found_404";
pub(crate) const COOLDOWN_REASON_OVERLOADED_529: &str = "overloaded_529";
pub(crate) const COOLDOWN_REASON_BACKOFF_PREFIX: &str = "backoff_level_";
pub(crate) const COOLDOWN_REASON_TRANSIENT_PREFIX: &str = "transient_upstream_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderCooldownConfig {
    /// 关闭全部冷却写入（回滚开关）。
    pub(crate) disable: bool,
    /// 5xx / 408 / 520–526 等瞬时错误的冷却秒数。
    pub(crate) transient_error_seconds: u64,
    /// 冷却按 Key+模型 而不是整把 Key。
    pub(crate) model_level: bool,
}

impl Default for ProviderCooldownConfig {
    fn default() -> Self {
        Self {
            disable: false,
            transient_error_seconds: DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS,
            model_level: false,
        }
    }
}

impl ProviderCooldownConfig {
    pub(crate) fn to_json(self) -> Value {
        json!({
            "disable": self.disable,
            "transient_error_seconds": self.transient_error_seconds,
            "model_level": self.model_level,
        })
    }
}

fn json_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        _ => None,
    }
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|raw| u64::try_from(raw).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|raw| raw.is_finite() && *raw >= 0.0)
                .map(|raw| raw.floor() as u64)
        })
        .or_else(|| {
            value
                .as_str()
                .and_then(|raw| raw.trim().parse::<u64>().ok())
        })
}

fn apply_cooldown_object(
    mut config: ProviderCooldownConfig,
    object: &Map<String, Value>,
) -> ProviderCooldownConfig {
    if let Some(disable) = json_bool(object.get("disable")) {
        config.disable = disable;
    }
    if let Some(seconds) = json_u64(object.get("transient_error_seconds")) {
        config.transient_error_seconds = seconds.min(MAX_TRANSIENT_ERROR_COOLDOWN_SECONDS);
    }
    if let Some(model_level) = json_bool(object.get("model_level")) {
        config.model_level = model_level;
    }
    config
}

/// 供应商级 `config.cooldown`。缺失或形状不对时全部走默认值。
pub(crate) fn provider_cooldown_config_from_config_value(
    config: Option<&Value>,
) -> ProviderCooldownConfig {
    let base = ProviderCooldownConfig::default();
    let Some(object) = config
        .and_then(Value::as_object)
        .and_then(|config| config.get(PROVIDER_COOLDOWN_CONFIG_KEY))
        .and_then(Value::as_object)
    else {
        return base;
    };
    apply_cooldown_object(base, object)
}

/// Key 级 `auth_config.cooldown` 覆盖供应商级配置；只覆盖出现的字段。
pub(crate) fn apply_key_cooldown_override(
    base: ProviderCooldownConfig,
    auth_config: Option<&Map<String, Value>>,
) -> ProviderCooldownConfig {
    let Some(object) = auth_config
        .and_then(|config| config.get(PROVIDER_COOLDOWN_CONFIG_KEY))
        .and_then(Value::as_object)
    else {
        return base;
    };
    apply_cooldown_object(base, object)
}

/// 从解密后的 auth_config 原文解析 Key 级覆盖。
pub(crate) fn apply_key_cooldown_override_from_raw(
    base: ProviderCooldownConfig,
    raw_auth_config: Option<&str>,
) -> ProviderCooldownConfig {
    let Some(raw) = raw_auth_config.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return base;
    };
    let Some(object) = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_object().cloned())
    else {
        return base;
    };
    apply_key_cooldown_override(base, Some(&object))
}

/// 管理端写入前的归一化：只接受三个已知字段，类型必须正确。
pub(crate) fn normalize_provider_cooldown_config(value: &Value) -> Result<Value, String> {
    let Some(object) = value.as_object() else {
        return Err("cooldown 必须是 JSON 对象".to_string());
    };
    let mut normalized = Map::new();
    for (key, raw) in object {
        match key.as_str() {
            "disable" | "model_level" => {
                let Some(flag) = json_bool(Some(raw)) else {
                    return Err(format!("cooldown.{key} 必须是布尔值"));
                };
                normalized.insert(key.clone(), Value::Bool(flag));
            }
            "transient_error_seconds" => {
                if raw.is_null() {
                    continue;
                }
                let Some(seconds) = json_u64(Some(raw)) else {
                    return Err("cooldown.transient_error_seconds 必须是非负整数".to_string());
                };
                if seconds > MAX_TRANSIENT_ERROR_COOLDOWN_SECONDS {
                    return Err(format!(
                        "cooldown.transient_error_seconds 不能超过 {MAX_TRANSIENT_ERROR_COOLDOWN_SECONDS}"
                    ));
                }
                normalized.insert(key.clone(), json!(seconds));
            }
            other => return Err(format!("cooldown 不支持字段 {other}")),
        }
    }
    Ok(Value::Object(normalized))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CooldownAction {
    /// 不写冷却。
    None,
    /// 上游提示很短：不冷却，同 Key 立即重试一次。
    ImmediateRetry,
    /// 短冷却，换 Key。
    Cooldown,
    /// 提示的等待时长足够长，视为配额耗尽：冷却 + 把 `reset_at` 写进 quota 元数据。
    QuotaExhausted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CooldownDecision {
    pub(crate) action: CooldownAction,
    /// 写进冷却 KV 的原因码，前端有对应文案映射。
    pub(crate) reason: String,
    pub(crate) ttl_seconds: u64,
    /// 本次采用的退避等级（只有无提示 429 才有）。
    pub(crate) backoff_level: Option<u32>,
    /// 写回 KV 的下一等级；`None` 表示不改。
    pub(crate) next_backoff_level: Option<u32>,
    pub(crate) scope: RetryHintScope,
    pub(crate) hint: UpstreamRetryHint,
}

impl CooldownDecision {
    fn none(reason: &str, hint: UpstreamRetryHint) -> Self {
        Self {
            action: CooldownAction::None,
            reason: reason.to_string(),
            ttl_seconds: 0,
            backoff_level: None,
            next_backoff_level: None,
            scope: hint.scope,
            hint,
        }
    }

    pub(crate) fn writes_cooldown(&self) -> bool {
        matches!(
            self.action,
            CooldownAction::Cooldown | CooldownAction::QuotaExhausted
        ) && self.ttl_seconds > 0
    }

    pub(crate) fn cooldown_until_unix_secs(&self, now_unix_secs: u64) -> Option<u64> {
        self.writes_cooldown()
            .then(|| now_unix_secs.saturating_add(self.ttl_seconds))
    }

    /// 与冷却 KV 同生命周期的元数据，前端据此解释「为什么冷却这么久」。
    pub(crate) fn to_meta_json(&self, now_unix_secs: u64) -> Value {
        json!({
            "action": match self.action {
                CooldownAction::None => "none",
                CooldownAction::ImmediateRetry => "immediate_retry",
                CooldownAction::Cooldown => "cooldown",
                CooldownAction::QuotaExhausted => "quota_exhausted",
            },
            "reason": self.reason,
            "ttl_seconds": self.ttl_seconds,
            "until": self.cooldown_until_unix_secs(now_unix_secs),
            "backoff_level": self.backoff_level,
            "source": self.hint.source.as_str(),
            "scope": match self.scope {
                RetryHintScope::Key => "key",
                RetryHintScope::KeyModel => "key_model",
            },
            "retry_after_secs": self.hint.retry_after.map(|value| value.as_secs()),
            "reset_at": self.hint.reset_at_unix_secs,
            "quota_exhausted": self.hint.quota_exhausted,
            "rejected_windows": self.hint.rejected_windows,
            "decided_at": now_unix_secs,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CooldownDecisionInput<'a> {
    pub(crate) provider_type: &'a str,
    pub(crate) status_code: u16,
    pub(crate) headers: Option<&'a BTreeMap<String, String>>,
    pub(crate) error_body: Option<&'a str>,
    pub(crate) now_unix_secs: u64,
    pub(crate) config: ProviderCooldownConfig,
    /// 号池 `rate_limit_cooldown_seconds == 0` 时为 false：429 不冷却（历史语义）。
    pub(crate) rate_limit_cooldown_enabled: bool,
    /// 号池 `overload_cooldown_seconds`；0 表示 529 不冷却。
    pub(crate) overload_cooldown_seconds: u64,
    /// KV 里记录的退避等级（上一次无提示 429 之后写回的值）。
    pub(crate) previous_backoff_level: Option<u32>,
    /// 这把 Key（或 Key+模型）当前仍在生效的冷却剩余秒数。
    pub(crate) active_cooldown_ttl_seconds: Option<u64>,
}

/// 无提示 429 的退避阶梯：30s、60s、120s … 上限 30min。
pub(crate) const fn rate_limit_backoff_seconds(level: u32) -> u64 {
    let capped_level = if level > 20 { 20 } else { level };
    let shifted = RATE_LIMIT_BACKOFF_BASE_SECONDS.saturating_mul(1u64 << capped_level);
    if shifted > RATE_LIMIT_BACKOFF_MAX_SECONDS {
        RATE_LIMIT_BACKOFF_MAX_SECONDS
    } else {
        shifted
    }
}

fn immediate_retry_applies(status_code: u16) -> bool {
    matches!(status_code, 408 | 409 | 425 | 429 | 500..=599)
}

fn transient_status_reason(status_code: u16) -> Option<String> {
    match status_code {
        408 => Some("request_timeout_408".to_string()),
        409 => Some("conflict_409".to_string()),
        423 => Some("locked_423".to_string()),
        425 => Some("too_early_425".to_string()),
        500 => Some("server_error_500".to_string()),
        502 => Some("bad_gateway_502".to_string()),
        503 => Some("service_unavailable_503".to_string()),
        504 => Some("gateway_timeout_504".to_string()),
        501 | 505..=528 | 530..=599 => {
            Some(format!("{COOLDOWN_REASON_TRANSIENT_PREFIX}{status_code}"))
        }
        _ => None,
    }
}

pub(crate) fn decide_provider_cooldown(input: CooldownDecisionInput<'_>) -> CooldownDecision {
    let hint = extract_upstream_retry_hint(
        input.provider_type,
        input.status_code,
        input.headers,
        input.error_body,
        input.now_unix_secs,
    );
    if input.config.disable {
        return CooldownDecision::none(COOLDOWN_REASON_DISABLED, hint);
    }

    if hint.is_immediate_retry() && immediate_retry_applies(input.status_code) {
        return CooldownDecision {
            action: CooldownAction::ImmediateRetry,
            reason: hint.source.as_str().to_string(),
            ttl_seconds: 0,
            backoff_level: None,
            next_backoff_level: None,
            scope: hint.scope,
            hint,
        };
    }

    match input.status_code {
        429 => decide_rate_limit_cooldown(input, hint),
        401 => fixed_cooldown(
            COOLDOWN_REASON_AUTH_FAILED_401,
            AUTH_FAILURE_COOLDOWN_SECONDS,
            hint,
        ),
        402 => fixed_cooldown(
            COOLDOWN_REASON_PAYMENT_REQUIRED_402,
            AUTH_FAILURE_COOLDOWN_SECONDS,
            hint,
        ),
        403 => fixed_cooldown(
            COOLDOWN_REASON_FORBIDDEN_403,
            AUTH_FAILURE_COOLDOWN_SECONDS,
            hint,
        ),
        404 => {
            // 404 几乎总是「这个模型在这把 Key 上不存在」，而一把 Key 通常服务多个模型：
            // 只冷却 Key+模型，别让一个错的模型映射把整把 Key 关 12 小时。
            // 调用方没有模型名时（老路径）仍退回 Key 级。
            let mut decision = fixed_cooldown(
                COOLDOWN_REASON_NOT_FOUND_404,
                NOT_FOUND_COOLDOWN_SECONDS,
                hint,
            );
            decision.scope = RetryHintScope::KeyModel;
            decision
        }
        529 => {
            if input.overload_cooldown_seconds == 0 {
                return CooldownDecision::none("overload_cooldown_disabled", hint);
            }
            fixed_cooldown(
                COOLDOWN_REASON_OVERLOADED_529,
                input.overload_cooldown_seconds,
                hint,
            )
        }
        status_code => {
            let Some(default_reason) = transient_status_reason(status_code) else {
                return CooldownDecision::none("status_not_cooled", hint);
            };
            // 「瞬时错误冷却 = 0」是整体关闭开关：上游带 Retry-After 也不写冷却，
            // 与号池 `overload_cooldown_seconds = 0` 的历史语义一致。
            if input.config.transient_error_seconds == 0 {
                return CooldownDecision::none("transient_cooldown_disabled", hint);
            }
            if let Some(retry_after) = hint.retry_after {
                let ttl_seconds = retry_after
                    .as_secs()
                    .max(1)
                    .min(TRANSIENT_HINT_COOLDOWN_MAX_SECONDS);
                return CooldownDecision {
                    action: CooldownAction::Cooldown,
                    reason: hint.source.as_str().to_string(),
                    ttl_seconds,
                    backoff_level: None,
                    next_backoff_level: None,
                    scope: hint.scope,
                    hint,
                };
            }
            fixed_cooldown(&default_reason, input.config.transient_error_seconds, hint)
        }
    }
}

fn fixed_cooldown(reason: &str, ttl_seconds: u64, hint: UpstreamRetryHint) -> CooldownDecision {
    CooldownDecision {
        action: CooldownAction::Cooldown,
        reason: reason.to_string(),
        ttl_seconds,
        backoff_level: None,
        next_backoff_level: None,
        scope: hint.scope,
        hint,
    }
}

fn decide_rate_limit_cooldown(
    input: CooldownDecisionInput<'_>,
    hint: UpstreamRetryHint,
) -> CooldownDecision {
    if !input.rate_limit_cooldown_enabled {
        return CooldownDecision::none(COOLDOWN_REASON_RATE_LIMIT_DISABLED, hint);
    }
    if let Some(retry_after) = hint.retry_after {
        let ttl_seconds = retry_after.as_secs().max(1);
        let action = if retry_after >= RETRY_HINT_QUOTA_EXHAUSTED_THRESHOLD {
            CooldownAction::QuotaExhausted
        } else {
            CooldownAction::Cooldown
        };
        return CooldownDecision {
            action,
            reason: hint.source.as_str().to_string(),
            ttl_seconds,
            backoff_level: None,
            next_backoff_level: None,
            scope: hint.scope,
            hint,
        };
    }

    // 无提示：指数退避。仍在生效的冷却窗口内再失败不升级，只是复用当前窗口。
    let previous_level = input.previous_backoff_level.unwrap_or(0);
    if input.active_cooldown_ttl_seconds.is_some_and(|ttl| ttl > 0) {
        let level = previous_level.saturating_sub(1);
        return CooldownDecision {
            action: CooldownAction::Cooldown,
            reason: format!("{COOLDOWN_REASON_BACKOFF_PREFIX}{level}"),
            ttl_seconds: rate_limit_backoff_seconds(level),
            backoff_level: Some(level),
            next_backoff_level: None,
            scope: hint.scope,
            hint,
        };
    }
    let ttl_seconds = rate_limit_backoff_seconds(previous_level);
    let next_level = if ttl_seconds >= RATE_LIMIT_BACKOFF_MAX_SECONDS {
        previous_level
    } else {
        previous_level.saturating_add(1)
    };
    CooldownDecision {
        action: CooldownAction::Cooldown,
        reason: format!("{COOLDOWN_REASON_BACKOFF_PREFIX}{previous_level}"),
        ttl_seconds,
        backoff_level: Some(previous_level),
        next_backoff_level: Some(next_level),
        scope: hint.scope,
        hint,
    }
}

/// 只在提示来源是上游明确声明配额窗口时，把 `reset_at` 记进 quota 元数据。
pub(crate) fn decision_is_quota_window_signal(decision: &CooldownDecision) -> bool {
    decision.action == CooldownAction::QuotaExhausted
        && !matches!(decision.hint.source, RetryHintSource::None)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        apply_key_cooldown_override, decide_provider_cooldown, normalize_provider_cooldown_config,
        provider_cooldown_config_from_config_value, rate_limit_backoff_seconds, CooldownAction,
        CooldownDecisionInput, ProviderCooldownConfig, AUTH_FAILURE_COOLDOWN_SECONDS,
        DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS, NOT_FOUND_COOLDOWN_SECONDS,
        RATE_LIMIT_BACKOFF_MAX_SECONDS,
    };
    use crate::provider_transport::{RetryHintScope, RetryHintSource};

    const NOW: u64 = 1_800_000_000;

    fn input<'a>(
        provider_type: &'a str,
        status_code: u16,
        headers: Option<&'a BTreeMap<String, String>>,
        body: Option<&'a str>,
    ) -> CooldownDecisionInput<'a> {
        CooldownDecisionInput {
            provider_type,
            status_code,
            headers,
            error_body: body,
            now_unix_secs: NOW,
            config: ProviderCooldownConfig::default(),
            rate_limit_cooldown_enabled: true,
            overload_cooldown_seconds: 30,
            previous_backoff_level: None,
            active_cooldown_ttl_seconds: None,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn config_defaults_and_overrides_compose() {
        let base = provider_cooldown_config_from_config_value(None);
        assert_eq!(base, ProviderCooldownConfig::default());
        assert_eq!(
            base.transient_error_seconds,
            DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS
        );

        let provider = provider_cooldown_config_from_config_value(Some(&json!({
            "cooldown": {"disable": false, "transient_error_seconds": 90, "model_level": true}
        })));
        assert!(!provider.disable);
        assert_eq!(provider.transient_error_seconds, 90);
        assert!(provider.model_level);

        let key_override = json!({"cooldown": {"disable": true}});
        let merged = apply_key_cooldown_override(provider, key_override.as_object());
        assert!(merged.disable);
        assert_eq!(merged.transient_error_seconds, 90);
        assert!(merged.model_level);
    }

    #[test]
    fn normalization_rejects_unknown_fields_and_bad_types() {
        assert!(normalize_provider_cooldown_config(&json!({"disable": "maybe"})).is_err());
        assert!(normalize_provider_cooldown_config(&json!({"unknown": 1})).is_err());
        assert!(normalize_provider_cooldown_config(&json!([])).is_err());
        assert_eq!(
            normalize_provider_cooldown_config(&json!({
                "disable": "true",
                "transient_error_seconds": "120",
                "model_level": false
            }))
            .expect("valid config"),
            json!({"disable": true, "transient_error_seconds": 120, "model_level": false})
        );
    }

    #[test]
    fn short_retry_after_asks_for_an_immediate_same_key_retry() {
        let headers = headers(&[("Retry-After", "2")]);
        let decision = decide_provider_cooldown(input("custom", 429, Some(&headers), None));
        assert_eq!(decision.action, CooldownAction::ImmediateRetry);
        assert_eq!(decision.ttl_seconds, 0);
        assert!(!decision.writes_cooldown());
        assert_eq!(decision.hint.source, RetryHintSource::RetryAfterHeader);

        // 401 带一个短 Retry-After 不算「立即重试」信号。
        let decision = decide_provider_cooldown(input("custom", 401, Some(&headers), None));
        assert_eq!(decision.action, CooldownAction::Cooldown);
        assert_eq!(decision.ttl_seconds, AUTH_FAILURE_COOLDOWN_SECONDS);
    }

    #[test]
    fn medium_retry_after_cools_down_to_the_hint() {
        let headers = headers(&[("Retry-After", "120")]);
        let decision = decide_provider_cooldown(input("custom", 429, Some(&headers), None));
        assert_eq!(decision.action, CooldownAction::Cooldown);
        assert_eq!(decision.ttl_seconds, 120);
        assert_eq!(decision.reason, "retry_after_header");
        assert_eq!(decision.cooldown_until_unix_secs(NOW), Some(NOW + 120));
    }

    #[test]
    fn long_hint_is_quota_exhaustion_with_reset_at() {
        let reset_at = NOW + 3 * 3600;
        let headers = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &reset_at.to_string(),
            ),
        ]);
        let decision = decide_provider_cooldown(input("claude_code", 429, Some(&headers), None));
        assert_eq!(decision.action, CooldownAction::QuotaExhausted);
        assert_eq!(decision.reason, "ratelimit_window_5h");
        assert!(decision.ttl_seconds >= 3 * 3600 && decision.ttl_seconds <= 3 * 3600 + 30);
        assert!(decision
            .hint
            .reset_at_unix_secs
            .is_some_and(|value| value >= reset_at));
        assert!(super::decision_is_quota_window_signal(&decision));
    }

    #[test]
    fn no_hint_429_walks_the_backoff_ladder_once_per_window() {
        let first = decide_provider_cooldown(input("custom", 429, None, None));
        assert_eq!(first.action, CooldownAction::Cooldown);
        assert_eq!(first.ttl_seconds, 30);
        assert_eq!(first.reason, "backoff_level_0");
        assert_eq!(first.next_backoff_level, Some(1));

        // 同一窗口内的并发失败：不升级，复用当前窗口。
        let mut concurrent = input("custom", 429, None, None);
        concurrent.previous_backoff_level = Some(1);
        concurrent.active_cooldown_ttl_seconds = Some(12);
        let concurrent = decide_provider_cooldown(concurrent);
        assert_eq!(concurrent.reason, "backoff_level_0");
        assert_eq!(concurrent.ttl_seconds, 30);
        assert_eq!(concurrent.next_backoff_level, None);

        let mut second = input("custom", 429, None, None);
        second.previous_backoff_level = Some(1);
        let second = decide_provider_cooldown(second);
        assert_eq!(second.ttl_seconds, 60);
        assert_eq!(second.reason, "backoff_level_1");
        assert_eq!(second.next_backoff_level, Some(2));

        let mut third = input("custom", 429, None, None);
        third.previous_backoff_level = Some(2);
        let third = decide_provider_cooldown(third);
        assert_eq!(third.ttl_seconds, 120);
        assert_eq!(third.next_backoff_level, Some(3));

        assert_eq!(
            rate_limit_backoff_seconds(10),
            RATE_LIMIT_BACKOFF_MAX_SECONDS
        );
        let mut capped = input("custom", 429, None, None);
        capped.previous_backoff_level = Some(10);
        let capped = decide_provider_cooldown(capped);
        assert_eq!(capped.ttl_seconds, RATE_LIMIT_BACKOFF_MAX_SECONDS);
        assert_eq!(capped.next_backoff_level, Some(10));
    }

    #[test]
    fn status_code_table_matches_the_plan() {
        for (status, reason, ttl) in [
            (401, "auth_failed_401", AUTH_FAILURE_COOLDOWN_SECONDS),
            (402, "payment_required_402", AUTH_FAILURE_COOLDOWN_SECONDS),
            (403, "forbidden_403", AUTH_FAILURE_COOLDOWN_SECONDS),
            (404, "not_found_404", NOT_FOUND_COOLDOWN_SECONDS),
            (
                500,
                "server_error_500",
                DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS,
            ),
            (
                408,
                "request_timeout_408",
                DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS,
            ),
            (
                522,
                "transient_upstream_522",
                DEFAULT_TRANSIENT_ERROR_COOLDOWN_SECONDS,
            ),
            (529, "overloaded_529", 30),
        ] {
            let decision = decide_provider_cooldown(input("custom", status, None, None));
            assert_eq!(decision.action, CooldownAction::Cooldown, "status {status}");
            assert_eq!(decision.reason, reason, "status {status}");
            assert_eq!(decision.ttl_seconds, ttl, "status {status}");
        }
        let decision = decide_provider_cooldown(input("custom", 400, None, None));
        assert_eq!(decision.action, CooldownAction::None);
    }

    #[test]
    fn not_found_is_scoped_to_the_model_not_the_whole_key() {
        let decision = decide_provider_cooldown(input("custom", 404, None, None));
        assert_eq!(decision.action, CooldownAction::Cooldown);
        assert_eq!(decision.reason, "not_found_404");
        assert_eq!(decision.scope, RetryHintScope::KeyModel);
    }

    #[test]
    fn disabled_transient_cooldown_ignores_retry_after_too() {
        let headers = headers(&[("Retry-After", "45")]);
        let mut disabled = input("custom", 503, Some(&headers), None);
        disabled.config.transient_error_seconds = 0;
        let decision = decide_provider_cooldown(disabled);
        assert_eq!(decision.action, CooldownAction::None);
        assert_eq!(decision.reason, "transient_cooldown_disabled");
    }

    #[test]
    fn transient_cooldown_seconds_is_configurable_and_hint_caps_apply() {
        let mut configured = input("custom", 502, None, None);
        configured.config.transient_error_seconds = 15;
        let decision = decide_provider_cooldown(configured);
        assert_eq!(decision.ttl_seconds, 15);

        let headers = headers(&[("Retry-After", "7200")]);
        let decision = decide_provider_cooldown(input("custom", 503, Some(&headers), None));
        assert_eq!(decision.action, CooldownAction::Cooldown);
        assert_eq!(decision.ttl_seconds, 30 * 60);
        assert_eq!(decision.reason, "retry_after_header");
    }

    #[test]
    fn disable_switches_turn_everything_off() {
        let mut disabled = input("custom", 429, None, None);
        disabled.config.disable = true;
        assert_eq!(
            decide_provider_cooldown(disabled).action,
            CooldownAction::None
        );

        let mut no_rate_limit = input("custom", 429, None, None);
        no_rate_limit.rate_limit_cooldown_enabled = false;
        assert_eq!(
            decide_provider_cooldown(no_rate_limit).action,
            CooldownAction::None
        );

        let mut no_overload = input("custom", 529, None, None);
        no_overload.overload_cooldown_seconds = 0;
        assert_eq!(
            decide_provider_cooldown(no_overload).action,
            CooldownAction::None
        );
    }

    #[test]
    fn google_quota_signal_keeps_model_scope_for_antigravity() {
        let body = r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"Quota exhausted.","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"45s"}]}}"#;
        let decision = decide_provider_cooldown(input("antigravity", 429, None, Some(body)));
        assert_eq!(decision.action, CooldownAction::Cooldown);
        assert_eq!(decision.ttl_seconds, 45);
        assert_eq!(decision.reason, "google_retry_info");
        assert_eq!(decision.scope, RetryHintScope::KeyModel);
    }
}
