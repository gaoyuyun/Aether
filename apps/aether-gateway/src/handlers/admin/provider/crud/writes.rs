use super::responses::build_admin_providers_data_unavailable_response;
use crate::handlers::admin::provider::shared::paths::{
    admin_provider_id_for_manage_path, is_admin_providers_root,
};
use crate::handlers::admin::provider::shared::payloads::{
    AdminProviderCreateRequest, AdminProviderUpdatePatch,
};
use crate::handlers::admin::provider::write::provider::{
    provider_cloak_sensitive_words_changed, reconcile_admin_fixed_provider_template_endpoints,
    reconcile_admin_fixed_provider_template_endpoints_after_update,
    CLOAK_SENSITIVE_WORDS_CHANGED_WARNING,
};
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::handlers::admin::shared::attach_admin_audit_response;
use crate::GatewayError;
use axum::{
    body::{Body, Bytes},
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

fn build_admin_provider_bad_request_response(detail: impl Into<String>) -> Response<Body> {
    (
        http::StatusCode::BAD_REQUEST,
        Json(json!({ "detail": detail.into() })),
    )
        .into_response()
}

/// 写入后需要提醒管理员的事项（目前只有 P6 的「词表变更会让提示词缓存失效」）。
/// 只在实际变化时才产生，避免每次保存都弹提示。
fn provider_write_warnings(
    existing_config: Option<&serde_json::Value>,
    updated_config: Option<&serde_json::Value>,
) -> Vec<&'static str> {
    let mut warnings = Vec::new();
    if provider_cloak_sensitive_words_changed(existing_config, updated_config) {
        warnings.push(CLOAK_SENSITIVE_WORDS_CHANGED_WARNING);
    }
    warnings
}

/// 把提示并列挂在响应 JSON 顶层的 `warnings` 数组里；没有提示时不加字段，
/// 保持既有响应形状不变。
fn attach_provider_write_warnings(payload: &mut serde_json::Value, warnings: Vec<&'static str>) {
    if warnings.is_empty() {
        return;
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert("warnings".to_string(), json!(warnings));
    }
}

fn build_admin_provider_not_found_response(detail: impl Into<String>) -> Response<Body> {
    (
        http::StatusCode::NOT_FOUND,
        Json(json!({ "detail": detail.into() })),
    )
        .into_response()
}

