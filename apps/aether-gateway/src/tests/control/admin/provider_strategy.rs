use std::sync::{Arc, Mutex};

use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data::repository::quota::InMemoryProviderQuotaRepository;
use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data_contracts::repository::provider_catalog::ProviderCatalogReadRepository;
use aether_data_contracts::repository::quota::{
    ProviderQuotaReadRepository, StoredProviderQuotaSnapshot,
};
use aether_data_contracts::repository::usage::StoredProviderUsageWindow;
use axum::body::Body;
use axum::routing::any;
use axum::{extract::Request, Router};
use http::{Method, StatusCode};
use serde_json::json;

use super::super::{build_router_with_state, sample_provider, start_server, AppState};
use crate::constants::{
    GATEWAY_HEADER, TRUSTED_ADMIN_SESSION_ID_HEADER, TRUSTED_ADMIN_USER_ID_HEADER,
    TRUSTED_ADMIN_USER_ROLE_HEADER,
};
use crate::data::GatewayDataState;

fn run_provider_strategy_test<F, Fut>(test_name: &'static str, make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build")
                .block_on(make_future());
        })
        .expect("provider strategy test thread should spawn");

    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

macro_rules! provider_strategy_async_test {
    ($test_name:ident, $impl_name:ident) => {
        #[test]
        fn $test_name() {
            run_provider_strategy_test(stringify!($test_name), $impl_name);
        }
    };
}

provider_strategy_async_test!(
    gateway_handles_admin_provider_strategy_list_locally_with_trusted_admin_principal,
    gateway_handles_admin_provider_strategy_list_locally_with_trusted_admin_principal_impl
);
provider_strategy_async_test!(
    gateway_handles_admin_provider_strategy_billing_locally_returns_service_unavailable_when_disabled,
    gateway_handles_admin_provider_strategy_billing_locally_returns_service_unavailable_when_disabled_impl
);
provider_strategy_async_test!(
    gateway_handles_admin_provider_strategy_quota_locally_returns_service_unavailable_without_writer,
    gateway_handles_admin_provider_strategy_quota_locally_returns_service_unavailable_without_writer_impl
);
provider_strategy_async_test!(
    gateway_schedules_admin_provider_strategy_quota_reset_locally_with_trusted_admin_principal,
    gateway_schedules_admin_provider_strategy_quota_reset_locally_with_trusted_admin_principal_impl
);
provider_strategy_async_test!(
    gateway_updates_admin_provider_strategy_billing_locally_with_trusted_admin_principal,
    gateway_updates_admin_provider_strategy_billing_locally_with_trusted_admin_principal_impl
);
provider_strategy_async_test!(
    gateway_preserves_admin_provider_strategy_monthly_usage_when_epoch_is_unchanged,
    gateway_preserves_admin_provider_strategy_monthly_usage_when_epoch_is_unchanged_impl
);
provider_strategy_async_test!(
    gateway_reads_admin_provider_strategy_stats_locally_with_trusted_admin_principal,
    gateway_reads_admin_provider_strategy_stats_locally_with_trusted_admin_principal_impl
);
provider_strategy_async_test!(
    gateway_rounds_admin_provider_strategy_stats_total_cost_locally,
    gateway_rounds_admin_provider_strategy_stats_total_cost_locally_impl
);

