use std::collections::BTreeMap;

use aether_data_contracts::repository::candidate_selection::StoredMinimalCandidateSelectionRow;
use aether_data_contracts::repository::global_models::StoredPublicGlobalModel;
use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

const PUBLIC_MODELS_OWNER: &str = "aether";

pub(crate) fn build_models_auth_error_response(api_format: &str) -> Response<Body> {
    match api_format {
        "claude:messages" => (
            http::StatusCode::UNAUTHORIZED,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "authentication_error",
                    "message": "Invalid API key provided",
                },
            })),
        )
            .into_response(),
        "gemini:generate_content" => (
            http::StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "code": 401,
                    "message": "API key not valid. Please pass a valid API key.",
                    "status": "UNAUTHENTICATED",
                }
            })),
        )
            .into_response(),
        _ => (
            http::StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "Incorrect API key provided. You can find your API key at https://platform.openai.com/account/api-keys.",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_api_key",
                }
            })),
        )
            .into_response(),
    }
}

pub(super) fn build_models_not_found_response(model_id: &str, api_format: &str) -> Response<Body> {
    match api_format {
        "claude:messages" => (
            http::StatusCode::NOT_FOUND,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "not_found_error",
                    "message": format!("Model '{model_id}' not found"),
                },
            })),
        )
            .into_response(),
        "gemini:generate_content" => (
            http::StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": 404,
                    "message": format!("models/{model_id} is not found"),
                    "status": "NOT_FOUND",
                }
            })),
        )
            .into_response(),
        _ => (
            http::StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("The model '{model_id}' does not exist"),
                    "type": "invalid_request_error",
                    "param": "model",
                    "code": "model_not_found",
                }
            })),
        )
            .into_response(),
    }
}

pub(super) fn build_empty_models_list_response(api_format: &str) -> Response<Body> {
    match api_format {
        "openai:responses" => Json(json!({ "models": [] })).into_response(),
        "claude:messages" => Json(json!({
            "data": [],
            "has_more": false,
            "first_id": serde_json::Value::Null,
            "last_id": serde_json::Value::Null,
        }))
        .into_response(),
        "gemini:generate_content" => Json(json!({ "models": [] })).into_response(),
        _ => Json(json!({ "object": "list", "data": [] })).into_response(),
    }
}

pub(super) fn build_codex_models_list_response(
    models: Vec<serde_json::Value>,
    etag: Option<&str>,
) -> Response<Body> {
    let mut response = Json(json!({ "models": models })).into_response();
    if let Some(etag) = etag.and_then(|value| http::HeaderValue::from_str(value).ok()) {
        response.headers_mut().insert(http::header::ETAG, etag);
    }
    response
}

