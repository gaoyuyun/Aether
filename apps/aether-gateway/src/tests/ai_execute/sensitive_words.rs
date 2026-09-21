//! P6 敏感词零宽混淆端到端验收（计划 §6.2 验收原文）。
//!
//! - 供应商词表 `["proxy","API"]` 下，第三方客户端请求经网关后 mock 上游收到的 body 中
//!   `proxy` 变成 `p\u{200B}roxy`、`API` 被混淆且大小写不敏感；`tool_use.input` 与
//!   `tool_result` 不变；`report_context.sensitive_words_obfuscation` 出现在**顶层**并随 usage
//!   落库；CCH 签名对混淆后的 body 校验通过。
//! - 同一词表下原生 Claude Code 请求（oracle fixture 的原生头）逐字节不变。
//! - `cloak.mode=off` 时不混淆。
//! - Key 级 `auth_config.cloak_sensitive_words` 覆盖供应商词表。
//! - Antigravity：`request.systemInstruction.parts[].text` 被混淆、`contents[].parts[].functionCall`
//!   不变、报告在顶层。

use std::collections::BTreeMap;

use aether_crypto::{encrypt_python_fernet_plaintext, DEVELOPMENT_ENCRYPTION_KEY};
use aether_data::repository::auth::{
    InMemoryAuthApiKeySnapshotRepository, StoredAuthApiKeySnapshot,
};
use aether_data::repository::candidate_selection::InMemoryMinimalCandidateSelectionReadRepository;
use aether_data::repository::candidates::InMemoryRequestCandidateRepository;
use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data_contracts::repository::candidate_selection::{
    StoredMinimalCandidateSelectionRow, StoredProviderModelMapping,
};
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogEndpoint, StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use aether_data_contracts::repository::usage::{StoredRequestUsageAudit, UsageReadRepository};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::{
    any, build_router_with_state, build_state_with_execution_runtime_override, json, start_server,
    to_bytes, Arc, Body, HeaderValue, Json, Mutex, Request, Response, Router, StatusCode,
    UsageRuntimeConfig, TRACE_ID_HEADER,
};

const ORACLE_BODY: &[u8] =
    include_bytes!("../fixtures/claude_code/native_cli_2_1_161_messages.body.json");
const ORACLE_HEADERS: &str =
    include_str!("../fixtures/claude_code/native_cli_2_1_161_messages.headers.json");
const SENSITIVE_WORDS_TEST_STACK_BYTES: usize = 16 * 1024 * 1024;
const ZWSP: char = '\u{200B}';

fn run_sensitive_words_test<F, Fut>(test_name: &'static str, make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(SENSITIVE_WORDS_TEST_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(make_future());
        })
        .expect("sensitive words test thread should spawn");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

