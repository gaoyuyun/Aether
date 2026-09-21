use super::{
    any, build_router_with_state, build_state_with_execution_runtime_override,
    encrypt_python_fernet_plaintext, json, run_async_test_on_large_stack, start_server, to_bytes,
    Arc, Body, Digest, InMemoryAuthApiKeySnapshotRepository,
    InMemoryMinimalCandidateSelectionReadRepository, InMemoryProviderCatalogReadRepository,
    InMemoryRequestCandidateRepository, Json, Mutex, Request, RequestCandidateReadRepository,
    RequestCandidateStatus, Router, Sha256, StatusCode, StoredAuthApiKeySnapshot,
    StoredMinimalCandidateSelectionRow, StoredProviderCatalogEndpoint, StoredProviderCatalogKey,
    StoredProviderCatalogProvider, StoredProviderModelMapping, DEVELOPMENT_ENCRYPTION_KEY,
    EXECUTION_PATH_EXECUTION_RUNTIME_SYNC, EXECUTION_PATH_HEADER, TRACE_ID_HEADER,
};

// P1 验收：上游 429 带很短的 `Retry-After` 时，网关不冷却这把 Key，而是在同一把 Key 上
// 立即重试一次（即使同 Key 重试预算为 0）。

large_stack_async_test!(
    gateway_retries_same_key_once_when_upstream_retry_after_is_short,
    gateway_retries_same_key_once_when_upstream_retry_after_is_short_impl
);

