use crate::handlers::admin::provider::oauth::provisioning::rotate_codex_credential_generation;
use crate::handlers::admin::provider::pool::cooldown::{
    normalize_provider_cooldown_config, PROVIDER_COOLDOWN_CONFIG_KEY,
};
use crate::handlers::admin::provider::shared::payloads::AdminProviderKeyUpdatePatch;
use crate::handlers::admin::provider::write::normalize::{
    normalize_allow_auth_channel_mismatch_formats, normalize_api_format_json_object_keys,
    normalize_api_format_list, normalize_auth_type, normalize_auth_type_by_format,
    normalize_max_probe_interval_minutes, normalize_rate_multipliers,
    reconcile_allow_auth_channel_mismatch_formats, validate_vertex_api_formats,
};
use crate::handlers::admin::provider::write::provider::{
    CLOAK_SENSITIVE_WORDS_FIELD, PROVIDER_TRANSPORT_PROFILE_FIELD,
};
use crate::handlers::admin::request::AdminAppState;
use crate::handlers::admin::shared::{
    json_string_list, normalize_json_object, normalize_string_list, parse_catalog_auth_config_json,
};
use crate::handlers::shared::normalize_optional_api_key_concurrent_limit;
use crate::provider_key_auth::provider_key_is_oauth_managed;
use aether_admin::provider::redaction::{
    admin_restore_secret_safe_json, admin_restore_secret_safe_proxy,
};
use aether_data_contracts::repository::provider_catalog::{
    ProviderCatalogKeyAdminCasUpdate, ProviderCatalogKeyOAuthCredentialFence,
    StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use aether_provider_transport::provider_types::provider_type_is_fixed;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) async fn build_admin_update_provider_key_record(
    state: &AdminAppState<'_>,
    provider: &StoredProviderCatalogProvider,
    existing: &StoredProviderCatalogKey,
    patch: AdminProviderKeyUpdatePatch,
) -> Result<StoredProviderCatalogKey, String> {
    let existing_keys = state
        .as_ref()
        .list_provider_catalog_keys_by_provider_ids(std::slice::from_ref(&provider.id))
        .await
        .map_err(|err| format!("{err:?}"))?;
    build_admin_update_provider_key_record_with_existing_keys(
        state,
        provider,
        existing,
        &existing_keys,
        patch,
    )
}