async fn assert_provider_strategy_route_returns_local_503(
    data_state: GatewayDataState,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
    expected_message: &str,
) {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/{*path}",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(data_state),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let mut request = reqwest::Client::new()
        .request(method, format!("{gateway_url}{path}"))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request should succeed");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["error"]["message"], expected_message);
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

const PROVIDER_STRATEGY_DATA_UNAVAILABLE_MESSAGE: &str = "Admin provider strategy data unavailable";
const PROVIDER_STRATEGY_STATS_DATA_UNAVAILABLE_MESSAGE: &str =
    "Admin provider strategy stats data unavailable";

async fn gateway_handles_admin_provider_strategy_list_locally_with_trusted_admin_principal_impl() {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/strategies",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(AppState::new().expect("gateway should build"));
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/api/admin/provider-strategy/strategies"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["total"], 1);
    assert_eq!(payload["strategies"][0]["name"], "sticky_priority");
    assert_eq!(payload["strategies"][0]["priority"], 110);
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

async fn gateway_handles_admin_provider_strategy_billing_locally_returns_service_unavailable_when_disabled_impl(
) {
    assert_provider_strategy_route_returns_local_503(
        crate::data::GatewayDataState::disabled(),
        Method::PUT,
        "/api/admin/provider-strategy/providers/provider-openai/billing",
        Some(json!({
            "billing_type": "monthly_quota",
            "monthly_quota_usd": 100.0,
            "quota_reset_day": 30,
            "quota_last_reset_at": "2024-03-21T00:00:00Z",
            "quota_expires_at": "2024-04-21T00:00:00Z",
            "rpm_limit": 20,
            "provider_priority": 5
        })),
        PROVIDER_STRATEGY_DATA_UNAVAILABLE_MESSAGE,
    )
    .await;
}

#[test]
fn gateway_handles_admin_provider_strategy_stats_locally_returns_service_unavailable_without_usage_reader(
) {
    run_provider_strategy_test("provider-strategy-stats-unavailable", || async {
        let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
            vec![sample_provider("provider-openai", "openai", 10)],
            vec![],
            vec![],
        ));
        let catalog_reader: Arc<dyn ProviderCatalogReadRepository> =
            provider_catalog_repository.clone();
        assert_provider_strategy_route_returns_local_503(
            crate::data::GatewayDataState::with_provider_catalog_reader_for_tests(catalog_reader),
            Method::GET,
            "/api/admin/provider-strategy/providers/provider-openai/stats?hours=1",
            None,
            PROVIDER_STRATEGY_STATS_DATA_UNAVAILABLE_MESSAGE,
        )
        .await;
    });
}

