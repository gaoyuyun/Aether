use super::{
    hash_api_key, sample_models_candidate_row, unrestricted_models_snapshot,
    InMemoryAuthApiKeySnapshotRepository, InMemoryMinimalCandidateSelectionReadRepository,
    InMemoryVideoTaskRepository, StoredAuthApiKeySnapshot, UpsertVideoTask, VideoTaskLookupKey,
    VideoTaskReadRepository, VideoTaskStatus, VideoTaskWriteRepository, DEVELOPMENT_ENCRYPTION_KEY,
};
use crate::image_capabilities::openai_image_gateway_max_generation_count;
use crate::tests::{
    any, build_router_with_state, build_state_with_execution_runtime_override, json, start_server,
    to_bytes, wait_until, AppState, Arc, Body, Json, Mutex, Request, Router, StatusCode,
    EXECUTION_PATH_HEADER, EXECUTION_PATH_LOCAL_AI_PUBLIC,
    EXECUTION_PATH_LOCAL_EXECUTION_RUNTIME_MISS,
};
use aether_contracts::{ExecutionResult, ExecutionTelemetry, ResponseBody};
use aether_crypto::encrypt_python_fernet_plaintext;
use aether_data::repository::global_models::InMemoryGlobalModelReadRepository;
use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data::DataLayerError;
use aether_data_contracts::repository::auth::AuthApiKeyWriteRepository;
use aether_data_contracts::repository::candidate_selection::{
    MinimalCandidateSelectionReadRepository, StoredMinimalCandidateSelectionRow,
    StoredPoolKeyCandidateRowsByKeyIdsQuery, StoredPoolKeyCandidateRowsQuery,
    StoredRequestedModelCandidateRowsQuery,
};
use aether_data_contracts::repository::global_models::{
    StoredAdminGlobalModel, StoredPublicGlobalModel, UpdateAdminGlobalModelRecord,
};
use aether_data_contracts::repository::provider_catalog::{
    ProviderCatalogReadRepository, StoredProviderCatalogEndpoint, StoredProviderCatalogKey,
    StoredProviderCatalogProvider,
};
use async_trait::async_trait;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn codex_models_snapshot(
    api_key_id: &str,
    user_id: &str,
    allowed_models: &[&str],
) -> StoredAuthApiKeySnapshot {
    StoredAuthApiKeySnapshot::new(
        user_id.to_string(),
        "alice".to_string(),
        Some("alice@example.com".to_string()),
        "user".to_string(),
        "local".to_string(),
        true,
        false,
        Some(json!(["codex"])),
        Some(json!(["openai:responses"])),
        Some(json!(allowed_models)),
        api_key_id.to_string(),
        Some("codex-models".to_string()),
        true,
        false,
        false,
        Some(10),
        Some(5),
        Some(4_102_444_800),
        Some(json!(["codex"])),
        Some(json!(["openai:responses"])),
        Some(json!(allowed_models)),
    )
    .expect("Codex models auth snapshot should build")
}

fn sample_codex_models_candidate_row(
    provider_id: &str,
    global_model_name: &str,
    source_model_name: &str,
) -> StoredMinimalCandidateSelectionRow {
    let mut row = sample_models_candidate_row(
        provider_id,
        "codex",
        "openai:responses",
        global_model_name,
        10,
    );
    row.provider_type = "codex".to_string();
    row.key_auth_type = "oauth".to_string();
    row.model_provider_model_name = source_model_name.to_string();
    row.model_provider_model_mappings = Some(vec![
        aether_data_contracts::repository::candidate_selection::StoredProviderModelMapping {
            name: source_model_name.to_string(),
            priority: 1,
            api_formats: Some(vec!["openai:responses".to_string()]),
            endpoint_ids: None,
            operations: None,
        },
    ]);
    row
}

fn complete_codex_model_card(source_model_name: &str) -> serde_json::Value {
    json!({
        "id": source_model_name,
        "api_formats": ["openai:responses"],
        "slug": source_model_name,
        "display_name": "GPT-5.6-Sol",
        "description": "Frontier coding model",
        "default_reasoning_level": "low",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Low"},
            {"effort": "medium", "description": "Medium"},
            {"effort": "high", "description": "High"},
            {"effort": "xhigh", "description": "XHigh"},
            {"effort": "max", "description": "Max"},
            {"effort": "ultra", "description": "Ultra"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "Use the current Codex instructions.",
        "model_messages": null,
        "available_in_plans": ["plus", "pro"],
        "support_verbosity": true,
        "default_verbosity": "low",
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "supports_parallel_tool_calls": true,
        "experimental_supported_tools": [],
        "minimal_client_version": "0.144.0",
        "future_capability": {"enabled": true}
    })
}

fn codex_catalog_provider(provider_id: &str) -> StoredProviderCatalogProvider {
    StoredProviderCatalogProvider::new(
        provider_id.to_string(),
        "codex".to_string(),
        Some("https://chatgpt.com".to_string()),
        "codex".to_string(),
    )
    .expect("Codex provider should build")
}

fn codex_catalog_endpoint(provider_id: &str, endpoint_id: &str) -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        endpoint_id.to_string(),
        provider_id.to_string(),
        "openai:responses".to_string(),
        Some("openai".to_string()),
        Some("responses".to_string()),
        true,
    )
    .expect("Codex endpoint should build")
    .with_transport_fields(
        "https://chatgpt.example/backend-api/codex".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("Codex endpoint transport should build")
}

fn codex_catalog_key(
    provider_id: &str,
    key_id: &str,
    allowed_models: &[&str],
) -> StoredProviderCatalogKey {
    let mut key = StoredProviderCatalogKey::new(
        key_id.to_string(),
        provider_id.to_string(),
        "manual".to_string(),
        "bearer".to_string(),
        None,
        true,
    )
    .expect("Codex key should build")
    .with_transport_fields(
        Some(json!(["openai:responses"])),
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "oauth-upstream-secret")
            .expect("Codex test token should encrypt"),
        None,
        None,
        None,
        Some(json!(allowed_models)),
        None,
        None,
        None,
    )
    .expect("Codex key transport should build");
    key.auto_fetch_models = false;
    key.locked_models = Some(json!(["manual-locked-model"]));
    key.model_include_patterns = Some(json!(["gpt-future-*"]));
    key.model_exclude_patterns = Some(json!(["gpt-future-denied"]));
    key
}

fn codex_catalog_execution_result(
    plan: &aether_contracts::ExecutionPlan,
    status_code: u16,
    body: serde_json::Value,
    etag: Option<&str>,
) -> ExecutionResult {
    let mut headers = std::collections::BTreeMap::from([(
        "content-type".to_string(),
        "application/json".to_string(),
    )]);
    if let Some(etag) = etag {
        headers.insert("ETag".to_string(), etag.to_string());
    }
    ExecutionResult {
        request_id: plan.request_id.clone(),
        candidate_id: plan.candidate_id.clone(),
        status_code,
        headers,
        response_observation: None,
        body: Some(ResponseBody {
            json_body: Some(body),
            body_bytes_b64: None,
        }),
        telemetry: Some(ExecutionTelemetry {
            ttfb_ms: Some(1),
            elapsed_ms: Some(2),
            upstream_bytes: None,
        }),
        error: None,
    }
}