fn hash_api_key(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn oracle_headers() -> Vec<(String, String)> {
    serde_json::from_str::<Vec<(String, String)>>(ORACLE_HEADERS).expect("oracle headers parse")
}

fn third_party_headers() -> Vec<(String, String)> {
    vec![
        ("accept".to_string(), "application/json".to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        (
            "authorization".to_string(),
            "Bearer placeholder".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
        (
            "user-agent".to_string(),
            "anthropic-sdk-python/0.40.0".to_string(),
        ),
    ]
}

/// 第三方客户端的非流式请求：系统提示与用户文本含敏感词，工具调用与工具结果也含同样的词。
fn third_party_body() -> serde_json::Value {
    json!({
        "model": "claude-opus-4-6",
        "max_tokens": 256,
        "system": [
            {"type": "text", "text": "You are a helpful proxy for the Anthropic API."}
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Use the proxy tool now."}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "proxy_lookup", "input": {"query": "proxy API"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "proxy API result"},
                {"type": "text", "text": "Thanks."}
            ]}
        ],
        "metadata": {"user_id": "third-party-user"}
    })
}

async fn wait_for_completed_usage<T>(repository: &T, request_id: &str) -> StoredRequestUsageAudit
where
    T: UsageReadRepository + ?Sized,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(usage) = repository
            .find_by_request_id(request_id)
            .await
            .expect("usage should read")
        {
            if usage.status == "completed" {
                return usage;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "usage {request_id} should complete"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn usage_obfuscation_report(usage: &StoredRequestUsageAudit) -> Option<serde_json::Value> {
    usage
        .request_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("sensitive_words_obfuscation"))
        .cloned()
}

fn contains_zero_width(value: &serde_json::Value) -> bool {
    value.to_string().contains(ZWSP)
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

fn claude_auth_snapshot() -> StoredAuthApiKeySnapshot {
    StoredAuthApiKeySnapshot::new(
        "user-claude-words-1".to_string(),
        "alice".to_string(),
        Some("alice@example.com".to_string()),
        "user".to_string(),
        "local".to_string(),
        true,
        false,
        Some(json!(["claude", "claude_code"])),
        Some(json!(["claude:messages"])),
        Some(json!(["claude-opus-4-6"])),
        "api-key-claude-words-1".to_string(),
        Some("default".to_string()),
        true,
        false,
        false,
        Some(60),
        Some(5),
        Some(4_102_444_800),
        Some(json!(["claude", "claude_code"])),
        Some(json!(["claude:messages"])),
        Some(json!(["claude-opus-4-6"])),
    )
    .expect("auth snapshot should build")
}

fn claude_candidate_row() -> StoredMinimalCandidateSelectionRow {
    StoredMinimalCandidateSelectionRow {
        provider_id: "provider-claude-words-1".to_string(),
        provider_name: "claude_code".to_string(),
        provider_type: "claude_code".to_string(),
        provider_priority: 10,
        provider_is_active: true,
        endpoint_id: "endpoint-claude-words-1".to_string(),
        endpoint_api_format: "claude:messages".to_string(),
        endpoint_api_family: Some("claude".to_string()),
        endpoint_kind: Some("cli".to_string()),
        endpoint_is_active: true,
        key_id: "key-claude-words-1".to_string(),
        key_name: "prod".to_string(),
        key_auth_type: "oauth".to_string(),
        key_is_active: true,
        key_api_formats: Some(vec!["claude:messages".to_string()]),
        key_allowed_models: None,
        key_capabilities: None,
        key_internal_priority: 5,
        key_global_priority_by_format: Some(json!({"claude:messages": 1})),
        model_id: "model-claude-words-1".to_string(),
        global_model_id: "global-model-claude-words-1".to_string(),
        global_model_name: "claude-opus-4-6".to_string(),
        global_model_mappings: None,
        global_model_supports_streaming: Some(true),
        model_provider_model_name: "claude-opus-4-6".to_string(),
        model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
            name: "claude-opus-4-6".to_string(),
            priority: 1,
            api_formats: Some(vec!["claude:messages".to_string()]),
            endpoint_ids: None,
            operations: None,
        }]),
        model_supports_streaming: Some(true),
        model_is_active: true,
        model_is_available: true,
        provider_pool_enabled: false,
    }
}

fn claude_provider(cloak_mode: &str, words: &[&str]) -> StoredProviderCatalogProvider {
    StoredProviderCatalogProvider::new(
        "provider-claude-words-1".to_string(),
        "claude_code".to_string(),
        Some("https://example.com".to_string()),
        "claude_code".to_string(),
    )
    .expect("provider should build")
    .with_transport_fields(
        true,
        false,
        false,
        None,
        Some(2),
        None,
        Some(20.0),
        None,
        Some(json!({
            "claude_code_advanced": {"cli_only_enabled": false},
            "cloak": {"mode": cloak_mode, "sensitive_words": words}
        })),
    )
}

fn claude_endpoint() -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        "endpoint-claude-words-1".to_string(),
        "provider-claude-words-1".to_string(),
        "claude:messages".to_string(),
        Some("claude".to_string()),
        Some("cli".to_string()),
        true,
    )
    .expect("endpoint should build")
    .with_transport_fields(
        "https://api.anthropic.com/v1".to_string(),
        None,
        None,
        Some(2),
        None,
        None,
        None,
        None,
    )
    .expect("endpoint transport should build")
}

