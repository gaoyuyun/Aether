use std::collections::{BTreeMap, BTreeSet};

use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::handlers::shared::{
    api_key_placeholder_display, deserialize_optional_json_patch,
    deserialize_optional_string_list_patch, generate_gateway_api_key_plaintext,
    masked_gateway_api_key_display, normalize_feature_settings, normalize_ip_rules,
    normalize_optional_api_key_concurrent_limit,
};

use super::{
    build_auth_error_response, decrypt_catalog_secret_with_fallbacks,
    encrypt_catalog_secret_with_fallbacks, format_users_me_optional_unix_secs_iso8601,
    known_capability_names, normalize_user_model_capability_settings_input,
    query_param_optional_bool, resolve_authenticated_local_user,
    user_configurable_capability_names, AppState, GatewayPublicRequestContext,
};

const USERS_ME_API_KEY_WRITE_UNAVAILABLE_DETAIL: &str = "用户 API 密钥写入暂不可用";
const USERS_ME_PROVIDER_CATALOG_UNAVAILABLE_DETAIL: &str = "用户提供商目录暂不可用";

#[derive(Debug, Deserialize)]
struct UsersMeCreateApiKeyRequest {
    name: String,
    #[serde(default)]
    rate_limit: Option<i32>,
    #[serde(default)]
    concurrent_limit: Option<i32>,
    #[serde(default)]
    feature_settings: Option<serde_json::Value>,
    #[serde(default, alias = "allowed_ips")]
    ip_rules: Option<Vec<String>>,
    /// `null`/omitted = inherit the user's full provider allowance; list = key-level subset.
    #[serde(default, deserialize_with = "deserialize_optional_provider_list_patch")]
    allowed_providers: Option<Option<Vec<UsersMeApiKeyProviderValue>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    providers: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    allowed_api_formats: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    allowed_models: Option<Option<Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct UsersMeUpdateApiKeyRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    rate_limit: Option<i32>,
    #[serde(default)]
    concurrent_limit: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_json_patch")]
    feature_settings: Option<Option<serde_json::Value>>,
    #[serde(
        default,
        alias = "allowed_ips",
        deserialize_with = "deserialize_optional_string_list_patch"
    )]
    ip_rules: Option<Option<Vec<String>>>,
    /// Present when the client patches providers in the same request as basic fields.
    /// `null` = inherit account allowance; list = key-level subset (including empty deny-all).
    #[serde(default, deserialize_with = "deserialize_optional_provider_list_patch")]
    allowed_providers: Option<Option<Vec<UsersMeApiKeyProviderValue>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    providers: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    allowed_api_formats: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    allowed_models: Option<Option<Vec<String>>>,
}

fn deserialize_optional_provider_list_patch<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Vec<UsersMeApiKeyProviderValue>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<Vec<UsersMeApiKeyProviderValue>> as serde::Deserialize>::deserialize(deserializer)
        .map(Some)
}