fn gemini_operation_status_label(status: VideoTaskStatus) -> &'static str {
    match status {
        VideoTaskStatus::Pending => "Pending",
        VideoTaskStatus::Submitted => "Submitted",
        VideoTaskStatus::Queued => "Queued",
        VideoTaskStatus::Processing => "Processing",
        VideoTaskStatus::Completed => "Completed",
        VideoTaskStatus::Failed => "Failed",
        VideoTaskStatus::Cancelled => "Cancelled",
        VideoTaskStatus::Expired => "Expired",
        VideoTaskStatus::Deleted => "Deleted",
    }
}

fn sample_gemini_video_task(
    id: &str,
    short_id: &str,
    user_id: &str,
    api_key_id: &str,
    external_task_id: &str,
    status: VideoTaskStatus,
) -> UpsertVideoTask {
    let completed = matches!(status, VideoTaskStatus::Completed);
    UpsertVideoTask {
        id: id.to_string(),
        short_id: Some(short_id.to_string()),
        request_id: format!("request-{id}"),
        user_id: Some(user_id.to_string()),
        api_key_id: Some(api_key_id.to_string()),
        username: Some(format!("user-{user_id}")),
        api_key_name: Some("video-key".to_string()),
        external_task_id: Some(external_task_id.to_string()),
        provider_id: Some("provider-gemini-video-local-1".to_string()),
        endpoint_id: Some("endpoint-gemini-video-local-1".to_string()),
        key_id: Some("key-gemini-video-local-1".to_string()),
        client_api_format: Some("gemini:video".to_string()),
        provider_api_format: Some("gemini:video".to_string()),
        format_converted: false,
        model: Some("veo-3".to_string()),
        prompt: Some("gemini video prompt".to_string()),
        original_request_body: Some(json!({"prompt": "gemini video prompt"})),
        duration_seconds: Some(8),
        resolution: Some("720p".to_string()),
        aspect_ratio: Some("16:9".to_string()),
        size: Some("720p".to_string()),
        status,
        progress_percent: if completed { 100 } else { 50 },
        progress_message: None,
        retry_count: 0,
        poll_interval_seconds: 10,
        next_poll_at_unix_secs: (!completed).then_some(124),
        poll_count: 0,
        max_poll_count: 360,
        created_at_unix_ms: 123,
        submitted_at_unix_secs: Some(123),
        completed_at_unix_secs: completed.then_some(124),
        updated_at_unix_secs: 124,
        error_code: None,
        error_message: None,
        video_url: None,
        request_metadata: Some(json!({
            "rust_local_snapshot": {
                "Gemini": {
                    "local_short_id": short_id,
                    "upstream_operation_name": external_task_id,
                    "user_id": user_id,
                    "api_key_id": api_key_id,
                    "model": "veo-3",
                    "status": gemini_operation_status_label(status),
                    "progress_percent": if completed { 100 } else { 50 },
                    "error_code": null,
                    "error_message": null,
                    "metadata": {},
                    "persistence": {
                        "request_id": format!("request-{id}"),
                        "username": format!("user-{user_id}"),
                        "api_key_name": "video-key",
                        "client_api_format": "gemini:video",
                        "provider_api_format": "gemini:video",
                        "original_request_body": {
                            "prompt": "gemini video prompt"
                        },
                        "format_converted": false
                    },
                    "transport": {
                        "upstream_base_url": "https://generativelanguage.googleapis.com",
                        "provider_name": "gemini-video",
                        "provider_id": "provider-gemini-video-local-1",
                        "endpoint_id": "endpoint-gemini-video-local-1",
                        "key_id": "key-gemini-video-local-1",
                        "headers": {
                            "x-goog-api-key": "sk-upstream-gemini-video",
                            "content-type": "application/json"
                        },
                        "content_type": "application/json",
                        "model_name": "veo-3-upstream",
                        "proxy": null,
                        "transport_profile": null,
                        "timeouts": null
                    }
                }
            }
        })),
    }
}

struct PendingMinimalCandidateSelectionReadRepository;

impl PendingMinimalCandidateSelectionReadRepository {
    async fn pending_rows(
        &self,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        pending::<Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError>>().await
    }
}

#[async_trait]
impl MinimalCandidateSelectionReadRepository for PendingMinimalCandidateSelectionReadRepository {
    async fn list_for_exact_api_format(
        &self,
        _api_format: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }

    async fn list_for_exact_api_format_and_global_model(
        &self,
        _api_format: &str,
        _global_model_name: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }

    async fn list_for_exact_api_format_and_requested_model(
        &self,
        _api_format: &str,
        _requested_model_name: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }

    async fn list_for_exact_api_format_and_requested_model_page(
        &self,
        _query: &StoredRequestedModelCandidateRowsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }

    async fn list_pool_key_rows_for_group(
        &self,
        _query: &StoredPoolKeyCandidateRowsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }

    async fn list_pool_key_rows_for_group_key_ids(
        &self,
        _query: &StoredPoolKeyCandidateRowsByKeyIdsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        self.pending_rows().await
    }
}

struct CachedToggleMinimalCandidateSelectionReadRepository {
    row: StoredMinimalCandidateSelectionRow,
    active: AtomicBool,
    cached_rows_by_api_format: Mutex<HashMap<String, Vec<StoredMinimalCandidateSelectionRow>>>,
}