async fn gateway_retries_same_key_once_when_upstream_retry_after_is_short_impl() {
    #[derive(Debug, Clone)]
    struct SeenExecutionRuntimeSyncRequest {
        trace_id: String,
        url: String,
        model: String,
        authorization: String,
    }

    fn hash_api_key(value: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(value.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn sample_auth_snapshot(api_key_id: &str, user_id: &str) -> StoredAuthApiKeySnapshot {
        StoredAuthApiKeySnapshot::new(
            user_id.to_string(),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            "user".to_string(),
            "local".to_string(),
            true,
            false,
            Some(serde_json::json!(["openai"])),
            Some(serde_json::json!(["openai:chat"])),
            Some(serde_json::json!(["gpt-5"])),
            api_key_id.to_string(),
            Some("default".to_string()),
            true,
            false,
            false,
            Some(60),
            Some(5),
            Some(4_102_444_800),
            Some(serde_json::json!(["openai"])),
            Some(serde_json::json!(["openai:chat"])),
            Some(serde_json::json!(["gpt-5"])),
        )
        .expect("auth snapshot should build")
    }

    fn sample_candidate_row(
        provider_id: &str,
        endpoint_id: &str,
        key_id: &str,
        provider_priority: i32,
        global_priority: i32,
        mapped_model: &str,
    ) -> StoredMinimalCandidateSelectionRow {
        StoredMinimalCandidateSelectionRow {
            provider_id: provider_id.to_string(),
            provider_name: "openai".to_string(),
            provider_type: "custom".to_string(),
            provider_priority,
            provider_is_active: true,
            endpoint_id: endpoint_id.to_string(),
            endpoint_api_format: "openai:chat".to_string(),
            endpoint_api_family: Some("openai".to_string()),
            endpoint_kind: Some("chat".to_string()),
            endpoint_is_active: true,
            key_id: key_id.to_string(),
            key_name: "prod".to_string(),
            key_auth_type: "api_key".to_string(),
            key_is_active: true,
            key_api_formats: Some(vec!["openai:chat".to_string()]),
            key_allowed_models: None,
            key_capabilities: None,
            key_internal_priority: 5,
            key_global_priority_by_format: Some(
                serde_json::json!({"openai:chat": global_priority}),
            ),
            model_id: format!("model-{provider_id}"),
            global_model_id: "global-model-openai-sync-failover".to_string(),
            global_model_name: "gpt-5".to_string(),
            global_model_mappings: None,
            global_model_supports_streaming: Some(true),
            model_provider_model_name: mapped_model.to_string(),
            model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
                name: mapped_model.to_string(),
                priority: 1,
                api_formats: Some(vec!["openai:chat".to_string()]),
                endpoint_ids: None,
                operations: None,
            }]),
            model_supports_streaming: Some(true),
            model_is_active: true,
            model_is_available: true,
            provider_pool_enabled: false,
        }
    }

    fn sample_provider_catalog_provider(
        provider_id: &str,
        provider_name: &str,
        same_key_retries: Option<i32>,
    ) -> StoredProviderCatalogProvider {
        StoredProviderCatalogProvider::new(
            provider_id.to_string(),
            provider_name.to_string(),
            Some("https://example.com".to_string()),
            "custom".to_string(),
        )
        .expect("provider should build")
        .with_transport_fields(
            true,
            false,
            false,
            None,
            same_key_retries,
            None,
            Some(20.0),
            None,
            None,
        )
    }

    fn sample_provider_catalog_endpoint(
        endpoint_id: &str,
        provider_id: &str,
        base_url: &str,
    ) -> StoredProviderCatalogEndpoint {
        StoredProviderCatalogEndpoint::new(
            endpoint_id.to_string(),
            provider_id.to_string(),
            "openai:chat".to_string(),
            Some("openai".to_string()),
            Some("chat".to_string()),
            true,
        )
        .expect("endpoint should build")
        .with_transport_fields(
            base_url.to_string(),
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

    fn sample_provider_catalog_key(
        key_id: &str,
        provider_id: &str,
        secret: &str,
        global_priority: i32,
    ) -> StoredProviderCatalogKey {
        StoredProviderCatalogKey::new(
            key_id.to_string(),
            provider_id.to_string(),
            "prod".to_string(),
            "api_key".to_string(),
            None,
            true,
        )
        .expect("key should build")
        .with_transport_fields(
            Some(serde_json::json!(["openai:chat"])),
            encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, secret)
                .expect("api key should encrypt"),
            None,
            None,
            Some(serde_json::json!({"openai:chat": global_priority})),
            None,
            None,
            None,
            None,
        )
        .expect("key transport should build")
    }

    let seen_execution_runtime =
        Arc::new(Mutex::new(Vec::<SeenExecutionRuntimeSyncRequest>::new()));
    let seen_execution_runtime_clone = Arc::clone(&seen_execution_runtime);
    let seen_report = Arc::new(Mutex::new(false));
    let seen_report_clone = Arc::clone(&seen_report);
    let execution_runtime_hits = Arc::new(Mutex::new(0usize));
    let execution_runtime_hits_clone = Arc::clone(&execution_runtime_hits);
    let decision_hits = Arc::new(Mutex::new(0usize));
    let decision_hits_clone = Arc::clone(&decision_hits);
    let plan_hits = Arc::new(Mutex::new(0usize));
    let plan_hits_clone = Arc::clone(&plan_hits);
    let public_hits = Arc::new(Mutex::new(0usize));
    let public_hits_clone = Arc::clone(&public_hits);

    let upstream = Router::new()
        .route(
            "/api/internal/gateway/decision-sync",
            any(move |_request: Request| {
                let decision_hits_inner = Arc::clone(&decision_hits_clone);
                async move {
                    *decision_hits_inner.lock().expect("mutex should lock") += 1;
                    Json(json!({"action": "proxy_public"}))
                }
            }),
        )
        .route(
            "/api/internal/gateway/plan-sync",
            any(move |_request: Request| {
                let plan_hits_inner = Arc::clone(&plan_hits_clone);
                async move {
                    *plan_hits_inner.lock().expect("mutex should lock") += 1;
                    Json(json!({"action": "proxy_public"}))
                }
            }),
        )
        .route(
            "/api/internal/gateway/report-sync",
            any(move |_request: Request| {
                let seen_report_inner = Arc::clone(&seen_report_clone);
                async move {
                    *seen_report_inner.lock().expect("mutex should lock") = true;
                    Json(json!({"ok": true}))
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            any(move |_request: Request| {
                let public_hits_inner = Arc::clone(&public_hits_clone);
                async move {
                    *public_hits_inner.lock().expect("mutex should lock") += 1;
                    (StatusCode::IM_A_TEAPOT, Body::from("public-route-hit"))
                }
            }),
        );

    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |request: Request| {
            let seen_execution_runtime_inner = Arc::clone(&seen_execution_runtime_clone);
            let execution_runtime_hits_inner = Arc::clone(&execution_runtime_hits_clone);
            async move {
                let (parts, body) = request.into_parts();
                let raw_body = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: serde_json::Value = serde_json::from_slice(&raw_body)
                    .expect("execution runtime payload should parse");
                let mut hits = execution_runtime_hits_inner
                    .lock()
                    .expect("mutex should lock");
                *hits += 1;
                let attempt = *hits;
                drop(hits);

                seen_execution_runtime_inner
                    .lock()
                    .expect("mutex should lock")
                    .push(SeenExecutionRuntimeSyncRequest {
                        trace_id: parts
                            .headers
                            .get(TRACE_ID_HEADER)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        url: payload
                            .get("url")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        model: payload
                            .get("body")
                            .and_then(|value| value.get("json_body"))
                            .and_then(|value| value.get("model"))
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        authorization: payload
                            .get("headers")
                            .and_then(|value| value.get("authorization"))
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    });

                // Attempt 1: the primary answers 429 with `Retry-After: 2`.
                // Neither provider configures a same-key retry budget, so
                // without the upstream hint the gateway would fail over to the
                // backup immediately. The hint grants one same-key retry
                // instead, and attempt 2 (still the primary) succeeds.
                if attempt == 1 {
                    return Json(json!({
                        "request_id": "trace-openai-chat-retry-hint-123",
                        "status_code": 429,
                        "headers": {
                            "content-type": "application/json",
                            "retry-after": "2"
                        },
                        "body": {
                            "json_body": {
                                "error": {
                                    "type": "rate_limit_error",
                                    "message": "slow down"
                                }
                            }
                        },
                        "telemetry": {
                            "elapsed_ms": 9
                        }
                    }));
                }

                Json(json!({
                    "request_id": "trace-openai-chat-retry-hint-123",
                    "status_code": 200,
                    "headers": {
                        "content-type": "application/json"
                    },
                    "body": {
                        "json_body": {
                            "id": "chatcmpl-retry-hint-123",
                            "object": "chat.completion",
                            "model": "gpt-5-upstream-primary",
                            "choices": [{
                                "message": {"role": "assistant", "content": "done"}
                            }],
                            "usage": {
                                "prompt_tokens": 2,
                                "completion_tokens": 4,
                                "total_tokens": 6
                            }
                        }
                    },
                    "telemetry": {
                        "elapsed_ms": 19
                    }
                }))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-client-openai-retry-hint")),
        sample_auth_snapshot("api-key-openai-retry-hint-1", "user-openai-retry-hint-1"),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_candidate_row(
                "provider-openai-local-primary",
                "endpoint-openai-local-primary",
                "key-openai-local-primary",
                10,
                1,
                "gpt-5-upstream-primary",
            ),
            sample_candidate_row(
                "provider-openai-local-backup",
                "endpoint-openai-local-backup",
                "key-openai-local-backup",
                20,
                2,
                "gpt-5-upstream-backup",
            ),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            // Neither provider overrides the routing policy default of no
            // same-key retry: only the upstream hint can grant one.
            sample_provider_catalog_provider("provider-openai-local-primary", "openai", None),
            sample_provider_catalog_provider("provider-openai-local-backup", "openai", None),
        ],
        vec![
            sample_provider_catalog_endpoint(
                "endpoint-openai-local-primary",
                "provider-openai-local-primary",
                "https://api.openai.primary.example",
            ),
            sample_provider_catalog_endpoint(
                "endpoint-openai-local-backup",
                "provider-openai-local-backup",
                "https://api.openai.backup.example",
            ),
        ],
        vec![
            sample_provider_catalog_key(
                "key-openai-local-primary",
                "provider-openai-local-primary",
                "sk-upstream-openai-primary",
                1,
            ),
            sample_provider_catalog_key(
                "key-openai-local-backup",
                "provider-openai-local-backup",
                "sk-upstream-openai-backup",
                2,
            ),
        ],
    ));

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway_state =
        build_state_with_execution_runtime_override(execution_runtime_url.clone())
    .with_data_state_for_tests(
        crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_and_request_candidate_repository_for_tests(
            auth_repository,
            candidate_selection_repository,
            provider_catalog_repository,
            Arc::clone(&request_candidate_repository),
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .with_system_config_values_for_tests(vec![(
            "provider_priority_mode".to_string(),
            json!("global_key"),
        )]),
    );
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            "Bearer sk-client-openai-retry-hint",
        )
        .header(TRACE_ID_HEADER, "trace-openai-chat-retry-hint-123")
        .body("{\"model\":\"gpt-5\",\"messages\":[]}")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_EXECUTION_RUNTIME_SYNC)
    );
    let response_json: serde_json::Value = response.json().await.expect("body should parse");
    assert_eq!(response_json["model"], "gpt-5-upstream-primary");

    let seen_execution_runtime_requests = seen_execution_runtime
        .lock()
        .expect("mutex should lock")
        .clone();
    // Both attempts land on the primary key: the short Retry-After keeps the
    // key out of cooldown and buys exactly one same-key retry; the backup is
    // never contacted.
    assert_eq!(seen_execution_runtime_requests.len(), 2);
    for primary_request in &seen_execution_runtime_requests {
        assert_eq!(primary_request.trace_id, "trace-openai-chat-retry-hint-123");
        assert_eq!(
            primary_request.url,
            "https://api.openai.primary.example/chat/completions"
        );
        assert_eq!(primary_request.model, "gpt-5-upstream-primary");
        assert_eq!(
            primary_request.authorization,
            "Bearer sk-upstream-openai-primary"
        );
    }
    let stored_candidates = request_candidate_repository
        .list_by_request_id("trace-openai-chat-retry-hint-123")
        .await
        .expect("request candidate trace should read");
    let mut attempts = stored_candidates
        .iter()
        .map(|candidate| {
            (
                candidate.candidate_index,
                candidate.retry_index,
                candidate.status,
                candidate.status_code,
            )
        })
        .collect::<Vec<_>>();
    attempts.sort_unstable_by_key(|attempt| (attempt.0, attempt.1));
    assert_eq!(
        attempts,
        vec![
            (0, 0, RequestCandidateStatus::Failed, Some(429)),
            (0, 1, RequestCandidateStatus::Success, Some(200)),
        ]
    );

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !*seen_report.lock().expect("mutex should lock"),
        "report-sync should stay local when request candidate persistence is available"
    );

    assert_eq!(
        *execution_runtime_hits.lock().expect("mutex should lock"),
        2
    );
    assert_eq!(*decision_hits.lock().expect("mutex should lock"), 0);
    assert_eq!(*plan_hits.lock().expect("mutex should lock"), 0);
    assert_eq!(*public_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    execution_runtime_handle.abort();
    upstream_handle.abort();
}
