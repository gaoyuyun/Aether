use super::shared::{
    build_provider_quota_execution_plan, build_quota_snapshot_payload,
    default_provider_quota_execution_timeouts, execute_provider_quota_plan,
    extract_execution_error_message, oauth_refresh_auto_removed_result,
    persist_provider_quota_refresh_state, quota_key_auto_removed,
    quota_refresh_success_invalid_state, ProviderQuotaExecutionOutcome,
};
use crate::handlers::admin::request::{AdminAppState, AdminGatewayProviderTransportSnapshot};
use crate::GatewayError;
use aether_admin::provider::quota::parse_gemini_cli_retrieve_user_quota_response;
use aether_admin::provider::redaction::admin_provider_metadata_bucket_safe_json;
use aether_contracts::ExecutionResult;
use aether_contracts::ProxySnapshot;
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogEndpoint, StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use aether_provider_pool::build_gemini_cli_pool_quota_request;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一次 retrieveUserQuota 响应折算出的 Key 状态更新。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GeminiCliQuotaRefreshOutcome {
    pub(super) status: &'static str,
    pub(super) message: Option<String>,
    pub(super) metadata_update: Option<serde_json::Value>,
    pub(super) oauth_invalid_at_unix_secs: Option<u64>,
    pub(super) oauth_invalid_reason: Option<String>,
}

/// 把 retrieveUserQuota 的 HTTP 结果映射成状态与元数据：200 且有 buckets 记成功，
/// 200 无 buckets 记 `no_metadata`，403 标记账户被禁并写入 `is_forbidden`，其余状态码
/// 只报错不改 OAuth 状态。
pub(super) fn classify_gemini_cli_quota_refresh_result(
    key: &StoredProviderCatalogKey,
    result: &ExecutionResult,
    now_unix_secs: u64,
) -> GeminiCliQuotaRefreshOutcome {
    let (mut oauth_invalid_at_unix_secs, mut oauth_invalid_reason) =
        quota_refresh_success_invalid_state(key);
    let mut metadata_update = None::<serde_json::Value>;
    let mut status = "error";
    let mut message = None::<String>;

    if result.status_code == 200 {
        if let Some(body_json) = result
            .body
            .as_ref()
            .and_then(|body| body.json_body.as_ref())
        {
            metadata_update =
                parse_gemini_cli_retrieve_user_quota_response(body_json, now_unix_secs)
                    .map(|metadata| json!({ "gemini_cli": metadata }));
            if metadata_update.is_some() {
                status = "success";
            } else {
                status = "no_metadata";
                message = Some("响应中未包含配额 buckets".to_string());
            }
        } else {
            status = "no_metadata";
            message = Some("响应中未包含配额信息".to_string());
        }
    } else {
        message = Some(format!(
            "retrieveUserQuota 返回状态码 {}",
            result.status_code
        ));
        if result.status_code == 403 {
            let reason = "账户访问被禁止".to_string();
            oauth_invalid_at_unix_secs = Some(now_unix_secs);
            oauth_invalid_reason = Some(format!("账户访问被禁止: {reason}"));
            metadata_update = Some(json!({
                "gemini_cli": {
                    "is_forbidden": true,
                    "forbidden_reason": reason,
                    "forbidden_at": now_unix_secs,
                    "updated_at": now_unix_secs,
                }
            }));
            status = "forbidden";
        }
    }

    GeminiCliQuotaRefreshOutcome {
        status,
        message,
        metadata_update,
        oauth_invalid_at_unix_secs,
        oauth_invalid_reason,
    }
}

/// 组装单个 Key 的刷新结果条目（管理端批量刷新响应里的一行）。
pub(super) fn build_gemini_cli_quota_result_payload(
    key: &StoredProviderCatalogKey,
    outcome: &GeminiCliQuotaRefreshOutcome,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert("key_id".to_string(), json!(key.id));
    payload.insert("key_name".to_string(), json!(key.name));
    payload.insert("status".to_string(), json!(outcome.status));
    if let Some(message) = outcome.message.as_ref() {
        payload.insert("message".to_string(), json!(message));
    }
    if let Some(metadata) = outcome
        .metadata_update
        .as_ref()
        .and_then(|value| value.get("gemini_cli"))
    {
        payload.insert(
            "metadata".to_string(),
            admin_provider_metadata_bucket_safe_json("gemini_cli", Some(metadata)),
        );
    }
    if let Some(quota_snapshot) = build_quota_snapshot_payload(
        "gemini_cli",
        key.status_snapshot.as_ref(),
        outcome.metadata_update.as_ref(),
    ) {
        payload.insert("quota_snapshot".to_string(), quota_snapshot);
    }
    serde_json::Value::Object(payload)
}