fn claude_key(key_override: Option<&[&str]>) -> StoredProviderCatalogKey {
    let mut auth_config = json!({
        "provider_type": "claude_code",
        "account_uuid": "6f1c7d8e-1b2c-4d3e-8f90-123456789abc",
        "refresh_token": "sk-ant-ort01-words"
    });
    if let Some(words) = key_override {
        auth_config["cloak_sensitive_words"] = json!(words);
    }
    StoredProviderCatalogKey::new(
        "key-claude-words-1".to_string(),
        "provider-claude-words-1".to_string(),
        "prod".to_string(),
        "oauth".to_string(),
        None,
        true,
    )
    .expect("key should build")
    .with_transport_fields(
        Some(json!(["claude:messages"])),
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "sk-ant-oat01-upstream-words")
            .expect("api key should encrypt"),
        Some(
            encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, &auth_config.to_string())
                .expect("auth config should encrypt"),
        ),
        None,
        Some(json!({"claude:messages": 1})),
        None,
        None,
        None,
        None,
    )
    .expect("key transport should build")
}

#[derive(Debug, Clone, Default)]
struct SeenPlan {
    headers: BTreeMap<String, String>,
    body_bytes: Option<Vec<u8>>,
    json_body: Option<serde_json::Value>,
}

fn capture_plan(payload: &serde_json::Value) -> SeenPlan {
    let headers = payload
        .get("headers")
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let body_bytes = payload
        .pointer("/body/body_bytes_b64")
        .and_then(serde_json::Value::as_str)
        .map(|b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .expect("body_bytes_b64 should decode")
        });
    SeenPlan {
        headers,
        body_bytes,
        json_body: payload.pointer("/body/json_body").cloned(),
    }
}

struct ClaudeRun {
    seen: SeenPlan,
    usage: Arc<InMemoryUsageReadRepository>,
}