pub(crate) fn build_admin_update_provider_key_record_with_existing_keys(
    state: &AdminAppState<'_>,
    provider: &StoredProviderCatalogProvider,
    existing: &StoredProviderCatalogKey,
    existing_keys: &[StoredProviderCatalogKey],
    patch: AdminProviderKeyUpdatePatch,
) -> Result<StoredProviderCatalogKey, String> {
    let state = state.as_ref();
    let mut updated = existing.clone();
    if provider.id != existing.provider_id {
        updated.encrypted_api_key = state
            .decrypt_provider_catalog_key_api_key(existing)
            .map_err(|_| "无法验证现有 provider API Key".to_string())?
            .map(|plaintext| {
                state
                    .seal_provider_catalog_key_api_key(&provider.id, &existing.id, &plaintext)
                    .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())
            })
            .transpose()?;
        updated.encrypted_auth_config = state
            .decrypt_provider_catalog_key_auth_config(existing)
            .map_err(|_| "无法验证现有 provider auth_config".to_string())?
            .map(|plaintext| {
                state
                    .seal_provider_catalog_key_auth_config(&provider.id, &existing.id, &plaintext)
                    .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())
            })
            .transpose()?;
        updated.provider_id = provider.id.clone();
    }
    let (fields, payload) = patch.into_parts();
    let auto_fetch_disabled =
        existing.auto_fetch_models && matches!(payload.auto_fetch_models, Some(false));
    let current_auth_type = normalize_auth_type(Some(&existing.auth_type))?;
    let target_auth_type = payload
        .auth_type
        .as_deref()
        .map(|value| normalize_auth_type(Some(value)))
        .transpose()?
        .unwrap_or_else(|| current_auth_type.clone());
    let auth_type_switch = payload
        .auth_type
        .as_deref()
        .is_some_and(|_| target_auth_type != current_auth_type);
    let managed_fixed_oauth_key = provider_type_is_fixed(&provider.provider_type)
        && (provider_key_is_oauth_managed(existing, &provider.provider_type)
            || target_auth_type.eq_ignore_ascii_case("oauth"));

    let api_key_present = fields.contains("api_key");
    let api_key_value = payload
        .api_key
        .as_deref()
        .map(str::trim)
        .map(ToOwned::to_owned);
    let auth_config_present = fields.contains("auth_config");
    let mut auth_config = normalize_json_object(payload.auth_config, "auth_config")?;
    super::normalize_auth_config_sensitive_words(&mut auth_config, &provider.provider_type)?;
    let auth_config_object = auth_config
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .cloned();

    if auth_config
        .as_ref()
        .is_some_and(aether_provider_transport::is_codex_agent_identity_auth_config_value)
    {
        return Err(
            "Agent Identity 凭据必须通过专属创建或导入接口管理，不能通过通用 Key 接口写入"
                .to_string(),
        );
    }
    if let Some(cooldown) = auth_config_object
        .as_ref()
        .and_then(|config| config.get(PROVIDER_COOLDOWN_CONFIG_KEY))
        .filter(|value| !value.is_null())
    {
        // Key 级冷却覆盖与供应商级共用一套字段与校验。
        normalize_provider_cooldown_config(cooldown).map_err(|err| format!("auth_config.{err}"))?;
    }

    match target_auth_type.as_str() {
        "api_key" | "bearer" => {
            if let Some(api_key) = api_key_value
                .as_deref()
                .filter(|value| !value.is_empty() && *value != "__placeholder__")
            {
                for existing_key in existing_keys
                    .iter()
                    .filter(|key| key.id != existing.id && raw_secret_auth_type(&key.auth_type))
                {
                    let Some(decrypted) = state
                        .decrypt_provider_catalog_key_api_key(existing_key)
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    if decrypted != "__placeholder__" && decrypted == api_key {
                        return Err(format!(
                            "该 API Key 已存在于当前 Provider 中（名称: {}）",
                            existing_key.name
                        ));
                    }
                }
                updated.encrypted_api_key = Some(
                    state
                        .seal_provider_catalog_key_api_key(&provider.id, &existing.id, api_key)
                        .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())?,
                );
            } else if api_key_present {
                updated.encrypted_api_key = None;
            }
            updated.encrypted_auth_config = None;
        }
        "service_account" => {
            if auth_type_switch && auth_config_object.is_none() {
                return Err(
                    "切换到 Service Account 认证模式时，必须提供 Service Account JSON".to_string(),
                );
            }
            if api_key_present
                && !matches!(
                    api_key_value.as_deref(),
                    None | Some("") | Some("__placeholder__")
                )
            {
                return Err("Service Account 认证模式下不允许直接填写 api_key".to_string());
            }
            if auth_type_switch || api_key_present {
                updated.encrypted_api_key = None;
            }
            if let Some(client_email) = auth_config_object
                .as_ref()
                .and_then(|config| config.get("client_email"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                for existing_key in existing_keys.iter().filter(|key| {
                    key.id != existing.id
                        && matches!(
                            key.auth_type.trim().to_ascii_lowercase().as_str(),
                            "service_account" | "vertex_ai"
                        )
                }) {
                    let Some(existing_config) = parse_catalog_auth_config_json(state, existing_key)
                    else {
                        continue;
                    };
                    let Some(existing_email) = existing_config
                        .get("client_email")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    if existing_email == client_email {
                        return Err(format!(
                            "该 Service Account ({client_email}) 已存在于当前 Provider 中（名称: {}）",
                            existing_key.name
                        ));
                    }
                }
            }
            if auth_config_present {
                updated.encrypted_auth_config = auth_config
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|err| err.to_string())?
                    .map(|plaintext| {
                        state
                            .seal_provider_catalog_key_auth_config(
                                &provider.id,
                                &existing.id,
                                &plaintext,
                            )
                            .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())
                    })
                    .transpose()?;
            }
        }
        "oauth" => {
            if api_key_present
                && !matches!(
                    api_key_value.as_deref(),
                    None | Some("") | Some("__placeholder__")
                )
            {
                return Err("OAuth 认证模式下不允许直接填写 api_key".to_string());
            }
            if auth_type_switch {
                updated.encrypted_api_key = None;
                updated.encrypted_auth_config = None;
            }
        }
        _ => {}
    }

    if fields.contains("api_formats") {
        let api_formats = normalize_api_format_list(
            normalize_string_list(payload.api_formats)
                .ok_or_else(|| "api_formats 为必填字段".to_string())?,
        );
        if managed_fixed_oauth_key {
            updated.api_formats = None;
            updated.auth_type_by_format = None;
        } else {
            validate_vertex_api_formats(&provider.provider_type, &target_auth_type, &api_formats)?;
            updated.api_formats = Some(json!(api_formats));
        }
    } else if payload.auth_type.is_some() {
        if managed_fixed_oauth_key {
            updated.api_formats = None;
        } else {
            let api_formats =
                normalize_api_format_list(json_string_list(existing.api_formats.as_ref()));
            validate_vertex_api_formats(&provider.provider_type, &target_auth_type, &api_formats)?;
        }
    }

    let effective_api_formats =
        normalize_api_format_list(json_string_list(updated.api_formats.as_ref()));
    if matches!(target_auth_type.as_str(), "api_key" | "bearer") {
        if fields.contains("auth_type_by_format") {
            updated.auth_type_by_format = normalize_auth_type_by_format(
                payload.auth_type_by_format,
                "auth_type_by_format",
                &effective_api_formats,
            )?;
        } else if fields.contains("api_formats") {
            updated.auth_type_by_format = normalize_auth_type_by_format(
                updated.auth_type_by_format.clone(),
                "auth_type_by_format",
                &effective_api_formats,
            )?;
        }
    } else {
        updated.auth_type_by_format = None;
    }
    if fields.contains("allow_auth_channel_mismatch_formats") {
        updated.allow_auth_channel_mismatch_formats =
            normalize_allow_auth_channel_mismatch_formats(
                payload.allow_auth_channel_mismatch_formats,
                "allow_auth_channel_mismatch_formats",
                &effective_api_formats,
            )?;
    } else if fields.contains("api_formats") && !managed_fixed_oauth_key {
        let existing = updated
            .allow_auth_channel_mismatch_formats
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            });
        updated.allow_auth_channel_mismatch_formats =
            reconcile_allow_auth_channel_mismatch_formats(existing, &effective_api_formats);
    }

    updated.auth_type = target_auth_type;

    if let Some(name) = payload.name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name 为必填字段".to_string());
        }
        updated.name = trimmed.to_string();
    }
    if fields.contains("rate_multipliers") {
        updated.rate_multipliers = normalize_rate_multipliers(payload.rate_multipliers)?;
    }
    if let Some(internal_priority) = payload.internal_priority {
        updated.internal_priority = internal_priority;
    }
    if fields.contains("global_priority_by_format") {
        updated.global_priority_by_format = normalize_api_format_json_object_keys(
            payload.global_priority_by_format,
            "global_priority_by_format",
        )?;
    }
    if fields.contains("rpm_limit") {
        updated.rpm_limit = payload.rpm_limit;
        if payload.rpm_limit.is_none() {
            updated.learned_rpm_limit = None;
        }
    }
    if fields.contains("concurrent_limit") {
        updated.concurrent_limit =
            normalize_optional_api_key_concurrent_limit(payload.concurrent_limit)?;
    }
    if fields.contains("allowed_models") {
        updated.allowed_models =
            normalize_string_list(payload.allowed_models).map(|value| json!(value));
    }
    if fields.contains("capabilities") {
        updated.capabilities = normalize_json_object(payload.capabilities, "capabilities")?;
    }
    if let Some(cache_ttl_minutes) = payload.cache_ttl_minutes {
        updated.cache_ttl_minutes = cache_ttl_minutes;
    }
    if let Some(max_probe_interval_minutes) = payload.max_probe_interval_minutes {
        updated.max_probe_interval_minutes =
            normalize_max_probe_interval_minutes(max_probe_interval_minutes)?;
    }
    if let Some(is_active) = payload.is_active {
        updated.is_active = is_active;
    }
    if fields.contains("note") {
        updated.note = payload
            .note
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }
    if let Some(auto_fetch_models) = payload.auto_fetch_models {
        updated.auto_fetch_models = auto_fetch_models;
    }
    if auto_fetch_disabled && !fields.contains("allowed_models") {
        updated.allowed_models = None;
    }
    if fields.contains("locked_models") {
        updated.locked_models =
            normalize_string_list(payload.locked_models).map(|value| json!(value));
    }
    if fields.contains("model_include_patterns") {
        updated.model_include_patterns =
            normalize_string_list(payload.model_include_patterns).map(|value| json!(value));
    }
    if fields.contains("model_exclude_patterns") {
        updated.model_exclude_patterns =
            normalize_string_list(payload.model_exclude_patterns).map(|value| json!(value));
    }
    if fields.contains("proxy") {
        updated.proxy = normalize_json_object(payload.proxy, "proxy")?
            .map(|value| admin_restore_secret_safe_proxy(existing.proxy.as_ref(), &value));
    }
    if fields.contains("fingerprint") {
        updated.fingerprint = normalize_json_object(payload.fingerprint, "fingerprint")?
            .map(|value| admin_restore_secret_safe_json(existing.fingerprint.as_ref(), &value));
    }
    if fields.contains(PROVIDER_TRANSPORT_PROFILE_FIELD) {
        // P5：Key 级传输指纹 profile 三态。null = 删除（回到供应商 / 系统默认）；字符串 = 覆盖。
        let profile_id = if fields.is_null(PROVIDER_TRANSPORT_PROFILE_FIELD) {
            None
        } else {
            Some(
                payload
                    .transport_profile
                    .as_deref()
                    .ok_or_else(|| "transport_profile 必须是字符串".to_string())?,
            )
        };
        updated.fingerprint = apply_key_transport_profile(
            updated.fingerprint.take(),
            &provider.provider_type,
            profile_id,
        )?;
    }
    if auth_config_present && !auth_type_switch && !raw_secret_auth_type(&updated.auth_type) {
        updated.encrypted_auth_config = auth_config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| err.to_string())?
            .map(|plaintext| {
                state
                    .seal_provider_catalog_key_auth_config(&provider.id, &existing.id, &plaintext)
                    .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())
            })
            .transpose()?;
    }
    if fields.contains(CLOAK_SENSITIVE_WORDS_FIELD) {
        // P6 Key 级词表覆盖：三态。null = 删除覆盖（继承供应商）；数组（含空数组 = 关闭）= 覆盖。
        // 写在加密的 auth_config 里，所以这里解密 → 改键 → 重新密封。
        let override_words = if fields.is_null(CLOAK_SENSITIVE_WORDS_FIELD) {
            None
        } else {
            Some(
                payload
                    .cloak_sensitive_words
                    .as_deref()
                    .ok_or_else(|| "cloak_sensitive_words 必须是字符串数组".to_string())?,
            )
        };
        apply_key_cloak_sensitive_words_override(
            state,
            provider,
            existing,
            &mut updated,
            override_words,
        )?;
    }

    updated.updated_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs());
    if provider_key_credentials_changed(state, existing, &updated) {
        rotate_codex_credential_generation(&mut updated, &provider.provider_type);
    }
    Ok(updated)
}

