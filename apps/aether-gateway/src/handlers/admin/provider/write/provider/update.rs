use crate::handlers::admin::provider::pool::cooldown::{
    normalize_provider_cooldown_config, PROVIDER_COOLDOWN_CONFIG_KEY,
};
use crate::handlers::admin::provider::shared::payloads::AdminProviderUpdatePatch;
use crate::handlers::admin::provider::shared::support::{
    normalize_provider_billing_type, normalize_provider_quota_reservation,
    normalize_provider_quota_windows, normalize_provider_transfer_limit,
    normalize_provider_transfer_limit_json, parse_optional_rfc3339_unix_secs,
    PROVIDER_MAX_TRANSFER_COUNT_CONFIG_KEY, PROVIDER_MAX_TRANSFER_TIMEOUT_SECONDS_CONFIG_KEY,
    PROVIDER_QUOTA_RESERVATION_CONFIG_KEY, PROVIDER_QUOTA_WINDOWS_CONFIG_KEY,
};
use crate::handlers::admin::provider::write::normalize::normalize_chat_pii_redaction_config;
use crate::handlers::admin::provider::write::normalize::normalize_pool_advanced_config;
use crate::handlers::admin::provider::write::normalize::normalize_provider_type_input;
use crate::handlers::admin::provider::write::normalize::set_responses_websocket_enabled;
use crate::handlers::admin::provider::write::normalize::validate_responses_websocket_config;
use crate::handlers::admin::request::AdminAppState;
use crate::handlers::admin::shared::normalize_json_object;
use aether_admin::provider::redaction::{
    admin_restore_secret_safe_json, admin_restore_secret_safe_proxy,
};
use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) async fn build_admin_update_provider_record(
    state: &AdminAppState<'_>,
    existing: &StoredProviderCatalogProvider,
    patch: AdminProviderUpdatePatch,
) -> Result<StoredProviderCatalogProvider, String> {
    let state = state.as_ref();
    let mut updated = existing.clone();
    let (fields, payload) = patch.into_parts();

    if fields.contains("name") {
        let Some(name) = payload.name.as_deref() else {
            return Err(if fields.is_null("name") {
                "name 不能为空".to_string()
            } else {
                "name 必须是字符串".to_string()
            });
        };
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name 不能为空".to_string());
        }
        let duplicate = state
            .list_provider_catalog_providers(false)
            .await
            .map_err(|err| format!("{err:?}"))?
            .into_iter()
            .any(|provider| provider.id != existing.id && provider.name == trimmed);
        if duplicate {
            return Err(format!("提供商名称 '{trimmed}' 已存在"));
        }
        updated.name = trimmed.to_string();
    }

    let target_provider_type = if fields.contains("provider_type") {
        let Some(provider_type) = payload.provider_type.as_deref() else {
            return Err(if fields.is_null("provider_type") {
                "provider_type 不能为空".to_string()
            } else {
                "provider_type 必须是字符串".to_string()
            });
        };
        let normalized = normalize_provider_type_input(provider_type)?;
        updated.provider_type = normalized.clone();
        normalized
    } else {
        updated.provider_type.clone()
    };

    if fields.contains("description") {
        updated.description = payload
            .description
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }

    if fields.contains("website") {
        updated.website = match payload.website {
            None => {
                if fields.is_null("website") {
                    None
                } else {
                    return Err("website 必须是字符串".to_string());
                }
            }
            Some(website) => {
                let trimmed = website.trim();
                if trimmed.is_empty() {
                    None
                } else if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                    return Err("website 必须以 http:// 或 https:// 开头".to_string());
                } else {
                    Some(trimmed.to_string())
                }
            }
        };
    }

    if fields.contains("billing_type") {
        let Some(billing_type) = payload.billing_type.as_deref() else {
            return Err(if fields.is_null("billing_type") {
                "billing_type 不能为空".to_string()
            } else {
                "billing_type 必须是字符串".to_string()
            });
        };
        updated.billing_type = Some(normalize_provider_billing_type(billing_type)?);
    }

    if fields.contains("monthly_quota_usd") {
        if fields.is_null("monthly_quota_usd") {
            updated.monthly_quota_usd = None;
        } else {
            let Some(monthly_quota_usd) = payload.monthly_quota_usd else {
                return Err("monthly_quota_usd 必须是非负数".to_string());
            };
            if !monthly_quota_usd.is_finite() || monthly_quota_usd < 0.0 {
                return Err("monthly_quota_usd 必须是非负数".to_string());
            }
            updated.monthly_quota_usd = Some(monthly_quota_usd);
        }
    }

    if fields.contains("quota_reset_day") {
        if fields.is_null("quota_reset_day") {
            updated.quota_reset_day = None;
        } else {
            let Some(quota_reset_day) = payload.quota_reset_day else {
                return Err("quota_reset_day 必须是 1 到 30 之间的整数".to_string());
            };
            if !(1..=30).contains(&quota_reset_day) {
                return Err("quota_reset_day 必须是 1 到 30 之间的整数".to_string());
            }
            updated.quota_reset_day = Some(quota_reset_day);
        }
    }

    if existing.billing_type.as_deref() == Some("monthly_quota")
        && existing.quota_last_reset_at_unix_secs.is_some()
        && updated.quota_reset_day != existing.quota_reset_day
    {
        return Err(
            "请通过订阅配额中的调整周期操作修改周期长度，并选择生效时间和是否清零用量".to_string(),
        );
    }
    if fields.contains("quota_last_reset_at") {
        let value = payload
            .quota_last_reset_at
            .as_deref()
            .map(|v| parse_optional_rfc3339_unix_secs(v, "quota_last_reset_at"))
            .transpose()?;
        if existing.quota_last_reset_at_unix_secs.is_some()
            && value.map(|v| v / 60) != existing.quota_last_reset_at_unix_secs.map(|v| v / 60)
        {
            return Err("请通过订阅配额中的调整周期操作修改当前周期起点".to_string());
        }
        if existing.quota_last_reset_at_unix_secs.is_none() {
            updated.quota_last_reset_at_unix_secs = value;
        }
    }
    if fields.contains("quota_subscription_started_at") {
        let raw = payload
            .quota_subscription_started_at
            .as_deref()
            .ok_or("订阅开始时间不能为空")?;
        updated.quota_subscription_started_at_unix_secs =
            Some(parse_optional_rfc3339_unix_secs(raw, "quota_subscription_started_at")? / 60 * 60);
    }
    if updated.billing_type.as_deref() == Some("monthly_quota")
        && updated.quota_last_reset_at_unix_secs.is_none()
    {
        let start = updated
            .quota_subscription_started_at_unix_secs
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    / 60
                    * 60
            });
        updated.quota_last_reset_at_unix_secs = Some(start);
        updated.quota_subscription_started_at_unix_secs = Some(start);
        updated.quota_cycle_start_at_unix_secs = Some(start);
    }

    if fields.contains("quota_expires_at") {
        if fields.is_null("quota_expires_at") {
            updated.quota_expires_at_unix_secs = None;
        } else {
            let Some(raw) = payload.quota_expires_at.as_deref() else {
                return Err("quota_expires_at 必须是字符串".to_string());
            };
            updated.quota_expires_at_unix_secs =
                Some(parse_optional_rfc3339_unix_secs(raw, "quota_expires_at")?);
        }
    }

    if fields.contains("provider_priority") {
        let Some(provider_priority) = payload.provider_priority else {
            return Err(if fields.is_null("provider_priority") {
                "provider_priority 不能为空".to_string()
            } else {
                "provider_priority 必须是整数".to_string()
            });
        };
        if !(0..=10_000).contains(&provider_priority) {
            return Err("provider_priority 必须在 0 到 10000 之间".to_string());
        }
        updated.provider_priority = provider_priority;
    }

    if fields.contains("keep_priority_on_conversion") {
        let Some(keep_priority_on_conversion) = payload.keep_priority_on_conversion else {
            return Err("keep_priority_on_conversion 必须是布尔值".to_string());
        };
        updated.keep_priority_on_conversion = keep_priority_on_conversion;
    }

    if fields.contains("is_active") {
        let Some(is_active) = payload.is_active else {
            return Err("is_active 必须是布尔值".to_string());
        };
        updated.is_active = is_active;
    }

    if fields.contains("concurrent_limit") {
        updated.concurrent_limit = match payload.concurrent_limit {
            Some(value) if value >= 0 => Some(value),
            Some(_) => return Err("concurrent_limit 必须是非负整数".to_string()),
            None => None,
        };
    }

    if fields.contains("max_retries") {
        updated.max_retries = match payload.max_retries {
            Some(value) if (0..=999).contains(&value) => Some(value),
            Some(_) => return Err("max_retries 必须是 0 到 999 之间的整数".to_string()),
            None => None,
        };
    }

    if fields.contains("proxy") {
        updated.proxy = normalize_json_object(payload.proxy, "proxy")?
            .map(|value| admin_restore_secret_safe_proxy(existing.proxy.as_ref(), &value));
    }

    if fields.contains("stream_first_byte_timeout") {
        updated.stream_first_byte_timeout_secs =
            super::normalize_provider_stream_first_byte_timeout(payload.stream_first_byte_timeout)?;
    }

    if fields.contains("request_timeout") {
        updated.request_timeout_secs =
            super::normalize_provider_request_timeout(payload.request_timeout)?;
    }

    if fields.contains("enable_format_conversion") {
        let Some(enable_format_conversion) = payload.enable_format_conversion else {
            return Err("enable_format_conversion 必须是布尔值".to_string());
        };
        updated.enable_format_conversion = enable_format_conversion;
    }

    let mut config_map = updated
        .config
        .clone()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let sensitive_words_config_present = payload
        .config
        .as_ref()
        .and_then(|config| config.pointer("/cloak/sensitive_words"))
        .is_some();
    if fields.contains("config") {
        if fields.is_null("config") {
            config_map.clear();
        } else {
            let value = normalize_json_object(payload.config, "config")?
                .ok_or_else(|| "config 必须是 JSON 对象".to_string())?;
            let value = admin_restore_secret_safe_json(existing.config.as_ref(), &value);
            let serde_json::Value::Object(patch_map) = value else {
                return Err("config 必须是 JSON 对象".to_string());
            };
            for (key, value) in patch_map {
                if value.is_null() {
                    config_map.remove(&key);
                } else {
                    config_map.insert(key, value);
                }
            }
        }
    }

    if fields.contains("codex_fingerprint_convergence_enabled") {
        let Some(enabled) = payload.codex_fingerprint_convergence_enabled else {
            return Err("codex_fingerprint_convergence_enabled 必须是布尔值".to_string());
        };
        if target_provider_type != "codex" && enabled {
            return Err(
                "codex_fingerprint_convergence_enabled 仅适用于 provider_type=codex".to_string(),
            );
        }
        if target_provider_type == "codex" {
            let codex_config = config_map
                .entry(crate::provider_transport::CODEX_FINGERPRINT_CONFIG_NAMESPACE.to_string())
                .or_insert_with(|| json!({}));
            let Some(codex_config) = codex_config.as_object_mut() else {
                return Err("config.codex 必须是 JSON 对象".to_string());
            };
            codex_config.insert(
                crate::provider_transport::CODEX_FINGERPRINT_ENABLED_CONFIG_KEY.to_string(),
                json!(enabled),
            );
        }
    }
    if target_provider_type != "codex" {
        remove_codex_fingerprint_config(&mut config_map);
    }
    if fields.contains("claude_code_cloak_mode") {
        if fields.is_null("claude_code_cloak_mode") {
            remove_claude_code_cloak_mode(&mut config_map);
        } else {
            let mode = payload
                .claude_code_cloak_mode
                .as_deref()
                .ok_or_else(|| "claude_code_cloak_mode 必须是字符串".to_string())?;
            set_claude_code_cloak_mode(&mut config_map, &target_provider_type, mode)?;
        }
    }
    if target_provider_type != "claude_code" {
        remove_claude_code_cloak_mode(&mut config_map);
    }
    if fields.contains(CLOAK_SENSITIVE_WORDS_FIELD) {
        if fields.is_null(CLOAK_SENSITIVE_WORDS_FIELD) {
            remove_provider_cloak_sensitive_words(&mut config_map);
        } else {
            let words = payload
                .cloak_sensitive_words
                .as_deref()
                .ok_or_else(|| "cloak_sensitive_words 必须是字符串数组".to_string())?;
            set_provider_cloak_sensitive_words(&mut config_map, &target_provider_type, words)?;
        }
    } else if provider_type_supports_sensitive_words(&target_provider_type)
        || sensitive_words_config_present
    {
        if let Some(raw_words) = config_map
            .get(crate::provider_transport::CLOAK_CONFIG_NAMESPACE)
            .and_then(|cloak| {
                cloak.get(crate::provider_transport::CLOAK_SENSITIVE_WORDS_CONFIG_KEY)
            })
            .cloned()
        {
            // 显式写入的 config 仍需校验；切换类型时遗留词表由下面清理。
            let words = raw_words
                .as_array()
                .ok_or_else(|| "config.cloak.sensitive_words 必须是字符串数组".to_string())?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| "config.cloak.sensitive_words 必须是字符串数组".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            set_provider_cloak_sensitive_words(&mut config_map, &target_provider_type, &words)?;
        }
    }
    if !provider_type_supports_sensitive_words(&target_provider_type) {
        remove_provider_cloak_sensitive_words(&mut config_map);
    }
    if fields.contains(PROVIDER_TRANSPORT_PROFILE_FIELD) {
        if fields.is_null(PROVIDER_TRANSPORT_PROFILE_FIELD) {
            remove_provider_transport_profile(&mut config_map);
        } else {
            let profile_id = payload
                .transport_profile
                .as_deref()
                .ok_or_else(|| "transport_profile 必须是字符串".to_string())?;
            set_provider_transport_profile(&mut config_map, &target_provider_type, profile_id)?;
        }
    }
    if fields.contains(PROVIDER_COOLDOWN_CONFIG_KEY) {
        if fields.is_null(PROVIDER_COOLDOWN_CONFIG_KEY) {
            config_map.remove(PROVIDER_COOLDOWN_CONFIG_KEY);
        } else {
            let value = payload
                .cooldown
                .as_ref()
                .ok_or_else(|| "cooldown 必须是 JSON 对象".to_string())?;
            let value = normalize_provider_cooldown_config(value)?;
            if value.as_object().is_some_and(|object| object.is_empty()) {
                config_map.remove(PROVIDER_COOLDOWN_CONFIG_KEY);
            } else {
                config_map.insert(PROVIDER_COOLDOWN_CONFIG_KEY.to_string(), value);
            }
        }
    } else if let Some(raw_cooldown) = config_map.get(PROVIDER_COOLDOWN_CONFIG_KEY).cloned() {
        // `config` 整体写入时也校验形状，避免手写 JSON 把非法值带进去。
        config_map.insert(
            PROVIDER_COOLDOWN_CONFIG_KEY.to_string(),
            normalize_provider_cooldown_config(&raw_cooldown)?,
        );
    }
    if fields.contains("quota_windows") {
        if fields.is_null("quota_windows") {
            // Keep no empty policy object around; this also makes switching back to an ordinary
            // pay-as-you-go provider remove the window enforcement cleanly.
            config_map.remove(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY);
        } else {
            let value = serde_json::to_value(payload.quota_windows.as_ref())
                .map_err(|err| format!("quota_windows 无法解析: {err}"))?;
            let value = normalize_provider_quota_windows(Some(&value))?;
            if value.as_array().is_some_and(|entries| entries.is_empty()) {
                config_map.remove(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY);
            } else {
                config_map.insert(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY.to_string(), value);
            }
        }
    }
    if let Some(raw_windows) = config_map.get(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY).cloned() {
        let value = normalize_provider_quota_windows(Some(&raw_windows))?;
        if value.as_array().is_some_and(|entries| entries.is_empty()) {
            config_map.remove(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY);
        } else {
            config_map.insert(PROVIDER_QUOTA_WINDOWS_CONFIG_KEY.to_string(), value);
        }
    }
    if fields.contains("quota_reservation") {
        // `null` or an empty object restores the billing defaults.
        match normalize_provider_quota_reservation(payload.quota_reservation.as_ref())? {
            Some(value) => {
                config_map.insert(PROVIDER_QUOTA_RESERVATION_CONFIG_KEY.to_string(), value);
            }
            None => {
                config_map.remove(PROVIDER_QUOTA_RESERVATION_CONFIG_KEY);
            }
        }
    }
    if let Some(raw_reservation) = config_map
        .get(PROVIDER_QUOTA_RESERVATION_CONFIG_KEY)
        .cloned()
    {
        match normalize_provider_quota_reservation(Some(&raw_reservation))? {
            Some(value) => {
                config_map.insert(PROVIDER_QUOTA_RESERVATION_CONFIG_KEY.to_string(), value);
            }
            None => {
                config_map.remove(PROVIDER_QUOTA_RESERVATION_CONFIG_KEY);
            }
        }
    }

    for (field_name, payload_value) in [
        (
            PROVIDER_MAX_TRANSFER_COUNT_CONFIG_KEY,
            payload.max_transfer_count,
        ),
        (
            PROVIDER_MAX_TRANSFER_TIMEOUT_SECONDS_CONFIG_KEY,
            payload.max_transfer_timeout_seconds,
        ),
    ] {
        if fields.contains(field_name) {
            let value = payload_value
                .map(|value| normalize_provider_transfer_limit(value, field_name))
                .transpose()?
                .unwrap_or(0);
            config_map.insert(field_name.to_string(), json!(value));
        } else if fields.contains("config") {
            if let Some(value) = config_map.get(field_name) {
                let value = normalize_provider_transfer_limit_json(value, field_name)?;
                config_map.insert(field_name.to_string(), json!(value));
            }
        }
    }

    if fields.contains("claude_code_advanced") {
        if fields.is_null("claude_code_advanced") {
            config_map.remove("claude_code_advanced");
        } else {
            if target_provider_type != "claude_code" {
                return Err("claude_code_advanced 仅适用于 provider_type=claude_code".to_string());
            }
            let value =
                normalize_json_object(payload.claude_code_advanced, "claude_code_advanced")?
                    .ok_or_else(|| "claude_code_advanced 必须是 JSON 对象".to_string())?;
            let value = admin_restore_secret_safe_json(
                existing
                    .config
                    .as_ref()
                    .and_then(|config| config.get("claude_code_advanced")),
                &value,
            );
            config_map.insert("claude_code_advanced".to_string(), value);
        }
    } else if target_provider_type != "claude_code" {
        config_map.remove("claude_code_advanced");
    }

    if fields.contains("pool_advanced") {
        if fields.is_null("pool_advanced") {
            config_map.remove("pool_advanced");
        } else {
            let value = normalize_pool_advanced_config(payload.pool_advanced)?
                .ok_or_else(|| "pool_advanced 必须是 JSON 对象".to_string())?;
            let value = admin_restore_secret_safe_json(
                existing
                    .config
                    .as_ref()
                    .and_then(|config| config.get("pool_advanced")),
                &value,
            );
            config_map.insert("pool_advanced".to_string(), value);
        }
    }

    if fields.contains("failover_rules") {
        if fields.is_null("failover_rules") {
            config_map.remove("failover_rules");
        } else {
            let value = normalize_json_object(payload.failover_rules, "failover_rules")?
                .ok_or_else(|| "failover_rules 必须是 JSON 对象".to_string())?;
            let value = admin_restore_secret_safe_json(
                existing
                    .config
                    .as_ref()
                    .and_then(|config| config.get("failover_rules")),
                &value,
            );
            config_map.insert("failover_rules".to_string(), value);
        }
    }

    if config_map.contains_key("chat_pii_redaction") {
        let value = normalize_chat_pii_redaction_config(config_map.remove("chat_pii_redaction"))?;
        if let Some(value) = value {
            config_map.insert("chat_pii_redaction".to_string(), value);
        }
    }

    if fields.contains("responses_websocket_enabled") {
        let enabled = payload
            .responses_websocket_enabled
            .ok_or_else(|| "responses_websocket_enabled 必须是布尔值".to_string())?;
        set_responses_websocket_enabled(&mut config_map, enabled)?;
    }
    validate_responses_websocket_config(&config_map)?;

    updated.config = (!config_map.is_empty()).then_some(serde_json::Value::Object(config_map));
    crate::provider_transport::validate_anthropic_compatibility_profile_config(
        updated.config.as_ref(),
    )
    .map_err(|_| "无效的 Anthropic compatibility profile".to_string())?;

    updated.updated_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs());
    Ok(updated)
}