#[derive(Debug, Deserialize)]
struct UsersMePatchApiKeyRequest {
    #[serde(default)]
    is_active: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UsersMeApiKeyProviderValue {
    ProviderId(String),
    ProviderConfig {
        provider_id: String,
        #[serde(default)]
        priority: Option<i32>,
        #[serde(default)]
        weight: Option<f64>,
        #[serde(default)]
        enabled: Option<bool>,
    },
}

#[derive(Debug, Deserialize)]
struct UsersMeUpdateApiKeyProvidersRequest {
    #[serde(default, deserialize_with = "deserialize_optional_provider_list_patch")]
    allowed_providers: Option<Option<Vec<UsersMeApiKeyProviderValue>>>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list_patch")]
    providers: Option<Option<Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct UsersMeUpdateApiKeyCapabilitiesRequest {
    #[serde(default)]
    force_capabilities: Option<serde_json::Value>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

fn users_me_api_key_path_segments(request_path: &str) -> Option<Vec<&str>> {
    let raw = request_path
        .strip_prefix("/api/users/me/api-keys/")?
        .trim()
        .trim_matches('/');
    if raw.is_empty() {
        return None;
    }
    let segments = raw
        .split('/')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    (!segments.is_empty()).then_some(segments)
}

fn users_me_api_key_detail_id_from_path(request_path: &str) -> Option<String> {
    let segments = users_me_api_key_path_segments(request_path)?;
    (segments.len() == 1).then(|| segments[0].to_string())
}

fn users_me_api_key_nested_id_from_path(request_path: &str, suffix: &str) -> Option<String> {
    let segments = users_me_api_key_path_segments(request_path)?;
    (segments.len() == 2 && segments[1] == suffix).then(|| segments[0].to_string())
}

pub(super) fn users_me_api_key_detail_path_matches(request_path: &str) -> bool {
    users_me_api_key_detail_id_from_path(request_path).is_some()
}

pub(super) fn users_me_api_key_providers_path_matches(request_path: &str) -> bool {
    users_me_api_key_nested_id_from_path(request_path, "providers").is_some()
}

pub(super) fn users_me_api_key_capabilities_path_matches(request_path: &str) -> bool {
    users_me_api_key_nested_id_from_path(request_path, "capabilities").is_some()
}

fn users_me_masked_api_key_display(state: &AppState, ciphertext: Option<&str>) -> String {
    let Some(ciphertext) = ciphertext.map(str::trim).filter(|value| !value.is_empty()) else {
        return api_key_placeholder_display();
    };
    let Some(full_key) = decrypt_catalog_secret_with_fallbacks(state.encryption_key(), ciphertext)
    else {
        return api_key_placeholder_display();
    };
    masked_gateway_api_key_display(Some(full_key.as_str()))
}

fn build_users_me_api_key_writer_unavailable_response() -> Response<Body> {
    build_auth_error_response(
        http::StatusCode::SERVICE_UNAVAILABLE,
        USERS_ME_API_KEY_WRITE_UNAVAILABLE_DETAIL,
        false,
    )
}

fn build_users_me_api_key_list_payload(
    state: &AppState,
    record: &aether_data::repository::auth::StoredAuthApiKeyExportRecord,
    is_locked: bool,
) -> serde_json::Value {
    json!({
        "id": record.api_key_id,
        "name": record.name,
        "key_display": users_me_masked_api_key_display(state, record.key_encrypted.as_deref()),
        "is_active": record.is_active,
        "is_locked": is_locked,
        "last_used_at": format_users_me_optional_unix_secs_iso8601(record.last_used_at_unix_secs),
        "created_at": format_users_me_optional_unix_secs_iso8601(record.created_at_unix_secs),
        "total_requests": record.total_requests,
        "total_cost_usd": record.total_cost_usd,
        "rate_limit": record.rate_limit,
        "concurrent_limit": record.concurrent_limit,
        "allowed_providers": record.allowed_providers,
        "allowed_api_formats": record.allowed_api_formats,
        "allowed_models": record.allowed_models,
        "ip_rules": record.ip_rules,
        "force_capabilities": record.force_capabilities,
        "feature_settings": record.feature_settings,
    })
}

fn build_users_me_api_key_detail_payload(
    state: &AppState,
    record: &aether_data::repository::auth::StoredAuthApiKeyExportRecord,
    is_locked: bool,
) -> serde_json::Value {
    json!({
        "id": record.api_key_id,
        "name": record.name,
        "key_display": users_me_masked_api_key_display(state, record.key_encrypted.as_deref()),
        "is_active": record.is_active,
        "is_locked": is_locked,
        "allowed_providers": record.allowed_providers,
        "allowed_api_formats": record.allowed_api_formats,
        "allowed_models": record.allowed_models,
        "ip_rules": record.ip_rules,
        "force_capabilities": record.force_capabilities,
        "feature_settings": record.feature_settings,
        "rate_limit": record.rate_limit,
        "concurrent_limit": record.concurrent_limit,
        "last_used_at": format_users_me_optional_unix_secs_iso8601(record.last_used_at_unix_secs),
        "expires_at": format_users_me_optional_unix_secs_iso8601(record.expires_at_unix_secs),
        "created_at": format_users_me_optional_unix_secs_iso8601(record.created_at_unix_secs),
    })
}

fn normalize_users_me_required_api_key_name(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("API密钥名称不能为空".to_string());
    }
    Ok(trimmed.chars().take(100).collect())
}

fn generate_users_me_api_key_plaintext() -> String {
    generate_gateway_api_key_plaintext()
}

fn normalize_users_me_ip_rules(values: Option<Vec<String>>) -> Result<Option<Vec<String>>, String> {
    normalize_ip_rules(values)
}

fn hash_users_me_api_key(value: &str) -> String {
    use sha2::Digest;

    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize_users_me_api_key_provider_values(
    allowed_providers: Option<Vec<UsersMeApiKeyProviderValue>>,
    providers: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, String> {
    let values = if let Some(values) = allowed_providers {
        values
            .into_iter()
            .map(|value| match value {
                UsersMeApiKeyProviderValue::ProviderId(provider_id) => provider_id,
                UsersMeApiKeyProviderValue::ProviderConfig { provider_id, .. } => provider_id,
            })
            .collect::<Vec<_>>()
    } else if let Some(values) = providers {
        values
    } else {
        return Ok(None);
    };

    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let provider_id = value.trim();
        if provider_id.is_empty() {
            return Err("提供商ID不能为空".to_string());
        }
        if seen.insert(provider_id.to_string()) {
            normalized.push(provider_id.to_string());
        }
    }
    Ok(Some(normalized))
}

fn normalize_users_me_api_key_providers(
    payload: UsersMeUpdateApiKeyProvidersRequest,
) -> Result<Option<Vec<String>>, String> {
    normalize_users_me_api_key_provider_patch_values(payload.allowed_providers, payload.providers)
}

fn normalize_users_me_api_key_provider_patch_values(
    allowed_providers: Option<Option<Vec<UsersMeApiKeyProviderValue>>>,
    providers: Option<Option<Vec<String>>>,
) -> Result<Option<Vec<String>>, String> {
    if allowed_providers.is_some() && providers.is_some() {
        return Err("allowed_providers 与 providers 不能同时提供".to_string());
    }
    normalize_users_me_api_key_provider_values(allowed_providers.flatten(), providers.flatten())
}

fn catalog_provider_allowed_by_user_policy(
    provider_id: &str,
    provider_name: &str,
    provider_type: &str,
    user_allowed_providers: Option<&[String]>,
) -> bool {
    let Some(allowed) = user_allowed_providers else {
        return true;
    };
    allowed.iter().any(|value| {
        aether_scheduler_core::provider_matches_allowed_value(
            value,
            provider_id,
            provider_name,
            provider_type,
        )
    })
}

fn filter_catalog_providers_to_user_allowed(
    providers: &[(String, String, String)],
    requested_ids: &[String],
    user_allowed_providers: Option<&[String]>,
) -> Result<Vec<String>, Vec<String>> {
    let by_id = providers
        .iter()
        .map(|(id, name, provider_type)| (id.as_str(), (name.as_str(), provider_type.as_str())))
        .collect::<BTreeMap<_, _>>();
    let mut allowed = Vec::new();
    let mut denied = Vec::new();
    for provider_id in requested_ids {
        let Some((name, provider_type)) = by_id.get(provider_id.as_str()).copied() else {
            denied.push(provider_id.clone());
            continue;
        };
        if catalog_provider_allowed_by_user_policy(
            provider_id,
            name,
            provider_type,
            user_allowed_providers,
        ) {
            allowed.push(provider_id.clone());
        } else {
            denied.push(provider_id.clone());
        }
    }
    if denied.is_empty() {
        Ok(allowed)
    } else {
        Err(denied)
    }
}

async fn resolve_users_me_api_key_allowed_providers(
    state: &AppState,
    user: &aether_data::repository::users::StoredUserAuthRecord,
    requested: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, Response<Body>> {
    let Some(providers) = requested else {
        return Ok(None);
    };

    // An explicit empty list is always safe: it is a key-level deny-all and
    // does not require provider metadata to validate.
    if providers.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if !state.has_provider_catalog_data_reader() {
        return Err(build_auth_error_response(
            http::StatusCode::SERVICE_UNAVAILABLE,
            USERS_ME_PROVIDER_CATALOG_UNAVAILABLE_DETAIL,
            false,
        ));
    }

    let catalog_providers = match state.list_provider_catalog_provider_identities(true).await {
        Ok(value) => value,
        Err(err) => {
            return Err(build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("provider validation failed: {err:?}"),
                false,
            ))
        }
    };
    let mut by_key = BTreeMap::new();
    for provider in &catalog_providers {
        by_key.insert(provider.id.to_ascii_lowercase(), provider.id.clone());
        by_key.insert(provider.name.to_ascii_lowercase(), provider.id.clone());
    }
    let mut invalid = Vec::new();
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for provider_id in providers {
        let key = provider_id.trim().to_ascii_lowercase();
        if let Some(mapped) = by_key.get(&key) {
            if seen.insert(mapped.clone()) {
                normalized.push(mapped.clone());
            }
        } else {
            invalid.push(provider_id);
        }
    }
    if !invalid.is_empty() {
        return Err(build_auth_error_response(
            http::StatusCode::BAD_REQUEST,
            format!("无效的提供商ID: {}", invalid.join(", ")),
            false,
        ));
    }
    let catalog_meta = catalog_providers
        .into_iter()
        .map(|provider| (provider.id, provider.name, provider.provider_type))
        .collect::<Vec<_>>();

    let effective_policies = match state.data.resolve_user_effective_list_policies(user).await {
        Ok(value) => value,
        Err(err) => {
            return Err(build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user policy lookup failed: {err:?}"),
                false,
            ))
        }
    };

    match filter_catalog_providers_to_user_allowed(
        &catalog_meta,
        &normalized,
        effective_policies.allowed_providers.as_deref(),
    ) {
        Ok(allowed) => Ok(Some(allowed)),
        Err(denied) => Err(build_auth_error_response(
            http::StatusCode::BAD_REQUEST,
            format!("以下提供商不在您的可用范围内: {}", denied.join(", ")),
            false,
        )),
    }
}

fn normalize_users_me_api_key_string_list(
    values: Option<Vec<String>>,
    field_name: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(values) = values else {
        return Ok(None);
    };
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("{field_name} 不能为空"));
        }
        if seen.insert(value.to_string()) {
            normalized.push(value.to_string());
        }
    }
    Ok(Some(normalized))
}

