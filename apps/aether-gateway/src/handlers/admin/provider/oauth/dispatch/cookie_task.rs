use super::super::errors::build_internal_control_error_response;
use super::super::provisioning::{
    provider_oauth_key_proxy_value, provision_provider_oauth_token_payload_for_provider,
};
use super::super::runtime::resolve_provider_oauth_runtime_endpoints;
use super::super::state::{
    authorize_admin_provider_oauth_with_cookie,
    build_admin_provider_oauth_backend_unavailable_response,
};
use super::batch::build_admin_provider_oauth_batch_task_state;
use super::cookie::normalize_claude_session_key;
use crate::handlers::admin::provider::oauth::format_provider_oauth_task_timestamps;
use crate::handlers::admin::provider::shared::paths::admin_provider_oauth_cookie_task_provider_id;
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::task_runtime::{
    append_event_with_logging, now_unix_secs, task_definition, update_run_status,
    upsert_run_with_logging, TASK_KEY_PROVIDER_OAUTH_BATCH_IMPORT,
};
use crate::GatewayError;
use aether_data_contracts::repository::background_tasks::{
    BackgroundTaskKind, BackgroundTaskStatus, UpsertBackgroundTaskRun,
};
use axum::{
    body::{to_bytes, Body, Bytes},
    http,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task;
use uuid::Uuid;

const CLAUDE_COOKIE_TASK_IMPORT_KIND: &str = "cookie_authorize";
const CLAUDE_COOKIE_TASK_ID_PREFIX: &str = "claude-cookie-";
const MAX_CLAUDE_COOKIE_TASK_ENTRIES: usize = 20;
const CLAUDE_COOKIE_AUTHORIZATION_CONCURRENCY: usize = 3;
const MAX_SAFE_ERROR_DETAIL_BYTES: usize = 512;

type ClaudeCookieTaskEntry = Result<String, String>;

struct ClaudeCookieTaskRequest {
    entries: Vec<ClaudeCookieTaskEntry>,
    proxy_node_id: Option<String>,
}

pub(super) async fn handle_admin_provider_oauth_start_cookie_task(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
    request_body: Option<&Bytes>,
) -> Result<Response<Body>, GatewayError> {
    if !state.has_provider_catalog_data_reader() {
        return Ok(build_admin_provider_oauth_backend_unavailable_response());
    }
    let Some(provider_id) = admin_provider_oauth_cookie_task_provider_id(request_context.path())
    else {
        return Ok(build_internal_control_error_response(
            http::StatusCode::NOT_FOUND,
            "Provider 不存在",
        ));
    };
    let payload = match parse_claude_cookie_task_request(request_body) {
        Ok(payload) => payload,
        Err(response) => return Ok(response),
    };

    let Some(provider) = state
        .read_provider_catalog_providers_by_ids(std::slice::from_ref(&provider_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(build_internal_control_error_response(
            http::StatusCode::NOT_FOUND,
            "Provider 不存在",
        ));
    };
    let provider_type = provider.provider_type.trim().to_ascii_lowercase();
    if provider_type != "claude_code" {
        return Ok(build_internal_control_error_response(
            http::StatusCode::BAD_REQUEST,
            "Cookie 授权仅支持 Claude Code Provider",
        ));
    }

    let endpoint_resolution =
        resolve_provider_oauth_runtime_endpoints(state, &provider, &provider_type).await?;
    let endpoints = endpoint_resolution.endpoints;
    let request_proxy = state
        .resolve_admin_provider_oauth_operation_proxy_snapshot(
            payload.proxy_node_id.as_deref(),
            &[
                endpoint_resolution
                    .runtime_endpoint
                    .as_ref()
                    .and_then(|endpoint| endpoint.proxy.as_ref()),
                provider.proxy.as_ref(),
            ],
        )
        .await;
    let key_proxy = provider_oauth_key_proxy_value(payload.proxy_node_id.as_deref());

    let task_id = format!("{CLAUDE_COOKIE_TASK_ID_PREFIX}{}", Uuid::new_v4());
    let total = payload.entries.len();
    let created_at = now_unix_secs();
    let submitted_state = build_admin_provider_oauth_batch_task_state(
        &task_id,
        &provider_id,
        &provider_type,
        CLAUDE_COOKIE_TASK_IMPORT_KIND,
        "submitted",
        total,
        0,
        0,
        0,
        0,
        0,
        Some("任务已提交，等待执行"),
        None,
        Vec::new(),
        created_at,
        None,
        None,
    );
    if state
        .save_provider_oauth_batch_task_payload(&task_id, &submitted_state)
        .await
        .is_err()
    {
        return Ok(build_internal_control_error_response(
            http::StatusCode::SERVICE_UNAVAILABLE,
            "provider oauth batch task redis unavailable",
        ));
    }

    if state.has_background_task_data_writer() {
        let max_attempts = task_definition(TASK_KEY_PROVIDER_OAUTH_BATCH_IMPORT)
            .map(|item| item.retry_policy.max_attempts)
            .unwrap_or(1);
        let run = UpsertBackgroundTaskRun {
            id: task_id.clone(),
            task_key: TASK_KEY_PROVIDER_OAUTH_BATCH_IMPORT.to_string(),
            kind: BackgroundTaskKind::OnDemand,
            trigger: "manual".to_string(),
            status: BackgroundTaskStatus::Queued,
            attempt: 1,
            max_attempts,
            owner_instance: Some(state.app().tunnel.local_instance_id().to_string()),
            progress_percent: 0,
            progress_message: Some("Claude Cookie authorization queued".to_string()),
            payload_json: Some(json!({
                "provider_id": provider_id.clone(),
                "provider_type": provider_type.clone(),
                "import_kind": CLAUDE_COOKIE_TASK_IMPORT_KIND,
                "total": total,
            })),
            result_json: None,
            error_message: None,
            cancel_requested: false,
            created_by: Some("admin".to_string()),
            created_at_unix_secs: created_at,
            started_at_unix_secs: None,
            finished_at_unix_secs: None,
            updated_at_unix_secs: created_at,
        };
        let _ = upsert_run_with_logging(state.app(), run).await;
        append_event_with_logging(
            state.app(),
            &task_id,
            "queued",
            "Claude Cookie authorization queued",
            Some(json!({
                "provider_id": provider_id.clone(),
                "provider_type": provider_type.clone(),
                "import_kind": CLAUDE_COOKIE_TASK_IMPORT_KIND,
                "total": total,
            })),
        )
        .await;
    }

    let task_state = state.cloned_app();
    let task_id_for_worker = task_id.clone();
    let provider_id_for_worker = provider_id.clone();
    let provider_type_for_worker = provider_type.clone();
    task::spawn(async move {
        let started_at = current_unix_secs_or(created_at);
        let task_admin_state = AdminAppState::new(&task_state);
        save_cookie_task_state(
            &task_admin_state,
            &task_id_for_worker,
            &provider_id_for_worker,
            &provider_type_for_worker,
            "processing",
            total,
            0,
            0,
            0,
            0,
            0,
            Some("正在获取 Claude 授权"),
            Vec::new(),
            created_at,
            started_at,
            None,
        )
        .await;
        let _ = update_run_status(
            &task_state,
            &task_id_for_worker,
            BackgroundTaskStatus::Running,
            Some(1),
            Some("Claude Cookie authorization started".to_string()),
            None,
            None,
            Some(started_at),
            None,
        )
        .await;
        append_event_with_logging(
            &task_state,
            &task_id_for_worker,
            "running",
            "Claude Cookie authorization started",
            None,
        )
        .await;

        let mut pending = stream::iter(payload.entries.into_iter().enumerate().map(
            |(index, entry)| {
                let proxy = request_proxy.clone();
                let task_admin_state = &task_admin_state;
                async move {
                    let result = match entry {
                        Ok(session_key) => authorize_admin_provider_oauth_with_cookie(
                            task_admin_state,
                            session_key,
                            proxy,
                        )
                        .await
                        .map_err(|_| "Claude Cookie 授权失败".to_string()),
                        Err(detail) => Err(detail),
                    };
                    (index, result)
                }
            },
        ))
        .buffer_unordered(CLAUDE_COOKIE_AUTHORIZATION_CONCURRENCY);
        let mut authorization_results = Vec::with_capacity(total);
        while let Some(result) = pending.next().await {
            authorization_results.push(result);
        }
        authorization_results.sort_by_key(|(index, _)| *index);

        let mut success = 0usize;
        let mut failed = 0usize;
        let mut created_count = 0usize;
        let mut replaced_count = 0usize;
        let mut error_samples = Vec::new();

        for (index, authorization_result) in authorization_results {
            let result = match authorization_result {
                Ok(token_payload) => match provision_provider_oauth_token_payload_for_provider(
                    &task_admin_state,
                    &provider,
                    &endpoints,
                    &token_payload,
                    None,
                    key_proxy.clone(),
                    request_proxy.clone(),
                    "cookie-authorize-batch",
                )
                .await
                {
                    Ok(response) => cookie_task_item_from_response(index, response).await,
                    Err(_) => cookie_task_error(index, "provider oauth write unavailable"),
                },
                Err(detail) => cookie_task_error(index, detail.as_str()),
            };

            if result.get("status").and_then(Value::as_str) == Some("success") {
                success += 1;
                if result.get("replaced").and_then(Value::as_bool) == Some(true) {
                    replaced_count += 1;
                } else {
                    created_count += 1;
                }
            } else {
                failed += 1;
                error_samples.push(result);
            }
            let processed = success.saturating_add(failed);
            let message = format!("处理中 {processed}/{total}");
            save_cookie_task_state(
                &task_admin_state,
                &task_id_for_worker,
                &provider_id_for_worker,
                &provider_type_for_worker,
                "processing",
                total,
                processed,
                success,
                failed,
                created_count,
                replaced_count,
                Some(message.as_str()),
                error_samples.clone(),
                created_at,
                started_at,
                None,
            )
            .await;
        }

        let finished_at = current_unix_secs_or(started_at);
        let message = format!("授权完成：成功 {success}，失败 {failed}");
        save_cookie_task_state(
            &task_admin_state,
            &task_id_for_worker,
            &provider_id_for_worker,
            &provider_type_for_worker,
            "completed",
            total,
            total,
            success,
            failed,
            created_count,
            replaced_count,
            Some(message.as_str()),
            error_samples,
            created_at,
            started_at,
            Some(finished_at),
        )
        .await;
        let _ = update_run_status(
            &task_state,
            &task_id_for_worker,
            BackgroundTaskStatus::Succeeded,
            Some(100),
            Some(message),
            Some(json!({
                "provider_id": provider_id_for_worker,
                "provider_type": provider_type_for_worker,
                "import_kind": CLAUDE_COOKIE_TASK_IMPORT_KIND,
                "total": total,
                "success": success,
                "failed": failed,
                "created_count": created_count,
                "replaced_count": replaced_count,
            })),
            None,
            None,
            Some(finished_at),
        )
        .await;
        append_event_with_logging(
            &task_state,
            &task_id_for_worker,
            "succeeded",
            "Claude Cookie authorization completed",
            None,
        )
        .await;
    });

    let mut submitted_response = submitted_state;
    format_provider_oauth_task_timestamps(&mut submitted_response);
    Ok(Json(submitted_response).into_response())
}

#[allow(clippy::too_many_arguments)]
async fn save_cookie_task_state(
    state: &AdminAppState<'_>,
    task_id: &str,
    provider_id: &str,
    provider_type: &str,
    status: &str,
    total: usize,
    processed: usize,
    success: usize,
    failed: usize,
    created_count: usize,
    replaced_count: usize,
    message: Option<&str>,
    error_samples: Vec<Value>,
    created_at: u64,
    started_at: u64,
    finished_at: Option<u64>,
) {
    let task_state = build_admin_provider_oauth_batch_task_state(
        task_id,
        provider_id,
        provider_type,
        CLAUDE_COOKIE_TASK_IMPORT_KIND,
        status,
        total,
        processed,
        success,
        failed,
        created_count,
        replaced_count,
        message,
        None,
        error_samples,
        created_at,
        Some(started_at),
        finished_at,
    );
    let _ = state
        .save_provider_oauth_batch_task_payload(task_id, &task_state)
        .await;
}

async fn cookie_task_item_from_response(index: usize, response: Response<Body>) -> Value {
    let status = response.status();
    let body = to_bytes(response.into_body(), crate::MAX_ERROR_BODY_BYTES)
        .await
        .ok();
    let payload = body
        .as_deref()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    if status.is_success() {
        let Some(payload) = payload else {
            return cookie_task_error(index, "provider oauth write unavailable");
        };
        let Some(key_id) = payload.get("key_id").and_then(Value::as_str) else {
            return cookie_task_error(index, "provider oauth write unavailable");
        };
        return json!({
            "index": index,
            "status": "success",
            "key_id": key_id,
            "email": payload.get("email").cloned().unwrap_or(Value::Null),
            "replaced": payload.get("replaced").and_then(Value::as_bool).unwrap_or(false),
            "error": Value::Null,
        });
    }

    let detail = payload
        .as_ref()
        .and_then(|payload| payload.get("detail"))
        .and_then(Value::as_str)
        .and_then(safe_error_detail)
        .unwrap_or("Claude 账号创建或更新失败");
    cookie_task_error(index, detail)
}

fn safe_error_detail(detail: &str) -> Option<&str> {
    let detail = detail.trim();
    if detail.is_empty() || detail.len() > MAX_SAFE_ERROR_DETAIL_BYTES {
        return None;
    }
    let normalized = detail.to_ascii_lowercase();
    if normalized.contains("sessionkey")
        || normalized.contains("sk-ant-")
        || normalized.contains("cookie:")
    {
        return None;
    }
    Some(detail)
}

fn cookie_task_error(index: usize, detail: &str) -> Value {
    json!({
        "index": index,
        "status": "error",
        "error": detail,
        "replaced": false,
    })
}

fn parse_claude_cookie_task_request(
    request_body: Option<&Bytes>,
) -> Result<ClaudeCookieTaskRequest, Response<Body>> {
    let Some(request_body) = request_body else {
        return Err(bad_cookie_task_request("请求体必须是合法的 JSON 对象"));
    };
    let payload = serde_json::from_slice::<Value>(request_body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| bad_cookie_task_request("请求体必须是合法的 JSON 对象"))?;

    let legacy_keys = ["cookie", "session_key", "sessionKey"];
    let has_legacy_cookie = legacy_keys.iter().any(|key| payload.contains_key(*key));
    let raw_entries = if let Some(cookies) = payload.get("cookies") {
        if has_legacy_cookie {
            return Err(bad_cookie_task_request("cookie 与 cookies 不能同时提供"));
        }
        let cookies = cookies
            .as_array()
            .ok_or_else(|| bad_cookie_task_request("cookies 必须是字符串数组"))?;
        if cookies.is_empty() {
            return Err(bad_cookie_task_request("Cookie 不能为空"));
        }
        if cookies.len() > MAX_CLAUDE_COOKIE_TASK_ENTRIES {
            return Err(bad_cookie_task_request("Cookie 批量授权最多支持 20 条"));
        }
        cookies
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| bad_cookie_task_request("cookies 必须是字符串数组"))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let raw = legacy_keys
            .into_iter()
            .find_map(|key| payload.get(key).and_then(Value::as_str))
            .ok_or_else(|| bad_cookie_task_request("Cookie 不能为空"))?;
        let entries = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Err(bad_cookie_task_request("Cookie 不能为空"));
        }
        if entries.len() > MAX_CLAUDE_COOKIE_TASK_ENTRIES {
            return Err(bad_cookie_task_request("Cookie 批量授权最多支持 20 条"));
        }
        entries
    };

    let mut seen_session_keys = HashSet::new();
    let entries = raw_entries
        .into_iter()
        .map(|raw| {
            let session_key =
                normalize_claude_session_key(&raw).ok_or_else(|| "Cookie 格式无效".to_string())?;
            if !seen_session_keys.insert(session_key.clone()) {
                return Err("Cookie 重复".to_string());
            }
            Ok(session_key)
        })
        .collect();
    Ok(ClaudeCookieTaskRequest {
        entries,
        proxy_node_id: optional_trimmed_string(&payload, "proxy_node_id")
            .or_else(|| optional_trimmed_string(&payload, "proxyNodeId")),
    })
}

