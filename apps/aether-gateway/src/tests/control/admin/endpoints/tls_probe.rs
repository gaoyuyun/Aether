use std::sync::{Arc, Mutex};

use aether_contracts::ExecutionPlan;
use aether_crypto::DEVELOPMENT_ENCRYPTION_KEY;
use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data_contracts::repository::provider_catalog::ProviderCatalogReadRepository;
use axum::routing::any;
use axum::{Json, Router};
use http::StatusCode;
use serde_json::json;

use super::super::super::{
    build_router_with_state, build_state_with_execution_runtime_override, sample_endpoint,
    sample_key, sample_provider, start_server,
};
use crate::constants::{
    GATEWAY_HEADER, TRUSTED_ADMIN_SESSION_ID_HEADER, TRUSTED_ADMIN_USER_ID_HEADER,
    TRUSTED_ADMIN_USER_ROLE_HEADER,
};
use crate::data::GatewayDataState;

const SYNTHETIC_PROBE_FIXTURE: &str =
    include_str!("../../../fixtures/claude_code/tls_probe_synthetic.json");

/// P5 探针接口：按 Key 的 TLS 仿真 profile 发探针，回显落到 `upstream_metadata.tls_probe`，
/// 响应形状固定，且之后 Key 载荷带只读 `tls_probe` 摘要。
#[tokio::test]
async fn gateway_tls_probe_records_fingerprint_into_key_metadata() {
    let execution_plans = Arc::new(Mutex::new(Vec::<ExecutionPlan>::new()));
    let execution_plans_clone = Arc::clone(&execution_plans);
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |Json(plan): Json<ExecutionPlan>| {
            let execution_plans_inner = Arc::clone(&execution_plans_clone);
            async move {
                execution_plans_inner
                    .lock()
                    .expect("mutex should lock")
                    .push(plan.clone());
                let fixture: serde_json::Value =
                    serde_json::from_str(SYNTHETIC_PROBE_FIXTURE).expect("fixture json");
                Json(json!({
                    "request_id": plan.request_id,
                    "status_code": 200,
                    "headers": {"content-type": "application/json"},
                    "body": {"json_body": fixture}
                }))
            }
        }),
    );

    let mut provider = sample_provider("provider-claude", "claude", 10);
    provider.provider_type = "claude_code".to_string();
    let endpoint = sample_endpoint(
        "endpoint-claude-messages",
        "provider-claude",
        "claude:messages",
        "https://api.anthropic.com",
    );
    let mut key = sample_key(
        "key-claude-probe",
        "provider-claude",
        "claude:messages",
        "sk-ant-oat01-probe",
    );
    key.auth_type = "oauth".to_string();
    key.fingerprint = Some(json!({
        "device_id": "device-keep-me",
        "transport_profile": "claude_code_node_openssl"
    }));
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![provider],
        vec![endpoint],
        vec![key],
    ));

    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway = build_router_with_state(
        build_state_with_execution_runtime_override(execution_runtime_url)
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_repository_for_tests(
                    provider_catalog_repository.clone(),
                )
                .with_encryption_key_for_tests(DEVELOPMENT_ENCRYPTION_KEY),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!(
            "{gateway_url}/api/admin/endpoints/keys/key-claude-probe/tls-probe"
        ))
        .header(GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("probe request should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["message"], "已完成 TLS 指纹探测");
    assert_eq!(payload["key_id"], "key-claude-probe");
    let probe = &payload["probe"];
    assert_eq!(probe["observed"], true);
    assert_eq!(probe["probe_url"], "https://tls.peet.ws/api/all");
    assert_eq!(probe["emulation_profile"], "claude_code_node_openssl");
    assert_eq!(probe["backend"], "browser_wreq");
    assert_eq!(probe["tls_stack"], "boringssl_wreq");
    assert_eq!(probe["http_version"], "h1");
    assert_eq!(probe["ja3_hash"], "3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5");
    assert_eq!(probe["ja4"], "t13d1716h1_5b57614c22b0_3d5db4fb5c1e");
    assert!(probe["probed_at_unix_secs"].as_u64().is_some());

    // 探针计划：GET 固定地址、走该 Key 的仿真 profile、带原生 CLI UA、不跟随重定向。
    let plans = execution_plans.lock().expect("mutex should lock");
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.method, "GET");
    assert_eq!(plan.url, "https://tls.peet.ws/api/all");
    assert_eq!(plan.key_id, "key-claude-probe");
    assert_eq!(plan.request_id, "admin-tls-probe:key-claude-probe");
    let transport_profile = plan
        .transport_profile
        .as_ref()
        .expect("probe should carry the key transport profile");
    assert_eq!(transport_profile.profile_id, "claude_code_node_openssl");
    assert_eq!(transport_profile.backend, "browser_wreq");
    assert_eq!(transport_profile.http_mode, "http1_only");
    assert_eq!(
        plan.headers.get("user-agent").map(String::as_str),
        Some("claude-cli/2.1.161 (external, cli)")
    );
    drop(plans);

    // 结果落到 upstream_metadata.tls_probe，Key 载荷带只读摘要，fingerprint 里的 device_id 不受影响。
    let reloaded = provider_catalog_repository
        .list_keys_by_ids(&["key-claude-probe".to_string()])
        .await
        .expect("keys should read");
    assert_eq!(reloaded.len(), 1);
    let stored_probe = reloaded[0]
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("tls_probe"))
        .expect("tls_probe metadata should be persisted");
    assert_eq!(stored_probe["ja4"], "t13d1716h1_5b57614c22b0_3d5db4fb5c1e");
    assert_eq!(
        stored_probe["emulation_profile"],
        "claude_code_node_openssl"
    );
    assert_eq!(
        reloaded[0]
            .fingerprint
            .as_ref()
            .and_then(|value| value.get("device_id")),
        Some(&json!("device-keep-me"))
    );

    let list_response = client
        .get(format!(
            "{gateway_url}/api/admin/endpoints/providers/provider-claude/keys"
        ))
        .header(GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("list request should complete");
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_payload: serde_json::Value = list_response
        .json()
        .await
        .expect("list body should be JSON");
    let listed_key = list_payload
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == "key-claude-probe"))
        .expect("probed key should be listed");
    assert_eq!(listed_key["transport_profile"], "claude_code_node_openssl");
    assert_eq!(listed_key["tls_probe"]["observed"], true);
    assert_eq!(
        listed_key["tls_probe"]["ja3_hash"],
        "3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5"
    );
    // 探针不会把头值（UA）落进摘要。
    assert!(!listed_key["tls_probe"]
        .to_string()
        .contains("claude-cli/2.1.161"));

    gateway_handle.abort();
    execution_runtime_handle.abort();
}