fn normalize_users_me_api_key_api_formats(
    values: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, String> {
    let Some(values) = values else {
        return Ok(None);
    };
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let value = value.trim();
        let Some(value) = crate::api::ai::normalize_admin_endpoint_signature(value) else {
            return Err(format!("allowed_api_formats 格式无效: {value}"));
        };
        if seen.insert(value.to_string()) {
            normalized.push(value.to_string());
        }
    }
    Ok(Some(normalized))
}

async fn resolve_users_me_api_key_allowed_formats_and_models(
    state: &AppState,
    user: &aether_data::repository::users::StoredUserAuthRecord,
    requested_api_formats: Option<Vec<String>>,
    requested_models: Option<Vec<String>>,
) -> Result<(Option<Vec<String>>, Option<Vec<String>>), Response<Body>> {
    let needs_policy_lookup = requested_api_formats
        .as_ref()
        .is_some_and(|values| !values.is_empty())
        || requested_models
            .as_ref()
            .is_some_and(|values| !values.is_empty());
    if !needs_policy_lookup {
        return Ok((requested_api_formats, requested_models));
    }

    let effective_policies = match state.data.resolve_user_effective_list_policies(user).await {
        Ok(value) => value,
        Err(err) => {
            return Err(build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user policy lookup failed: {err:?}"),
                false,
            ))
        }
    };

    if let (Some(requested), Some(user_allowed)) = (
        requested_api_formats.as_ref(),
        effective_policies.allowed_api_formats.as_ref(),
    ) {
        let denied = requested
            .iter()
            .filter(|requested_format| {
                !user_allowed.iter().any(|allowed_format| {
                    crate::ai_serving::api::api_format_permission_covers(
                        allowed_format,
                        requested_format,
                    )
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            return Err(build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                format!("以下端点不在您的可用范围内: {}", denied.join(", ")),
                false,
            ));
        }
    }

    if let (Some(requested), Some(user_allowed)) = (
        requested_models.as_ref(),
        effective_policies.allowed_models.as_ref(),
    ) {
        let allowed = user_allowed.iter().collect::<BTreeSet<_>>();
        let denied = requested
            .iter()
            .filter(|model| !allowed.contains(model))
            .cloned()
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            return Err(build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                format!("以下模型不在您的可用范围内: {}", denied.join(", ")),
                false,
            ));
        }
    }

    Ok((requested_api_formats, requested_models))
}