async fn gateway_handles_admin_provider_strategy_quota_locally_returns_service_unavailable_without_writer_impl(
) {
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![sample_provider("provider-openai", "openai", 10)],
        vec![],
        vec![],
    ));
    let catalog_reader: Arc<dyn ProviderCatalogReadRepository> =
        provider_catalog_repository.clone();
    assert_provider_strategy_route_returns_local_503(
        crate::data::GatewayDataState::with_provider_catalog_reader_for_tests(catalog_reader),
        Method::DELETE,
        "/api/admin/provider-strategy/providers/provider-openai/quota",
        None,
        PROVIDER_STRATEGY_DATA_UNAVAILABLE_MESSAGE,
    )
    .await;
}
async fn gateway_schedules_admin_provider_strategy_quota_reset_locally_with_trusted_admin_principal_impl(
) {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/providers/provider-openai/quota/reset",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            sample_provider("provider-openai", "openai", 10).with_billing_fields(
                Some("monthly_quota".to_string()),
                Some(100.0),
                Some(12.5),
                Some(30),
                Some(1_711_000_000),
                Some(1_711_000_000 + 30 * 24 * 60 * 60),
            ),
        ],
        vec![],
        vec![],
    ));
    let quota_repository = Arc::new(InMemoryProviderQuotaRepository::seed([
        StoredProviderQuotaSnapshot::new(
            "provider-openai".to_string(),
            "monthly_quota".to_string(),
            Some(100.0),
            12.5,
            Some(30),
            Some(1_711_000_000),
            Some(1_711_000_000 + 30 * 24 * 60 * 60),
            true,
        )
        .expect("quota should build"),
    ]));

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                GatewayDataState::with_provider_quota_repository_for_tests(Arc::clone(
                    &quota_repository,
                ))
                .attach_provider_catalog_repository_for_tests(Arc::clone(
                    &provider_catalog_repository,
                )),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let requested_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be valid")
        .as_secs();

    let response = reqwest::Client::new()
        .post(format!(
            "{gateway_url}/api/admin/provider-strategy/providers/provider-openai/quota/reset"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["provider_name"], "openai");
    assert_eq!(payload["previous_used"], 12.5);
    assert_eq!(payload["current_used"], 12.5);
    assert_eq!(payload["pending"], true);
    assert!(payload["effective_at"].is_string());

    let updated = provider_catalog_repository
        .list_providers_by_ids(&["provider-openai".to_string()])
        .await
        .expect("provider query should succeed");
    assert_eq!(updated[0].monthly_used_usd, Some(12.5));
    let quota = quota_repository
        .find_by_provider_id("provider-openai")
        .await
        .expect("quota query should succeed")
        .expect("quota should exist");
    assert_eq!(quota.monthly_used_usd, 12.5);
    let pending_reset = quota
        .pending_quota_reset_at_unix_secs
        .expect("reset should be pending");
    assert_eq!(pending_reset % 60, 0);
    assert!(pending_reset > requested_after);
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

async fn gateway_updates_admin_provider_strategy_billing_locally_with_trusted_admin_principal_impl()
{
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/providers/provider-openai/billing",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            sample_provider("provider-openai", "openai", 7).with_billing_fields(
                Some("pay_as_you_go".to_string()),
                None,
                Some(0.0),
                Some(9),
                None,
                None,
            ),
        ],
        vec![],
        vec![],
    ));

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_repository_for_tests(Arc::clone(
                    &provider_catalog_repository,
                )),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .put(format!(
            "{gateway_url}/api/admin/provider-strategy/providers/provider-openai/billing"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .json(&json!({
            "billing_type": "monthly_quota",
            "monthly_quota_usd": 50.0,
            "quota_last_reset_at": "2026-03-01T00:00:00Z",
            "quota_expires_at": "2026-04-01T00:00:00Z"
        }))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["provider"]["id"], "provider-openai");
    assert_eq!(payload["provider"]["billing_type"], "monthly_quota");
    assert_eq!(payload["provider"]["provider_priority"], 100);

    let updated = provider_catalog_repository
        .list_providers_by_ids(&["provider-openai".to_string()])
        .await
        .expect("provider query should succeed");
    assert_eq!(updated[0].billing_type.as_deref(), Some("monthly_quota"));
    assert_eq!(updated[0].monthly_quota_usd, Some(50.0));
    assert_eq!(updated[0].monthly_used_usd, Some(0.0));
    assert_eq!(updated[0].quota_reset_day, Some(30));
    assert_eq!(updated[0].provider_priority, 100);
    assert_eq!(
        updated[0].quota_last_reset_at_unix_secs,
        Some(1_772_323_200)
    );
    assert_eq!(updated[0].quota_expires_at_unix_secs, Some(1_775_001_600));
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

async fn gateway_preserves_admin_provider_strategy_monthly_usage_when_epoch_is_unchanged_impl() {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/providers/provider-openai/billing",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            sample_provider("provider-openai", "openai", 7).with_billing_fields(
                Some("monthly_quota".to_string()),
                Some(50.0),
                Some(12.5),
                Some(30),
                Some(1_772_323_200),
                None,
            ),
        ],
        vec![],
        vec![],
    ));
    let usage_repository = Arc::new(
        InMemoryUsageReadRepository::default().with_provider_usage_windows(vec![
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_772_236_799,
                8,
                7,
                1,
                100.0,
                9.99,
            )
            .expect("window should build"),
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_772_323_200,
                5,
                5,
                0,
                110.0,
                1.25,
            )
            .expect("window should build"),
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_772_409_600,
                6,
                5,
                1,
                150.0,
                0.75,
            )
            .expect("window should build"),
        ]),
    );

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_and_usage_reader_for_tests(
                    Arc::clone(&provider_catalog_repository),
                    Arc::clone(&usage_repository),
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .put(format!(
            "{gateway_url}/api/admin/provider-strategy/providers/provider-openai/billing"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .json(&json!({
            "billing_type": "monthly_quota",
            "monthly_quota_usd": 50.0,
            "quota_last_reset_at": "2026-03-01T00:00:00Z",
            "quota_expires_at": "2026-04-01T00:00:00Z"
        }))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    let updated = provider_catalog_repository
        .list_providers_by_ids(&["provider-openai".to_string()])
        .await
        .expect("provider query should succeed");
    assert_eq!(updated[0].monthly_used_usd, Some(12.5));
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

async fn gateway_reads_admin_provider_strategy_stats_locally_with_trusted_admin_principal_impl() {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/providers/provider-openai/stats",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            sample_provider("provider-openai", "openai", 7).with_billing_fields(
                Some("monthly_quota".to_string()),
                Some(80.0),
                Some(12.5),
                Some(30),
                Some(1_711_000_000),
                Some(1_774_966_400),
            ),
        ],
        vec![],
        vec![],
    ));
    let usage_repository = Arc::new(
        InMemoryUsageReadRepository::default().with_provider_usage_windows(vec![
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_700_000_000,
                10,
                9,
                1,
                120.0,
                1.25,
            )
            .expect("window should build"),
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_700_003_600,
                6,
                5,
                1,
                180.0,
                0.75,
            )
            .expect("window should build"),
        ]),
    );

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_and_usage_reader_for_tests(
                    Arc::clone(&provider_catalog_repository),
                    Arc::clone(&usage_repository),
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/api/admin/provider-strategy/providers/provider-openai/stats?hours=999999"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["provider_id"], "provider-openai");
    assert_eq!(payload["provider_name"], "openai");
    assert_eq!(payload["billing_info"]["billing_type"], "monthly_quota");
    assert_eq!(payload["billing_info"]["monthly_quota_usd"], 80.0);
    assert_eq!(payload["billing_info"]["monthly_used_usd"], 12.5);
    assert_eq!(payload["billing_info"]["quota_remaining_usd"], 67.5);
    assert_eq!(payload["usage_stats"]["total_requests"], 16);
    assert_eq!(payload["usage_stats"]["successful_requests"], 14);
    assert_eq!(payload["usage_stats"]["failed_requests"], 2);
    assert_eq!(payload["usage_stats"]["success_rate"], 0.875);
    assert_eq!(payload["usage_stats"]["avg_response_time_ms"], 150.0);
    assert_eq!(payload["usage_stats"]["total_cost_usd"], 2.0);
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}