fn remove_codex_fingerprint_config(config_map: &mut serde_json::Map<String, serde_json::Value>) {
    let namespace = crate::provider_transport::CODEX_FINGERPRINT_CONFIG_NAMESPACE;
    let key = crate::provider_transport::CODEX_FINGERPRINT_ENABLED_CONFIG_KEY;
    let mut remove_namespace = false;
    if let Some(codex_config) = config_map
        .get_mut(namespace)
        .and_then(|value| value.as_object_mut())
    {
        codex_config.remove(key);
        remove_namespace = codex_config.is_empty();
    }
    if remove_namespace {
        config_map.remove(namespace);
    }
}

/// 管理端字段名：供应商 / Key 的传输指纹 profile。
pub(crate) const PROVIDER_TRANSPORT_PROFILE_FIELD: &str = "transport_profile";

/// 该供应商类型允许选择的内置 TLS 仿真 profile。控制面 profile 由网关按供应商类型自动选择，
/// 不作为可选项暴露。
pub(crate) fn allowed_transport_profiles_for_provider_type(
    provider_type: &str,
) -> &'static [&'static str] {
    crate::provider_transport::claude_code::selectable_tls_emulation_profiles_for_provider_type(
        provider_type,
    )
}

pub(crate) fn provider_type_supports_transport_profile(provider_type: &str) -> bool {
    !allowed_transport_profiles_for_provider_type(provider_type).is_empty()
}