async fn run_claude_request(
    cloak_mode: &str,
    words: &[&str],
    key_override: Option<&[&str]>,
    request_headers: Vec<(String, String)>,
    body: Vec<u8>,
    trace_id: &'static str,
) -> ClaudeRun {
    let seen = Arc::new(Mutex::new(SeenPlan::default()));
    let seen_stream = Arc::clone(&seen);
    let seen_sync = Arc::clone(&seen);
    let execution_runtime = Router::new()
        .route(
            "/v1/execute/stream",
            any(move |request: Request| {
                let seen_inner = Arc::clone(&seen_stream);
                async move {
                    let (_parts, body) = request.into_parts();
                    let raw = to_bytes(body, usize::MAX).await.expect("body should read");
                    let payload: serde_json::Value =
                        serde_json::from_slice(&raw).expect("execution plan should parse");
                    *seen_inner.lock().expect("mutex should lock") = capture_plan(&payload);
                    let frames = concat!(
                        "{\"type\":\"headers\",\"payload\":{\"kind\":\"headers\",\"status_code\":200,\"headers\":{\"content-type\":\"text/event-stream\"}}}\n",
                        "{\"type\":\"data\",\"payload\":{\"kind\":\"data\",\"text\":\"event: message_start\\ndata: {\\\"type\\\":\\\"message_start\\\"}\\n\\n\"}}\n",
                        "{\"type\":\"data\",\"payload\":{\"kind\":\"data\",\"text\":\"event: message_stop\\ndata: {\\\"type\\\":\\\"message_stop\\\"}\\n\\n\"}}\n",
                        "{\"type\":\"telemetry\",\"payload\":{\"kind\":\"telemetry\",\"telemetry\":{\"elapsed_ms\":31,\"ttfb_ms\":11,\"upstream_bytes\":37}}}\n",
                        "{\"type\":\"eof\",\"payload\":{\"kind\":\"eof\"}}\n"
                    );
                    let mut response = Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(frames))
                        .expect("response should build");
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-ndjson"),
                    );
                    response
                }
            }),
        )
        .route(
            "/v1/execute/sync",
            any(move |request: Request| {
                let seen_inner = Arc::clone(&seen_sync);
                async move {
                    let (_parts, body) = request.into_parts();
                    let raw = to_bytes(body, usize::MAX).await.expect("body should read");
                    let payload: serde_json::Value =
                        serde_json::from_slice(&raw).expect("execution plan should parse");
                    let request_id = payload
                        .get("request_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    *seen_inner.lock().expect("mutex should lock") = capture_plan(&payload);
                    Json(json!({
                        "request_id": request_id,
                        "status_code": 200,
                        "headers": {"content-type": "application/json"},
                        "body": {
                            "json_body": {
                                "id": "msg_words_1",
                                "type": "message",
                                "role": "assistant",
                                "model": "claude-opus-4-6",
                                "content": [{"type": "text", "text": "ok"}],
                                "stop_reason": "end_turn",
                                "stop_sequence": null,
                                "usage": {"input_tokens": 2, "output_tokens": 3}
                            }
                        },
                        "telemetry": {"elapsed_ms": 27}
                    }))
                }
            }),
        );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-client-claude-words")),
        claude_auth_snapshot(),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            claude_candidate_row(),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![claude_provider(cloak_mode, words)],
        vec![claude_endpoint()],
        vec![claude_key(key_override)],
    ));
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway_state = build_state_with_execution_runtime_override(execution_runtime_url.clone())
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_request_candidates_and_usage_for_tests(
                auth_repository,
                candidate_selection_repository,
                provider_catalog_repository,
                Arc::clone(&request_candidate_repository),
                Arc::clone(&usage_repository),
                DEVELOPMENT_ENCRYPTION_KEY,
            ),
        )
        .with_usage_runtime_for_tests(UsageRuntimeConfig {
            enabled: true,
            ..UsageRuntimeConfig::default()
        });
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .header(TRACE_ID_HEADER, trace_id);
    for (name, value) in request_headers {
        if name == "authorization" {
            request = request.header(name, "Bearer sk-client-claude-words");
        } else if name == "accept-encoding" || name == "content-length" {
            continue;
        } else {
            request = request.header(name, value);
        }
    }
    let response = request
        .body(body)
        .send()
        .await
        .expect("request should succeed");
    let status = response.status();
    let text = response.text().await.expect("body should read");
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected gateway response: {text}"
    );

    let usage = wait_for_completed_usage(usage_repository.as_ref(), trace_id).await;
    assert_eq!(usage.request_id, trace_id);

    gateway_handle.abort();
    execution_runtime_handle.abort();
    let seen = seen.lock().expect("mutex should lock").clone();
    ClaudeRun {
        seen,
        usage: usage_repository,
    }
}

#[test]
fn third_party_claude_request_gets_words_obfuscated_signed_and_reported_at_top_level() {
    run_sensitive_words_test(
        "third_party_claude_request_gets_words_obfuscated_signed_and_reported_at_top_level",
        third_party_claude_request_gets_words_obfuscated_signed_and_reported_at_top_level_impl,
    );
}

