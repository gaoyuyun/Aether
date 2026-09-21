use crate::handlers::admin::provider::shared::paths::admin_reset_claude_code_device_key_id;
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::GatewayError;
use axum::{
    body::{Body, Bytes},
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// 「重置设备身份」：清掉 Key 的 Claude Code 设备 profile，下一次第三方请求重新派生
/// device_id 与软件版本元组。只对 claude_code 供应商的 Key 有意义。
pub(super) async fn maybe_handle(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
    _request_body: Option<&Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let Some(decision) = request_context.decision() else {
        return Ok(None);
    };
    if decision.route_family.as_deref() != Some("endpoints_manage")
        || decision.route_kind.as_deref() != Some("reset_claude_code_device")
        || request_context.method() != http::Method::POST
        || !request_context
            .path()
            .starts_with("/api/admin/endpoints/keys/")
        || !request_context
            .path()
            .ends_with("/reset-claude-code-device")
    {
        return Ok(None);
    }

    let Some(key_id) = admin_reset_claude_code_device_key_id(request_context.path()) else {
        return Ok(Some(not_found_response("Key 不存在")));
    };
    let Some(key) = state
        .read_provider_catalog_keys_by_ids(std::slice::from_ref(&key_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(Some(not_found_response(format!("Key {key_id} 不存在"))));
    };
    let provider = state
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&key.provider_id))
        .await?
        .into_iter()
        .next();
    if !provider.as_ref().is_some_and(|provider| {
        provider
            .provider_type
            .trim()
            .eq_ignore_ascii_case("claude_code")
    }) {
        return Ok(Some(
            (
                http::StatusCode::BAD_REQUEST,
                Json(json!({ "detail": "重置设备身份仅适用于 claude_code 供应商的 Key" })),
            )
                .into_response(),
        ));
    }
    let reset = crate::ai_serving::reset_claude_code_device_profile(state.app(), &key_id).await?;
    Ok(Some(
        Json(json!({
            "message": if reset {
                "已重置设备身份，下一次请求将派生新的设备标识"
            } else {
                "该 Key 尚未生成设备身份，无需重置"
            },
            "reset": reset,
        }))
        .into_response(),
    ))
}

fn not_found_response(detail: impl Into<String>) -> Response<Body> {
    (
        http::StatusCode::NOT_FOUND,
        Json(json!({ "detail": detail.into() })),
    )
        .into_response()
}