fn optional_trimmed_string(payload: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn bad_cookie_task_request(detail: &'static str) -> Response<Body> {
    build_internal_control_error_response(http::StatusCode::BAD_REQUEST, detail)
}

fn current_unix_secs_or(fallback: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_claude_cookie_task_request, safe_error_detail, MAX_CLAUDE_COOKIE_TASK_ENTRIES,
    };
    use axum::body::{to_bytes, Bytes};
    use serde_json::json;

    #[test]
    fn parses_canonical_and_multiline_cookie_batches_without_retaining_raw_headers() {
        for payload in [
            json!({
                "cookies": [
                    "sessionKey=sk-ant-sid01-one",
                    "Cookie: theme=dark; sessionKey=sk-ant-sid01-two"
                ],
                "proxy_node_id": "proxy-1"
            }),
            json!({
                "cookie": "sessionKey=sk-ant-sid01-one\n\nCookie: sessionKey=sk-ant-sid01-two",
                "proxyNodeId": "proxy-1"
            }),
        ] {
            let body = Bytes::from(payload.to_string());
            let parsed =
                parse_claude_cookie_task_request(Some(&body)).expect("cookie batch should parse");
            assert_eq!(parsed.entries.len(), 2);
            assert_eq!(parsed.entries[0].as_deref(), Ok("sk-ant-sid01-one"));
            assert_eq!(parsed.entries[1].as_deref(), Ok("sk-ant-sid01-two"));
            assert_eq!(parsed.proxy_node_id.as_deref(), Some("proxy-1"));
        }
    }

    #[test]
    fn keeps_invalid_cookie_lines_as_independent_sanitized_results() {
        let body = Bytes::from(
            json!({
                "cookies": ["foo=bar", "sessionKey=valid", "Cookie: sessionKey=valid"]
            })
            .to_string(),
        );
        let parsed =
            parse_claude_cookie_task_request(Some(&body)).expect("request should be accepted");
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(
            parsed.entries[0].as_ref().expect_err("entry should fail"),
            "Cookie 格式无效"
        );
        assert_eq!(parsed.entries[1].as_deref(), Ok("valid"));
        assert_eq!(
            parsed.entries[2].as_ref().expect_err("entry should fail"),
            "Cookie 重复"
        );
    }

    #[test]
    fn accepts_twenty_long_session_keys_above_previous_body_cap() {
        let cookies = (0..MAX_CLAUDE_COOKIE_TASK_ENTRIES)
            .map(|index| {
                let prefix = format!("{index:02}-");
                format!("{prefix}{}", "x".repeat(40 * 1024))
            })
            .collect::<Vec<_>>();
        let body = Bytes::from(json!({"cookies": cookies}).to_string());
        assert!(body.len() > 768 * 1024);
        let parsed =
            parse_claude_cookie_task_request(Some(&body)).expect("large valid batch should parse");
        assert_eq!(parsed.entries.len(), MAX_CLAUDE_COOKIE_TASK_ENTRIES);
        assert!(parsed.entries.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn rejects_ambiguous_cookie_batches_without_echoing_secrets() {
        let too_many = vec!["sessionKey=value"; MAX_CLAUDE_COOKIE_TASK_ENTRIES + 1];
        for payload in [
            json!({"cookie": "sessionKey=secret", "cookies": ["sessionKey=other"]}),
            json!({"cookies": too_many}),
            json!({"cookies": []}),
        ] {
            let body = Bytes::from(payload.to_string());
            let response = match parse_claude_cookie_task_request(Some(&body)) {
                Ok(_) => panic!("request should fail"),
                Err(response) => response,
            };
            let response_body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body should read");
            let text = String::from_utf8_lossy(&response_body);
            assert!(!text.contains("secret"));
            assert!(!text.contains("other"));
        }
    }

    #[test]
    fn error_detail_filter_rejects_possible_cookie_or_token_leaks() {
        assert_eq!(safe_error_detail("账号重复"), Some("账号重复"));
        assert!(safe_error_detail("sessionKey=secret").is_none());
        assert!(safe_error_detail("upstream sk-ant-oat01-secret").is_none());
        assert!(safe_error_detail("Cookie: secret").is_none());
    }
}