impl CachedToggleMinimalCandidateSelectionReadRepository {
    fn new(row: StoredMinimalCandidateSelectionRow) -> Self {
        Self {
            row,
            active: AtomicBool::new(true),
            cached_rows_by_api_format: Mutex::new(HashMap::new()),
        }
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    fn rows_for_api_format(&self, api_format: &str) -> Vec<StoredMinimalCandidateSelectionRow> {
        let api_format = api_format.trim().to_string();
        let mut cached = self
            .cached_rows_by_api_format
            .lock()
            .expect("candidate row cache lock");
        if let Some(rows) = cached.get(&api_format) {
            return rows.clone();
        }

        let rows = if self.active.load(Ordering::SeqCst)
            && self
                .row
                .endpoint_api_format
                .eq_ignore_ascii_case(&api_format)
        {
            vec![self.row.clone()]
        } else {
            Vec::new()
        };
        cached.insert(api_format, rows.clone());
        rows
    }
}

#[async_trait]
impl MinimalCandidateSelectionReadRepository
    for CachedToggleMinimalCandidateSelectionReadRepository
{
    fn clear_local_cache(&self) {
        self.cached_rows_by_api_format
            .lock()
            .expect("candidate row cache lock")
            .clear();
    }

    async fn list_for_exact_api_format(
        &self,
        api_format: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(self.rows_for_api_format(api_format))
    }

    async fn list_for_exact_api_format_and_global_model(
        &self,
        api_format: &str,
        global_model_name: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(self
            .rows_for_api_format(api_format)
            .into_iter()
            .filter(|row| row.global_model_name == global_model_name)
            .collect())
    }

    async fn list_for_exact_api_format_and_requested_model(
        &self,
        api_format: &str,
        requested_model_name: &str,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(self
            .rows_for_api_format(api_format)
            .into_iter()
            .filter(|row| row.global_model_name == requested_model_name)
            .collect())
    }

    async fn list_for_exact_api_format_and_requested_model_page(
        &self,
        query: &StoredRequestedModelCandidateRowsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(self
            .rows_for_api_format(&query.api_format)
            .into_iter()
            .filter(|row| row.global_model_name == query.requested_model_name)
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect())
    }

    async fn list_pool_key_rows_for_group(
        &self,
        _query: &StoredPoolKeyCandidateRowsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(Vec::new())
    }

    async fn list_pool_key_rows_for_group_key_ids(
        &self,
        _query: &StoredPoolKeyCandidateRowsByKeyIdsQuery,
    ) -> Result<Vec<StoredMinimalCandidateSelectionRow>, DataLayerError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn gateway_handles_public_openai_models_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("proxied"))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-models")),
        unrestricted_models_snapshot("key-1", "user-1"),
    )]));
    let candidate_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_models_candidate_row("provider-openai", "openai", "openai:chat", "gpt-5", 10),
            sample_models_candidate_row("provider-openai", "openai", "openai:chat", "gpt-4.1", 10),
        ]));
    let global_model_repository = Arc::new(InMemoryGlobalModelReadRepository::seed(vec![
        StoredPublicGlobalModel::new(
            "global-gpt-4.1".to_string(),
            "gpt-4.1".to_string(),
            Some("GPT-4.1".to_string()),
            true,
            Some(0.02),
            Some(json!({
                "tiers": [
                    {
                        "up_to": 128000,
                        "input_price_per_1m": 2.0,
                        "output_price_per_1m": 8.0
                    },
                    {
                        "up_to": null,
                        "input_price_per_1m": 4.0,
                        "output_price_per_1m": 12.0
                    }
                ]
            })),
            Some(json!(["vision", "extended_thinking"])),
            Some(json!({
                "context_limit": 1_000_000,
                "output_limit": 32_768,
                "input_modalities": ["text", "image"],
                "extended_thinking": true,
                "reasoning_levels": ["low", "medium", "high"]
            })),
            0,
        )
        .expect("public global model should build"),
        StoredPublicGlobalModel::new(
            "global-gpt-5".to_string(),
            "gpt-5".to_string(),
            None,
            true,
            None,
            None,
            None,
            Some(json!({
                "extended_thinking": false,
                "reasoning_levels": ["low", "medium", "high"]
            })),
            0,
        )
        .expect("non-reasoning public global model should build"),
    ]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                )
                .with_global_model_repository_for_tests(global_model_repository.clone()),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["object"], "list");
    assert_eq!(payload["data"][0]["id"], "gpt-4.1");
    assert_eq!(payload["data"][1]["id"], "gpt-5");
    assert_eq!(payload["data"][0]["owned_by"], "aether");
    assert_eq!(payload["data"][0]["display_name"], "GPT-4.1");
    assert_eq!(payload["data"][0]["context_limit"], 1_000_000);
    assert_eq!(payload["data"][0]["output_limit"], 32_768);
    assert_eq!(
        payload["data"][0]["input_modalities"],
        json!(["text", "image"])
    );
    assert_eq!(
        payload["data"][0]["reasoning_levels"],
        json!(["low", "medium", "high"])
    );
    assert_eq!(payload["data"][0]["pricing"]["price_per_request"], 0.02);
    assert_eq!(
        payload["data"][0]["pricing"]["tiers"][1]["input_price_per_1m"],
        4.0
    );
    assert_eq!(payload["data"][1]["display_name"], "gpt-5");
    assert!(payload["data"][1]["pricing"].is_null());
    assert!(payload["data"][1]["reasoning_levels"].is_null());

    let cached_response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models")
        .send()
        .await
        .expect("cached request should succeed");
    assert_eq!(cached_response.status(), StatusCode::OK);
    assert_eq!(global_model_repository.public_model_read_count(), 1);
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[test]
fn gateway_versioned_models_fail_closed_when_cached_auth_becomes_unusable_or_missing() {
    std::thread::Builder::new()
        .name("codex-model-catalog-auth-race".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Codex auth race test runtime should build")
                .block_on(run_versioned_models_auth_race_scenario());
        })
        .expect("Codex auth race test thread should spawn")
        .join()
        .expect("Codex auth race test thread should finish");
}

async fn run_versioned_models_auth_race_scenario() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-codex-models-auth-race")),
        codex_models_snapshot(
            "key-codex-models-auth-race",
            "user-codex-models-auth-race",
            &["future-alias"],
        ),
    )]));
    let candidate_repository = Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(
        Vec::new(),
    ));
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository.clone(),
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();
    let models_url = format!("{gateway_url}/v1/models?client_version=0.145.2");

    let warm_response = client
        .get(&models_url)
        .header("authorization", "Bearer sk-codex-models-auth-race")
        .send()
        .await
        .expect("initial versioned models request should succeed");
    assert_eq!(warm_response.status(), StatusCode::OK);

    assert!(auth_repository
        .set_user_api_key_locked(
            "user-codex-models-auth-race",
            "key-codex-models-auth-race",
            true,
        )
        .await
        .expect("locking the API key should succeed"));
    let locked_response = client
        .get(&models_url)
        .header("authorization", "Bearer sk-codex-models-auth-race")
        .send()
        .await
        .expect("locked versioned models request should complete");
    assert_eq!(locked_response.status(), StatusCode::UNAUTHORIZED);

    assert!(auth_repository
        .delete_user_api_key("user-codex-models-auth-race", "key-codex-models-auth-race",)
        .await
        .expect("deleting the API key should succeed"));
    let missing_response = client
        .get(&models_url)
        .header("authorization", "Bearer sk-codex-models-auth-race")
        .send()
        .await
        .expect("missing versioned models request should complete");
    assert_eq!(missing_response.status(), StatusCode::UNAUTHORIZED);

    gateway_handle.abort();
}

#[test]
fn gateway_serves_codex_model_cards_for_versioned_models_requests() {
    std::thread::Builder::new()
        .name("codex-model-catalog-frontdoor".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Codex frontdoor test runtime should build")
                .block_on(run_versioned_codex_model_cards_frontdoor_scenario());
        })
        .expect("Codex frontdoor test thread should spawn")
        .join()
        .expect("Codex frontdoor test thread should finish");
}