pub(crate) fn admin_provider_key_update_requires_immediate_model_fetch(
    existing: &StoredProviderCatalogKey,
    updated: &StoredProviderCatalogKey,
) -> bool {
    let filters_changed = existing.model_include_patterns != updated.model_include_patterns
        || existing.model_exclude_patterns != updated.model_exclude_patterns;
    let locked_models_changed = existing.locked_models != updated.locked_models;
    updated.auto_fetch_models
        && (!existing.auto_fetch_models || filters_changed || locked_models_changed)
}

pub(crate) fn build_provider_catalog_key_admin_cas_update(
    state: &crate::AppState,
    existing: &StoredProviderCatalogKey,
    updated: StoredProviderCatalogKey,
    provider_type: &str,
) -> ProviderCatalogKeyAdminCasUpdate {
    let previous_generation = existing
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.pointer("/codex/credential_generation"))
        .and_then(serde_json::Value::as_str);
    let next_generation = updated
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.pointer("/codex/credential_generation"))
        .and_then(serde_json::Value::as_str);
    let credential_changed = provider_key_credentials_changed(state, existing, &updated);
    let codex_rotation = provider_type
        .trim()
        .eq_ignore_ascii_case("codex")
        .then(|| next_generation.filter(|next| Some(*next) != previous_generation))
        .flatten()
        .map(|generation| {
            json!({
                aether_admin::provider::quota::CODEX_CREDENTIAL_GENERATION_KEY: generation,
            })
        });

    ProviderCatalogKeyAdminCasUpdate {
        expected_encrypted_auth_config: existing.encrypted_auth_config.clone(),
        expected_credential: ProviderCatalogKeyOAuthCredentialFence {
            encrypted_api_key: existing.encrypted_api_key.clone(),
            auth_type: existing.auth_type.clone(),
            provider_id: existing.provider_id.clone(),
            provider_type: provider_type.to_string(),
        },
        key: updated,
        codex_rotation,
        reset_oauth_runtime: credential_changed,
    }
}