async fn execute_gemini_cli_quota_plan(
    state: &AdminAppState<'_>,
    transport: &AdminGatewayProviderTransportSnapshot,
    authorization: (String, String),
    project_id: &str,
    proxy_override: Option<&ProxySnapshot>,
) -> Result<ProviderQuotaExecutionOutcome, GatewayError> {
    let proxy = match proxy_override {
        Some(proxy) => Some(proxy.clone()),
        None => {
            state
                .resolve_transport_proxy_snapshot_with_tunnel_affinity(transport)
                .await
        }
    };
    let timeouts = state
        .resolve_transport_execution_timeouts(transport)
        .or(Some(default_provider_quota_execution_timeouts(
            proxy.as_ref(),
        )));
    let spec = build_gemini_cli_pool_quota_request(
        &transport.key.id,
        &transport.endpoint.base_url,
        authorization,
        project_id,
    );
    let plan = build_provider_quota_execution_plan(
        transport,
        spec,
        proxy,
        state.resolve_transport_profile(transport),
        timeouts,
    );

    execute_provider_quota_plan(state, transport, plan, "gemini_cli").await
}

pub(crate) async fn refresh_gemini_cli_provider_quota_locally(
    state: &AdminAppState<'_>,
    provider: &StoredProviderCatalogProvider,
    endpoint: &StoredProviderCatalogEndpoint,
    keys: Vec<StoredProviderCatalogKey>,
    proxy_override: Option<ProxySnapshot>,
) -> Result<Option<serde_json::Value>, GatewayError> {
    let mut results = Vec::new();
    let mut success_count = 0usize;
    let mut failed_count = 0usize;
    let mut auto_removed_count = 0usize;

    for key in keys {
        let mut transport = match state
            .read_provider_transport_snapshot(&provider.id, &endpoint.id, &key.id)
            .await?
        {
            Some(transport) => transport,
            None => {
                failed_count += 1;
                results.push(json!({
                    "key_id": key.id,
                    "key_name": key.name,
                    "status": "error",
                    "message": "Provider transport snapshot unavailable",
                }));
                continue;
            }
        };

        let authorization = match state.resolve_local_oauth_header_auth(&transport).await? {
            Some(auth) => auth,
            _ => {
                if quota_key_auto_removed(state, &key.id).await? {
                    auto_removed_count += 1;
                    results.push(oauth_refresh_auto_removed_result(&key));
                    continue;
                }
                failed_count += 1;
                results.push(json!({
                    "key_id": key.id,
                    "key_name": key.name,
                    "status": "error",
                    "message": "缺少 OAuth 认证信息，请先授权/刷新 Token",
                }));
                continue;
            }
        };

        let project_id = match crate::provider_transport::resolve_gemini_cli_project_id(&transport)
        {
            Some(project_id) => Some(project_id),
            None => state
                .app()
                .hydrate_gemini_cli_project_metadata_for_transport(&transport)
                .await
                .and_then(|hydrated| {
                    let project_id =
                        crate::provider_transport::resolve_gemini_cli_project_id(&hydrated);
                    transport = hydrated;
                    project_id
                }),
        };
        let Some(project_id) = project_id else {
            failed_count += 1;
            results.push(json!({
                "key_id": key.id,
                "key_name": key.name,
                "status": "error",
                "message": "缺少 Gemini CLI project_id，loadCodeAssist 未返回可用项目信息",
            }));
            continue;
        };

        let result = match execute_gemini_cli_quota_plan(
            state,
            &transport,
            authorization,
            &project_id,
            proxy_override.as_ref(),
        )
        .await?
        {
            ProviderQuotaExecutionOutcome::Response(result) => result,
            ProviderQuotaExecutionOutcome::Failure(_) => {
                failed_count += 1;
                results.push(json!({
                    "key_id": key.id,
                    "key_name": key.name,
                    "status": "error",
                    "message": "retrieveUserQuota 请求执行失败",
                    "status_code": 502,
                }));
                continue;
            }
        };

        let now_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let outcome = classify_gemini_cli_quota_refresh_result(&key, &result, now_unix_secs);

        if !persist_provider_quota_refresh_state(
            state,
            &key.id,
            outcome.metadata_update.as_ref(),
            outcome.oauth_invalid_at_unix_secs,
            outcome.oauth_invalid_reason.clone(),
            None,
        )
        .await?
        {
            failed_count += 1;
            results.push(json!({
                "key_id": key.id,
                "key_name": key.name,
                "status": "error",
                "message": "Key 状态写入失败",
            }));
            continue;
        }

        if outcome.status == "success" {
            success_count += 1;
        } else {
            failed_count += 1;
        }

        results.push(build_gemini_cli_quota_result_payload(&key, &outcome));
    }

    Ok(Some(json!({
        "success": success_count,
        "failed": failed_count,
        "total": results.len(),
        "results": results,
        "message": format!("已处理 {} 个 Key", results.len()),
        "auto_removed": auto_removed_count,
    })))
}

#[cfg(test)]
mod tests {
    use aether_contracts::{ExecutionResult, ResponseBody};
    use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogKey;
    use serde_json::json;
    use std::collections::BTreeMap;

    use super::{build_gemini_cli_quota_result_payload, classify_gemini_cli_quota_refresh_result};