fn normalize_users_me_api_key_force_capabilities(
    payload: UsersMeUpdateApiKeyCapabilitiesRequest,
) -> Result<Option<serde_json::Value>, String> {
    if let Some(capabilities) = payload.capabilities {
        let mut map = serde_json::Map::new();
        for capability in capabilities {
            let capability = capability.trim();
            if capability.is_empty() {
                return Err("能力名称不能为空".to_string());
            }
            map.insert(capability.to_string(), serde_json::Value::Bool(true));
        }
        return validate_users_me_force_capabilities(Some(serde_json::Value::Object(map)));
    }
    validate_users_me_force_capabilities(payload.force_capabilities)
}

fn validate_users_me_force_capabilities(
    value: Option<serde_json::Value>,
) -> Result<Option<serde_json::Value>, String> {
    let Some(value) = normalize_user_model_capability_settings_input(value) else {
        return Ok(None);
    };
    let Some(map) = value.as_object() else {
        return Err("force_capabilities 必须是对象类型".to_string());
    };

    let user_configurable = user_configurable_capability_names();
    let known_capabilities = known_capability_names();
    for (capability_name, capability_value) in map {
        if !known_capabilities.contains(capability_name.as_str()) {
            return Err(format!("未知的能力类型: {capability_name}"));
        }
        if !user_configurable.contains(capability_name.as_str()) {
            return Err(format!("能力 {capability_name} 不支持用户配置"));
        }
        if !capability_value.is_boolean() {
            return Err(format!("能力 {capability_name} 的值必须是布尔类型"));
        }
    }

    Ok(Some(value))
}

pub(super) async fn handle_users_me_api_keys_get(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
) -> Response<Body> {
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut records = match state
        .list_auth_api_key_export_records_by_user_ids(std::slice::from_ref(&auth.user.id))
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key lookup failed: {err:?}"),
                false,
            )
        }
    };
    records.retain(|record| !record.is_standalone);
    records.sort_by(|left, right| left.api_key_id.cmp(&right.api_key_id));

    let snapshot_ids = records
        .iter()
        .map(|record| record.api_key_id.clone())
        .collect::<Vec<_>>();
    let snapshot_by_id = match state
        .data
        .list_auth_api_key_snapshots_by_ids(&snapshot_ids)
        .await
    {
        Ok(value) => value
            .into_iter()
            .map(|snapshot| (snapshot.api_key_id.clone(), snapshot))
            .collect::<BTreeMap<_, _>>(),
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key snapshot lookup failed: {err:?}"),
                false,
            )
        }
    };

    Json(
        records
            .iter()
            .map(|record| {
                let is_locked = snapshot_by_id
                    .get(&record.api_key_id)
                    .map(|snapshot| snapshot.api_key_is_locked)
                    .unwrap_or(false);
                build_users_me_api_key_list_payload(state, record, is_locked)
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

pub(super) async fn handle_users_me_api_key_detail_get(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
) -> Response<Body> {
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) = users_me_api_key_detail_id_from_path(&request_context.request_path)
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let include_key = query_param_optional_bool(
        request_context.request_query_string.as_deref(),
        "include_key",
    )
    .unwrap_or(false);

    let records = match state
        .list_auth_api_key_export_records_by_user_ids(std::slice::from_ref(&auth.user.id))
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key lookup failed: {err:?}"),
                false,
            )
        }
    };
    let Some(record) = records
        .into_iter()
        .find(|record| !record.is_standalone && record.api_key_id == api_key_id)
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };

    if include_key {
        let Some(ciphertext) = record.key_encrypted.as_deref().map(str::trim) else {
            return build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                "该密钥没有存储完整密钥信息",
                false,
            );
        };
        if ciphertext.is_empty() {
            return build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                "该密钥没有存储完整密钥信息",
                false,
            );
        }
        let Some(full_key) =
            decrypt_catalog_secret_with_fallbacks(state.encryption_key(), ciphertext)
        else {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "解密密钥失败",
                false,
            );
        };
        return Json(json!({ "key": full_key })).into_response();
    }

    let snapshot_ids = vec![api_key_id.clone()];
    let is_locked = match state
        .data
        .list_auth_api_key_snapshots_by_ids(&snapshot_ids)
        .await
    {
        Ok(value) => value
            .into_iter()
            .find(|snapshot| snapshot.api_key_id == api_key_id)
            .map(|snapshot| snapshot.api_key_is_locked)
            .unwrap_or(false),
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key snapshot lookup failed: {err:?}"),
                false,
            )
        }
    };

    Json(build_users_me_api_key_detail_payload(
        state, &record, is_locked,
    ))
    .into_response()
}

async fn resolve_users_me_api_key_snapshot_by_id(
    state: &AppState,
    user_id: &str,
    api_key_id: &str,
) -> Result<crate::data::auth::GatewayAuthApiKeySnapshot, Response<Body>> {
    let snapshot = match state
        .data
        .read_auth_api_key_snapshot(
            user_id,
            api_key_id,
            chrono::Utc::now().timestamp().max(0) as u64,
        )
        .await
    {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return Err(build_auth_error_response(
                http::StatusCode::NOT_FOUND,
                "API密钥不存在",
                false,
            ))
        }
        Err(err) => {
            return Err(build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key snapshot lookup failed: {err:?}"),
                false,
            ))
        }
    };
    if snapshot.api_key_is_standalone {
        return Err(build_auth_error_response(
            http::StatusCode::NOT_FOUND,
            "API密钥不存在",
            false,
        ));
    }
    Ok(snapshot)
}

fn ensure_users_me_api_key_mutable(
    snapshot: &crate::data::auth::GatewayAuthApiKeySnapshot,
) -> Result<(), Response<Body>> {
    if snapshot.api_key_is_locked {
        return Err(build_auth_error_response(
            http::StatusCode::FORBIDDEN,
            "该密钥已被管理员锁定，无法修改",
            false,
        ));
    }
    Ok(())
}

