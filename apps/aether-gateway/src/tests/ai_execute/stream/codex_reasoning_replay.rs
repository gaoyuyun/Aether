use super::{
    any, build_router_with_state, build_state_with_execution_runtime_override, start_server,
    to_bytes, Arc, Body, HeaderValue, Json, Mutex, Request, Response, Router, StatusCode,
    TRACE_ID_HEADER,
};
use aether_crypto::{encrypt_python_fernet_plaintext, DEVELOPMENT_ENCRYPTION_KEY};
use aether_data::repository::auth::{
    InMemoryAuthApiKeySnapshotRepository, StoredAuthApiKeySnapshot,
};
use aether_data::repository::candidate_selection::InMemoryMinimalCandidateSelectionReadRepository;
use aether_data::repository::candidates::InMemoryRequestCandidateRepository;
use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data_contracts::repository::candidate_selection::{
    StoredMinimalCandidateSelectionRow, StoredProviderModelMapping,
};
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogEndpoint, StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TEST_STACK_BYTES: usize = 16 * 1024 * 1024;
const PROVIDER_ID: &str = "provider-codex-chat-replay-1";
const ENDPOINT_ID: &str = "endpoint-codex-chat-replay-1";
const KEY_ID: &str = "key-codex-chat-replay-1";
const CLIENT_API_KEY: &str = "sk-client-codex-chat-replay";
/// 70 字符的 MCP 工具名：超过 Codex 的 64 字符上限。
const LONG_TOOL_NAME: &str =
    "mcp__filesystem_server__list_directory_entries_recursively_with_sizes";

fn run_on_large_stack<F, Fut>(test_name: &'static str, make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(TEST_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(make_future());
        })
        .expect("test thread should spawn");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

fn hash_api_key(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 形状合法的 Fernet 令牌（0x80 + 8 字节时间戳 + 16 字节 IV + N*16 密文 + 32 字节 HMAC）。
fn fernet_like(seed: u8) -> String {
    use base64::Engine as _;
    let mut bytes = vec![0x80u8];
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, seed]);
    bytes.extend_from_slice(&[seed.wrapping_add(1); 16]);
    bytes.extend_from_slice(&[seed.wrapping_add(2); 32]);
    bytes.extend_from_slice(&[seed.wrapping_add(3); 32]);
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

fn sample_auth_snapshot() -> StoredAuthApiKeySnapshot {
    StoredAuthApiKeySnapshot::new(
        "user-codex-chat-replay-1".to_string(),
        "alice".to_string(),
        Some("alice@example.com".to_string()),
        "user".to_string(),
        "local".to_string(),
        true,
        false,
        Some(json!(["openai", "codex"])),
        Some(json!(["openai:chat"])),
        Some(json!(["gpt-5.6-sol"])),
        "api-key-codex-chat-replay-1".to_string(),
        Some("default".to_string()),
        true,
        false,
        false,
        Some(60),
        Some(5),
        Some(4_102_444_800),
        Some(json!(["openai", "codex"])),
        Some(json!(["openai:chat"])),
        Some(json!(["gpt-5.6-sol"])),
    )
    .expect("auth snapshot should build")
}

fn sample_candidate_row() -> StoredMinimalCandidateSelectionRow {
    StoredMinimalCandidateSelectionRow {
        provider_id: PROVIDER_ID.to_string(),
        provider_name: "codex".to_string(),
        provider_type: "codex".to_string(),
        provider_priority: 10,
        provider_is_active: true,
        endpoint_id: ENDPOINT_ID.to_string(),
        endpoint_api_format: "openai:responses".to_string(),
        endpoint_api_family: Some("openai".to_string()),
        endpoint_kind: Some("cli".to_string()),
        endpoint_is_active: true,
        key_id: KEY_ID.to_string(),
        key_name: "prod".to_string(),
        key_auth_type: "oauth".to_string(),
        key_is_active: true,
        key_api_formats: Some(vec!["openai:responses".to_string()]),
        key_allowed_models: None,
        key_capabilities: None,
        key_internal_priority: 5,
        key_global_priority_by_format: Some(json!({"openai:responses": 1})),
        model_id: "model-codex-chat-replay-1".to_string(),
        global_model_id: "global-model-codex-chat-replay-1".to_string(),
        global_model_name: "gpt-5.6-sol".to_string(),
        global_model_mappings: None,
        global_model_supports_streaming: Some(true),
        model_provider_model_name: "gpt-5.6-sol".to_string(),
        model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
            name: "gpt-5.6-sol".to_string(),
            priority: 1,
            api_formats: Some(vec!["openai:responses".to_string()]),
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
        PROVIDER_ID.to_string(),
        "codex".to_string(),
        Some("https://chatgpt.com".to_string()),
        "codex".to_string(),
    )
    .expect("provider should build")
    .with_transport_fields(true, false, true, None, None, None, Some(20.0), None, None)
}