async fn third_party_claude_request_gets_words_obfuscated_signed_and_reported_at_top_level_impl() {
    let trace_id = "trace-claude-words-third-party";
    let run = run_claude_request(
        "auto",
        &["proxy", "API"],
        None,
        third_party_headers(),
        third_party_body().to_string().into_bytes(),
        trace_id,
    )
    .await;
    assert!(
        run.seen.body_bytes.is_none(),
        "cloaked requests are rewritten, so they must not travel as the original bytes"
    );
    let body = run
        .seen
        .json_body
        .clone()
        .expect("cloaked request should carry json_body");

    // 系统提示：两个词都被混淆（大小写不敏感），计费头块不被碰。
    let system = body["system"].as_array().expect("system blocks");
    let prompt_block = system
        .iter()
        .find(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains("helpful"))
        })
        .expect("prompt block should survive");
    assert_eq!(
        prompt_block["text"],
        "You are a helpful p\u{200B}roxy for the Anthropic A\u{200B}PI."
    );
    for block in system {
        let text = block["text"].as_str().unwrap_or_default();
        if text.starts_with("x-anthropic-billing-header:") {
            assert!(
                !text.contains(ZWSP),
                "billing header block must not be obfuscated"
            );
        }
    }

    // 对话文本被混淆；tool_use.input、tool_use.name、tool_result 原样。
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "Use the p\u{200B}roxy tool now."
    );
    assert_eq!(body["messages"][1]["content"][0]["name"], "proxy_lookup");
    assert_eq!(
        body["messages"][1]["content"][0]["input"],
        json!({"query": "proxy API"})
    );
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        "proxy API result"
    );
    assert_eq!(body["messages"][2]["content"][1]["text"], "Thanks.");

    // CCH 对混淆后的最终字节校验通过。
    let cch = crate::ai_serving::transport::claude_code::claude_code_cch_from_body(&body)
        .expect("signed cch");
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let resigned =
        crate::ai_serving::transport::claude_code::sign_claude_code_request_bytes(&bytes)
            .expect("resign");
    assert_eq!(resigned.cch.as_deref(), Some(cch.as_str()));
    assert_eq!(
        resigned.bytes, bytes,
        "body must already carry its own signature"
    );

    // 报告落在 usage.request_metadata 顶层（前端徽标与落库白名单只认顶层）。
    let usage = wait_for_completed_usage(run.usage.as_ref(), trace_id).await;
    let report = usage_obfuscation_report(&usage).expect("top-level obfuscation report");
    assert_eq!(report["applied"], true);
    assert_eq!(report["replaced"], 3);
    let fields = report["fields"].as_array().expect("fields");
    assert_eq!(fields.len(), 2, "{fields:?}");
    assert!(fields
        .iter()
        .any(|field| field.as_str().is_some_and(|f| f.starts_with("system["))));
    assert!(fields.iter().any(|field| field == "messages[0].content[0]"));
}

#[test]
fn native_claude_code_request_is_not_obfuscated_even_with_a_word_list() {
    run_sensitive_words_test(
        "native_claude_code_request_is_not_obfuscated_even_with_a_word_list",
        native_claude_code_request_is_not_obfuscated_even_with_a_word_list_impl,
    );
}

async fn native_claude_code_request_is_not_obfuscated_even_with_a_word_list_impl() {
    // 词表故意命中 oracle body 里的词（"Claude"、"native"），原生请求仍须逐字节透传。
    let trace_id = "trace-claude-words-native";
    let run = run_claude_request(
        "auto",
        &["claude", "native"],
        None,
        oracle_headers(),
        ORACLE_BODY.to_vec(),
        trace_id,
    )
    .await;
    let body_bytes = run
        .seen
        .body_bytes
        .as_deref()
        .expect("native request must reach the execution runtime as raw bytes");
    assert_eq!(
        body_bytes, ORACLE_BODY,
        "native body must be byte-identical"
    );
    assert!(run.seen.json_body.is_none());
    for (name, expected) in oracle_headers() {
        if matches!(
            name.as_str(),
            "authorization" | "accept-encoding" | "content-length" | "content-type"
        ) {
            continue;
        }
        assert_eq!(
            run.seen.headers.get(&name).map(String::as_str),
            Some(expected.as_str()),
            "native header {name} must be forwarded verbatim"
        );
    }

    let usage = wait_for_completed_usage(run.usage.as_ref(), trace_id).await;
    assert!(
        usage_obfuscation_report(&usage).is_none(),
        "native passthrough must not report obfuscation"
    );
}

#[test]
fn cloak_mode_off_skips_obfuscation_for_third_party_requests() {
    run_sensitive_words_test(
        "cloak_mode_off_skips_obfuscation_for_third_party_requests",
        cloak_mode_off_skips_obfuscation_for_third_party_requests_impl,
    );
}