    fn sample_key() -> StoredProviderCatalogKey {
        StoredProviderCatalogKey::new(
            "key-gemini-cli".to_string(),
            "provider-gemini-cli".to_string(),
            "Gemini CLI Key".to_string(),
            "oauth".to_string(),
            None,
            true,
        )
        .expect("key should build")
    }

    fn execution_result(status_code: u16, body: Option<serde_json::Value>) -> ExecutionResult {
        ExecutionResult {
            request_id: "req-quota".to_string(),
            candidate_id: None,
            status_code,
            headers: BTreeMap::new(),
            response_observation: None,
            body: body.map(|json_body| ResponseBody {
                json_body: Some(json_body),
                body_bytes_b64: None,
            }),
            telemetry: None,
            error: None,
        }
    }

    #[test]
    fn buckets_in_a_200_response_become_a_successful_metadata_update() {
        let result = execution_result(
            200,
            Some(json!({
                "buckets": [
                    {
                        "modelId": "gemini-2.5-pro",
                        "remainingFraction": 0.4,
                        "resetTime": "2026-09-21T00:00:00Z"
                    }
                ]
            })),
        );

        let outcome =
            classify_gemini_cli_quota_refresh_result(&sample_key(), &result, 1_800_000_000);

        assert_eq!(outcome.status, "success");
        assert_eq!(outcome.message, None);
        assert_eq!(outcome.oauth_invalid_at_unix_secs, None);
        assert_eq!(outcome.oauth_invalid_reason, None);
        let metadata = outcome.metadata_update.as_ref().expect("metadata update");
        assert!(metadata
            .pointer("/gemini_cli/quota_by_model/gemini-2.5-pro")
            .is_some());
        assert_eq!(
            metadata.pointer("/gemini_cli/updated_at"),
            Some(&json!(1_800_000_000u64))
        );

        let payload = build_gemini_cli_quota_result_payload(&sample_key(), &outcome);
        assert_eq!(payload["key_id"], "key-gemini-cli");
        assert_eq!(payload["key_name"], "Gemini CLI Key");
        assert_eq!(payload["status"], "success");
        assert!(payload.get("message").is_none());
        assert!(payload["metadata"].is_object());
    }

    #[test]
    fn a_200_without_buckets_is_reported_as_no_metadata() {
        let outcome = classify_gemini_cli_quota_refresh_result(
            &sample_key(),
            &execution_result(200, Some(json!({ "unexpected": true }))),
            1_800_000_000,
        );
        assert_eq!(outcome.status, "no_metadata");
        assert_eq!(outcome.message.as_deref(), Some("响应中未包含配额 buckets"));
        assert_eq!(outcome.metadata_update, None);

        let outcome = classify_gemini_cli_quota_refresh_result(
            &sample_key(),
            &execution_result(200, None),
            1_800_000_000,
        );
        assert_eq!(outcome.status, "no_metadata");
        assert_eq!(outcome.message.as_deref(), Some("响应中未包含配额信息"));

        let payload = build_gemini_cli_quota_result_payload(&sample_key(), &outcome);
        assert_eq!(payload["status"], "no_metadata");
        assert_eq!(payload["message"], "响应中未包含配额信息");
        assert!(payload.get("metadata").is_none());
    }

    #[test]
    fn a_403_marks_the_account_forbidden_and_invalidates_oauth() {
        let outcome = classify_gemini_cli_quota_refresh_result(
            &sample_key(),
            &execution_result(403, Some(json!({ "error": { "message": "forbidden" } }))),
            1_800_000_123,
        );

        assert_eq!(outcome.status, "forbidden");
        assert_eq!(
            outcome.message.as_deref(),
            Some("retrieveUserQuota 返回状态码 403")
        );
        assert_eq!(outcome.oauth_invalid_at_unix_secs, Some(1_800_000_123));
        assert_eq!(
            outcome.oauth_invalid_reason.as_deref(),
            Some("账户访问被禁止: 账户访问被禁止")
        );
        let metadata = outcome
            .metadata_update
            .as_ref()
            .expect("forbidden metadata");
        assert_eq!(
            metadata.pointer("/gemini_cli/is_forbidden"),
            Some(&json!(true))
        );
        assert_eq!(
            metadata.pointer("/gemini_cli/forbidden_at"),
            Some(&json!(1_800_000_123u64))
        );
    }

    #[test]
    fn other_error_statuses_only_report_the_status_code() {
        let outcome = classify_gemini_cli_quota_refresh_result(
            &sample_key(),
            &execution_result(503, Some(json!({ "error": "unavailable" }))),
            1_800_000_000,
        );

        assert_eq!(outcome.status, "error");
        assert_eq!(
            outcome.message.as_deref(),
            Some("retrieveUserQuota 返回状态码 503")
        );
        assert_eq!(outcome.metadata_update, None);
        assert_eq!(outcome.oauth_invalid_at_unix_secs, None);
        assert_eq!(outcome.oauth_invalid_reason, None);
    }
}
