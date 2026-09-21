//! Claude Code 冻结 oracle 差分测试（计划 §7）。
//!
//! fixture 见 `tests/fixtures/claude_code/`。两条路径共用同一个 body：
//! - 原生 Claude Code 请求：经网关后 body 逐字节不变（执行计划走 `body_bytes_b64`），
//!   身份头（UA、x-app、anthropic-beta、x-stainless-*）逐字节不变；
//! - 第三方客户端（SDK UA）发同一 body：网关补齐 `metadata.user_id` JSON 形状、有序 beta、
//!   1h `cache_control`、`cch=` 签名，且 CCH 对最终 body 校验通过。

use std::collections::BTreeMap;

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
use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::{
    any, build_router_with_state, build_state_with_execution_runtime_override, start_server,
    to_bytes, Arc, Body, HeaderValue, Mutex, Request, Response, Router, StatusCode,
    TRACE_ID_HEADER,
};

const ORACLE_BODY: &[u8] =
    include_bytes!("../fixtures/claude_code/native_cli_2_1_161_messages.body.json");
const ORACLE_HEADERS: &str =
    include_str!("../fixtures/claude_code/native_cli_2_1_161_messages.headers.json");
const ORACLE_TEST_STACK_BYTES: usize = 16 * 1024 * 1024;

fn run_oracle_test<F, Fut>(test_name: &'static str, make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(ORACLE_TEST_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(make_future());
        })
        .expect("oracle test thread should spawn");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

fn oracle_headers() -> Vec<(String, String)> {
    serde_json::from_str::<Vec<(String, String)>>(ORACLE_HEADERS).expect("oracle headers parse")
}