fn provider_key_credentials_changed(
    state: &crate::AppState,
    existing: &StoredProviderCatalogKey,
    updated: &StoredProviderCatalogKey,
) -> bool {
    if existing.provider_id != updated.provider_id
        || !existing.auth_type.eq_ignore_ascii_case(&updated.auth_type)
        || existing.encrypted_api_key != updated.encrypted_api_key
    {
        return true;
    }
    if existing.encrypted_auth_config == updated.encrypted_auth_config {
        return false;
    }

    // 词表和凭据共用加密容器；重新密封词表不能清除 OAuth 失效状态。
    // CAS 仍比较完整密文，避免覆盖并发刷新；这里只排除明确的非凭据字段。
    let credential_config = |key: &StoredProviderCatalogKey| {
        state
            .decrypt_provider_catalog_key_auth_config(key)
            .map_err(|_| "无法验证现有 provider auth_config".to_string())
            .map(|plaintext| {
                plaintext.and_then(|raw| {
                    let mut value = serde_json::from_str::<serde_json::Value>(&raw)
                        .unwrap_or(serde_json::Value::String(raw));
                    if let Some(object) = value.as_object_mut() {
                        object.remove(
                            aether_provider_transport::CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY,
                        );
                        if object.is_empty() {
                            return None;
                        }
                    }
                    Some(value)
                })
            })
    };
    match (credential_config(existing), credential_config(updated)) {
        (Ok(previous), Ok(next)) => previous != next,
        // 旧配置损坏时仍允许管理员替换凭据；无法证明只是词表变化时保留重置语义。
        _ => true,
    }
}