async fn cloak_mode_off_skips_obfuscation_for_third_party_requests_impl() {
    let trace_id = "trace-claude-words-off";
    let run = run_claude_request(
        "off",
        &["proxy", "API"],
        None,
        third_party_headers(),
        third_party_body().to_string().into_bytes(),
        trace_id,
    )
    .await;
    let body = run
        .seen
        .json_body
        .clone()
        .or_else(|| {
            run.seen
                .body_bytes
                .as_deref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok())
        })
        .expect("body");
    assert!(
        !contains_zero_width(&body),
        "cloak.mode=off must not obfuscate"
    );
    assert_eq!(
        body["system"][0]["text"],
        "You are a helpful proxy for the Anthropic API."
    );
    let usage = wait_for_completed_usage(run.usage.as_ref(), trace_id).await;
    assert!(usage_obfuscation_report(&usage).is_none());
}

#[test]
fn key_level_word_list_overrides_the_provider_list() {
    run_sensitive_words_test(
        "key_level_word_list_overrides_the_provider_list",
        key_level_word_list_overrides_the_provider_list_impl,
    );
}

async fn key_level_word_list_overrides_the_provider_list_impl() {
    // 供应商词表 ["proxy"]，Key 覆盖为 ["API"]：只有 API 被混淆。
    let trace_id = "trace-claude-words-key-override";
    let run = run_claude_request(
        "auto",
        &["proxy"],
        Some(&["API"]),
        third_party_headers(),
        third_party_body().to_string().into_bytes(),
        trace_id,
    )
    .await;
    let body = run
        .seen
        .json_body
        .clone()
        .expect("cloaked request should carry json_body");
    let system = body["system"].as_array().expect("system blocks");
    let prompt_block = system
        .iter()
        .find(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains("helpful"))
        })
        .expect("prompt block should survive");
    assert_eq!(
        prompt_block["text"],
        "You are a helpful proxy for the Anthropic A\u{200B}PI."
    );
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "Use the proxy tool now."
    );

    let usage = wait_for_completed_usage(run.usage.as_ref(), trace_id).await;
    let report = usage_obfuscation_report(&usage).expect("top-level obfuscation report");
    assert_eq!(report["replaced"], 1);
}

// ---------------------------------------------------------------------------
// Antigravity
// ---------------------------------------------------------------------------

#[test]
fn antigravity_system_instruction_is_obfuscated_and_function_calls_stay_intact() {
    run_sensitive_words_test(
        "antigravity_system_instruction_is_obfuscated_and_function_calls_stay_intact",
        antigravity_system_instruction_is_obfuscated_and_function_calls_stay_intact_impl,
    );
}