pub(super) async fn handle_users_me_api_key_create(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
    request_body: Option<&axum::body::Bytes>,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(request_body) = request_body else {
        return build_auth_error_response(http::StatusCode::BAD_REQUEST, "请求数据验证失败", false);
    };
    let payload = match serde_json::from_slice::<UsersMeCreateApiKeyRequest>(request_body) {
        Ok(value) => value,
        Err(_) => {
            return build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                "请求数据验证失败",
                false,
            )
        }
    };
    let name = match normalize_users_me_required_api_key_name(&payload.name) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false)
        }
    };
    let rate_limit = payload.rate_limit.unwrap_or(0);
    if rate_limit < 0 {
        return build_auth_error_response(
            http::StatusCode::BAD_REQUEST,
            "rate_limit 必须大于等于 0",
            false,
        );
    }
    let concurrent_limit =
        match normalize_optional_api_key_concurrent_limit(payload.concurrent_limit) {
            Ok(value) => value,
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        };
    let feature_settings = match normalize_feature_settings(payload.feature_settings) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
        }
    };
    let ip_rules = match normalize_users_me_ip_rules(payload.ip_rules) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
        }
    };
    let requested_providers = match normalize_users_me_api_key_provider_patch_values(
        payload.allowed_providers,
        payload.providers,
    ) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
        }
    };
    let allowed_providers =
        match resolve_users_me_api_key_allowed_providers(state, &auth.user, requested_providers)
            .await
        {
            Ok(value) => value,
            Err(response) => return response,
        };
    let requested_api_formats =
        match normalize_users_me_api_key_api_formats(payload.allowed_api_formats.flatten()) {
            Ok(value) => value,
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        };
    let requested_models = match normalize_users_me_api_key_string_list(
        payload.allowed_models.flatten(),
        "allowed_models",
    ) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
        }
    };
    let (allowed_api_formats, allowed_models) =
        match resolve_users_me_api_key_allowed_formats_and_models(
            state,
            &auth.user,
            requested_api_formats,
            requested_models,
        )
        .await
        {
            Ok(value) => value,
            Err(response) => return response,
        };

    let plaintext_key = generate_users_me_api_key_plaintext();
    let Some(key_encrypted) = encrypt_catalog_secret_with_fallbacks(state, &plaintext_key) else {
        return build_auth_error_response(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "API密钥加密失败",
            false,
        );
    };
    let record = aether_data::repository::auth::CreateUserApiKeyRecord {
        user_id: auth.user.id.clone(),
        api_key_id: uuid::Uuid::new_v4().to_string(),
        key_hash: hash_users_me_api_key(&plaintext_key),
        key_encrypted: Some(key_encrypted),
        name: Some(name.clone()),
        allowed_providers,
        allowed_api_formats,
        allowed_models,
        ip_rules,
        rate_limit,
        concurrent_limit,
        force_capabilities: None,
        feature_settings,
        is_active: true,
        expires_at_unix_secs: None,
        auto_delete_on_expiry: false,
        total_requests: 0,
        total_tokens: 0,
        total_cost_usd: 0.0,
    };
    let Some(created) = (match state.create_user_api_key(record).await {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key create failed: {err:?}"),
                false,
            )
        }
    }) else {
        return build_users_me_api_key_writer_unavailable_response();
    };
    Json(json!({
        "id": created.api_key_id,
        "name": created.name,
        "key": plaintext_key,
        "key_display": users_me_masked_api_key_display(state, created.key_encrypted.as_deref()),
        "is_active": created.is_active,
        "is_locked": false,
        "rate_limit": created.rate_limit,
        "concurrent_limit": created.concurrent_limit,
        "ip_rules": created.ip_rules,
        "allowed_providers": created.allowed_providers,
        "allowed_api_formats": created.allowed_api_formats,
        "allowed_models": created.allowed_models,
        "feature_settings": created.feature_settings,
        "last_used_at": format_users_me_optional_unix_secs_iso8601(created.last_used_at_unix_secs),
        "created_at": format_users_me_optional_unix_secs_iso8601(created.created_at_unix_secs),
        "total_requests": created.total_requests,
        "total_cost_usd": created.total_cost_usd,
        "message": "API密钥创建成功",
    }))
    .into_response()
}