fn sample_endpoint() -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        ENDPOINT_ID.to_string(),
        PROVIDER_ID.to_string(),
        "openai:responses".to_string(),
        Some("openai".to_string()),
        Some("cli".to_string()),
        true,
    )
    .expect("endpoint should build")
    .with_transport_fields(
        "https://chatgpt.com/backend-api/codex".to_string(),
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
    StoredProviderCatalogKey::new(
        KEY_ID.to_string(),
        PROVIDER_ID.to_string(),
        "prod".to_string(),
        "oauth".to_string(),
        None,
        true,
    )
    .expect("key should build")
    .with_transport_fields(
        Some(json!(["openai:responses"])),
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "sk-upstream-codex-replay")
            .expect("api key should encrypt"),
        None,
        None,
        Some(json!({"openai:responses": 1})),
        None,
        None,
        None,
        None,
    )
    .expect("key transport should build")
}

fn sse_frames(events: &[Value]) -> String {
    let mut text = String::new();
    for event in events {
        let event_type = event["type"].as_str().unwrap_or_default();
        text.push_str(&format!("event: {event_type}\ndata: {event}\n\n"));
    }
    let payload = json!({"kind": "data", "text": text});
    format!(
        "{}\n{}\n{}\n{}\n",
        json!({"type": "headers", "payload": {"kind": "headers", "status_code": 200, "headers": {"content-type": "text/event-stream"}}}),
        json!({"type": "data", "payload": payload}),
        json!({"type": "telemetry", "payload": {"kind": "telemetry", "telemetry": {"elapsed_ms": 31, "ttfb_ms": 11, "upstream_bytes": 197}}}),
        json!({"type": "eof", "payload": {"kind": "eof"}}),
    )
}

fn completed_with_output(response_id: &str, output: Vec<Value>) -> Vec<Value> {
    let mut events = vec![json!({
        "type": "response.created",
        "response": {"id": response_id, "object": "response", "model": "gpt-5.6-sol", "status": "in_progress", "output": []}
    })];
    for (index, item) in output.iter().enumerate() {
        events.push(
            json!({"type": "response.output_item.added", "output_index": index, "item": item}),
        );
        events.push(
            json!({"type": "response.output_item.done", "output_index": index, "item": item}),
        );
    }
    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "object": "response",
            "model": "gpt-5.6-sol",
            "status": "completed",
            "output": output,
            "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
        }
    }));
    events
}

fn tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": LONG_TOOL_NAME,
            "description": "List directory entries.",
            "strict": true,
            "parameters": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "path": {"type": "string", "pattern": "^\\p{L}.*$"},
                    "mode": {"oneOf": [{"const": "fast"}, {"const": "deep"}]}
                },
                "required": ["path", "mode"],
                "additionalProperties": false
            }
        }
    })
}

#[test]
fn openai_chat_entry_replays_codex_reasoning_across_three_tool_use_turns() {
    run_on_large_stack(
        "openai_chat_entry_replays_codex_reasoning_across_three_tool_use_turns",
        openai_chat_entry_replays_codex_reasoning_across_three_tool_use_turns_impl,
    );
}

