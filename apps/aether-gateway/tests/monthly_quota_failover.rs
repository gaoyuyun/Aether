use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_crypto::{encrypt_python_fernet_plaintext, DEVELOPMENT_ENCRYPTION_KEY};
use aether_data::driver::sqlite::{
    run_migrations, SqliteProviderQuotaRepository, SqliteUsageWriteRepository,
};
use aether_data::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
use aether_data_contracts::repository::quota::ProviderQuotaReadRepository;
use aether_data_contracts::repository::usage::UsageWriteRepository;
use aether_gateway::{build_router_with_state, AppState, GatewayDataConfig, UsageRuntimeConfig};
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_listener(router, listener)
}

fn serve_listener(router: Router, listener: tokio::net::TcpListener) -> Server {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Server { url, task }
}

fn completion(provider: &str, stream: bool) -> Response {
    let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
    if stream {
        let chunk = json!({"id": "quota-test", "object": "chat.completion.chunk",
            "model": "quota-test", "choices": [{"index": 0, "delta": {"content": provider},
                "finish_reason": null}]});
        let finish = json!({"id": "quota-test", "object": "chat.completion.chunk",
            "model": "quota-test", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": usage});
        (
            [("content-type", "text/event-stream")],
            format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        )
            .into_response()
    } else {
        Json(
            json!({"id": "quota-test", "object": "chat.completion", "model": "quota-test",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": provider},
                "finish_reason": "stop"}], "usage": usage}),
        )
        .into_response()
    }
}