/// 校验并归一化管理端提交的传输指纹 profile id。
pub(crate) fn normalize_admin_transport_profile(
    provider_type: &str,
    profile_id: &str,
) -> Result<&'static str, String> {
    let allowed = allowed_transport_profiles_for_provider_type(provider_type);
    if allowed.is_empty() {
        return Err("transport_profile 仅适用于 provider_type=claude_code / codex".to_string());
    }
    let normalized =
        crate::provider_transport::claude_code::normalize_claude_code_tls_profile_id(profile_id);
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == normalized)
        .ok_or_else(|| format!("transport_profile 必须是 {} 之一", allowed.join(" / ")))
}

/// 写 `config.fingerprint.transport_profile`；保留 `fingerprint` 对象里其它键。
pub(crate) fn set_provider_transport_profile(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
    provider_type: &str,
    profile_id: &str,
) -> Result<(), String> {
    let profile_id = normalize_admin_transport_profile(provider_type, profile_id)?;
    let fingerprint = config_map
        .entry("fingerprint".to_string())
        .or_insert_with(|| json!({}));
    let Some(fingerprint) = fingerprint.as_object_mut() else {
        return Err("config.fingerprint 必须是 JSON 对象".to_string());
    };
    fingerprint.insert(
        PROVIDER_TRANSPORT_PROFILE_FIELD.to_string(),
        json!(profile_id),
    );
    Ok(())
}