pub(super) async fn handle_users_me_api_key_update(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
    request_body: Option<&axum::body::Bytes>,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) = users_me_api_key_detail_id_from_path(&request_context.request_path)
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let snapshot =
        match resolve_users_me_api_key_snapshot_by_id(state, &auth.user.id, &api_key_id).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = ensure_users_me_api_key_mutable(&snapshot) {
        return response;
    }
    let Some(request_body) = request_body else {
        return build_auth_error_response(http::StatusCode::BAD_REQUEST, "请求数据验证失败", false);
    };
    let payload = match serde_json::from_slice::<UsersMeUpdateApiKeyRequest>(request_body) {
        Ok(value) => value,
        Err(_) => {
            return build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                "请求数据验证失败",
                false,
            )
        }
    };
    let name = match payload.name {
        Some(value) => match normalize_users_me_required_api_key_name(&value) {
            Ok(value) => Some(value),
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false)
            }
        },
        None => None,
    };
    let rate_limit = payload.rate_limit;
    if rate_limit.is_some_and(|value| value < 0) {
        return build_auth_error_response(
            http::StatusCode::BAD_REQUEST,
            "rate_limit 必须大于等于 0",
            false,
        );
    }
    let concurrent_limit =
        match normalize_optional_api_key_concurrent_limit(payload.concurrent_limit) {
            Ok(value) => value,
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        };
    let feature_settings = match payload.feature_settings {
        Some(value) => match normalize_feature_settings(value) {
            Ok(value) => Some(value),
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        },
        None => None,
    };
    let ip_rules = match payload.ip_rules {
        Some(value) => match normalize_users_me_ip_rules(value) {
            Ok(value) => Some(value),
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        },
        None => None,
    };
    // Resolve providers before any write so invalid provider patches fail without partial saves.
    let allowed_providers = {
        let providers_patch_present =
            payload.allowed_providers.is_some() || payload.providers.is_some();
        if !providers_patch_present {
            None
        } else {
            let requested = match normalize_users_me_api_key_provider_patch_values(
                payload.allowed_providers,
                payload.providers,
            ) {
                Ok(value) => value,
                Err(detail) => {
                    return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
                }
            };
            match resolve_users_me_api_key_allowed_providers(state, &auth.user, requested).await {
                Ok(value) => Some(value),
                Err(response) => return response,
            }
        }
    };
    let api_formats_patch_present = payload.allowed_api_formats.is_some();
    let models_patch_present = payload.allowed_models.is_some();
    let requested_api_formats =
        match normalize_users_me_api_key_api_formats(payload.allowed_api_formats.flatten()) {
            Ok(value) => value,
            Err(detail) => {
                return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
            }
        };
    let requested_models = match normalize_users_me_api_key_string_list(
        payload.allowed_models.flatten(),
        "allowed_models",
    ) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false);
        }
    };
    let (resolved_api_formats, resolved_models) =
        match resolve_users_me_api_key_allowed_formats_and_models(
            state,
            &auth.user,
            requested_api_formats,
            requested_models,
        )
        .await
        {
            Ok(value) => value,
            Err(response) => return response,
        };
    let allowed_api_formats = api_formats_patch_present.then_some(resolved_api_formats);
    let allowed_models = models_patch_present.then_some(resolved_models);

    let Some(updated) = (match state
        .update_user_api_key_basic(aether_data::repository::auth::UpdateUserApiKeyBasicRecord {
            user_id: auth.user.id.clone(),
            api_key_id: snapshot.api_key_id.clone(),
            name,
            rate_limit,
            concurrent_limit,
            ip_rules,
            allowed_providers,
            allowed_api_formats,
            allowed_models,
            // Same write as basic fields so partial "success then 500" cannot occur.
            feature_settings,
        })
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key update failed: {err:?}"),
                false,
            )
        }
    }) else {
        return build_users_me_api_key_writer_unavailable_response();
    };

    let mut payload =
        build_users_me_api_key_detail_payload(state, &updated, snapshot.api_key_is_locked);
    payload["message"] = json!("API密钥已更新");
    Json(payload).into_response()
}

pub(super) async fn handle_users_me_api_key_patch(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
    request_body: Option<&axum::body::Bytes>,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) = users_me_api_key_detail_id_from_path(&request_context.request_path)
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let snapshot =
        match resolve_users_me_api_key_snapshot_by_id(state, &auth.user.id, &api_key_id).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = ensure_users_me_api_key_mutable(&snapshot) {
        return response;
    }
    let desired_is_active = if let Some(request_body) = request_body {
        match serde_json::from_slice::<UsersMePatchApiKeyRequest>(request_body) {
            Ok(value) => value.is_active.unwrap_or(!snapshot.api_key_is_active),
            Err(_) => {
                return build_auth_error_response(
                    http::StatusCode::BAD_REQUEST,
                    "请求数据验证失败",
                    false,
                )
            }
        }
    } else {
        !snapshot.api_key_is_active
    };

    let Some(updated) = (match state
        .set_user_api_key_active(&auth.user.id, &snapshot.api_key_id, desired_is_active)
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key toggle failed: {err:?}"),
                false,
            )
        }
    }) else {
        return build_users_me_api_key_writer_unavailable_response();
    };

    Json(json!({
        "id": updated.api_key_id,
        "is_active": updated.is_active,
        "message": format!("API密钥已{}", if updated.is_active { "启用" } else { "禁用" }),
    }))
    .into_response()
}

pub(super) async fn handle_users_me_api_key_delete(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) = users_me_api_key_detail_id_from_path(&request_context.request_path)
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let snapshot =
        match resolve_users_me_api_key_snapshot_by_id(state, &auth.user.id, &api_key_id).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = ensure_users_me_api_key_mutable(&snapshot) {
        return response;
    }

    match state
        .delete_user_api_key(&auth.user.id, &snapshot.api_key_id)
        .await
    {
        Ok(true) => Json(json!({ "message": "API密钥已删除" })).into_response(),
        Ok(false) => build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false),
        Err(err) => build_auth_error_response(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("user api key delete failed: {err:?}"),
            false,
        ),
    }
}

pub(super) async fn handle_users_me_api_key_providers_put(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
    request_body: Option<&axum::body::Bytes>,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) =
        users_me_api_key_nested_id_from_path(&request_context.request_path, "providers")
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let snapshot =
        match resolve_users_me_api_key_snapshot_by_id(state, &auth.user.id, &api_key_id).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = ensure_users_me_api_key_mutable(&snapshot) {
        return response;
    }
    let Some(request_body) = request_body else {
        return build_auth_error_response(http::StatusCode::BAD_REQUEST, "请求数据验证失败", false);
    };
    let payload = match serde_json::from_slice::<UsersMeUpdateApiKeyProvidersRequest>(request_body)
    {
        Ok(value) => value,
        Err(_) => {
            return build_auth_error_response(
                http::StatusCode::BAD_REQUEST,
                "请求数据验证失败",
                false,
            )
        }
    };
    let requested_providers = match normalize_users_me_api_key_providers(payload) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false)
        }
    };
    let allowed_providers =
        match resolve_users_me_api_key_allowed_providers(state, &auth.user, requested_providers)
            .await
        {
            Ok(value) => value,
            Err(response) => return response,
        };

    let Some(updated) = (match state
        .set_user_api_key_allowed_providers(&auth.user.id, &snapshot.api_key_id, allowed_providers)
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key providers update failed: {err:?}"),
                false,
            )
        }
    }) else {
        return build_users_me_api_key_writer_unavailable_response();
    };

    Json(json!({
        "message": "API密钥可用提供商已更新",
        "allowed_providers": updated.allowed_providers,
    }))
    .into_response()
}