async fn seed(pool: &SqlitePool, a_url: &str, b_url: &str, now: u64, a_id: &str, b_id: &str) {
    run_migrations(pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO users (id, username, role, created_at, updated_at) VALUES ('quota-user', 'quota-user', 'user', 1, 1);
        INSERT INTO global_models (id, name, default_tiered_pricing, config, created_at, updated_at)
        VALUES ('quota-model', 'quota-test', '{\"tiers\":[{\"up_to\":null,\"input_price_per_1m\":1,\"output_price_per_1m\":2}]}', '{\"streaming\":true}', 1, 1);")
        .execute(pool).await.unwrap();
    for (id, allowed) in [("both", None), ("only-a", Some(json!([a_id]).to_string()))] {
        let hash = format!("{:x}", Sha256::digest(format!("sk-quota-{id}").as_bytes()));
        sqlx::query("INSERT INTO api_keys (id, user_id, key_hash, allowed_providers, created_at, updated_at) VALUES (?, 'quota-user', ?, ?, 1, 1)")
            .bind(id).bind(hash).bind(allowed).execute(pool).await.unwrap();
    }
    for (id, url, priority, billing) in [
        (a_id, a_url, 1, "monthly_quota"),
        (b_id, b_url, 100, "pay_as_you_go"),
    ] {
        sqlx::query("INSERT INTO providers (id, name, provider_type, billing_type, monthly_quota_usd, monthly_used_usd, quota_reset_day, quota_last_reset_at, quota_subscription_started_at, quota_cycle_start_at, provider_priority, max_retries, request_timeout, stream_first_byte_timeout, config, created_at, updated_at) VALUES (?, ?, 'custom', ?, 100, 0, 7, ?, ?, ?, ?, 0, 1, 1, ?, 1, 1)")
            .bind(id).bind(id).bind(billing).bind((now / 60 * 60 - 3600) as i64)
            .bind((now / 60 * 60 - 3600) as i64).bind((now / 60 * 60 - 3600) as i64).bind(priority)
            .bind("{}")
            .execute(pool).await.unwrap();
        sqlx::query("INSERT INTO provider_endpoints (id, provider_id, name, base_url, api_format, api_family, endpoint_kind, max_retries, created_at, updated_at) VALUES (?, ?, ?, ?, 'openai:chat', 'openai', 'chat', 0, 1, 1)")
            .bind(format!("endpoint-{id}")).bind(id).bind(id).bind(format!("{url}/v1")).execute(pool).await.unwrap();
        let encrypted =
            encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "sk-local-fixture")
                .unwrap();
        sqlx::query("INSERT INTO provider_api_keys (id, provider_id, name, api_key, api_formats, global_priority_by_format, created_at, updated_at) VALUES (?, ?, ?, ?, '[\"openai:chat\"]', ?, 1, 1)")
            .bind(format!("key-{id}")).bind(id).bind(id).bind(encrypted)
            .bind(json!({"openai:chat": priority}).to_string()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO models (id, provider_id, global_model_id, provider_model_name, supports_streaming, created_at, updated_at) VALUES (?, ?, 'quota-model', 'quota-test', 1, 1, 1)")
            .bind(format!("model-{id}")).bind(id).execute(pool).await.unwrap();
    }
}

async fn run_case(stream: bool, failure: usize, fallback: bool) {
    let mode = Arc::new(AtomicUsize::new(failure));
    let a_hits = Arc::new(AtomicUsize::new(0));
    let b_hits = Arc::new(AtomicUsize::new(0));
    let a_mode = mode.clone();
    let a_counter = a_hits.clone();
    let a_router = Router::new().route("/v1/chat/completions", post(move |Json(payload): Json<Value>| {
        let mode = a_mode.clone();
        let hits = a_counter.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let streaming = payload["stream"].as_bool().unwrap_or(false);
            match mode.load(Ordering::SeqCst) {
                1 => (http::StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "injected upstream failure"}}))).into_response(),
                2 => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    completion("monthly-a", streaming)
                }
                3 => {
                    let body = async_stream::stream! {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        yield Err::<Bytes, _>(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected stream disconnect"));
                    };
                    ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
                }
                5 => {
                    let body = async_stream::stream! {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"{}"));
                    };
                    ([("content-type", if streaming { "text/event-stream" } else { "application/json" })], Body::from_stream(body)).into_response()
                }
                6 => {
                    let body = async_stream::stream! {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(
                            b"data: {\"id\":\"partial\",\"object\":\"chat.completion.chunk\",\"model\":\"quota-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial output\"},\"finish_reason\":null}]}\n\n"
                        ));
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected disconnect after business output"));
                    };
                    ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
                }
                _ => completion("monthly-a", streaming),
            }
        }
    }));
    // A bound, non-listening socket reserves the endpoint while connections are refused.
    let mut refused = if failure == 4 {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        Some(socket)
    } else {
        None
    };
    let mut a = if refused.is_some() {
        None
    } else {
        Some(serve(a_router.clone()).await)
    };
    let a_url = refused
        .as_ref()
        .map(|socket| format!("http://{}", socket.local_addr().unwrap()))
        .unwrap_or_else(|| a.as_ref().unwrap().url.clone());
    let b_counter = b_hits.clone();
    let b = serve(Router::new().route(
        "/v1/chat/completions",
        post(move |Json(payload): Json<Value>| {
            b_counter.fetch_add(1, Ordering::SeqCst);
            async move { completion("fallback-b", payload["stream"].as_bool().unwrap_or(false)) }
        }),
    ))
    .await;
    let path = std::env::temp_dir().join(format!("aether-quota-http-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let a_id = format!("monthly-a-{}", uuid::Uuid::new_v4());
    let b_id = format!("fallback-b-{}", uuid::Uuid::new_v4());
    seed(&pool, &a_url, &b.url, now, &a_id, &b_id).await;
    let state = AppState::new()
        .unwrap()
        .with_data_config_and_background_isolation(
            GatewayDataConfig::from_database_config(
                SqlDatabaseConfig::new(DatabaseDriver::Sqlite, url, SqlPoolConfig::default())
                    .unwrap(),
            )
            .with_encryption_key(DEVELOPMENT_ENCRYPTION_KEY),
            false,
        )
        .unwrap()
        .with_usage_runtime_config(UsageRuntimeConfig {
            enabled: true,
            ..UsageRuntimeConfig::default()
        })
        .unwrap();
    state.ensure_system_default_routing_group().await.unwrap();
    let gateway = serve(build_router_with_state(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let first = client.post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth(if fallback { "sk-quota-both" } else { "sk-quota-only-a" })
        .header("x-trace-id", format!("quota-{stream}-{failure}-{fallback}"))
        .json(&json!({"model": "quota-test", "messages": [{"role": "user", "content": "test"}], "stream": stream}))
        .send().await.unwrap();
    let status = first.status();
    let body = first.text().await.unwrap();
    let committed_stream_error = stream && failure == 6;
    if !fallback {
        assert!(status.is_server_error(), "{status} {body}");
    } else {
        assert!(
            status.is_success(),
            "stream={stream} failure={failure}: {status} {body}"
        );
        if committed_stream_error {
            assert!(body.contains("partial output"), "{body}");
            assert!(body.contains("\"error\""), "{body}");
        } else {
            assert!(body.contains("fallback-b"), "expected fallback: {body}");
        }
    }
    // The first key gets one same-key retry by default. A committed stream
    // cannot retry, while refused connections never reach the HTTP handler.
    let failed_attempts = if committed_stream_error { 1 } else { 2 };
    assert_eq!(
        a_hits.load(Ordering::SeqCst),
        if failure == 4 { 0 } else { failed_attempts },
        "stream={stream} failure={failure} fallback={fallback}"
    );
    let expected_b_hits = usize::from(fallback && !committed_stream_error);
    assert_eq!(b_hits.load(Ordering::SeqCst), expected_b_hits);

    let quota = SqliteProviderQuotaRepository::new(pool.clone());
    for _ in 0..100 {
        let terminal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_candidates WHERE provider_id=? AND status='failed'",
        )
        .bind(&a_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        if terminal == failed_attempts as i64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let recovered = quota.find_by_provider_id(&a_id).await.unwrap().unwrap();
    assert!(
        recovered.is_active,
        "failed attempt left monthly provider blocked"
    );
    assert_eq!(recovered.monthly_used_usd, 0.0);
    let failed = sqlx::query("SELECT c.status, d.quota_accounting_status, r.state AS reservation_state, r.reserved_cost_units FROM request_candidates c JOIN usage_counter_deltas d ON d.request_id=c.id JOIN provider_quota_reservations r ON r.candidate_id=c.id WHERE c.provider_id=? AND d.kind='provider_monthly'")
        .bind(&a_id).fetch_all(&pool).await.unwrap();
    assert_eq!(failed.len(), failed_attempts);
    for attempt in failed {
        assert_eq!(attempt.get::<String, _>("status"), "failed");
        let accounting = attempt.get::<String, _>("quota_accounting_status");
        if accounting == "ready" {
            assert_eq!(attempt.get::<String, _>("reservation_state"), "settled");
        } else {
            assert!(matches!(accounting.as_str(), "pending" | "failed"));
            assert_eq!(attempt.get::<String, _>("reservation_state"), "uncertain");
            assert!(
                attempt.get::<i64, _>("reserved_cost_units") > 0,
                "unmeasurable usage must retain only that attempt's estimate"
            );
        }
    }

    // Restrict a new request to A so sticky affinity to the successful fallback cannot hide A.
    mode.store(0, Ordering::SeqCst);
    if let Some(socket) = refused.take() {
        a = Some(serve_listener(a_router, socket.listen(128).unwrap()));
    }
    let hits_before = a_hits.load(Ordering::SeqCst);
    let next = client.post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("sk-quota-only-a")
        .json(&json!({"model": "quota-test", "messages": [{"role": "user", "content": "retry"}], "stream": stream}))
        .send().await.unwrap();
    let status = next.status();
    let body = next.text().await.unwrap();
    assert!(
        status.is_success(),
        "A must be callable again: {status} {body}"
    );
    assert!(body.contains("monthly-a"));
    assert_eq!(a_hits.load(Ordering::SeqCst), hits_before + 1);
    assert_eq!(b_hits.load(Ordering::SeqCst), expected_b_hits);
    for _ in 0..100 {
        let finalized: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage WHERE finalized_at IS NOT NULL AND billing_status IN ('settled', 'void')")
                .fetch_one(&pool)
                .await
                .unwrap();
        let successful_candidates: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_candidates WHERE status='success'")
                .fetch_one(&pool)
                .await
                .unwrap();
        if finalized == 2 && successful_candidates == 1 + expected_b_hits as i64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let successes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_candidates WHERE status='success'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(successes, 1 + expected_b_hits as i64);
    let usage = sqlx::query("SELECT provider_id, status, input_tokens, output_tokens, total_cost_usd, billing_status FROM usage ORDER BY created_at_unix_ms")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(usage.len(), 2);
    for (index, row) in usage.iter().enumerate() {
        let completed = index == 1 || expected_b_hits == 1;
        assert_eq!(
            row.get::<String, _>("provider_id"),
            if index == 0 && expected_b_hits == 1 {
                &b_id
            } else {
                &a_id
            }
            .as_str()
        );
        assert_eq!(
            row.get::<String, _>("status"),
            if completed { "completed" } else { "failed" }
        );
        assert_eq!(
            row.get::<String, _>("billing_status"),
            if completed { "settled" } else { "void" }
        );
        assert_eq!(
            row.get::<i64, _>("input_tokens"),
            if completed { 10 } else { 0 }
        );
        assert_eq!(
            row.get::<i64, _>("output_tokens"),
            if completed { 5 } else { 0 }
        );
        assert!(
            (row.get::<f64, _>("total_cost_usd") - if completed { 0.00002 } else { 0.0 }).abs()
                < 1e-12
        );
    }
    let unresolved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_counter_deltas d WHERE d.kind='provider_monthly' AND d.quota_accounting_status != 'ready' AND NOT EXISTS (SELECT 1 FROM provider_quota_reservations r WHERE r.candidate_id=d.request_id AND r.state='uncertain')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(unresolved, 0);
    let writer = SqliteUsageWriteRepository::new(pool.clone());
    for _ in 0..2 {
        writer.flush_usage_counter_deltas(1000).await.unwrap();
        let used = quota
            .find_by_provider_id(&a_id)
            .await
            .unwrap()
            .unwrap()
            .monthly_used_usd;
        assert!(
            (used - 0.00002).abs() < 1e-12,
            "monthly consumption must be applied exactly once: {used}"
        );
    }
    println!("validated HTTP recovery and billing: stream={stream}, failure={failure}, fallback={fallback}");
    drop(gateway);
    drop(a);
    drop(client);
    pool.close().await;
    std::fs::remove_file(path).unwrap();
}

#[test]
fn monthly_quota_recovers_after_http_timeout_and_stream_failover() {
    // This dedicated test binary must exercise A before B, including streaming requests.
    std::env::set_var(
        "AETHER_GATEWAY_OPENAI_CHAT_STREAM_TARGET_SELECT_WINDOW",
        "1",
    );
    let handle = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    // 1: HTTP 500, 2: response headers timeout, 3: body disconnect,
                    // 4: connection refused, 5: response body timeout,
                    // 6: disconnect after business output has committed the stream.
                    for (stream, failure, fallback) in [
                        (false, 1, true),
                        (false, 2, true),
                        (false, 4, true),
                        (false, 5, true),
                        (true, 1, true),
                        (true, 2, true),
                        (true, 3, true),
                        (true, 4, true),
                        (true, 5, true),
                        (true, 6, true),
                        (false, 1, false),
                        (true, 1, false),
                        (true, 3, false),
                    ] {
                        run_case(stream, failure, fallback).await;
                    }
                });
        })
        .unwrap();
    if let Err(error) = handle.join() {
        std::panic::resume_unwind(error);
    }
}

async fn concurrent_reservation_case(stream: bool) {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let arrivals = Arc::new(AtomicUsize::new(0));
    let search_hits = Arc::new(AtomicUsize::new(0));
    let permits = release.clone();
    let hits = arrivals.clone();
    let searches = search_hits.clone();
    let a = serve(
        Router::new()
            .route(
                "/v1/chat/completions",
                post(move |Json(payload): Json<Value>| {
                    let permits = permits.clone();
                    let hits = hits.clone();
                    async move {
                        let hit = hits.fetch_add(1, Ordering::SeqCst);
                        if hit < 2 {
                            permits.acquire().await.unwrap().forget();
                        }
                        completion("monthly-a", payload["stream"].as_bool().unwrap_or(false))
                    }
                }),
            )
            .route(
                "/v1/alpha/search",
                post(move || {
                    searches.fetch_add(1, Ordering::SeqCst);
                    async { Json(json!({"output":"local search result","encrypted_output":"local-search"})) }
                }),
            ),
    )
    .await;
    let b = serve(Router::new().route(
        "/v1/chat/completions",
        post(|Json(payload): Json<Value>| async move {
            completion("fallback-b", payload["stream"].as_bool().unwrap_or(false))
        }),
    ))
    .await;
    let path = std::env::temp_dir().join(format!(
        "aether-quota-concurrent-{}.db",
        uuid::Uuid::new_v4()
    ));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let a_id = "monthly-concurrent-a";
    let b_id = "fallback-concurrent-b";
    seed(&pool, &a.url, &b.url, now, a_id, b_id).await;
    sqlx::query("UPDATE providers SET monthly_quota_usd=0.5,request_timeout=30,stream_first_byte_timeout=30,config=? WHERE id=?")
        .bind(json!({"quota_reservation":{"minimum_usd":0.25,"output_tokens":16,"safety_multiplier":1.0}}).to_string())
        .bind(a_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO provider_endpoints (id,provider_id,name,base_url,api_format,api_family,endpoint_kind,max_retries,created_at,updated_at) VALUES ('search-endpoint',?,'search',?,'openai:search','openai','search',0,1,1)")
        .bind(a_id).bind(format!("{}/v1",a.url)).execute(&pool).await.unwrap();
    sqlx::query("UPDATE provider_api_keys SET api_formats='[\"openai:chat\",\"openai:search\"]',global_priority_by_format='{\"openai:chat\":1,\"openai:search\":1}' WHERE provider_id=?")
        .bind(a_id).execute(&pool).await.unwrap();
    let state = AppState::new()
        .unwrap()
        .with_data_config_and_background_isolation(
            GatewayDataConfig::from_database_config(
                SqlDatabaseConfig::new(DatabaseDriver::Sqlite, url, SqlPoolConfig::default())
                    .unwrap(),
            )
            .with_encryption_key(DEVELOPMENT_ENCRYPTION_KEY),
            false,
        )
        .unwrap()
        .with_usage_runtime_config(UsageRuntimeConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
    state.ensure_system_default_routing_group().await.unwrap();
    let gateway = serve(build_router_with_state(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let body = json!({"model":"quota-test","messages":[{"role":"user","content":"concurrent probe"}],"stream":stream});
    let mut running = Vec::new();
    for index in 0..2 {
        let client = client.clone();
        let url = gateway.url.clone();
        let body = body.clone();
        running.push(tokio::spawn(async move {
            let response = client
                .post(format!("{url}/v1/chat/completions"))
                .bearer_auth("sk-quota-only-a")
                .header("x-trace-id", format!("concurrent-{stream}-{index}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            assert!(response.text().await.unwrap().contains("monthly-a"));
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while arrivals.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both requests must reach the monthly upstream before either completes");
    let failover_id = format!("concurrent-failover-{stream}");
    let start = std::time::Instant::now();
    let fallback = client
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("sk-quota-both")
        .header("x-trace-id", &failover_id)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(fallback.status().is_success());
    assert!(fallback.text().await.unwrap().contains("fallback-b"));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "quota rejection must transfer within the same HTTP request"
    );
    let denied_id = format!("concurrent-denied-{stream}");
    let denied = client
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("sk-quota-only-a")
        .header("x-trace-id", &denied_id)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let search=client.post(format!("{}/v1/alpha/search",gateway.url)).bearer_auth("sk-quota-only-a")
        .json(&json!({"id":"quota-search","model":"quota-test","commands":{"search_query":[{"q":"test"}]}})).send().await.unwrap();
    let status = search.status();
    let text = search.text().await.unwrap();
    assert!(
        status.is_success(),
        "free Search must work with a fully reserved provider: {status} {text}"
    );
    assert_eq!(search_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        arrivals.load(Ordering::SeqCst),
        2,
        "insufficient reservations must never reach the upstream"
    );
    release.add_permits(2);
    for task in running {
        task.await.unwrap();
    }
    // HTTP delivery can finish before the background terminal writer. Wait for
    // durable settlement, not provider.is_active (which now remains true in flight).
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reserved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_quota_reservations WHERE provider_id=? AND state='reserved' AND reserved_cost_units>0")
                .bind(a_id).fetch_one(&pool).await.unwrap();
            let active_candidates: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_candidates WHERE status IN ('pending','streaming')")
                .fetch_one(&pool).await.unwrap();
            if reserved == 0 && active_candidates == 0 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("successful requests must settle their reservations promptly");
    for _ in 0..100 {
        let pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM usage WHERE request_id IN (?,?) AND status NOT IN ('pending','streaming')")
            .bind(&failover_id).bind(&denied_id).fetch_one(&pool).await.unwrap();
        if pending == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for id in [&failover_id, &denied_id] {
        let row=sqlx::query("SELECT COUNT(*) n,SUM(CASE WHEN status IN ('pending','streaming') THEN 1 ELSE 0 END) pending FROM usage WHERE request_id=?")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(
            row.get::<i64, _>("n"),
            1,
            "a retry/skip must remain one usage request"
        );
        assert_eq!(
            row.get::<i64, _>("pending"),
            0,
            "no orphan usage awaiting the ten-minute cleaner"
        );
    }
    let rows=sqlx::query("SELECT provider_id,retry_index,status FROM request_candidates WHERE request_id=? ORDER BY candidate_index,retry_index")
        .bind(&failover_id).fetch_all(&pool).await.unwrap();
    assert_eq!(
        rows.iter()
            .filter(|r| r.get::<String, _>("provider_id") == a_id)
            .count(),
        1,
        "skip exhausted provider without same-key retries"
    );
    assert!(rows
        .iter()
        .any(|r| r.get::<String, _>("provider_id") == b_id
            && r.get::<String, _>("status") == "success"));
    let next = client
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("sk-quota-only-a")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(next.status().is_success());
    assert!(next.text().await.unwrap().contains("monthly-a"));
    drop(gateway);
    drop(pool);
    let _ = std::fs::remove_file(path);
}

fn run_concurrent_reservation_case(stream: bool) {
    std::env::set_var(
        "AETHER_GATEWAY_OPENAI_CHAT_STREAM_TARGET_SELECT_WINDOW",
        "1",
    );
    let thread = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(concurrent_reservation_case(stream));
        })
        .unwrap();
    if let Err(error) = thread.join() {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn monthly_quota_allows_concurrent_sync_and_free_search() {
    run_concurrent_reservation_case(false);
}

#[test]
fn monthly_quota_allows_concurrent_stream_and_free_search() {
    run_concurrent_reservation_case(true);
}