fn hash_api_key(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sample_auth_snapshot() -> StoredAuthApiKeySnapshot {
    StoredAuthApiKeySnapshot::new(
        "user-claude-oracle-1".to_string(),
        "alice".to_string(),
        Some("alice@example.com".to_string()),
        "user".to_string(),
        "local".to_string(),
        true,
        false,
        Some(serde_json::json!(["claude", "claude_code"])),
        Some(serde_json::json!(["claude:messages"])),
        Some(serde_json::json!(["claude-opus-4-6"])),
        "api-key-claude-oracle-1".to_string(),
        Some("default".to_string()),
        true,
        false,
        false,
        Some(60),
        Some(5),
        Some(4_102_444_800),
        Some(serde_json::json!(["claude", "claude_code"])),
        Some(serde_json::json!(["claude:messages"])),
        Some(serde_json::json!(["claude-opus-4-6"])),
    )
    .expect("auth snapshot should build")
}

fn sample_candidate_row() -> StoredMinimalCandidateSelectionRow {
    StoredMinimalCandidateSelectionRow {
        provider_id: "provider-claude-oracle-1".to_string(),
        provider_name: "claude_code".to_string(),
        provider_type: "claude_code".to_string(),
        provider_priority: 10,
        provider_is_active: true,
        endpoint_id: "endpoint-claude-oracle-1".to_string(),
        endpoint_api_format: "claude:messages".to_string(),
        endpoint_api_family: Some("claude".to_string()),
        endpoint_kind: Some("cli".to_string()),
        endpoint_is_active: true,
        key_id: "key-claude-oracle-1".to_string(),
        key_name: "prod".to_string(),
        key_auth_type: "oauth".to_string(),
        key_is_active: true,
        key_api_formats: Some(vec!["claude:messages".to_string()]),
        key_allowed_models: None,
        key_capabilities: None,
        key_internal_priority: 5,
        key_global_priority_by_format: Some(serde_json::json!({"claude:messages": 1})),
        model_id: "model-claude-oracle-1".to_string(),
        global_model_id: "global-model-claude-oracle-1".to_string(),
        global_model_name: "claude-opus-4-6".to_string(),
        global_model_mappings: None,
        global_model_supports_streaming: Some(true),
        // 模型名映射到同名，保证原生路径 body 不因模型映射而改变。
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

fn sample_provider(cloak_mode: &str) -> StoredProviderCatalogProvider {
    StoredProviderCatalogProvider::new(
        "provider-claude-oracle-1".to_string(),
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
        Some(serde_json::json!({
            "claude_code_advanced": {"cli_only_enabled": false},
            "cloak": {"mode": cloak_mode}
        })),
    )
}

fn sample_endpoint() -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        "endpoint-claude-oracle-1".to_string(),
        "provider-claude-oracle-1".to_string(),
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

fn sample_key() -> StoredProviderCatalogKey {
    StoredProviderCatalogKey::new(
        "key-claude-oracle-1".to_string(),
        "provider-claude-oracle-1".to_string(),
        "prod".to_string(),
        "oauth".to_string(),
        None,
        true,
    )
    .expect("key should build")
    .with_transport_fields(
        Some(serde_json::json!(["claude:messages"])),
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "sk-ant-oat01-upstream-oracle")
            .expect("api key should encrypt"),
        Some(
            encrypt_python_fernet_plaintext(
                DEVELOPMENT_ENCRYPTION_KEY,
                r#"{"provider_type":"claude_code","account_uuid":"6f1c7d8e-1b2c-4d3e-8f90-123456789abc","refresh_token":"sk-ant-ort01-oracle"}"#,
            )
            .expect("auth config should encrypt"),
        ),
        None,
        Some(serde_json::json!({"claude:messages": 1})),
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

async fn run_oracle_request(
    cloak_mode: &str,
    request_headers: Vec<(String, String)>,
    trace_id: &'static str,
) -> SeenPlan {
    let seen = Arc::new(Mutex::new(SeenPlan::default()));
    let seen_clone = Arc::clone(&seen);
    let execution_runtime = Router::new().route(
        "/v1/execute/stream",
        any(move |request: Request| {
            let seen_inner = Arc::clone(&seen_clone);
            async move {
                let (_parts, body) = request.into_parts();
                let raw = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: serde_json::Value =
                    serde_json::from_slice(&raw).expect("execution plan should parse");
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
                let json_body = payload.pointer("/body/json_body").cloned();
                *seen_inner.lock().expect("mutex should lock") = SeenPlan {
                    headers,
                    body_bytes,
                    json_body,
                };
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
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-client-claude-oracle")),
        sample_auth_snapshot(),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_candidate_row(),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![sample_provider(cloak_mode)],
        vec![sample_endpoint()],
        vec![sample_key()],
    ));
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway_state = build_state_with_execution_runtime_override(execution_runtime_url.clone())
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_and_request_candidate_repository_for_tests(
                auth_repository,
                candidate_selection_repository,
                provider_catalog_repository,
                Arc::clone(&request_candidate_repository),
                DEVELOPMENT_ENCRYPTION_KEY,
            ),
        );
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .header(TRACE_ID_HEADER, trace_id);
    for (name, value) in request_headers {
        if name == "authorization" {
            request = request.header(name, "Bearer sk-client-claude-oracle");
        } else if name == "accept-encoding" || name == "content-length" {
            continue;
        } else {
            request = request.header(name, value);
        }
    }
    let response = request
        .body(ORACLE_BODY.to_vec())
        .send()
        .await
        .expect("request should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.text().await.expect("body should read");

    gateway_handle.abort();
    execution_runtime_handle.abort();
    let seen = seen.lock().expect("mutex should lock").clone();
    seen
}

#[test]
fn native_claude_code_oracle_request_passes_through_byte_identical() {
    run_oracle_test(
        "native_claude_code_oracle_request_passes_through_byte_identical",
        native_claude_code_oracle_request_passes_through_byte_identical_impl,
    );
}

async fn native_claude_code_oracle_request_passes_through_byte_identical_impl() {
    let seen = run_oracle_request("auto", oracle_headers(), "trace-claude-oracle-native").await;

    let body_bytes = seen
        .body_bytes
        .as_deref()
        .expect("native request must reach the execution runtime as raw bytes");
    assert_eq!(
        body_bytes, ORACLE_BODY,
        "native Claude Code body must be forwarded byte-for-byte"
    );
    assert!(
        seen.json_body.is_none(),
        "raw bytes must win over a re-serialized json_body"
    );

    for (name, expected) in oracle_headers() {
        if matches!(
            name.as_str(),
            "authorization" | "accept-encoding" | "content-length" | "content-type"
        ) {
            continue;
        }
        assert_eq!(
            seen.headers.get(&name).map(String::as_str),
            Some(expected.as_str()),
            "native header {name} must be forwarded verbatim"
        );
    }
    assert_eq!(
        seen.headers.get("authorization").map(String::as_str),
        Some("Bearer sk-ant-oat01-upstream-oracle"),
        "only the credential is replaced"
    );
}

#[test]
fn third_party_client_sending_oracle_body_gets_cloaked_and_signed() {
    run_oracle_test(
        "third_party_client_sending_oracle_body_gets_cloaked_and_signed",
        third_party_client_sending_oracle_body_gets_cloaked_and_signed_impl,
    );
}

async fn third_party_client_sending_oracle_body_gets_cloaked_and_signed_impl() {
    // 同一 body，但客户端是普通 SDK：没有 x-app、UA 不是 CLI、只带一个自定义 beta。
    let headers = vec![
        ("accept".to_string(), "application/json".to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        (
            "anthropic-beta".to_string(),
            "custom-third-party-beta".to_string(),
        ),
        (
            "authorization".to_string(),
            "Bearer placeholder".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
        (
            "user-agent".to_string(),
            "anthropic-sdk-python/0.40.0".to_string(),
        ),
    ];
    let seen = run_oracle_request("auto", headers, "trace-claude-oracle-third-party").await;

    assert!(
        seen.body_bytes.is_none(),
        "cloaked requests are rewritten, so they must not travel as the original bytes"
    );
    let body = seen
        .json_body
        .expect("cloaked request should carry json_body");

    // 身份：user_id 是原生 JSON 形状，device_id 由 Key 派生（64 位小写十六进制）。
    let user_id: serde_json::Value = serde_json::from_str(
        body["metadata"]["user_id"]
            .as_str()
            .expect("user_id should be a JSON string"),
    )
    .expect("user_id should parse as JSON");
    let device_id = user_id["device_id"].as_str().expect("device_id");
    assert_eq!(device_id.len(), 64);
    assert!(device_id
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(
        user_id["account_uuid"],
        "6f1c7d8e-1b2c-4d3e-8f90-123456789abc"
    );
    assert!(uuid::Uuid::parse_str(user_id["session_id"].as_str().unwrap()).is_ok());

    // 计费头块保留在 system[0]，且 cch 已由签名替换（不再是 oracle 里的占位值）。
    let system0 = body["system"][0]["text"].as_str().expect("system[0]");
    assert!(system0.starts_with("x-anthropic-billing-header: cc_version=2.1.161."));
    assert!(
        !system0.contains("cch=1a2b3;"),
        "placeholder cch must be re-signed"
    );
    let cch = crate::ai_serving::transport::claude_code::claude_code_cch_from_body(&body)
        .expect("signed cch");
    assert_eq!(cch.len(), 5);

    // CCH 对最终字节校验通过：重新序列化再签名得到同一值。
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let resigned =
        crate::ai_serving::transport::claude_code::sign_claude_code_request_bytes(&bytes)
            .expect("resign");
    assert_eq!(resigned.cch.as_deref(), Some(cch.as_str()));
    assert_eq!(
        resigned.bytes, bytes,
        "body must already carry its own signature"
    );

    // cache_control：OAuth 凭据升到 1h；断点总数 ≤ 4。
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
    let breakpoints = body["system"]
        .as_array()
        .unwrap()
        .iter()
        .chain(body["messages"][0]["content"].as_array().unwrap().iter())
        .filter(|block| block.get("cache_control").is_some())
        .count();
    assert!(breakpoints <= 4);

    // 有序 beta：网关组装的固定序列 + 客户端自定义 beta 追加在末尾。
    assert_eq!(
        seen.headers.get("anthropic-beta").map(String::as_str),
        Some("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,prompt-caching-scope-2026-01-05,effort-2025-11-24,context-management-2025-06-27,extended-cache-ttl-2025-04-11,custom-third-party-beta")
    );
    assert_eq!(
        seen.headers.get("user-agent").map(String::as_str),
        Some("claude-cli/2.1.161 (external, cli)")
    );
    assert_eq!(seen.headers.get("x-app").map(String::as_str), Some("cli"));
    assert_eq!(
        seen.headers.get("authorization").map(String::as_str),
        Some("Bearer sk-ant-oat01-upstream-oracle")
    );
}

#[test]
fn cloak_mode_off_leaves_third_party_body_unsigned() {
    run_oracle_test(
        "cloak_mode_off_leaves_third_party_body_unsigned",
        cloak_mode_off_leaves_third_party_body_unsigned_impl,
    );
}

async fn cloak_mode_off_leaves_third_party_body_unsigned_impl() {
    let headers = vec![
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
    ];
    let seen = run_oracle_request("off", headers, "trace-claude-oracle-off").await;
    let body = seen
        .json_body
        .or_else(|| {
            seen.body_bytes
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok())
        })
        .expect("body");
    // 关闭伪装：user_id 原样（不是 JSON 形状）、cch 仍是 oracle 占位值（只做版本同步）。
    assert_eq!(
        body["metadata"]["user_id"],
        serde_json::json!("{\"device_id\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\"account_uuid\":\"6f1c7d8e-1b2c-4d3e-8f90-123456789abc\",\"session_id\":\"9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3\"}")
    );
    assert!(body["system"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cch=1a2b3;"));
}