pub(super) fn build_openai_models_list_response(
    rows: &[StoredMinimalCandidateSelectionRow],
    metadata_by_name: &BTreeMap<String, StoredPublicGlobalModel>,
) -> Response<Body> {
    Json(json!({
        "object": "list",
        "data": rows.iter().map(|row| {
            let metadata = metadata_by_name.get(&row.global_model_name);
            json!({
                "id": row.global_model_name,
                "object": "model",
                "created": 0,
                "owned_by": PUBLIC_MODELS_OWNER,
                "display_name": metadata
                    .and_then(|model| model.display_name.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&row.global_model_name),
                "context_limit": model_config_value(
                    metadata,
                    &["context_limit", "context_window", "context_length"],
                ),
                "output_limit": model_config_value(
                    metadata,
                    &["output_limit", "max_output_tokens", "max_completion_tokens"],
                ),
                "input_modalities": model_config_value(
                    metadata,
                    &["input_modalities", "input"],
                ),
                "pricing": model_pricing_value(metadata),
                "reasoning_levels": model_reasoning_levels_value(metadata),
            })
        }).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn model_reasoning_levels_value(metadata: Option<&StoredPublicGlobalModel>) -> serde_json::Value {
    let config = metadata.and_then(|model| model.config.as_ref());
    if !config
        .and_then(|value| value.get("extended_thinking"))
        .is_some_and(|value| value.as_bool() == Some(true))
    {
        return serde_json::Value::Null;
    }

    let value = model_config_value(
        metadata,
        &[
            "reasoning_levels",
            "supported_reasoning_levels",
            "reasoning_efforts",
            "supported_reasoning_efforts",
        ],
    );
    let Some(levels) = value.as_array() else {
        return serde_json::Value::Null;
    };
    let levels = levels
        .iter()
        .filter_map(|level| {
            level
                .as_str()
                .or_else(|| level.get("effort").and_then(serde_json::Value::as_str))
        })
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(|level| serde_json::Value::String(level.to_string()))
        .collect::<Vec<_>>();
    if levels.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Array(levels)
    }
}

fn model_config_value(
    metadata: Option<&StoredPublicGlobalModel>,
    keys: &[&str],
) -> serde_json::Value {
    let config = metadata.and_then(|model| model.config.as_ref());
    keys.iter()
        .find_map(|key| config.and_then(|value| value.get(*key)))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

fn model_pricing_value(metadata: Option<&StoredPublicGlobalModel>) -> serde_json::Value {
    let Some(metadata) = metadata else {
        return serde_json::Value::Null;
    };

    let mut pricing = match metadata.default_tiered_pricing.as_ref() {
        Some(serde_json::Value::Object(object)) => object.clone(),
        Some(value) => serde_json::Map::from_iter([("tiered_pricing".to_string(), value.clone())]),
        None => serde_json::Map::new(),
    };
    if let Some(price_per_request) = metadata.default_price_per_request {
        pricing.insert("price_per_request".to_string(), json!(price_per_request));
    }

    if pricing.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(pricing)
    }
}

pub(super) fn build_openai_model_detail_response(
    row: &StoredMinimalCandidateSelectionRow,
) -> Response<Body> {
    Json(json!({
        "id": row.global_model_name,
        "object": "model",
        "created": 0,
        "owned_by": PUBLIC_MODELS_OWNER,
    }))
    .into_response()
}

pub(super) fn build_claude_models_list_response(
    rows: &[StoredMinimalCandidateSelectionRow],
    before_id: Option<&str>,
    after_id: Option<&str>,
    limit: usize,
) -> Response<Body> {
    let model_data = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.global_model_name,
                "type": "model",
                "display_name": row.global_model_name,
                "created_at": serde_json::Value::Null,
            })
        })
        .collect::<Vec<_>>();

    let mut start_idx = 0usize;
    if let Some(after_id) = after_id {
        if let Some(index) = model_data.iter().position(|item| item["id"] == after_id) {
            start_idx = index.saturating_add(1);
        }
    }
    let mut end_idx = model_data.len();
    if let Some(before_id) = before_id {
        if let Some(index) = model_data.iter().position(|item| item["id"] == before_id) {
            end_idx = index;
        }
    }
    let window = &model_data[start_idx.min(end_idx)..end_idx];
    let paginated = window.iter().take(limit).cloned().collect::<Vec<_>>();
    let first_id = paginated
        .first()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let last_id = paginated
        .last()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    Json(json!({
        "data": paginated,
        "has_more": window.len() > limit,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

pub(super) fn build_claude_model_detail_response(
    row: &StoredMinimalCandidateSelectionRow,
) -> Response<Body> {
    Json(json!({
        "id": row.global_model_name,
        "type": "model",
        "display_name": row.global_model_name,
        "created_at": serde_json::Value::Null,
    }))
    .into_response()
}

fn build_gemini_model_value(row: &StoredMinimalCandidateSelectionRow) -> serde_json::Value {
    json!({
        "name": format!("models/{}", row.global_model_name),
        "baseModelId": row.global_model_name,
        "version": "001",
        "displayName": row.global_model_name,
        "description": format!("Model {}", row.global_model_name),
        "inputTokenLimit": 128000,
        "outputTokenLimit": 8192,
        "supportedGenerationMethods": ["generateContent", "countTokens"],
        "temperature": 1.0,
        "maxTemperature": 2.0,
        "topP": 0.95,
        "topK": 64,
    })
}

pub(super) fn build_gemini_models_list_response(
    rows: &[StoredMinimalCandidateSelectionRow],
    page_size: usize,
    page_token: Option<&str>,
) -> Response<Body> {
    let start_idx = page_token
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let end_idx = start_idx.saturating_add(page_size);
    let window = rows
        .iter()
        .skip(start_idx)
        .take(page_size)
        .map(build_gemini_model_value)
        .collect::<Vec<_>>();
    let mut payload = json!({ "models": window });
    if end_idx < rows.len() {
        payload["nextPageToken"] = serde_json::Value::String(end_idx.to_string());
    }
    Json(payload).into_response()
}

pub(super) fn build_gemini_model_detail_response(
    row: &StoredMinimalCandidateSelectionRow,
) -> Response<Body> {
    Json(build_gemini_model_value(row)).into_response()
}