/// 回显服务不可用 / 返回非指纹 JSON 时返回 502，不写元数据。
#[tokio::test]
async fn gateway_tls_probe_reports_bad_gateway_when_echo_service_fails() {
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |Json(plan): Json<ExecutionPlan>| async move {
            Json(json!({
                "request_id": plan.request_id,
                "status_code": 503,
                "headers": {"content-type": "text/plain"},
                "body": {"json_body": {"error": "unavailable"}}
            }))
        }),
    );
    let mut provider = sample_provider("provider-codex", "codex", 10);
    provider.provider_type = "codex".to_string();
    provider.config = Some(json!({"fingerprint": {"transport_profile": "chatgpt_com_chrome"}}));
    let endpoint = sample_endpoint(
        "endpoint-codex",
        "provider-codex",
        "openai:responses",
        "https://chatgpt.com/backend-api/codex",
    );
    let key = sample_key(
        "key-codex-probe",
        "provider-codex",
        "openai:responses",
        "codex-access-token",
    );
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![provider],
        vec![endpoint],
        vec![key],
    ));
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway = build_router_with_state(
        build_state_with_execution_runtime_override(execution_runtime_url)
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_repository_for_tests(
                    provider_catalog_repository.clone(),
                )
                .with_encryption_key_for_tests(DEVELOPMENT_ENCRYPTION_KEY),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!(
            "{gateway_url}/api/admin/endpoints/keys/key-codex-probe/tls-probe"
        ))
        .header(GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("probe request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert!(payload["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("503")));

    let reloaded = provider_catalog_repository
        .list_keys_by_ids(&["key-codex-probe".to_string()])
        .await
        .expect("keys should read");
    assert!(reloaded[0]
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("tls_probe"))
        .is_none());

    gateway_handle.abort();
    execution_runtime_handle.abort();
}