pub(crate) fn remove_provider_transport_profile(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    let remove_container = match config_map
        .get_mut("fingerprint")
        .and_then(|value| value.as_object_mut())
    {
        Some(fingerprint) => {
            fingerprint.remove(PROVIDER_TRANSPORT_PROFILE_FIELD);
            fingerprint.is_empty()
        }
        None => false,
    };
    if remove_container {
        config_map.remove("fingerprint");
    }
}

/// 回读 `config.fingerprint.transport_profile`（字符串或 `{profile_id}` 对象）。
pub(crate) fn provider_transport_profile_id(config: Option<&serde_json::Value>) -> Option<String> {
    transport_profile_id_from_fingerprint(config?.get("fingerprint"))
}

/// 从 `fingerprint` 对象里取 `transport_profile` 的 id。
pub(crate) fn transport_profile_id_from_fingerprint(
    fingerprint: Option<&serde_json::Value>,
) -> Option<String> {
    crate::provider_transport::configured_transport_profile_id_from_fingerprint(fingerprint)
}

/// 写 `config.cloak.mode`；只保留 `cloak` 对象里其它键（例如 P6 的 `sensitive_words`）。
pub(crate) fn set_claude_code_cloak_mode(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
    provider_type: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(mode) = crate::provider_transport::claude_code::ClaudeCodeCloakMode::parse(mode)
    else {
        return Err("claude_code_cloak_mode 必须是 auto / always / off".to_string());
    };
    if provider_type != "claude_code" {
        return Err("claude_code_cloak_mode 仅适用于 provider_type=claude_code".to_string());
    }
    let cloak = config_map
        .entry(crate::provider_transport::claude_code::CLAUDE_CODE_CLOAK_CONFIG_KEY.to_string())
        .or_insert_with(|| json!({}));
    let Some(cloak) = cloak.as_object_mut() else {
        return Err("config.cloak 必须是 JSON 对象".to_string());
    };
    cloak.insert(
        crate::provider_transport::claude_code::CLAUDE_CODE_CLOAK_MODE_CONFIG_KEY.to_string(),
        json!(mode.as_str()),
    );
    Ok(())
}

