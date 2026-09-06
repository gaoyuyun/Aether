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
    let committed_stream_error = stream && matches!(failure, 3 | 5);
    if !fallback {
        assert!(status.is_server_error(), "{status} {body}");
    } else {
        assert!(
            status.is_success(),
            "stream={stream} failure={failure}: {status} {body}"
        );
        if committed_stream_error {
            assert!(body.contains("\"error\""), "{body}");
        } else {
            assert!(body.contains("fallback-b"), "expected fallback: {body}");
        }
    }
    assert_eq!(a_hits.load(Ordering::SeqCst), usize::from(failure != 4));
    let expected_b_hits = usize::from(fallback && !committed_stream_error);
    assert_eq!(b_hits.load(Ordering::SeqCst), expected_b_hits);

    let quota = SqliteProviderQuotaRepository::new(pool.clone());
    for _ in 0..100 {
        if quota
            .find_by_provider_id(&a_id)
            .await
            .unwrap()
            .unwrap()
            .is_active
        {
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
    let failed = sqlx::query("SELECT c.status, d.quota_accounting_status FROM request_candidates c JOIN usage_counter_deltas d ON d.request_id=c.id WHERE c.provider_id=? AND d.kind='provider_monthly'")
        .bind(&a_id).fetch_all(&pool).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].get::<String, _>("status"), "failed");
    assert_eq!(
        failed[0].get::<String, _>("quota_accounting_status"),
        "ready"
    );

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
            sqlx::query_scalar("SELECT COUNT(*) FROM usage WHERE finalized_at IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        if finalized == 2 {
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
    let unresolved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_counter_deltas WHERE kind='provider_monthly' AND quota_accounting_status != 'ready'")
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
                    // 4: connection refused, 5: response body timeout.
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
                        (false, 1, false),
                        (true, 1, false),
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