async fn run_versioned_codex_model_cards_frontdoor_scenario() {
    const PROVIDER_ID: &str = "provider-codex-models";
    const CATALOG_KEY_ID: &str = "key-provider-codex-models";
    const CATALOG_ENDPOINT_ID: &str = "endpoint-provider-codex-models";
    const SOURCE_MODELS: &[&str] = &[
        "gpt-future-dynamic",
        "gpt-future-legacy",
        "gpt-future-second",
        "gpt-hidden-direct",
    ];
    const GLOBAL_MODELS: &[&str] = &["future-alias", "legacy-alias"];

    let mut codex_rows = vec![
        sample_codex_models_candidate_row(PROVIDER_ID, "future-alias", "gpt-future-dynamic"),
        sample_codex_models_candidate_row(PROVIDER_ID, "legacy-alias", "gpt-future-legacy"),
        sample_codex_models_candidate_row(PROVIDER_ID, "second-alias", "gpt-future-second"),
        sample_codex_models_candidate_row(PROVIDER_ID, "hidden-alias", "gpt-hidden-direct"),
    ];
    for row in &mut codex_rows {
        row.key_allowed_models = Some(
            SOURCE_MODELS
                .iter()
                .map(|value| value.to_string())
                .collect(),
        );
    }
    let mut all_candidate_rows = codex_rows.clone();
    all_candidate_rows.push(sample_models_candidate_row(
        "provider-openai-responses",
        "openai",
        "openai:responses",
        "custom-responses-model",
        20,
    ));
    let candidate_repository = Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(
        all_candidate_rows,
    ));
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![
        (
            Some(hash_api_key("sk-codex-models")),
            codex_models_snapshot("key-codex-models", "user-codex-models", GLOBAL_MODELS),
        ),
        (
            Some(hash_api_key("sk-standard-models")),
            unrestricted_models_snapshot("key-standard-models", "user-standard-models"),
        ),
        (
            Some(hash_api_key("sk-codex-legacy-only")),
            codex_models_snapshot(
                "key-codex-legacy-only",
                "user-codex-legacy-only",
                &["legacy-alias"],
            ),
        ),
        (
            Some(hash_api_key("sk-codex-hidden-mixed")),
            codex_models_snapshot(
                "key-codex-hidden-mixed",
                "user-codex-hidden-mixed",
                &["future-alias", "hidden-alias"],
            ),
        ),
        (
            Some(hash_api_key("sk-codex-second-mixed")),
            codex_models_snapshot(
                "key-codex-second-mixed",
                "user-codex-second-mixed",
                &["future-alias", "second-alias"],
            ),
        ),
    ]));
    let original_catalog_key = codex_catalog_key(PROVIDER_ID, CATALOG_KEY_ID, SOURCE_MODELS);
    assert!(!original_catalog_key.auto_fetch_models);
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![codex_catalog_provider(PROVIDER_ID)],
        vec![codex_catalog_endpoint(PROVIDER_ID, CATALOG_ENDPOINT_ID)],
        vec![original_catalog_key.clone()],
    ));
    let rows_before = candidate_repository
        .list_for_exact_api_format("openai:responses")
        .await
        .expect("candidate rows should load before request");

    let catalog_generation = Arc::new(AtomicUsize::new(0));
    let catalog_hits = Arc::new(AtomicUsize::new(0));
    let captured_plans = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let generation_for_runtime = Arc::clone(&catalog_generation);
    let hits_for_runtime = Arc::clone(&catalog_hits);
    let plans_for_runtime = Arc::clone(&captured_plans);
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |request: Request| {
            let generation_for_request = Arc::clone(&generation_for_runtime);
            let hits_for_request = Arc::clone(&hits_for_runtime);
            let plans_for_request = Arc::clone(&plans_for_runtime);
            async move {
                let (_parts, body) = request.into_parts();
                let raw_body = to_bytes(body, usize::MAX)
                    .await
                    .expect("execution runtime request body should read");
                let plan: aether_contracts::ExecutionPlan =
                    serde_json::from_slice(&raw_body).expect("execution runtime plan should parse");
                plans_for_request
                    .lock()
                    .expect("plans mutex")
                    .push((plan.url.clone(), plan.headers.get("user-agent").cloned()));
                if plan.url.contains("/models?") {
                    hits_for_request.fetch_add(1, Ordering::SeqCst);
                    let generation = generation_for_request.load(Ordering::SeqCst);
                    if generation == 1 {
                        return Json(codex_catalog_execution_result(
                            &plan,
                            503,
                            json!({"error":{"message":"temporary catalog outage"}}),
                            None,
                        ));
                    }

                    let mut current = complete_codex_model_card("gpt-future-dynamic");
                    let current_object = current.as_object_mut().expect("current card object");
                    current_object.remove("base_instructions");
                    current_object.insert(
                        "model_messages".to_string(),
                        json!({"instructions_template":"Use future dynamic instructions."}),
                    );
                    current_object.insert(
                        "future_capability".to_string(),
                        json!({"mode":"opaque-current"}),
                    );

                    let mut legacy = complete_codex_model_card("gpt-future-legacy");
                    legacy["base_instructions"] = json!("Use legacy future instructions.");
                    legacy["future_capability"] = json!({"mode":"opaque-legacy"});

                    let mut models = vec![current, legacy];
                    if generation >= 2 {
                        let mut second = complete_codex_model_card("gpt-future-second");
                        second
                            .as_object_mut()
                            .expect("second card object")
                            .remove("base_instructions");
                        second["model_messages"] =
                            json!({"instructions_template":"Use second future instructions."});
                        second["future_capability"] = json!({"mode":"added-without-code-change"});
                        models.push(second);
                        models.push(complete_codex_model_card("gpt-future-unmapped"));
                    }
                    let etag = if generation >= 2 {
                        "\"catalog-etag-v2\""
                    } else {
                        "\"catalog-etag-v1\""
                    };
                    return Json(codex_catalog_execution_result(
                        &plan,
                        200,
                        json!({"models": models, "future_top_level": true}),
                        Some(etag),
                    ));
                }

                Json(codex_catalog_execution_result(
                    &plan,
                    200,
                    json!({
                        "id": "resp-future-dynamic",
                        "object": "response",
                        "model": "gpt-future-dynamic",
                        "output": [{
                            "type": "message",
                            "id": "msg-future-dynamic",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": "future dynamic response"
                            }]
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 2,
                            "total_tokens": 3
                        }
                    }),
                    Some("\"catalog-etag-v2\""),
                ))
            }
        }),
    );
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let state = build_state_with_execution_runtime_override(execution_runtime_url)
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                candidate_repository.clone(),
                auth_repository,
            )
            .attach_provider_catalog_repository_for_tests(provider_catalog_repository.clone())
            .with_encryption_key_for_tests(DEVELOPMENT_ENCRYPTION_KEY),
        );

    let configured_rows = state
        .list_minimal_candidate_selection_rows_for_api_format("openai:responses")
        .await
        .expect("configured Codex candidate rows should be readable");
    assert_eq!(
        configured_rows
            .iter()
            .filter(|row| row.provider_type == "codex")
            .count(),
        codex_rows.len()
    );
    let resolved_auth =
        aether_data_contracts::repository::auth::ResolvedAuthApiKeySnapshot::from_stored(
            codex_models_snapshot("key-codex-models", "user-codex-models", GLOBAL_MODELS),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    let eligible_rows = crate::handlers::public::filter_eligible_model_rows(
        configured_rows.clone(),
        Some(&resolved_auth),
        "openai:responses",
    );
    assert_eq!(
        eligible_rows
            .iter()
            .filter(|row| row.provider_type == "codex")
            .count(),
        GLOBAL_MODELS.len(),
        "Codex fixture rows must survive the same provider/model/key authorization filters as the route"
    );
    let actual_auth = state
        .data
        .read_auth_api_key_snapshot(
            "user-codex-models",
            "key-codex-models",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .await
        .expect("Codex auth snapshot read should succeed")
        .expect("Codex auth snapshot should exist");
    assert_eq!(
        crate::handlers::public::filter_eligible_model_rows(
            configured_rows.clone(),
            Some(&actual_auth),
            "openai:responses",
        )
        .iter()
        .filter(|row| row.provider_type == "codex")
        .count(),
        GLOBAL_MODELS.len(),
        "stored Codex auth snapshot must preserve every authorized manual mapping"
    );
    assert!(
        <AppState as crate::model_fetch::CodexCatalogRuntime>::read_codex_catalog_transport_snapshot(
            &state,
            PROVIDER_ID,
            CATALOG_ENDPOINT_ID,
            CATALOG_KEY_ID,
        )
        .await
        .expect("Codex catalog transport lookup should succeed")
        .is_some(),
        "Codex catalog transport must be available even when auto_fetch_models is disabled"
    );

    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();

    let codex_response = client
        .get(format!(
            "{gateway_url}/v1/models?client_version=0.145.2-beta.7%2Bdesktop.9"
        ))
        .header("authorization", "Bearer sk-codex-models")
        .send()
        .await
        .expect("Codex models request should succeed");
    assert_eq!(codex_response.status(), StatusCode::OK);
    let codex_etag = codex_response
        .headers()
        .get(http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let codex_payload: serde_json::Value = codex_response
        .json()
        .await
        .expect("Codex models body should parse");
    assert_eq!(
        catalog_hits.load(Ordering::SeqCst),
        1,
        "cold Codex request must reach the upstream catalog once; payload={codex_payload}"
    );
    assert_eq!(codex_etag.as_deref(), Some("\"catalog-etag-v1\""));
    assert_eq!(codex_payload["models"].as_array().map(Vec::len), Some(2));
    let current_card = codex_payload["models"]
        .as_array()
        .and_then(|models| models.iter().find(|model| model["slug"] == "future-alias"))
        .expect("current model card should be projected");
    assert_eq!(
        current_card["model_messages"]["instructions_template"],
        "Use future dynamic instructions."
    );
    assert_eq!(
        current_card["future_capability"],
        json!({"mode":"opaque-current"})
    );
    assert_eq!(current_card["available_in_plans"], json!(["plus", "pro"]));
    assert!(current_card.get("base_instructions").is_none());
    assert!(current_card.get("id").is_none());
    assert!(current_card.get("api_formats").is_none());
    let legacy_card = codex_payload["models"]
        .as_array()
        .and_then(|models| models.iter().find(|model| model["slug"] == "legacy-alias"))
        .expect("legacy model card should be projected");
    assert_eq!(
        legacy_card["base_instructions"],
        "Use legacy future instructions."
    );
    assert_eq!(
        legacy_card["future_capability"],
        json!({"mode":"opaque-legacy"})
    );
    assert!(codex_payload["models"]
        .as_array()
        .is_some_and(
            |models| models.iter().all(|model| model["slug"] != "hidden-alias"
                && model["slug"] != "second-alias"
                && model["slug"] != "gpt-future-unmapped")
        ));
    assert!(codex_payload.get("object").is_none());
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 1);

    let restricted_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-legacy-only")
        .send()
        .await
        .expect("restricted Codex models request should succeed");
    assert_eq!(restricted_response.status(), StatusCode::OK);
    let restricted_payload: serde_json::Value = restricted_response
        .json()
        .await
        .expect("restricted Codex models body should parse");
    assert_eq!(
        restricted_payload["models"].as_array().map(|models| models
            .iter()
            .map(|model| model["slug"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()),
        Some(vec!["legacy-alias"])
    );
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 1);

    let incomplete_authorized_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-hidden-mixed")
        .send()
        .await
        .expect("incomplete authorized Codex catalog request should succeed");
    assert_eq!(incomplete_authorized_response.status(), StatusCode::OK);
    assert!(incomplete_authorized_response
        .headers()
        .get(http::header::ETAG)
        .is_none());
    let incomplete_authorized_payload: serde_json::Value = incomplete_authorized_response
        .json()
        .await
        .expect("incomplete authorized Codex body should parse");
    assert_eq!(
        incomplete_authorized_payload["models"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "a partial non-empty remote catalog would hide the client's bundled fallback models"
    );
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 1);

    let pending_second_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-second-mixed")
        .send()
        .await
        .expect("not-yet-published authorized model request should succeed");
    assert_eq!(pending_second_response.status(), StatusCode::OK);
    let pending_second_payload: serde_json::Value = pending_second_response
        .json()
        .await
        .expect("not-yet-published authorized model body should parse");
    assert_eq!(
        pending_second_payload["models"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 1);

    let captured_catalog_plan = captured_plans
        .lock()
        .expect("plans mutex")
        .iter()
        .find(|(url, _)| url.contains("/models?"))
        .cloned()
        .expect("catalog execution plan should be captured");
    assert_eq!(
        captured_catalog_plan.0,
        "https://chatgpt.example/backend-api/codex/models?client_version=0.145.2"
    );
    assert_eq!(
        captured_catalog_plan.1.as_deref(),
        Some("codex_cli_rs/0.145.2")
    );

    let fresh_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-models")
        .send()
        .await
        .expect("fresh Codex models request should succeed");
    assert_eq!(fresh_response.status(), StatusCode::OK);
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 1);

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    catalog_generation.store(1, Ordering::SeqCst);
    let stale_started = std::time::Instant::now();
    let stale_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-models")
        .send()
        .await
        .expect("stale Codex models request should succeed");
    assert_eq!(stale_response.status(), StatusCode::OK);
    assert!(stale_started.elapsed() < std::time::Duration::from_millis(400));
    let stale_payload: serde_json::Value = stale_response
        .json()
        .await
        .expect("stale body should parse");
    assert!(stale_payload["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|model| model["slug"] == "future-alias")));
    wait_until(1_000, || catalog_hits.load(Ordering::SeqCst) >= 2).await;

    let failed_refresh_lkg_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-models")
        .send()
        .await
        .expect("failed refresh should keep serving LKG");
    let failed_refresh_lkg_payload: serde_json::Value = failed_refresh_lkg_response
        .json()
        .await
        .expect("failed refresh LKG body should parse");
    assert!(failed_refresh_lkg_payload["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|model| model["slug"] == "future-alias")));
    assert_eq!(catalog_hits.load(Ordering::SeqCst), 2);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    catalog_generation.store(2, Ordering::SeqCst);
    let refresh_trigger = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-models")
        .send()
        .await
        .expect("recovered refresh trigger should succeed");
    assert_eq!(refresh_trigger.status(), StatusCode::OK);
    wait_until(1_000, || catalog_hits.load(Ordering::SeqCst) >= 3).await;

    let updated_response = client
        .get(format!("{gateway_url}/v1/models?client_version=0.145.2"))
        .header("authorization", "Bearer sk-codex-second-mixed")
        .send()
        .await
        .expect("updated catalog request should succeed");
    assert_eq!(
        updated_response
            .headers()
            .get(http::header::ETAG)
            .and_then(|value| value.to_str().ok()),
        Some("\"catalog-etag-v2\"")
    );
    let updated_payload: serde_json::Value = updated_response
        .json()
        .await
        .expect("updated body should parse");
    assert!(updated_payload["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|model| {
            model["slug"] == "second-alias"
                && model["future_capability"] == json!({"mode":"added-without-code-change"})
        })));
    assert!(updated_payload["models"]
        .as_array()
        .is_some_and(|models| models.iter().all(|model| {
            model["slug"] != "hidden-alias" && model["slug"] != "gpt-future-unmapped"
        })));

    let stored_keys = provider_catalog_repository
        .list_keys_by_ids(&[CATALOG_KEY_ID.to_string()])
        .await
        .expect("catalog key should remain readable");
    assert_eq!(stored_keys.as_slice(), &[original_catalog_key]);
    let rows_after = candidate_repository
        .list_for_exact_api_format("openai:responses")
        .await
        .expect("candidate rows should load after request");
    assert_eq!(rows_after, rows_before);

    let inference_response = client
        .post(format!("{gateway_url}/v1/responses"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("authorization", "Bearer sk-codex-models")
        .body(r#"{"model":"future-alias","input":"hello","store":false}"#)
        .send()
        .await
        .expect("manual alias inference should succeed");
    let inference_status = inference_response.status();
    let inference_payload: serde_json::Value = inference_response
        .json()
        .await
        .expect("inference body should parse");
    assert_eq!(
        inference_status,
        StatusCode::OK,
        "unexpected inference response: {inference_payload}"
    );
    assert_eq!(inference_payload["model"], "gpt-future-dynamic");

    let standard_response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-standard-models")
        .send()
        .await
        .expect("standard models request should succeed");
    assert_eq!(standard_response.status(), StatusCode::OK);
    let standard_payload: serde_json::Value = standard_response
        .json()
        .await
        .expect("standard models body should parse");
    assert_eq!(standard_payload["object"], "list");
    assert!(standard_payload["data"].is_array());
    assert!(standard_payload.get("models").is_none());

    gateway_handle.abort();
    execution_runtime_handle.abort();
}

#[tokio::test]
async fn gateway_openai_models_list_drops_disabled_global_model_after_cache_invalidation() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-models-cache")),
        unrestricted_models_snapshot("key-models-cache", "user-models-cache"),
    )]));
    let row = sample_models_candidate_row(
        "provider-openai-cache",
        "openai",
        "openai:chat",
        "gpt-5",
        10,
    );
    let global_model_id = row.global_model_id.clone();
    let candidate_repository = Arc::new(CachedToggleMinimalCandidateSelectionReadRepository::new(
        row.clone(),
    ));
    let global_model_repository = Arc::new(
        InMemoryGlobalModelReadRepository::seed(Vec::new()).with_admin_global_models(vec![
            StoredAdminGlobalModel::new(
                global_model_id.clone(),
                row.global_model_name.clone(),
                "GPT 5".to_string(),
                true,
                None,
                None,
                None,
                None,
                0,
                1,
                0,
                Some(1_711_000_000),
                Some(1_711_000_000),
            )
            .expect("global model should build"),
        ]),
    );
    let state = AppState::new()
        .expect("gateway should build")
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                candidate_repository.clone(),
                auth_repository,
            )
            .with_global_model_repository_for_tests(global_model_repository),
        );
    let gateway = build_router_with_state(state.clone());
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models-cache")
        .send()
        .await
        .expect("initial models request should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["data"][0]["id"], "gpt-5");

    candidate_repository.set_active(false);
    let disabled_global_model = UpdateAdminGlobalModelRecord::new(
        global_model_id,
        "GPT 5".to_string(),
        false,
        None,
        None,
        None,
        None,
    )
    .expect("global model update record should build");
    state
        .update_admin_global_model(&disabled_global_model)
        .await
        .expect("global model update should succeed")
        .expect("global model should update");

    let response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models-cache")
        .send()
        .await
        .expect("models request after disable should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(
        payload["data"]
            .as_array()
            .expect("data should be an array")
            .len(),
        0
    );

    gateway_handle.abort();
}

#[tokio::test]
async fn gateway_returns_empty_openai_models_when_candidate_rows_stall() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("proxied"))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-models-stalled")),
        unrestricted_models_snapshot("key-stalled", "user-stalled"),
    )]));
    let candidate_repository = Arc::new(PendingMinimalCandidateSelectionReadRepository);

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client should build")
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models-stalled")
        .send()
        .await
        .expect("request should return before client timeout");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["object"], "list");
    assert_eq!(
        payload["data"]
            .as_array()
            .expect("data should be an array")
            .len(),
        0
    );
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_returns_not_found_for_openai_model_detail_when_candidate_rows_stall() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("proxied"))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-model-detail-stalled")),
        unrestricted_models_snapshot("key-detail-stalled", "user-detail-stalled"),
    )]));
    let candidate_repository = Arc::new(PendingMinimalCandidateSelectionReadRepository);

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client should build")
        .get(format!("{gateway_url}/v1/models/gpt-stalled"))
        .header("authorization", "Bearer sk-openai-model-detail-stalled")
        .send()
        .await
        .expect("request should return before client timeout");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["error"]["code"], "model_not_found");
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_handles_public_openai_models_with_cross_format_candidates_without_hitting_fallback_probe(
) {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("proxied"))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-models-cross-format")),
        unrestricted_models_snapshot("key-1", "user-1"),
    )]));
    let candidate_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_models_candidate_row(
                "provider-claude",
                "claude",
                "claude:messages",
                "claude-3-7-sonnet",
                10,
            ),
        ]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let client = reqwest::Client::new();
    let list_response = client
        .get(format!("{gateway_url}/v1/models"))
        .header("authorization", "Bearer sk-openai-models-cross-format")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(list_response.status(), StatusCode::OK);
    let list_payload: serde_json::Value =
        list_response.json().await.expect("json body should parse");
    assert_eq!(list_payload["object"], "list");
    assert_eq!(list_payload["data"][0]["id"], "claude-3-7-sonnet");
    assert_eq!(list_payload["data"][0]["owned_by"], "aether");

    let detail_response = client
        .get(format!("{gateway_url}/v1/models/claude-3-7-sonnet"))
        .header("authorization", "Bearer sk-openai-models-cross-format")
        .send()
        .await
        .expect("request should succeed");
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail_payload: serde_json::Value = detail_response
        .json()
        .await
        .expect("json body should parse");
    assert_eq!(detail_payload["id"], "claude-3-7-sonnet");
    assert_eq!(detail_payload["owned_by"], "aether");

    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_handles_public_claude_models_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-claude-models")),
        unrestricted_models_snapshot("key-claude", "user-claude"),
    )]));
    let candidate_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_models_candidate_row(
                "provider-claude",
                "claude",
                "claude:messages",
                "claude-3-7-sonnet",
                10,
            ),
            sample_models_candidate_row(
                "provider-claude",
                "claude",
                "claude:messages",
                "claude-3-5-haiku",
                10,
            ),
        ]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!("{gateway_url}/v1/models?limit=1"))
        .header("x-api-key", "sk-claude-models")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["data"][0]["id"], "claude-3-5-haiku");
    assert_eq!(payload["first_id"], "claude-3-5-haiku");
    assert_eq!(payload["last_id"], "claude-3-5-haiku");
    assert_eq!(payload["has_more"], true);
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_handles_public_gemini_models_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-gemini-models")),
        unrestricted_models_snapshot("key-gemini", "user-gemini"),
    )]));
    let candidate_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            sample_models_candidate_row(
                "provider-gemini",
                "gemini",
                "gemini:generate_content",
                "gemini-2.5-flash",
                10,
            ),
            sample_models_candidate_row(
                "provider-gemini",
                "gemini",
                "gemini:generate_content",
                "gemini-2.5-pro",
                10,
            ),
        ]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_minimal_candidate_selection_and_auth_for_tests(
                    candidate_repository,
                    auth_repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/v1beta/models?pageSize=1&key=sk-gemini-models"
        ))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["models"][0]["name"], "models/gemini-2.5-flash");
    assert_eq!(payload["nextPageToken"], "1");
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_handles_antigravity_v1internal_control_plane_without_proxying() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(AppState::new().expect("gateway should build"));
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let client = reqwest::Client::new();

    let user_settings = json!({
        "preferredModelId": "gemini-3.1-flash-lite",
        "theme": "dark"
    });
    let requests = vec![
        (
            "/v1internal:loadCodeAssist",
            json!({"metadata": {"ideType": "ANTIGRAVITY_CLI"}}),
        ),
        (
            "/v1internal:fetchAvailableModels",
            json!({"project": "aether-antigravity-local"}),
        ),
        (
            "/v1internal:retrieveUserQuotaSummary",
            json!({"project": "aether-antigravity-local"}),
        ),
        (
            "/v1internal:fetchUserInfo",
            json!({"project": "aether-antigravity-local"}),
        ),
        (
            "/v1internal:fetchAdminControls",
            json!({"project": "aether-antigravity-local"}),
        ),
        ("/v1internal:listExperiments", json!({})),
        (
            "/v1internal:recordCodeAssistMetrics",
            json!({
                "project": "aether-antigravity-local",
                "requestId": "opaque-request-id",
                "metrics": []
            }),
        ),
        (
            "/v1internal:writeTrajectoryAcls",
            json!({"trajectoryId": "trajectory-ant-123"}),
        ),
        (
            "/v1internal:setUserSettings",
            json!({"userSettings": user_settings.clone()}),
        ),
    ];

    for (path, request_body) in requests {
        let response = client
            .post(format!("{gateway_url}{path}"))
            .header("authorization", "Bearer ant-access-token")
            .header("user-agent", "antigravity/cli/1.0.2 linux/arm64")
            .json(&request_body)
            .send()
            .await
            .expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK, "path {path}");
        assert_eq!(
            response
                .headers()
                .get(EXECUTION_PATH_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(EXECUTION_PATH_LOCAL_AI_PUBLIC),
            "path {path}"
        );
        let payload: serde_json::Value = response.json().await.expect("json body should parse");

        match path {
            "/v1internal:loadCodeAssist" => {
                assert_eq!(
                    payload["cloudaicompanionProject"],
                    "aether-antigravity-local"
                );
                assert_eq!(payload["currentTier"]["id"], "free-tier");
                assert_eq!(payload["currentTier"]["name"], "Antigravity");
                assert_eq!(payload["paidTier"]["id"], "g1-pro-tier");
                assert_eq!(payload["gcpManaged"], false);
                assert_eq!(payload["allowedTiers"][0]["id"], "free-tier");
                assert_eq!(payload["allowedTiers"][0]["isDefault"], true);
                assert_eq!(payload["allowedTiers"][1]["id"], "standard-tier");
                assert_eq!(
                    payload["upgradeSubscriptionUri"],
                    "https://codeassist.google.com/upgrade"
                );
            }
            "/v1internal:fetchAvailableModels" => {
                assert_eq!(payload["defaultAgentModelId"], "gemini-3.5-flash-low");
                assert_eq!(
                    payload["tieredModelIds"]["flash"],
                    json!(["gemini-3-flash-agent"])
                );
                assert_eq!(
                    payload["tieredModelIds"]["pro"],
                    json!(["gemini-3.1-pro-low"])
                );
                assert_eq!(
                    payload["models"]["gemini-3-flash-agent"]["displayName"],
                    "Gemini 3.5 Flash (High)"
                );
                assert_eq!(
                    payload["models"]["gemini-3.5-flash-low"]["displayName"],
                    "Gemini 3.5 Flash (Medium)"
                );
                assert_eq!(
                    payload["models"]["gemini-3.5-flash-extra-low"]["displayName"],
                    "Gemini 3.5 Flash (Low)"
                );
                assert_eq!(
                    payload["models"]["gemini-pro-agent"]["displayName"],
                    "Gemini 3.1 Pro (High)"
                );
                assert_eq!(
                    payload["models"]["claude-opus-4-6-thinking"]["displayName"],
                    "Claude Opus 4.6 (Thinking)"
                );
                assert_eq!(
                    payload["models"]["gpt-oss-120b-medium"]["displayName"],
                    "GPT-OSS 120B (Medium)"
                );
                assert_eq!(
                    payload["models"]["gemini-pro-agent"]["model"],
                    "MODEL_PLACEHOLDER_M16"
                );
                assert_eq!(
                    payload["models"]["gemini-3.1-pro-high"]["model"],
                    "MODEL_PLACEHOLDER_M37"
                );
                assert_eq!(
                    payload["models"]["gemini-3.5-flash-extra-low"]["model"],
                    "MODEL_PLACEHOLDER_M187"
                );
                assert_eq!(
                    payload["models"]["claude-sonnet-4-6"]["apiProvider"],
                    "API_PROVIDER_ANTHROPIC_VERTEX"
                );
                assert_eq!(
                    payload["models"]["gpt-oss-120b-medium"]["apiProvider"],
                    "API_PROVIDER_OPENAI_VERTEX"
                );
                assert_eq!(
                    payload["models"]["gemini-3.5-flash-low"]["apiProvider"],
                    "API_PROVIDER_GOOGLE_GEMINI"
                );
                assert_eq!(
                    payload["models"]["gemini-2.5-flash-lite"]["model"],
                    "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE"
                );
                assert_eq!(
                    payload["agentModelSorts"][0]["groups"][0]["modelIds"],
                    json!([
                        "gemini-3.5-flash-low",
                        "gemini-3-flash-agent",
                        "gemini-3.5-flash-extra-low",
                        "gemini-3.1-pro-low",
                        "gemini-pro-agent",
                        "claude-sonnet-4-6",
                        "claude-opus-4-6-thinking",
                        "gpt-oss-120b-medium"
                    ])
                );
                assert_eq!(
                    payload["deprecatedModelIds"]["gemini-3.1-pro-high"]["newModelId"],
                    "gemini-pro-agent"
                );
                assert_eq!(payload["commandModelIds"], json!(["gemini-3-flash"]));
                assert_eq!(
                    payload["imageGenerationModelIds"],
                    json!(["gemini-3.1-flash-image"])
                );
                assert_eq!(payload["tabModelIds"], json!(["chat_20706", "chat_23310"]));
                assert_eq!(payload["mqueryModelIds"], json!(["gemini-3.1-flash-lite"]));
                assert_eq!(
                    payload["webSearchModelIds"],
                    json!(["gemini-3.1-flash-lite"])
                );
                assert_eq!(
                    payload["commitMessageModelIds"],
                    json!(["gemini-3.1-flash-lite"])
                );
            }
            "/v1internal:fetchUserInfo" => {
                assert_eq!(payload["regionCode"], "US");
                assert_eq!(
                    payload["userSettings"]["preferredModelId"],
                    "gemini-3.5-flash-low"
                );
            }
            "/v1internal:retrieveUserQuotaSummary" => {
                assert_eq!(payload["description"], "");
                assert_eq!(payload["groups"], json!([]));
            }
            "/v1internal:fetchAdminControls" => {
                assert_eq!(payload, json!({}));
            }
            "/v1internal:listExperiments" => {
                assert_eq!(payload["experimentIds"], json!([]));
                assert_eq!(payload["flags"], json!([]));
            }
            "/v1internal:recordCodeAssistMetrics" => {
                assert_eq!(payload, json!({}));
            }
            "/v1internal:writeTrajectoryAcls" => {
                assert_eq!(payload, json!({}));
            }
            "/v1internal:setUserSettings" => {
                assert_eq!(payload["userSettings"], user_settings);
            }
            other => panic!("unexpected path {other}"),
        }
    }

    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_does_not_locally_reject_image_model_name_on_chat_completions() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-chat-image-model")),
        unrestricted_models_snapshot(
            "key-openai-chat-image-model",
            "user-openai-chat-image-model",
        ),
    )]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_auth_api_key_data_reader_for_tests(auth_repository),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header("authorization", "Bearer sk-openai-chat-image-model")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "gpt-image-2",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("request body should encode"),
        )
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_LOCAL_EXECUTION_RUNTIME_MISS)
    );
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_rejects_image_request_above_gateway_limit_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-image-n")),
        unrestricted_models_snapshot("key-openai-image-n", "user-openai-image-n"),
    )]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_auth_api_key_data_reader_for_tests(auth_repository),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/images/generations"))
        .header("authorization", "Bearer sk-openai-image-n")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "grok-imagine-image-lite",
                "prompt": "draw",
                "n": openai_image_gateway_max_generation_count() + 1,
                "response_format": "b64_json"
            }))
            .expect("request body should encode"),
        )
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_LOCAL_AI_PUBLIC)
    );
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(
        payload["detail"],
        format!(
            "当前图片反代仅支持 n=1..{}",
            openai_image_gateway_max_generation_count()
        )
    );
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_does_not_mount_image_variation_route_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-openai-image-variation")),
        unrestricted_models_snapshot("key-openai-image-variation", "user-openai-image-variation"),
    )]));

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_auth_api_key_data_reader_for_tests(auth_repository),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/images/variations"))
        .header("authorization", "Bearer sk-openai-image-variation")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "dall-e-2",
                "response_format": "url"
            }))
            .expect("request body should encode"),
        )
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_handles_gemini_operation_detail_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-gemini-operation-detail")),
        unrestricted_models_snapshot(
            "key-gemini-operation-detail",
            "user-gemini-operation-detail",
        ),
    )]));
    let repository = Arc::new(InMemoryVideoTaskRepository::default());
    repository
        .upsert(sample_gemini_video_task(
            "task-gemini-operation-detail",
            "opshort123",
            "user-gemini-operation-detail",
            "key-gemini-operation-detail",
            "operations/ext-op-123",
            VideoTaskStatus::Completed,
        ))
        .await
        .expect("upsert should succeed");

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_auth_and_video_task_repository_for_tests(
                    auth_repository,
                    repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/v1beta/operations/opshort123?key=sk-gemini-operation-detail"
        ))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_LOCAL_AI_PUBLIC)
    );
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["name"], "models/veo-3/operations/opshort123");
    assert_eq!(payload["done"], true);
    assert_eq!(
        payload["response"]["generateVideoResponse"]["generatedSamples"][0]["video"]["uri"],
        "/v1beta/files/aev_opshort123:download?alt=media"
    );
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_lists_gemini_operations_without_hitting_fallback_probe() {
    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-gemini-operation-list")),
        unrestricted_models_snapshot("key-gemini-operation-list", "user-gemini-operation-list"),
    )]));
    let repository = Arc::new(InMemoryVideoTaskRepository::default());
    repository
        .upsert(sample_gemini_video_task(
            "task-gemini-operation-list-1",
            "opshort-list-1",
            "user-gemini-operation-list",
            "key-gemini-operation-list",
            "operations/ext-list-1",
            VideoTaskStatus::Completed,
        ))
        .await
        .expect("upsert should succeed");
    repository
        .upsert(sample_gemini_video_task(
            "task-gemini-operation-list-2",
            "opshort-list-2",
            "user-gemini-operation-list",
            "key-gemini-operation-list",
            "operations/ext-list-2",
            VideoTaskStatus::Processing,
        ))
        .await
        .expect("upsert should succeed");
    repository
        .upsert(sample_gemini_video_task(
            "task-gemini-operation-list-other",
            "opshort-list-other",
            "user-other",
            "key-other",
            "operations/ext-list-other",
            VideoTaskStatus::Completed,
        ))
        .await
        .expect("upsert should succeed");

    let (_unused_fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_auth_and_video_task_repository_for_tests(
                    auth_repository,
                    repository,
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/v1beta/operations?key=sk-gemini-operation-list"
        ))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_LOCAL_AI_PUBLIC)
    );
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    let operations = payload["operations"]
        .as_array()
        .expect("operations should be an array");
    assert_eq!(operations.len(), 2);
    let operation_names = operations
        .iter()
        .map(|value| {
            value["name"]
                .as_str()
                .expect("operation name should be a string")
                .to_string()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        operation_names,
        std::collections::BTreeSet::from([
            "models/veo-3/operations/opshort-list-1".to_string(),
            "models/veo-3/operations/opshort-list-2".to_string(),
        ])
    );
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    fallback_probe_handle.abort();
}

