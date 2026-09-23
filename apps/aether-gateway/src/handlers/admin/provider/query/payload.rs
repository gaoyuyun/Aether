use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::BTreeSet;

pub(crate) fn parse_admin_provider_query_body(
    request_body: Option<&Bytes>,
) -> Result<serde_json::Value, Response<axum::body::Body>> {
    let Some(raw_body) = request_body else {
        return Ok(json!({}));
    };
    if raw_body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<serde_json::Value>(raw_body).map_err(|_| {
        super::response::build_admin_provider_query_bad_request_response(
            super::response::ADMIN_PROVIDER_QUERY_INVALID_JSON_DETAIL,
        )
    })
}

pub(crate) fn provider_query_extract_provider_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("provider_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn provider_query_extract_api_key_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("api_key_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn provider_query_insert_api_key_id(ids: &mut BTreeSet<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        ids.insert(value.to_string());
    }
}

pub(crate) fn provider_query_extract_api_key_ids(
    payload: &serde_json::Value,
) -> Option<BTreeSet<String>> {
    let mut ids = BTreeSet::new();

    if let Some(value) = payload
        .get("api_key_ids")
        .or_else(|| payload.get("provider_key_ids"))
        .or_else(|| payload.get("key_ids"))
    {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(value) = item.as_str() {
                        provider_query_insert_api_key_id(&mut ids, value);
                    }
                }
            }
            serde_json::Value::String(value) => {
                for item in value.split(',') {
                    provider_query_insert_api_key_id(&mut ids, item);
                }
            }
            _ => {}
        }
    }

    if let Some(api_key_id) = provider_query_extract_api_key_id(payload) {
        ids.insert(api_key_id);
    }

    (!ids.is_empty()).then_some(ids)
}

pub(crate) fn provider_query_extract_force_refresh(payload: &serde_json::Value) -> bool {
    payload
        .get("force_refresh")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn provider_query_extract_client_version(
    payload: &serde_json::Value,
) -> Result<Option<crate::model_fetch::NormalizedCodexClientVersion>, &'static str> {
    let raw = match payload.get("client_version") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(value)) if value.trim().is_empty() => return Ok(None),
        Some(serde_json::Value::String(value)) => value,
        Some(_) => return Err(super::response::ADMIN_PROVIDER_QUERY_INVALID_CLIENT_VERSION_DETAIL),
    };
    let normalized = crate::model_fetch::normalize_codex_client_version(Some(raw));
    if normalized.used_fallback() {
        return Err(super::response::ADMIN_PROVIDER_QUERY_INVALID_CLIENT_VERSION_DETAIL);
    }
    Ok(Some(normalized))
}

pub(crate) fn provider_query_extract_model(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("model")
        .or_else(|| payload.get("model_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn provider_query_extract_failover_models(payload: &serde_json::Value) -> Vec<String> {
    if let Some(items) = payload
        .get("failover_models")
        .or_else(|| payload.get("models"))
        .and_then(serde_json::Value::as_array)
    {
        return items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
    }

    provider_query_extract_model(payload)
        .into_iter()
        .collect::<Vec<_>>()
}

pub(crate) fn provider_query_extract_request_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn provider_query_payload_keys(payload: &serde_json::Value) -> Vec<String> {
    let Some(object) = payload.as_object() else {
        return Vec::new();
    };
    let mut keys = object.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}