async fn gateway_rounds_admin_provider_strategy_stats_total_cost_locally_impl() {
    let upstream_hits = Arc::new(Mutex::new(0usize));
    let upstream_hits_clone = Arc::clone(&upstream_hits);
    let upstream = Router::new().route(
        "/api/admin/provider-strategy/providers/provider-openai/stats",
        any(move |_request: Request| {
            let upstream_hits_inner = Arc::clone(&upstream_hits_clone);
            async move {
                *upstream_hits_inner.lock().expect("mutex should lock") += 1;
                (StatusCode::OK, Body::from("unexpected upstream hit"))
            }
        }),
    );

    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            sample_provider("provider-openai", "openai", 7).with_billing_fields(
                Some("monthly_quota".to_string()),
                Some(80.0),
                Some(12.5),
                Some(30),
                Some(1_711_000_000),
                Some(1_774_966_400),
            ),
        ],
        vec![],
        vec![],
    ));
    let usage_repository = Arc::new(
        InMemoryUsageReadRepository::default().with_provider_usage_windows(vec![
            StoredProviderUsageWindow::new(
                "provider-openai".to_string(),
                1_700_000_000,
                2,
                2,
                0,
                120.0,
                1.23456,
            )
            .expect("window should build"),
        ]),
    );

    let (upstream_url, upstream_handle) = start_server(upstream).await;
    let gateway = build_router_with_state(
        AppState::new()
            .expect("gateway should build")
            .with_data_state_for_tests(
                GatewayDataState::with_provider_catalog_and_usage_reader_for_tests(
                    Arc::clone(&provider_catalog_repository),
                    Arc::clone(&usage_repository),
                ),
            ),
    );
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{gateway_url}/api/admin/provider-strategy/providers/provider-openai/stats?hours=999999"
        ))
        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
        .header(TRUSTED_ADMIN_USER_ID_HEADER, "admin-user-123")
        .header(TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
        .header(TRUSTED_ADMIN_SESSION_ID_HEADER, "session-123")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = response.json().await.expect("json body should parse");
    assert_eq!(payload["usage_stats"]["total_cost_usd"], 1.2346);
    assert_eq!(*upstream_hits.lock().expect("mutex should lock"), 0);

    gateway_handle.abort();
    upstream_handle.abort();
}