async fn openai_chat_entry_replays_codex_reasoning_across_three_tool_use_turns_impl() {
    crate::orchestration::clear_reasoning_replay_ledger_for_tests();
    let seen_bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen_bodies_clone = Arc::clone(&seen_bodies);
    let signature_turn_1 = fernet_like(1);
    let signature_turn_2 = fernet_like(2);
    let shortened_name = aether_ai_formats::shorten_codex_tool_name(LONG_TOOL_NAME);
    // `mcp__server__leaf` 形状先缩成 `mcp__leaf`，再超长才走哈希。
    assert!(shortened_name.chars().count() <= 64);
    assert_ne!(shortened_name, LONG_TOOL_NAME);
    let signature_turn_1_for_runtime = signature_turn_1.clone();
    let signature_turn_2_for_runtime = signature_turn_2.clone();
    let shortened_name_for_runtime = shortened_name.clone();

    let execution_runtime = Router::new().route(
        "/v1/execute/stream",
        any(move |request: Request| {
            let seen_bodies_inner = Arc::clone(&seen_bodies_clone);
            let signature_turn_1 = signature_turn_1_for_runtime.clone();
            let signature_turn_2 = signature_turn_2_for_runtime.clone();
            let shortened_name = shortened_name_for_runtime.clone();
            async move {
                let (_parts, body) = request.into_parts();
                let raw_body = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: Value =
                    serde_json::from_slice(&raw_body).expect("execution runtime payload should parse");
                let provider_body = payload["body"]["json_body"].clone();
                let turn = {
                    let mut seen = seen_bodies_inner.lock().expect("mutex should lock");
                    seen.push(provider_body.clone());
                    seen.len()
                };
                let events = match turn {
                    1 => completed_with_output(
                        "resp_turn_1",
                        vec![
                            json!({"type": "reasoning", "id": "rs_turn_1", "summary": [], "encrypted_content": signature_turn_1}),
                            json!({"type": "function_call", "id": "fc_turn_1", "call_id": "call_turn_1", "name": shortened_name, "arguments": "{\"path\":\"/tmp\",\"mode\":\"fast\"}", "status": "completed"}),
                        ],
                    ),
                    2 => completed_with_output(
                        "resp_turn_2",
                        vec![
                            json!({"type": "reasoning", "id": "rs_turn_2", "summary": [], "encrypted_content": signature_turn_2}),
                            json!({"type": "function_call", "id": "fc_turn_2", "call_id": "call_turn_2", "name": shortened_name, "arguments": "{\"path\":\"/var\",\"mode\":\"deep\"}", "status": "completed"}),
                        ],
                    ),
                    _ => completed_with_output(
                        "resp_turn_3",
                        vec![json!({"type": "message", "id": "msg_turn_3", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "All done", "annotations": []}]})],
                    ),
                };
                let mut response = Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(sse_frames(&events)))
                    .expect("response should build");
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-ndjson"),
                );
                response
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key(CLIENT_API_KEY)),
        sample_auth_snapshot(),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_candidate_row(),
        ]));
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![sample_provider()],
        vec![sample_endpoint()],
        vec![sample_key()],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());

    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway_state = build_state_with_execution_runtime_override(execution_runtime_url)
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_and_request_candidate_repository_for_tests(
                auth_repository,
                candidate_selection_repository,
                provider_catalog_repository,
                request_candidate_repository,
                DEVELOPMENT_ENCRYPTION_KEY,
            )
            .with_system_config_values_for_tests(vec![
                ("scheduling_mode".to_string(), json!("fixed_order")),
                ("provider_priority_mode".to_string(), json!("global_key")),
            ]),
        );
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();

    // ---- 第一轮：用户提问，模型发起工具调用 ----
    let mut messages = vec![json!({"role": "user", "content": "List /tmp"})];
    let response = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {CLIENT_API_KEY}"))
        .header("x-aether-session-id", "session-codex-replay")
        .header(TRACE_ID_HEADER, "trace-codex-replay-1")
        .json(&json!({"model": "gpt-5.6-sol", "messages": messages, "tools": [tool_definition()], "stream": true}))
        .send()
        .await
        .expect("turn 1 should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let turn_1_text = response.text().await.expect("turn 1 body");
    assert!(
        turn_1_text.contains(&format!("\"name\":\"{LONG_TOOL_NAME}\"")),
        "the client must see the original 70-char tool name, got: {turn_1_text}"
    );
    assert!(
        !turn_1_text.contains(&shortened_name_marker()),
        "shortened name leaked"
    );
    assert!(turn_1_text.contains("\"finish_reason\":\"tool_calls\""));

    // ---- 第二轮：客户端回传工具结果（Chat 形状，没有推理项） ----
    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{"id": "call_turn_1", "type": "function", "function": {"name": LONG_TOOL_NAME, "arguments": "{\"path\":\"/tmp\",\"mode\":\"fast\"}"}}]
    }));
    messages
        .push(json!({"role": "tool", "tool_call_id": "call_turn_1", "content": "a.txt\nb.txt"}));
    let response = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {CLIENT_API_KEY}"))
        .header("x-aether-session-id", "session-codex-replay")
        .header(TRACE_ID_HEADER, "trace-codex-replay-2")
        .json(&json!({"model": "gpt-5.6-sol", "messages": messages, "tools": [tool_definition()], "stream": true}))
        .send()
        .await
        .expect("turn 2 should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let turn_2_text = response.text().await.expect("turn 2 body");
    assert!(turn_2_text.contains(&format!("\"name\":\"{LONG_TOOL_NAME}\"")));

    // ---- 第三轮：再回传一次工具结果，模型给出最终答复 ----
    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{"id": "call_turn_2", "type": "function", "function": {"name": LONG_TOOL_NAME, "arguments": "{\"path\":\"/var\",\"mode\":\"deep\"}"}}]
    }));
    messages.push(json!({"role": "tool", "tool_call_id": "call_turn_2", "content": "log/"}));
    let response = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {CLIENT_API_KEY}"))
        .header("x-aether-session-id", "session-codex-replay")
        .header(TRACE_ID_HEADER, "trace-codex-replay-3")
        .json(&json!({"model": "gpt-5.6-sol", "messages": messages, "tools": [tool_definition()], "stream": true}))
        .send()
        .await
        .expect("turn 3 should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let turn_3_text = response.text().await.expect("turn 3 body");
    assert!(turn_3_text.contains("\"content\":\"All done\""));

    let seen = seen_bodies.lock().expect("mutex should lock").clone();
    assert_eq!(seen.len(), 3, "three provider turns expected");

    // 工具清洗：$schema 删除、\p{} 正则剥离、oneOf const 折叠、strict 保留（清洗后满足）、名字缩短。
    // gpt-5.6-sol 走 Responses Lite：工具定义被搬进 input[] 的 additional_tools 项。
    for (turn, body) in seen.iter().enumerate() {
        let tool = provider_function_tool(body)
            .unwrap_or_else(|| panic!("turn {turn} function tool: {body}"));
        assert_eq!(tool["name"], shortened_name, "turn {turn}");
        assert!(tool["parameters"].get("$schema").is_none(), "turn {turn}");
        assert!(
            tool["parameters"]["properties"]["path"]
                .get("pattern")
                .is_none(),
            "turn {turn}"
        );
        assert_eq!(
            tool["parameters"]["properties"]["mode"]["enum"],
            json!(["fast", "deep"]),
            "turn {turn}"
        );
        assert!(
            tool["parameters"]["properties"]["mode"]
                .get("oneOf")
                .is_none(),
            "turn {turn}"
        );
        assert_eq!(tool["strict"], true, "turn {turn}");
    }

    // 第一轮没有历史，也没有回放。
    let turn_1_input = seen[0]["input"].as_array().expect("turn 1 input");
    assert!(turn_1_input.iter().all(|item| item["type"] != "reasoning"));

    // 第二轮：第一轮的推理项被插回到 call_turn_1 之前，工具调用名字是缩短后的。
    let turn_2_input = seen[1]["input"].as_array().expect("turn 2 input");
    let reasoning_positions = turn_2_input
        .iter()
        .enumerate()
        .filter(|(_, item)| item["type"] == "reasoning")
        .map(|(index, item)| (index, item.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning_positions.len(),
        1,
        "turn 2 input: {turn_2_input:?}"
    );
    let (reasoning_index, reasoning_item) = &reasoning_positions[0];
    assert_eq!(reasoning_item["encrypted_content"], signature_turn_1);
    assert_eq!(reasoning_item["id"], "rs_turn_1");
    let following = &turn_2_input[reasoning_index + 1];
    assert_eq!(following["type"], "function_call");
    assert_eq!(following["call_id"], "call_turn_1");
    assert_eq!(following["name"], shortened_name);
    assert_eq!(
        following["id"], "fc_turn_1",
        "native item id should be restored"
    );

    // 第三轮：两轮推理项都在，各自锚定到自己的工具调用。
    let turn_3_input = seen[2]["input"].as_array().expect("turn 3 input");
    let mut anchors = Vec::new();
    for window in turn_3_input.windows(2) {
        if window[0]["type"] == "reasoning" {
            assert_eq!(window[1]["type"], "function_call");
            anchors.push((
                window[0]["encrypted_content"]
                    .as_str()
                    .expect("signature")
                    .to_string(),
                window[1]["call_id"].as_str().expect("call id").to_string(),
            ));
        }
    }
    assert_eq!(
        anchors,
        vec![
            (signature_turn_1.clone(), "call_turn_1".to_string()),
            (signature_turn_2.clone(), "call_turn_2".to_string()),
        ]
    );
    // input item id 归一化：message 项带 msg 前缀或没有 id，function_call 项带 fc 前缀；
    // reasoning 项 id 原样保留（rs_turn_1 / rs_turn_2 来自上游）。
    for item in turn_3_input {
        if let Some(id) = item["id"].as_str() {
            let prefix = match item["type"].as_str() {
                Some("message") => "msg",
                Some("reasoning") => "rs",
                Some("function_call") => "fc",
                _ => continue,
            };
            assert!(
                id.starts_with(prefix),
                "item id {id} should start with {prefix}"
            );
            assert!(id.chars().count() <= 64);
        }
    }

    gateway_handle.abort();
    execution_runtime_handle.abort();
}

/// 在顶层 `tools` 或 Responses Lite 的 `additional_tools` 项里找到第一个 function 工具。
fn provider_function_tool(body: &Value) -> Option<Value> {
    let is_function = |tool: &Value| tool["type"] == "function";
    if let Some(tool) = body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| is_function(tool)))
    {
        return Some(tool.clone());
    }
    body["input"]
        .as_array()?
        .iter()
        .filter(|item| item["type"] == "additional_tools")
        .filter_map(|item| item["tools"].as_array())
        .flat_map(|tools| tools.iter())
        .find(|tool| is_function(tool))
        .cloned()
}

/// 缩短后的名字包含 sha256 片段；在客户端可见文本里不应出现。
fn shortened_name_marker() -> String {
    aether_ai_formats::shorten_codex_tool_name(LONG_TOOL_NAME)
}