pub(super) async fn handle_users_me_api_key_capabilities_put(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    headers: &http::HeaderMap,
    request_body: Option<&axum::body::Bytes>,
) -> Response<Body> {
    if !state.data.has_auth_api_key_writer() {
        return build_users_me_api_key_writer_unavailable_response();
    }
    let auth = match resolve_authenticated_local_user(state, request_context, headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(api_key_id) =
        users_me_api_key_nested_id_from_path(&request_context.request_path, "capabilities")
    else {
        return build_auth_error_response(http::StatusCode::NOT_FOUND, "API密钥不存在", false);
    };
    let snapshot =
        match resolve_users_me_api_key_snapshot_by_id(state, &auth.user.id, &api_key_id).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = ensure_users_me_api_key_mutable(&snapshot) {
        return response;
    }
    let Some(request_body) = request_body else {
        return build_auth_error_response(http::StatusCode::BAD_REQUEST, "请求数据验证失败", false);
    };
    let payload =
        match serde_json::from_slice::<UsersMeUpdateApiKeyCapabilitiesRequest>(request_body) {
            Ok(value) => value,
            Err(_) => {
                return build_auth_error_response(
                    http::StatusCode::BAD_REQUEST,
                    "请求数据验证失败",
                    false,
                )
            }
        };
    let force_capabilities = match normalize_users_me_api_key_force_capabilities(payload) {
        Ok(value) => value,
        Err(detail) => {
            return build_auth_error_response(http::StatusCode::BAD_REQUEST, detail, false)
        }
    };

    let Some(updated) = (match state
        .set_user_api_key_force_capabilities(
            &auth.user.id,
            &snapshot.api_key_id,
            force_capabilities,
        )
        .await
    {
        Ok(value) => value,
        Err(err) => {
            return build_auth_error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("user api key capabilities update failed: {err:?}"),
                false,
            )
        }
    }) else {
        return build_users_me_api_key_writer_unavailable_response();
    };

    Json(json!({
        "message": "API密钥能力配置已更新",
        "force_capabilities": updated.force_capabilities.unwrap_or(serde_json::Value::Null),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        catalog_provider_allowed_by_user_policy, filter_catalog_providers_to_user_allowed,
        normalize_users_me_api_key_api_formats, normalize_users_me_api_key_provider_patch_values,
        normalize_users_me_api_key_provider_values, normalize_users_me_api_key_string_list,
        normalize_users_me_ip_rules, resolve_users_me_api_key_allowed_providers,
        UsersMeCreateApiKeyRequest, UsersMeUpdateApiKeyRequest,
    };
    use axum::http;
    use serde_json::json;

    #[test]
    fn catalog_provider_policy_allows_unrestricted_user() {
        assert!(catalog_provider_allowed_by_user_policy(
            "provider-openai",
            "OpenAI",
            "openai",
            None,
        ));
    }

    #[test]
    fn catalog_provider_policy_matches_id_name_or_type() {
        let allowed = vec!["openai".to_string()];
        assert!(catalog_provider_allowed_by_user_policy(
            "provider-openai",
            "OpenAI Pool",
            "openai",
            Some(allowed.as_slice()),
        ));
        assert!(!catalog_provider_allowed_by_user_policy(
            "provider-claude",
            "Claude",
            "claude",
            Some(allowed.as_slice()),
        ));
    }

    #[test]
    fn filter_catalog_providers_rejects_providers_outside_user_allowance() {
        let catalog = vec![
            (
                "provider-openai".to_string(),
                "OpenAI".to_string(),
                "openai".to_string(),
            ),
            (
                "provider-claude".to_string(),
                "Claude".to_string(),
                "claude".to_string(),
            ),
        ];
        let err = filter_catalog_providers_to_user_allowed(
            &catalog,
            &["provider-openai".to_string(), "provider-claude".to_string()],
            Some(&["openai".to_string()]),
        )
        .expect_err("providers outside user allowance should fail");
        assert_eq!(err, vec!["provider-claude".to_string()]);

        let ok = filter_catalog_providers_to_user_allowed(
            &catalog,
            &["provider-openai".to_string()],
            Some(&["openai".to_string()]),
        )
        .expect("allowed provider should pass");
        assert_eq!(ok, vec!["provider-openai".to_string()]);
    }

    #[test]
    fn normalize_provider_values_accepts_string_or_object_entries() {
        let values = normalize_users_me_api_key_provider_values(
            Some(vec![
                super::UsersMeApiKeyProviderValue::ProviderId(" provider-a ".to_string()),
                super::UsersMeApiKeyProviderValue::ProviderConfig {
                    provider_id: "provider-b".to_string(),
                    priority: None,
                    weight: None,
                    enabled: None,
                },
            ]),
            None,
        )
        .expect("provider values should normalize");
        assert_eq!(
            values,
            Some(vec!["provider-a".to_string(), "provider-b".to_string()])
        );

        let omitted = normalize_users_me_api_key_provider_values(None, None)
            .expect("omitted providers mean inherit");
        assert_eq!(omitted, None);
    }

    #[test]
    fn update_payload_distinguishes_provider_patch_states() {
        let missing = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "name": "unchanged-providers",
        }))
        .expect("missing allowed_providers should deserialize");
        assert!(missing.allowed_providers.is_none());

        let inherited = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "allowed_providers": null,
        }))
        .expect("null allowed_providers should deserialize");
        assert!(matches!(inherited.allowed_providers, Some(None)));

        let denied = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "allowed_providers": [],
        }))
        .expect("empty allowed_providers should deserialize");
        assert!(matches!(
            denied.allowed_providers,
            Some(Some(values)) if values.is_empty()
        ));

        let selected = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "allowed_providers": [" provider-a ", {"provider_id": "provider-b"}],
        }))
        .expect("selected allowed_providers should deserialize");
        let normalized = normalize_users_me_api_key_provider_values(
            selected.allowed_providers.flatten(),
            selected.providers.flatten(),
        )
        .expect("selected providers should normalize");
        assert_eq!(
            normalized,
            Some(vec!["provider-a".to_string(), "provider-b".to_string()])
        );
    }

    #[test]
    fn update_payload_distinguishes_endpoint_and_model_patch_states() {
        let missing = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "name": "unchanged-access",
        }))
        .expect("missing access restrictions should deserialize");
        assert!(missing.allowed_api_formats.is_none());
        assert!(missing.allowed_models.is_none());

        let cleared = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "allowed_api_formats": null,
            "allowed_models": null,
        }))
        .expect("null access restrictions should deserialize");
        assert!(matches!(cleared.allowed_api_formats, Some(None)));
        assert!(matches!(cleared.allowed_models, Some(None)));

        let restricted = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "allowed_api_formats": [" /v1/responses ", "openai:responses"],
            "allowed_models": [" gpt-5 ", "gpt-5"],
        }))
        .expect("specific access restrictions should deserialize");
        assert_eq!(
            normalize_users_me_api_key_api_formats(restricted.allowed_api_formats.flatten())
                .expect("formats should normalize"),
            Some(vec!["openai:responses".to_string()]),
        );
        assert_eq!(
            normalize_users_me_api_key_string_list(
                restricted.allowed_models.flatten(),
                "allowed_models",
            )
            .expect("models should normalize"),
            Some(vec!["gpt-5".to_string()]),
        );
    }

    #[test]
    fn provider_payload_rejects_new_and_legacy_fields_together_even_when_new_field_is_null() {
        let payload = serde_json::from_value::<UsersMeCreateApiKeyRequest>(json!({
            "name": "conflicting-providers",
            "allowed_providers": null,
            "providers": ["provider-a"],
        }))
        .expect("provider field presence should deserialize");
        assert!(payload.allowed_providers.is_some());
        assert!(payload.providers.is_some());
        let err = normalize_users_me_api_key_provider_patch_values(
            payload.allowed_providers,
            payload.providers,
        )
        .expect_err("conflicting provider aliases should fail");
        assert_eq!(err, "allowed_providers 与 providers 不能同时提供");
    }

    #[tokio::test]
    async fn provider_resolution_without_catalog_allows_inherit_and_deny_all_but_rejects_subset() {
        let state = crate::AppState::new().expect("test app state should build");
        let user = aether_data::repository::users::StoredUserAuthRecord::new(
            "user-1".to_string(),
            Some("alice@example.com".to_string()),
            true,
            "alice".to_string(),
            None,
            "user".to_string(),
            "local".to_string(),
            None,
            None,
            None,
            true,
            false,
            None,
            None,
        )
        .expect("test user should build");

        let inherited = resolve_users_me_api_key_allowed_providers(&state, &user, None)
            .await
            .expect("inherit should not need catalog");
        assert_eq!(inherited, None);
        let denied = resolve_users_me_api_key_allowed_providers(&state, &user, Some(Vec::new()))
            .await
            .expect("deny-all should not need catalog");
        assert_eq!(denied, Some(Vec::new()));
        let response = match resolve_users_me_api_key_allowed_providers(
            &state,
            &user,
            Some(vec!["provider-a".to_string()]),
        )
        .await
        {
            Err(response) => response,
            Ok(value) => panic!("non-empty subset should require catalog, got {value:?}"),
        };
        assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn normalize_ip_rules_trims_ip_and_cidr_values() {
        let values = normalize_users_me_ip_rules(Some(vec![
            " 203.0.113.10 ".to_string(),
            "10.0.0.0/24".to_string(),
        ]))
        .expect("valid IP rules should normalize");

        assert_eq!(
            values,
            Some(vec!["203.0.113.10".to_string(), "10.0.0.0/24".to_string()]),
        );
    }

    #[test]
    fn normalize_ip_rules_rejects_invalid_cidr() {
        let err = normalize_users_me_ip_rules(Some(vec!["10.0.0.0/99".to_string()]))
            .expect_err("invalid cidr should fail");

        assert_eq!(err, "无效的 IP 限制规则: 10.0.0.0/99（第 1 项）");
    }

    #[test]
    fn update_payload_distinguishes_missing_null_and_present_ip_rules() {
        let missing = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "name": "unchanged-ip-rules",
        }))
        .expect("missing ip_rules should deserialize");
        assert_eq!(missing.ip_rules, None);

        let cleared = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "ip_rules": null,
        }))
        .expect("null ip_rules should deserialize");
        assert_eq!(cleared.ip_rules, Some(None));

        let updated = serde_json::from_value::<UsersMeUpdateApiKeyRequest>(json!({
            "ip_rules": ["203.0.113.10", "10.0.0.0/24"],
        }))
        .expect("present ip_rules should deserialize");
        assert_eq!(
            updated.ip_rules,
            Some(Some(vec![
                "203.0.113.10".to_string(),
                "10.0.0.0/24".to_string(),
            ])),
        );
    }
}