#[tokio::test]
async fn gateway_cancels_gemini_operation_without_hitting_fallback_probe() {
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenExecutionRuntimeSyncRequest {
        method: String,
        url: String,
        api_key: String,
    }

    let fallback_probe_hits = Arc::new(Mutex::new(0usize));
    let fallback_probe_hits_clone = Arc::clone(&fallback_probe_hits);
    let fallback_probe = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let fallback_probe_hits_inner = Arc::clone(&fallback_probe_hits_clone);
            async move {
                *fallback_probe_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Json(json!({"proxied": true}))).into_response()
            }
        }),
    );

    let seen_execution_runtime = Arc::new(Mutex::new(None::<SeenExecutionRuntimeSyncRequest>));
    let seen_execution_runtime_clone = Arc::clone(&seen_execution_runtime);
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |request: Request| {
            let seen_execution_runtime_inner = Arc::clone(&seen_execution_runtime_clone);
            async move {
                let (_parts, body) = request.into_parts();
                let raw_body = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: serde_json::Value = serde_json::from_slice(&raw_body)
                    .expect("execution runtime payload should parse");
                *seen_execution_runtime_inner
                    .lock()
                    .expect("mutex should lock") = Some(SeenExecutionRuntimeSyncRequest {
                    method: payload
                        .get("method")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: payload
                        .get("url")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    api_key: payload
                        .get("headers")
                        .and_then(|value| value.get("x-goog-api-key"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
                Json(json!({
                    "request_id": "trace-gemini-operation-cancel",
                    "status_code": 200,
                    "headers": {
                        "content-type": "application/json"
                    },
                    "body": {
                        "json_body": {}
                    },
                    "telemetry": {
                        "elapsed_ms": 12
                    }
                }))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-gemini-operation-cancel")),
        unrestricted_models_snapshot(
            "key-gemini-operation-cancel",
            "user-gemini-operation-cancel",
        ),
    )]));
    let repository = Arc::new(InMemoryVideoTaskRepository::default());
    repository
        .upsert(sample_gemini_video_task(
            "task-gemini-operation-cancel",
            "opshort-cancel",
            "user-gemini-operation-cancel",
            "key-gemini-operation-cancel",
            "operations/ext-op-123",
            VideoTaskStatus::Submitted,
        ))
        .await
        .expect("upsert should succeed");

    let (fallback_probe_url, fallback_probe_handle) = start_server(fallback_probe).await;
    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway = build_router_with_state(
        build_state_with_execution_runtime_override(execution_runtime_url)
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_auth_and_video_task_repository_for_tests(
                    auth_repository,
                    Arc::clone(&repository),
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .post(format!(
            "{gateway_url}/v1beta/operations/opshort-cancel:cancel"
        ))
        .header("x-goog-api-key", "sk-gemini-operation-cancel")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(EXECUTION_PATH_LOCAL_AI_PUBLIC)
    );
    assert_eq!(
        response
            .json::<serde_json::Value>()
            .await
            .expect("json body should parse"),
        json!({})
    );

    let seen_execution_runtime_request = seen_execution_runtime
        .lock()
        .expect("mutex should lock")
        .clone()
        .expect("execution runtime sync should be captured");
    assert_eq!(seen_execution_runtime_request.method, "POST");
    assert_eq!(
        seen_execution_runtime_request.url,
        "https://generativelanguage.googleapis.com/v1beta/models/veo-3/operations/ext-op-123:cancel"
    );
    assert_eq!(
        seen_execution_runtime_request.api_key,
        "sk-upstream-gemini-video"
    );

    let stored = repository
        .find(VideoTaskLookupKey::Id("task-gemini-operation-cancel"))
        .await
        .expect("task lookup should succeed")
        .expect("task should exist");
    assert_eq!(stored.status, VideoTaskStatus::Cancelled);
    assert_eq!(*fallback_probe_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    execution_runtime_handle.abort();
    fallback_probe_handle.abort();
}