pub(crate) async fn maybe_build_local_admin_provider_writes_response(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
    request_body: Option<&Bytes>,
    route_kind: Option<&str>,
) -> Result<Option<Response<Body>>, GatewayError> {
    if route_kind == Some("create_provider")
        && request_context.method() == http::Method::POST
        && is_admin_providers_root(request_context.path())
    {
        let Some(request_body) = request_body else {
            return Ok(Some(build_admin_provider_bad_request_response(
                "请求体不能为空",
            )));
        };
        if !state.has_provider_catalog_data_reader() || !state.has_provider_catalog_data_writer() {
            return Ok(Some(build_admin_providers_data_unavailable_response()));
        }
        let payload = match serde_json::from_slice::<AdminProviderCreateRequest>(request_body) {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(Some(build_admin_provider_bad_request_response(
                    "请求体必须是合法的 JSON 对象",
                )));
            }
        };
        let (record, shift_existing_priorities_from) =
            match state.build_admin_create_provider_record(payload).await {
                Ok(record) => record,
                Err(message) => {
                    return Ok(Some(build_admin_provider_bad_request_response(message)));
                }
            };
        let Some(created_provider) = state
            .create_provider_catalog_provider(&record, shift_existing_priorities_from)
            .await?
        else {
            return Ok(Some(build_admin_providers_data_unavailable_response()));
        };

        if state
            .fixed_provider_template(&created_provider.provider_type)
            .is_some()
        {
            reconcile_admin_fixed_provider_template_endpoints(state, &created_provider).await?;
        }
        let mut created_payload = json!({
            "id": created_provider.id,
            "name": created_provider.name,
            "message": "提供商创建成功",
        });
        attach_provider_write_warnings(
            &mut created_payload,
            provider_write_warnings(None, created_provider.config.as_ref()),
        );
        return Ok(Some(attach_admin_audit_response(
            Json(created_payload).into_response(),
            "admin_provider_created",
            "create_provider",
            "provider",
            &created_provider.id,
        )));
    }

    if route_kind == Some("update_provider")
        && request_context.method() == http::Method::PATCH
        && request_context.path().starts_with("/api/admin/providers/")
    {
        let Some(provider_id) = admin_provider_id_for_manage_path(request_context.path()) else {
            return Ok(Some(build_admin_provider_not_found_response(
                "Provider 不存在",
            )));
        };
        let Some(request_body) = request_body else {
            return Ok(Some(build_admin_provider_bad_request_response(
                "请求体不能为空",
            )));
        };
        if !state.has_provider_catalog_data_reader() || !state.has_provider_catalog_data_writer() {
            return Ok(Some(build_admin_providers_data_unavailable_response()));
        }
        let raw_value = match serde_json::from_slice::<serde_json::Value>(request_body) {
            Ok(value) => value,
            Err(_) => {
                return Ok(Some(build_admin_provider_bad_request_response(
                    "请求体必须是合法的 JSON 对象",
                )));
            }
        };
        let Some(raw_payload) = raw_value.as_object().cloned() else {
            return Ok(Some(build_admin_provider_bad_request_response(
                "请求体必须是合法的 JSON 对象",
            )));
        };
        let patch = match AdminProviderUpdatePatch::from_object(raw_payload) {
            Ok(patch) => patch,
            Err(_) => {
                return Ok(Some(build_admin_provider_bad_request_response(
                    "请求体必须是合法的 JSON 对象",
                )));
            }
        };
        let Some(existing_provider) = state
            .read_provider_catalog_providers_by_ids(std::slice::from_ref(&provider_id))
            .await?
            .into_iter()
            .next()
        else {
            return Ok(Some(build_admin_provider_not_found_response(format!(
                "Provider {provider_id} 不存在"
            ))));
        };
        let updated_record = match state
            .build_admin_update_provider_record(&existing_provider, patch)
            .await
        {
            Ok(record) => record,
            Err(detail) => return Ok(Some(build_admin_provider_bad_request_response(detail))),
        };
        let Some(_updated) = state
            .update_provider_catalog_provider(&updated_record)
            .await?
        else {
            return Ok(Some(build_admin_providers_data_unavailable_response()));
        };
        if existing_provider.quota_last_reset_at_unix_secs
            != updated_record.quota_last_reset_at_unix_secs
        {
            state
                .app()
                .clear_provider_quota_window_counters(&provider_id)
                .await?;
        }
        if state
            .fixed_provider_template(&updated_record.provider_type)
            .is_some()
        {
            reconcile_admin_fixed_provider_template_endpoints_after_update(
                state,
                &existing_provider,
                &updated_record,
            )
            .await?;
        }
        let warnings = provider_write_warnings(
            existing_provider.config.as_ref(),
            updated_record.config.as_ref(),
        );
        return Ok(Some(
            match state
                .build_admin_provider_summary_payload(&provider_id)
                .await
            {
                Some(mut payload) => {
                    attach_provider_write_warnings(&mut payload, warnings);
                    attach_admin_audit_response(
                        Json(payload).into_response(),
                        "admin_provider_updated",
                        "update_provider",
                        "provider",
                        &provider_id,
                    )
                }
                None => build_admin_provider_not_found_response(format!(
                    "Provider {provider_id} 不存在"
                )),
            },
        ));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{attach_provider_write_warnings, provider_write_warnings};
    use serde_json::json;

    #[test]
    fn sensitive_word_changes_produce_a_prompt_cache_warning_only_when_the_list_changed() {
        let before = json!({"cloak": {"sensitive_words": ["proxy"]}});
        let after = json!({"cloak": {"sensitive_words": ["proxy", "api"]}});
        assert_eq!(
            provider_write_warnings(Some(&before), Some(&after)),
            vec!["敏感词词表已变更，提示词缓存将失效"]
        );
        assert!(provider_write_warnings(Some(&before), Some(&before)).is_empty());
        assert!(provider_write_warnings(None, None).is_empty());
        // 新建时带词表也提示。
        assert_eq!(
            provider_write_warnings(None, Some(&after)),
            vec!["敏感词词表已变更，提示词缓存将失效"]
        );
    }

    #[test]
    fn warnings_are_attached_as_a_sibling_array_without_touching_the_rest() {
        let mut payload = json!({"id": "provider-1", "name": "claude"});
        attach_provider_write_warnings(&mut payload, Vec::new());
        assert!(payload.get("warnings").is_none());

        attach_provider_write_warnings(&mut payload, vec!["敏感词词表已变更，提示词缓存将失效"]);
        assert_eq!(payload["id"], "provider-1");
        assert_eq!(
            payload["warnings"],
            json!(["敏感词词表已变更，提示词缓存将失效"])
        );
    }
}