fn raw_secret_auth_type(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "api_key" | "bearer"
    )
}

/// 把 Key 级传输指纹 profile 写进（或从）`fingerprint.transport_profile`；保留 `fingerprint`
/// 里其它键（device_id 等）。
pub(crate) fn apply_key_transport_profile(
    fingerprint: Option<serde_json::Value>,
    provider_type: &str,
    profile_id: Option<&str>,
) -> Result<Option<serde_json::Value>, String> {
    let mut fingerprint = fingerprint
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    match profile_id {
        Some(profile_id) => {
            let normalized = crate::handlers::admin::provider::write::provider::normalize_admin_transport_profile(
                provider_type,
                profile_id,
            )?;
            fingerprint.insert(
                PROVIDER_TRANSPORT_PROFILE_FIELD.to_string(),
                json!(normalized),
            );
        }
        None => {
            fingerprint.remove(PROVIDER_TRANSPORT_PROFILE_FIELD);
        }
    }
    Ok((!fingerprint.is_empty()).then(|| serde_json::Value::Object(fingerprint)))
}

/// 把 Key 级敏感词覆盖写进（或从）加密的 `auth_config.cloak_sensitive_words`。
///
/// `override_words = None` 表示删除覆盖（继承供应商词表）；`Some(&[])` 表示覆盖为空词表
/// （关闭混淆）；`Some(words)` 覆盖为归一化后的词表。只有 claude_code / antigravity 的
/// Key 允许写入；其它类型收到非空覆盖时报错，`null` 则只做清理。
fn apply_key_cloak_sensitive_words_override(
    state: &crate::AppState,
    provider: &StoredProviderCatalogProvider,
    existing: &StoredProviderCatalogKey,
    updated: &mut StoredProviderCatalogKey,
    override_words: Option<&[String]>,
) -> Result<(), String> {
    use crate::handlers::admin::provider::write::provider::{
        normalize_cloak_sensitive_words, provider_type_supports_sensitive_words,
    };
    use aether_provider_transport::CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY;

    let normalized = override_words
        .map(normalize_cloak_sensitive_words)
        .transpose()?;
    if !provider_type_supports_sensitive_words(&provider.provider_type) {
        if normalized.is_some() {
            return Err(
                "cloak_sensitive_words 仅适用于 provider_type=claude_code / antigravity 的 Key"
                    .to_string(),
            );
        }
    }

    // 读取当前（可能已被本次更新改写过的）auth_config 明文；`updated` 总是持有最新密文，
    // 且跨供应商迁移时已按新 provider_id 重新密封，所以直接解密它。
    let plaintext = state
        .decrypt_provider_catalog_key_auth_config(updated)
        .map_err(|_| "无法验证现有 provider auth_config".to_string())?;
    let mut auth_config = match plaintext.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(object)) => object,
            _ => return Err("auth_config 不是 JSON 对象，无法写入敏感词覆盖".to_string()),
        },
        _ => {
            if normalized.is_none() {
                // 没有 auth_config 也就没有覆盖可删，静默即可。
                return Ok(());
            }
            return Err("该 Key 没有 auth_config，无法写入敏感词覆盖".to_string());
        }
    };

    let changed = match normalized {
        None => auth_config
            .remove(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY)
            .is_some(),
        Some(words) => {
            let next = json!(words);
            let unchanged = auth_config.get(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY) == Some(&next);
            auth_config.insert(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY.to_string(), next);
            !unchanged
        }
    };
    if !changed {
        return Ok(());
    }
    let sealed = state
        .seal_provider_catalog_key_auth_config(
            &provider.id,
            &existing.id,
            &serde_json::Value::Object(auth_config).to_string(),
        )
        .map_err(|_| "gateway 未配置 provider key 加密密钥".to_string())?;
    updated.encrypted_auth_config = Some(sealed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_admin_update_provider_key_record_with_existing_keys;
    use crate::handlers::admin::provider::shared::payloads::AdminProviderKeyUpdatePatch;
    use crate::handlers::admin::request::AdminAppState;
    use crate::AppState;
    use aether_crypto::{encrypt_python_fernet_plaintext, DEVELOPMENT_ENCRYPTION_KEY};
    use aether_data_contracts::repository::provider_catalog::{
        StoredProviderCatalogKey, StoredProviderCatalogProvider,
    };
    use serde_json::json;

    fn provider(provider_type: &str) -> StoredProviderCatalogProvider {
        StoredProviderCatalogProvider::new(
            "provider-1".to_string(),
            provider_type.to_string(),
            None,
            provider_type.to_string(),
        )
        .expect("provider should build")
    }

    fn key(auth_config: Option<&str>) -> StoredProviderCatalogKey {
        StoredProviderCatalogKey::new(
            "key-1".to_string(),
            "provider-1".to_string(),
            "oauth".to_string(),
            "oauth".to_string(),
            None,
            true,
        )
        .expect("key should build")
        .with_transport_fields(
            Some(json!(["gemini:generate_content"])),
            encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "__placeholder__")
                .expect("placeholder should encrypt"),
            auth_config.map(|raw| {
                encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, raw)
                    .expect("auth config should encrypt")
            }),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("key transport should build")
    }

    fn patch(raw: serde_json::Value) -> AdminProviderKeyUpdatePatch {
        AdminProviderKeyUpdatePatch::from_object(raw.as_object().expect("object").clone())
            .expect("patch should parse")
    }

    fn decrypted_auth_config(app: &AppState, key: &StoredProviderCatalogKey) -> serde_json::Value {
        let plaintext = app
            .decrypt_provider_catalog_key_auth_config(key)
            .expect("auth config should decrypt")
            .expect("auth config should exist");
        serde_json::from_str(&plaintext).expect("auth config should be JSON")
    }

    #[test]
    fn key_sensitive_word_override_follows_three_states_inside_encrypted_auth_config() {
        let app = AppState::new().expect("gateway should build");
        let state = AdminAppState::new(&app);
        let provider = provider("antigravity");
        let existing = key(Some(
            r#"{"provider_type":"antigravity","project_id":"project-1"}"#,
        ));

        // 数组 → 覆盖（归一化：去重、小写、长度降序）。
        let updated = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider,
            &existing,
            &[existing.clone()],
            patch(json!({"cloak_sensitive_words": ["Proxy", " api ", "proxy"]})),
        )
        .expect("override should apply");
        let auth_config = decrypted_auth_config(&app, &updated);
        assert_eq!(
            auth_config["cloak_sensitive_words"],
            json!(["proxy", "api"])
        );
        assert_eq!(
            auth_config["project_id"], "project-1",
            "other fields survive"
        );
        assert_ne!(
            updated.encrypted_auth_config,
            existing.encrypted_auth_config
        );

        // 空数组 → 覆盖为空（关闭混淆），键仍存在。
        let disabled = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider,
            &updated,
            &[updated.clone()],
            patch(json!({"cloak_sensitive_words": []})),
        )
        .expect("empty override should apply");
        assert_eq!(
            decrypted_auth_config(&app, &disabled)["cloak_sensitive_words"],
            json!([])
        );

        // null → 删除覆盖（继承供应商）。
        let inherited = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider,
            &disabled,
            &[disabled.clone()],
            patch(json!({"cloak_sensitive_words": null})),
        )
        .expect("null should remove the override");
        let auth_config = decrypted_auth_config(&app, &inherited);
        assert!(auth_config.get("cloak_sensitive_words").is_none());
        assert_eq!(auth_config["project_id"], "project-1");

        // 字段缺席 → 密文一个字节都不动。
        let untouched = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider,
            &inherited,
            &[inherited.clone()],
            patch(json!({"note": "unrelated"})),
        )
        .expect("unrelated update should apply");
        assert_eq!(
            untouched.encrypted_auth_config,
            inherited.encrypted_auth_config
        );
    }

    #[test]
    fn adding_or_removing_a_words_only_auth_config_preserves_oauth_runtime() {
        let app = AppState::new().expect("gateway should build");
        let state = AdminAppState::new(&app);
        let provider = provider("claude_code");
        for (previous, replacement) in [
            (None, json!({"cloak_sensitive_words": ["proxy"]})),
            (
                Some(r#"{"cloak_sensitive_words":["proxy"]}"#),
                serde_json::Value::Null,
            ),
        ] {
            let existing = key(previous);
            let updated = build_admin_update_provider_key_record_with_existing_keys(
                &state,
                &provider,
                &existing,
                &[existing.clone()],
                patch(json!({"auth_config": replacement})),
            )
            .expect("words-only auth config should update");
            let update = super::build_provider_catalog_key_admin_cas_update(
                &app,
                &existing,
                updated,
                &provider.provider_type,
            );
            assert!(!update.reset_oauth_runtime);
        }
    }

    #[test]
    fn replacing_unreadable_auth_config_still_resets_oauth_runtime() {
        let app = AppState::new().expect("gateway should build");
        let state = AdminAppState::new(&app);
        let provider = provider("claude_code");
        let mut existing = key(None);
        existing.encrypted_auth_config = Some("invalid-ciphertext".to_string());
        let updated = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider,
            &existing,
            &[existing.clone()],
            patch(json!({"auth_config": {"refresh_token": "replacement"}})),
        )
        .expect("corrupt old credentials must remain replaceable");
        assert_eq!(
            decrypted_auth_config(&app, &updated)["refresh_token"],
            "replacement"
        );
        let update = super::build_provider_catalog_key_admin_cas_update(
            &app,
            &existing,
            updated,
            &provider.provider_type,
        );
        assert!(update.reset_oauth_runtime);
    }

    #[test]
    fn key_sensitive_word_override_rejects_bad_words_and_other_provider_types() {
        let app = AppState::new().expect("gateway should build");
        let state = AdminAppState::new(&app);
        let existing = key(Some(r#"{"provider_type":"claude_code"}"#));

        let error = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider("claude_code"),
            &existing,
            &[existing.clone()],
            patch(json!({"cloak_sensitive_words": ["proxy", "x"]})),
        )
        .expect_err("short word must be rejected");
        assert!(error.contains("过短"), "{error}");

        let error = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider("codex"),
            &existing,
            &[existing.clone()],
            patch(json!({"cloak_sensitive_words": ["proxy"]})),
        )
        .expect_err("codex keys cannot carry an override");
        assert!(error.contains("仅适用于"), "{error}");

        // codex Key 收到 null 只做清理，不报错。
        build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider("codex"),
            &existing,
            &[existing.clone()],
            patch(json!({"cloak_sensitive_words": null})),
        )
        .expect("null on codex key is a no-op");

        // 没有 auth_config 的 Key 写不了覆盖，但 null 静默。
        let bare = key(None);
        let error = build_admin_update_provider_key_record_with_existing_keys(
            &state,
            &provider("claude_code"),
            &bare,
            &[bare.clone()],
            patch(json!({"cloak_sensitive_words": ["proxy"]})),
        )
        .expect_err("missing auth_config cannot hold an override");
        assert!(error.contains("auth_config"), "{error}");
    }
}