async fn antigravity_system_instruction_is_obfuscated_and_function_calls_stay_intact_impl() {
    fn sample_auth_snapshot() -> StoredAuthApiKeySnapshot {
        StoredAuthApiKeySnapshot::new(
            "user-antigravity-words-1".to_string(),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            "user".to_string(),
            "local".to_string(),
            true,
            false,
            Some(json!(["gemini", "antigravity"])),
            Some(json!(["gemini:generate_content"])),
            Some(json!(["gemini-cli"])),
            "api-key-antigravity-words-1".to_string(),
            Some("default".to_string()),
            true,
            false,
            false,
            Some(60),
            Some(5),
            Some(4_102_444_800),
            Some(json!(["gemini", "antigravity"])),
            Some(json!(["gemini:generate_content"])),
            Some(json!(["gemini-cli"])),
        )
        .expect("auth snapshot should build")
    }

    fn sample_candidate_row() -> StoredMinimalCandidateSelectionRow {
        StoredMinimalCandidateSelectionRow {
            provider_id: "provider-antigravity-words-1".to_string(),
            provider_name: "antigravity".to_string(),
            provider_type: "antigravity".to_string(),
            provider_priority: 10,
            provider_is_active: true,
            endpoint_id: "endpoint-antigravity-words-1".to_string(),
            endpoint_api_format: "gemini:generate_content".to_string(),
            endpoint_api_family: Some("gemini".to_string()),
            endpoint_kind: Some("cli".to_string()),
            endpoint_is_active: true,
            key_id: "key-antigravity-words-1".to_string(),
            key_name: "oauth".to_string(),
            key_auth_type: "oauth".to_string(),
            key_is_active: true,
            key_api_formats: Some(vec!["gemini:generate_content".to_string()]),
            key_allowed_models: None,
            key_capabilities: None,
            key_internal_priority: 5,
            key_global_priority_by_format: Some(json!({"gemini:generate_content": 1})),
            model_id: "model-antigravity-words-1".to_string(),
            global_model_id: "global-model-antigravity-words-1".to_string(),
            global_model_name: "gemini-cli".to_string(),
            global_model_mappings: None,
            global_model_supports_streaming: Some(true),
            model_provider_model_name: "claude-sonnet-4-5".to_string(),
            model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
                name: "claude-sonnet-4-5".to_string(),
                priority: 1,
                api_formats: Some(vec!["gemini:generate_content".to_string()]),
                endpoint_ids: None,
                operations: None,
            }]),
            model_supports_streaming: Some(true),
            model_is_active: true,
            model_is_available: true,
            provider_pool_enabled: false,
        }
    }

    fn sample_provider() -> StoredProviderCatalogProvider {
        StoredProviderCatalogProvider::new(
            "provider-antigravity-words-1".to_string(),
            "antigravity".to_string(),
            Some("https://example.com".to_string()),
            "antigravity".to_string(),
        )
        .expect("provider should build")
        .with_transport_fields(
            true,
            false,
            false,
            None,
            None,
            None,
            Some(20.0),
            None,
            Some(json!({"cloak": {"sensitive_words": ["proxy", "API"]}})),
        )
    }

    fn sample_endpoint() -> StoredProviderCatalogEndpoint {
        StoredProviderCatalogEndpoint::new(
            "endpoint-antigravity-words-1".to_string(),
            "provider-antigravity-words-1".to_string(),
            "gemini:generate_content".to_string(),
            Some("gemini".to_string()),
            Some("cli".to_string()),
            true,
        )
        .expect("endpoint should build")
        .with_transport_fields(
            "https://antigravity.googleapis.com".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("endpoint transport should build")
    }

    fn sample_key() -> StoredProviderCatalogKey {
        let encrypted_auth_config = encrypt_python_fernet_plaintext(
            DEVELOPMENT_ENCRYPTION_KEY,
            r#"{"provider_type":"antigravity","project_id":"project-antigravity-words-1","session_id":"sess-antigravity-words-123","refresh_token":"rt-antigravity-words-123"}"#,
        )
        .expect("auth config should encrypt");
        StoredProviderCatalogKey::new(
            "key-antigravity-words-1".to_string(),
            "provider-antigravity-words-1".to_string(),
            "oauth".to_string(),
            "oauth".to_string(),
            None,
            true,
        )
        .expect("key should build")
        .with_transport_fields(
            Some(json!(["gemini:generate_content"])),
            encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "__placeholder__")
                .expect("placeholder api key should encrypt"),
            Some(encrypted_auth_config),
            None,
            Some(json!({"gemini:generate_content": 1})),
            None,
            None,
            None,
            None,
        )
        .expect("key transport should build")
    }

    let trace_id = "trace-antigravity-words-sync";
    let seen = Arc::new(Mutex::new(None::<serde_json::Value>));
    let seen_clone = Arc::clone(&seen);
    let refresh = Router::new().route(
        "/oauth/token",
        any(|_request: Request| async move {
            Json(json!({
                "access_token": "refreshed-antigravity-words-access-token",
                "refresh_token": "rt-antigravity-words-456",
                "token_type": "Bearer",
                "expires_in": 3600
            }))
        }),
    );
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |request: Request| {
            let seen_inner = Arc::clone(&seen_clone);
            async move {
                let (_parts, body) = request.into_parts();
                let raw = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: serde_json::Value =
                    serde_json::from_slice(&raw).expect("execution plan should parse");
                *seen_inner.lock().expect("mutex should lock") =
                    payload.pointer("/body/json_body").cloned();
                Json(json!({
                    "request_id": trace_id,
                    "status_code": 200,
                    "headers": {"content-type": "text/event-stream"},
                    "body": {
                        "body_bytes_b64": base64::engine::general_purpose::STANDARD.encode(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"modelVersion\":\"claude-sonnet-4-5\",\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":3,\"totalTokenCount\":5}},\"responseId\":\"resp_antigravity_words_1\"}\n\n"
                        )
                    },
                    "telemetry": {"elapsed_ms": 27}
                }))
            }
        }),
    );

    let client_api_key = "client-antigravity-words";
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key(client_api_key)),
        sample_auth_snapshot(),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_candidate_row(),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![sample_provider()],
        vec![sample_endpoint()],
        vec![sample_key()],
    ));

    let (refresh_url, refresh_handle) = start_server(refresh).await;
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let oauth_refresh =
        crate::provider_transport::LocalOAuthRefreshCoordinator::with_adapters_for_tests(vec![
            Arc::new(
                crate::provider_transport::oauth_refresh::GenericOAuthRefreshAdapter::default()
                    .with_token_url_for_tests("antigravity", format!("{refresh_url}/oauth/token"))
                    .with_oauth_credentials_for_tests(
                        "antigravity",
                        "test-antigravity-client-id",
                        "test-antigravity-client-secret",
                    ),
            ),
        ]);
    let gateway_state = build_state_with_execution_runtime_override(execution_runtime_url.clone())
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_request_candidates_and_usage_for_tests(
                auth_repository,
                candidate_selection_repository,
                provider_catalog_repository,
                Arc::clone(&request_candidate_repository),
                Arc::clone(&usage_repository),
                DEVELOPMENT_ENCRYPTION_KEY,
            )
            .with_system_default_routing_group_for_tests(),
        )
        .with_oauth_refresh_coordinator_for_tests(oauth_refresh)
        .with_usage_runtime_for_tests(UsageRuntimeConfig {
            enabled: true,
            ..UsageRuntimeConfig::default()
        });
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1beta/models/gemini-cli:generateContent"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("user-agent", "GeminiCLI/1.0")
        .header("x-goog-api-key", client_api_key)
        .header(TRACE_ID_HEADER, trace_id)
        .body(
            json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "call the proxy"}]},
                    {"role": "model", "parts": [{"functionCall": {"name": "proxy_lookup", "args": {"query": "proxy API"}}}]},
                    {"role": "user", "parts": [{"functionResponse": {"name": "proxy_lookup", "response": {"text": "proxy API"}}}]}
                ],
                "systemInstruction": {"role": "user", "parts": [{"text": "You are a proxy for the API."}]},
                "generationConfig": {"temperature": 0.2}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("request should succeed");
    let status = response.status();
    let text = response.text().await.expect("body should read");
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected gateway response: {text}"
    );

    let envelope = seen
        .lock()
        .expect("mutex should lock")
        .clone()
        .expect("execution runtime sync should be captured");
    assert_eq!(envelope["project"], "project-antigravity-words-1");
    assert_eq!(
        envelope["request"]["systemInstruction"]["parts"][0]["text"],
        "You are a p\u{200B}roxy for the A\u{200B}PI."
    );
    // contents（含 functionCall / functionResponse）逐字段原样。
    assert_eq!(
        envelope["request"]["contents"],
        json!([
            {"role": "user", "parts": [{"text": "call the proxy"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "proxy_lookup", "args": {"query": "proxy API"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "proxy_lookup", "response": {"text": "proxy API"}}}]}
        ])
    );

    let usage = wait_for_completed_usage(usage_repository.as_ref(), trace_id).await;
    assert_eq!(
        usage_obfuscation_report(&usage),
        Some(json!({
            "applied": true,
            "replaced": 2,
            "fields": ["request.systemInstruction.parts[0]"]
        }))
    );

    gateway_handle.abort();
    execution_runtime_handle.abort();
    refresh_handle.abort();
}
