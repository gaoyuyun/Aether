use crate::handlers::admin::provider::shared::paths::admin_tls_probe_key_id;
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::GatewayError;
use axum::{
    body::{Body, Bytes},
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// P5「探测 TLS 指纹」：按该 Key 的传输 profile 与代理向固定清单里的回显服务发一次请求，
/// 把 JA3 / JA4 写回 `upstream_metadata.tls_probe`，并原样返回给前端。
pub(super) async fn maybe_handle(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
    _request_body: Option<&Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let Some(decision) = request_context.decision() else {
        return Ok(None);
    };
    if decision.route_family.as_deref() != Some("endpoints_manage")
        || decision.route_kind.as_deref() != Some("tls_probe")
        || request_context.method() != http::Method::POST
        || !request_context
            .path()
            .starts_with("/api/admin/endpoints/keys/")
        || !request_context.path().ends_with("/tls-probe")
    {
        return Ok(None);
    }

    let Some(key_id) = admin_tls_probe_key_id(request_context.path()) else {
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
    let Some(provider) = state
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&key.provider_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(Some(not_found_response("Provider 不存在")));
    };
    match crate::handlers::admin::provider::tls_probe::run_tls_probe_for_key(
        state.app(),
        &provider,
        &key,
    )
    .await
    {
        Ok(record) => Ok(Some(
            Json(json!({
                "message": "已完成 TLS 指纹探测",
                "key_id": key_id,
                "probe": record,
            }))
            .into_response(),
        )),
        Err(error) => Ok(Some(
            (
                http::StatusCode::BAD_GATEWAY,
                Json(json!({ "detail": error.message })),
            )
                .into_response(),
        )),
    }
}

fn not_found_response(detail: impl Into<String>) -> Response<Body> {
    (
        http::StatusCode::NOT_FOUND,
        Json(json!({ "detail": detail.into() })),
    )
        .into_response()
}