pub(crate) fn remove_claude_code_cloak_mode(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    let cloak_key = crate::provider_transport::claude_code::CLAUDE_CODE_CLOAK_CONFIG_KEY;
    let mode_key = crate::provider_transport::claude_code::CLAUDE_CODE_CLOAK_MODE_CONFIG_KEY;
    let remove_container = match config_map
        .get_mut(cloak_key)
        .and_then(|v| v.as_object_mut())
    {
        Some(cloak) => {
            cloak.remove(mode_key);
            cloak.is_empty()
        }
        None => false,
    };
    if remove_container {
        config_map.remove(cloak_key);
    }
}

/// 管理端字段名：供应商 `cloak_sensitive_words`，落到 `config.cloak.sensitive_words`。
pub(crate) const CLOAK_SENSITIVE_WORDS_FIELD: &str = "cloak_sensitive_words";

/// 敏感词混淆只对 claude_code 与 antigravity 有作用范围（transport crate 的 `sensitive_words.rs`）。
pub(crate) fn provider_type_supports_sensitive_words(provider_type: &str) -> bool {
    crate::provider_transport::provider_type_supports_sensitive_words(provider_type)
}

/// 归一化词表：去空、去重（不区分大小写）、按长度降序；每个词 2–256 个字符，
/// 不得含零宽字符，最多 256 条。返回归一化后的数组；校验失败返回中文错误。
pub(crate) fn normalize_cloak_sensitive_words(words: &[String]) -> Result<Vec<String>, String> {
    use crate::provider_transport::{
        normalize_sensitive_word, SensitiveWordList, SENSITIVE_WORD_MAX_CHARS,
        SENSITIVE_WORD_MAX_ENTRIES, SENSITIVE_WORD_MIN_CHARS, SENSITIVE_WORD_ZERO_WIDTH,
    };
    for word in words {
        let trimmed = word.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().count() < SENSITIVE_WORD_MIN_CHARS {
            return Err(format!(
                "敏感词「{trimmed}」过短，每个词至少 {SENSITIVE_WORD_MIN_CHARS} 个字符"
            ));
        }
        if trimmed.chars().count() > SENSITIVE_WORD_MAX_CHARS {
            return Err(format!("每个敏感词最多 {SENSITIVE_WORD_MAX_CHARS} 个字符"));
        }
        if trimmed.contains(SENSITIVE_WORD_ZERO_WIDTH) {
            return Err(format!("敏感词「{trimmed}」不能包含零宽字符"));
        }
    }
    let list = SensitiveWordList::from_words(words.iter().map(|word| word.trim()));
    let unique = words
        .iter()
        .map(|word| normalize_sensitive_word(word.trim()))
        .filter(|word| !word.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() > SENSITIVE_WORD_MAX_ENTRIES {
        return Err(format!(
            "敏感词最多 {SENSITIVE_WORD_MAX_ENTRIES} 条，当前 {} 条",
            unique.len()
        ));
    }
    Ok(list.words().to_vec())
}

/// 写 `config.cloak.sensitive_words`；空词表等价于删除该键，但保留 `cloak.mode`。
pub(crate) fn set_provider_cloak_sensitive_words(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
    provider_type: &str,
    words: &[String],
) -> Result<(), String> {
    let normalized = normalize_cloak_sensitive_words(words)?;
    if !provider_type_supports_sensitive_words(provider_type) {
        if normalized.is_empty() {
            remove_provider_cloak_sensitive_words(config_map);
            return Ok(());
        }
        return Err(
            "cloak_sensitive_words 仅适用于 provider_type=claude_code / antigravity".to_string(),
        );
    }
    if normalized.is_empty() {
        remove_provider_cloak_sensitive_words(config_map);
        return Ok(());
    }
    let cloak = config_map
        .entry(crate::provider_transport::CLOAK_CONFIG_NAMESPACE.to_string())
        .or_insert_with(|| json!({}));
    let Some(cloak) = cloak.as_object_mut() else {
        return Err("config.cloak 必须是 JSON 对象".to_string());
    };
    cloak.insert(
        crate::provider_transport::CLOAK_SENSITIVE_WORDS_CONFIG_KEY.to_string(),
        json!(normalized),
    );
    Ok(())
}

pub(crate) fn remove_provider_cloak_sensitive_words(
    config_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    let cloak_key = crate::provider_transport::CLOAK_CONFIG_NAMESPACE;
    let words_key = crate::provider_transport::CLOAK_SENSITIVE_WORDS_CONFIG_KEY;
    let remove_container = match config_map
        .get_mut(cloak_key)
        .and_then(|v| v.as_object_mut())
    {
        Some(cloak) => {
            cloak.remove(words_key);
            cloak.is_empty()
        }
        None => false,
    };
    if remove_container {
        config_map.remove(cloak_key);
    }
}

/// 供应商词表是否实际变化（归一化后比较）；管理端据此返回「提示词缓存将失效」的提示。
pub(crate) fn provider_cloak_sensitive_words_changed(
    existing_config: Option<&serde_json::Value>,
    updated_config: Option<&serde_json::Value>,
) -> bool {
    crate::provider_transport::provider_sensitive_word_list(existing_config)
        != crate::provider_transport::provider_sensitive_word_list(updated_config)
}

/// 词表变化时附加到写入响应里的提示文案。
pub(crate) const CLOAK_SENSITIVE_WORDS_CHANGED_WARNING: &str = "敏感词词表已变更，提示词缓存将失效";

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[tokio::test]
    async fn changing_provider_type_removes_inherited_sensitive_words() {
        use crate::handlers::admin::provider::shared::payloads::AdminProviderUpdatePatch;
        use crate::handlers::admin::request::AdminAppState;
        use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;

        let app = crate::AppState::new().expect("gateway should build");
        let state = AdminAppState::new(&app);
        let mut existing = StoredProviderCatalogProvider::new(
            "provider-cloak".to_string(),
            "Claude Code".to_string(),
            None,
            "claude_code".to_string(),
        )
        .expect("provider should build");
        existing.config = Some(json!({
            "cloak": {"mode": "auto", "sensitive_words": ["proxy"]},
            "unrelated": true,
        }));
        let patch = |raw: serde_json::Value| {
            AdminProviderUpdatePatch::from_object(raw.as_object().unwrap().clone()).unwrap()
        };

        let updated = super::build_admin_update_provider_record(
            &state,
            &existing,
            patch(json!({"provider_type": "codex"})),
        )
        .await
        .expect("old provider-specific settings must not block a type change");
        assert_eq!(updated.provider_type, "codex");
        let config = updated.config.unwrap();
        assert!(config.get("cloak").is_none());
        assert_eq!(config["unrelated"], true);

        for raw in [
            json!({"provider_type": "codex", "cloak_sensitive_words": ["proxy"]}),
            json!({"provider_type": "codex", "config": {"cloak": {"sensitive_words": ["proxy"]}}}),
        ] {
            let error = super::build_admin_update_provider_record(&state, &existing, patch(raw))
                .await
                .expect_err("explicit unsupported settings still need validation");
            assert!(error.contains("仅适用于"), "{error}");
        }
    }

    #[test]
    fn transport_profile_setting_is_typed_and_provider_scoped() {
        let mut config = json!({"fingerprint": {"device_id": "keep-me"}})
            .as_object()
            .cloned()
            .unwrap();
        super::set_provider_transport_profile(
            &mut config,
            "claude_code",
            "Claude-Code-Node-OpenSSL",
        )
        .expect("claude_code may select the node profile");
        assert_eq!(
            config["fingerprint"],
            json!({"device_id": "keep-me", "transport_profile": "claude_code_node_openssl"})
        );
        assert_eq!(
            super::provider_transport_profile_id(Some(&serde_json::Value::Object(config.clone()))),
            Some("claude_code_node_openssl".to_string())
        );

        assert!(super::set_provider_transport_profile(
            &mut config,
            "codex",
            "claude_code_node_openssl"
        )
        .is_err());
        super::set_provider_transport_profile(&mut config, "codex", "chatgpt_com_chrome")
            .expect("codex may select chrome");
        assert!(super::set_provider_transport_profile(
            &mut config,
            "gemini_cli",
            "chatgpt_com_chrome"
        )
        .is_err());
        assert!(
            super::set_provider_transport_profile(&mut config, "claude_code", "chrome_136")
                .is_err()
        );
        // 控制面 profile 由网关自动选择，不允许手工选。
        assert!(super::set_provider_transport_profile(
            &mut config,
            "claude_code",
            "claude_code_oauth_control_plane"
        )
        .is_err());

        super::remove_provider_transport_profile(&mut config);
        assert_eq!(config["fingerprint"], json!({"device_id": "keep-me"}));
        let mut only_profile = json!({"fingerprint": {"transport_profile": "chatgpt_com_chrome"}})
            .as_object()
            .cloned()
            .unwrap();
        super::remove_provider_transport_profile(&mut only_profile);
        assert!(only_profile.get("fingerprint").is_none());
        assert_eq!(
            super::transport_profile_id_from_fingerprint(Some(&json!({
                "transport_profile": {"profile_id": "claude_code_node_openssl", "backend": "browser_wreq"}
            }))),
            Some("claude_code_node_openssl".to_string())
        );
    }

    #[test]
    fn removing_fingerprint_setting_preserves_other_codex_config() {
        let mut config = json!({
            "codex": {
                "fingerprint_convergence_enabled": true,
                "pass_through_cyber_flag_interrupt": true
            },
            "other": {"kept": true}
        })
        .as_object()
        .expect("config object")
        .clone();

        super::remove_codex_fingerprint_config(&mut config);

        assert_eq!(
            config["codex"],
            json!({"pass_through_cyber_flag_interrupt": true})
        );
        assert_eq!(config["other"], json!({"kept": true}));
    }

    #[test]
    fn sensitive_words_normalize_dedupe_and_keep_cloak_mode() {
        let mut config = json!({"cloak": {"mode": "always"}})
            .as_object()
            .expect("config object")
            .clone();
        super::set_provider_cloak_sensitive_words(
            &mut config,
            "claude_code",
            &[
                "Proxy".to_string(),
                " API ".to_string(),
                "proxy".to_string(),
                "".to_string(),
            ],
        )
        .expect("word list should be accepted");
        assert_eq!(
            config["cloak"],
            json!({"mode": "always", "sensitive_words": ["proxy", "api"]})
        );

        // 空数组 = 清空，但 mode 保留。
        super::set_provider_cloak_sensitive_words(&mut config, "antigravity", &[])
            .expect("empty list should clear");
        assert_eq!(config["cloak"], json!({"mode": "always"}));

        // 只剩词表时整个 cloak 对象一起移除。
        let mut only_words = json!({"cloak": {"sensitive_words": ["proxy"]}})
            .as_object()
            .expect("config object")
            .clone();
        super::remove_provider_cloak_sensitive_words(&mut only_words);
        assert!(only_words.get("cloak").is_none());
    }

    #[test]
    fn sensitive_words_reject_short_words_zero_width_and_other_provider_types() {
        let mut config = serde_json::Map::new();
        let error = super::set_provider_cloak_sensitive_words(
            &mut config,
            "claude_code",
            &["a".to_string()],
        )
        .expect_err("single-char word must be rejected");
        assert!(error.contains("过短"), "{error}");

        let error = super::set_provider_cloak_sensitive_words(
            &mut config,
            "claude_code",
            &["p\u{200B}roxy".to_string()],
        )
        .expect_err("zero-width word must be rejected");
        assert!(error.contains("零宽"), "{error}");

        let error =
            super::set_provider_cloak_sensitive_words(&mut config, "codex", &["proxy".to_string()])
                .expect_err("codex cannot carry a word list");
        assert!(error.contains("仅适用于"), "{error}");

        let too_many = (0..257).map(|i| format!("word{i}")).collect::<Vec<_>>();
        let error = super::normalize_cloak_sensitive_words(&too_many)
            .expect_err("more than 256 entries must be rejected");
        assert!(error.contains("最多"), "{error}");

        let max_chars = crate::provider_transport::SENSITIVE_WORD_MAX_CHARS;
        assert!(super::normalize_cloak_sensitive_words(&["界".repeat(max_chars)]).is_ok());
        let error = super::normalize_cloak_sensitive_words(&["界".repeat(max_chars + 1)])
            .expect_err("oversized words must not silently disable obfuscation");
        assert!(error.contains("最多"), "{error}");
    }

    #[test]
    fn sensitive_words_change_detection_compares_normalized_lists() {
        let before = json!({"cloak": {"sensitive_words": ["Proxy", "api"]}});
        let same = json!({"cloak": {"sensitive_words": ["api", "proxy"]}});
        let changed = json!({"cloak": {"sensitive_words": ["proxy"]}});
        assert!(!super::provider_cloak_sensitive_words_changed(
            Some(&before),
            Some(&same)
        ));
        assert!(super::provider_cloak_sensitive_words_changed(
            Some(&before),
            Some(&changed)
        ));
        assert!(super::provider_cloak_sensitive_words_changed(
            Some(&before),
            None
        ));
        assert!(!super::provider_cloak_sensitive_words_changed(None, None));
    }
}
