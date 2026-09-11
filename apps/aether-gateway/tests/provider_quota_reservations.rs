//! The same concurrent admission/accounting scenarios run on all three SQL drivers.
use aether_data::backend::DataBackends;
use aether_data::lifecycle::migrate::{
    run_migrations, run_mysql_migrations, run_sqlite_migrations,
};
use aether_data::{DataLayerConfig, DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
use aether_data_contracts::repository::candidates::{
    ProviderQuotaDispatchSnapshot, RequestCandidateStatus, UpsertRequestCandidateRecord,
};
use aether_data_contracts::DataLayerError;
use serde_json::json;

struct Fixture {
    data: DataBackends,
    driver: DatabaseDriver,
    provider: String,
    epoch: u64,
    now: u64,
}

impl Fixture {
    async fn new(driver: DatabaseDriver, url: String) -> Self {
        let data = DataBackends::from_config(DataLayerConfig::from_database(SqlDatabaseConfig {
            driver,
            url,
            pool: SqlPoolConfig {
                min_connections: 1,
                max_connections: 8,
                ..Default::default()
            },
        }))
        .unwrap();
        match driver {
            DatabaseDriver::Postgres => run_migrations(data.postgres().unwrap().pool())
                .await
                .unwrap(),
            DatabaseDriver::Mysql => run_mysql_migrations(data.mysql().unwrap().pool())
                .await
                .unwrap(),
            DatabaseDriver::Sqlite => run_sqlite_migrations(data.sqlite().unwrap().pool())
                .await
                .unwrap(),
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let epoch = now / 60 * 60 - 3600;
        let fixture = Self {
            data,
            driver,
            provider: uuid::Uuid::new_v4().to_string(),
            epoch,
            now,
        };
        fixture.exec(&format!("INSERT INTO providers (id,name,provider_type,billing_type,monthly_quota_usd,monthly_used_usd,quota_reset_day,quota_last_reset_at,quota_subscription_started_at,quota_cycle_start_at,is_active,created_at,updated_at) VALUES ('{id}','{id}','custom','monthly_quota',1,0,7,{epoch_db},{epoch},{epoch},{active},{clock},{clock})",
            id=fixture.provider, epoch_db=fixture.timestamp(epoch), active=if driver==DatabaseDriver::Postgres {"true"} else {"1"}, clock=fixture.timestamp(now))).await;
        fixture
    }

    fn timestamp(&self, seconds: u64) -> String {
        if self.driver == DatabaseDriver::Postgres {
            format!("TO_TIMESTAMP({seconds})")
        } else {
            seconds.to_string()
        }
    }

    async fn exec(&self, sql: &str) {
        match self.driver {
            DatabaseDriver::Postgres => {
                sqlx::raw_sql(sql)
                    .execute(self.data.postgres().unwrap().pool())
                    .await
                    .unwrap();
            }
            DatabaseDriver::Mysql => {
                sqlx::raw_sql(sql)
                    .execute(self.data.mysql().unwrap().pool())
                    .await
                    .unwrap();
            }
            DatabaseDriver::Sqlite => {
                sqlx::raw_sql(sql)
                    .execute(self.data.sqlite().unwrap().pool())
                    .await
                    .unwrap();
            }
        }
    }

    async fn number(&self, sql: &str) -> i64 {
        match self.driver {
            DatabaseDriver::Postgres => sqlx::query_scalar(sql)
                .fetch_one(self.data.postgres().unwrap().pool())
                .await
                .unwrap(),
            DatabaseDriver::Mysql => sqlx::query_scalar(sql)
                .fetch_one(self.data.mysql().unwrap().pool())
                .await
                .unwrap(),
            DatabaseDriver::Sqlite => sqlx::query_scalar(sql)
                .fetch_one(self.data.sqlite().unwrap().pool())
                .await
                .unwrap(),
        }
    }

    fn attempt(&self, estimate: f64) -> UpsertRequestCandidateRecord {
        let id = uuid::Uuid::new_v4().to_string();
        UpsertRequestCandidateRecord {
            id: id.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            user_id: None,
            api_key_id: None,
            username: None,
            api_key_name: None,
            candidate_index: 0,
            retry_index: 0,
            provider_id: Some(self.provider.clone()),
            endpoint_id: None,
            key_id: None,
            status: RequestCandidateStatus::Pending,
            skip_reason: None,
            is_cached: Some(false),
            status_code: None,
            error_type: None,
            error_message: None,
            latency_ms: None,
            concurrent_requests: None,
            required_capabilities: None,
            created_at_unix_ms: Some(self.now * 1000),
            started_at_unix_ms: Some(self.now * 1000),
            finished_at_unix_ms: None,
            extra_data: Some(
                json!({"provider_quota_dispatch_snapshot": ProviderQuotaDispatchSnapshot {
                    schema_version: 1, provider_billing_type_at_usage: "monthly_quota".into(),
                    quota_epoch_start_at_usage: Some(self.epoch), provider_dispatch_at_unix_secs: self.now,
                    pricing_rule_version_at_usage: Some("reservation-test".into()), provider_pricing_snapshot_at_usage: None,
                    provider_quota_cost_usd: None, reserved_cost_usd: Some(estimate), quota_accounting_status: "pending".into(),
                }}),
            ),
        }
    }

    async fn admit(&self, attempt: &UpsertRequestCandidateRecord) -> Result<(), DataLayerError> {
        self.data
            .write()
            .request_candidates()
            .unwrap()
            .upsert(attempt.clone())
            .await
            .map(|_| ())
    }

    async fn reject(&self, estimate: f64) {
        let attempt = self.attempt(estimate);
        assert!(matches!(
            self.admit(&attempt).await,
            Err(DataLayerError::ProviderQuotaUnavailable { .. })
        ));
        assert_eq!(
            self.number(&format!(
                "SELECT COUNT(*) FROM request_candidates WHERE id='{}'",
                attempt.id
            ))
            .await,
            0,
            "rejected candidate must roll back atomically"
        );
        assert_eq!(
            self.number(&format!(
                "SELECT COUNT(*) FROM provider_quota_reservations WHERE candidate_id='{}'",
                attempt.id
            ))
            .await,
            0
        );
    }

    async fn settle(&self, attempt: &UpsertRequestCandidateRecord, actual: f64) {
        // Simulate the durable actual-cost outbox before the asynchronous flusher.
        self.exec(&format!("UPDATE usage_counter_deltas SET provider_quota_cost_usd={actual},total_cost_usd_delta={actual},quota_accounting_status='ready' WHERE request_id='{}' AND kind='provider_monthly'", attempt.id)).await;
        let mut terminal = attempt.clone();
        terminal.status = RequestCandidateStatus::Success;
        terminal.extra_data = None;
        terminal.finished_at_unix_ms = Some(self.now * 1000 + 1);
        self.admit(&terminal).await.unwrap();
        assert_eq!(self.number(&format!("SELECT COUNT(*) FROM provider_quota_reservations WHERE candidate_id='{}' AND state='settled'", attempt.id)).await, 1);
    }

    async fn flush(&self) {
        self.data
            .write()
            .usage()
            .unwrap()
            .flush_usage_counter_deltas(1000)
            .await
            .unwrap();
    }

    async fn windows(&self, limit: f64) {
        self.exec(&format!("UPDATE providers SET monthly_quota_usd=100, config='{{\"quota_windows\":[{{\"duration_secs\":10800,\"limit_usd\":{limit}}}]}}' WHERE id='{}'", self.provider)).await;
        self.exec(&format!("INSERT INTO provider_quota_window_counters (provider_id,duration_secs,quota_epoch_start,rolling_start,accounted_until,used_usd,status,updated_at) VALUES ('{}',10800,{},{},{},0,'ready',{})", self.provider,self.timestamp(self.epoch),self.timestamp(self.epoch),self.timestamp(self.now/60*60),self.timestamp(self.now))).await;
    }
}

async fn cycle_case(driver: DatabaseDriver, url: String) {
    let f = Fixture::new(driver, url).await;
    let attempts = (0..12).map(|_| f.attempt(0.25)).collect::<Vec<_>>();
    let results =
        futures_util::future::join_all(attempts.iter().map(|attempt| f.admit(attempt))).await;
    let accepted = attempts
        .iter()
        .zip(&results)
        .filter_map(|(a, r)| r.is_ok().then_some(a))
        .collect::<Vec<_>>();
    assert_eq!(
        accepted.len(),
        4,
        "only four concurrent reservations fit: {results:?}"
    );
    assert!(results
        .iter()
        .all(|r| r.is_ok() || matches!(r, Err(DataLayerError::ProviderQuotaUnavailable { .. }))));
    assert!(
        f.data
            .read()
            .provider_quotas()
            .unwrap()
            .find_by_provider_id(&f.provider)
            .await
            .unwrap()
            .unwrap()
            .is_active,
        "normal in-flight attempts must not disable the provider"
    );
    f.admit(accepted[0]).await.unwrap(); // Idempotent dispatch does not occupy twice.
    let mut changed = accepted[0].clone();
    changed.extra_data.as_mut().unwrap()["provider_quota_dispatch_snapshot"]["reserved_cost_usd"] =
        json!(99.0);
    changed.extra_data.as_mut().unwrap()["provider_quota_dispatch_snapshot"]
        ["quota_epoch_start_at_usage"] = json!(f.epoch + 60);
    f.admit(&changed).await.unwrap(); // A retry cannot replace the original dispatch snapshot.
    f.reject(0.01).await;
    f.settle(accepted[0], 0.05).await;
    f.reject(0.21).await;
    let replacement = f.attempt(0.20);
    f.admit(&replacement).await.unwrap();
    f.admit(accepted[0]).await.unwrap(); // Late Pending cannot reopen a completed reservation.
    f.reject(0.01).await;
    f.flush().await;
    f.reject(0.01).await; // Hand-off to the aggregate must not lose or double-count actual cost.
    f.settle(&replacement, 0.01).await;
    f.admit(&f.attempt(0.19)).await.unwrap();
}

async fn rolling_case(driver: DatabaseDriver, url: String) {
    let mut f = Fixture::new(driver, url).await;
    f.windows(0.5).await;
    let a = f.attempt(0.25);
    let b = f.attempt(0.25);
    f.admit(&a).await.unwrap();
    f.admit(&b).await.unwrap();
    f.reject(0.01).await;
    let mut waited = f.attempt(0.01);
    waited.extra_data.as_mut().unwrap()["provider_quota_dispatch_snapshot"]
        ["provider_dispatch_at_unix_secs"] = json!(f.now - 60);
    waited.started_at_unix_ms = Some((f.now - 60) * 1000);
    assert!(
        matches!(
            f.admit(&waited).await,
            Err(DataLayerError::ProviderQuotaUnavailable { .. })
        ),
        "a dispatch prepared in the previous minute must see current-minute reservations"
    );
    f.settle(&a, 0.10).await;
    f.reject(0.16).await;
    let c = f.attempt(0.15);
    f.admit(&c).await.unwrap();
    f.reject(0.01).await;
    f.flush().await;
    f.reject(0.01).await; // Current-minute bucket is not yet in the rolling aggregate.
    f.now += 60;
    f.reject(0.01).await; // An overdue minute worker is handled without a quota gap.
    f.now += 10800;
    f.admit(&f.attempt(0.5)).await.unwrap(); // Natural expiry, same epoch.
}

async fn recovery_and_free_case(driver: DatabaseDriver, url: String) {
    let f = Fixture::new(driver, url).await;
    let a = f.attempt(0.25);
    f.admit(&a).await.unwrap();
    f.data
        .write()
        .provider_quotas()
        .unwrap()
        .recover_attempts(Some(&f.provider), 100, f.now + 900, false)
        .await
        .unwrap();
    assert_eq!(f.number(&format!("SELECT COUNT(*) FROM provider_quota_reservations WHERE candidate_id='{}' AND state='reserved'",a.id)).await,1,"elapsed time cannot release an active request");
    let mut failed = a.clone();
    failed.status = RequestCandidateStatus::Failed;
    failed.extra_data = None;
    failed.finished_at_unix_ms = Some(f.now * 1000 + 1);
    f.admit(&failed).await.unwrap();
    assert_eq!(f.number(&format!("SELECT COUNT(*) FROM provider_quota_reservations WHERE candidate_id='{}' AND state='uncertain'",a.id)).await,1);
    f.admit(&f.attempt(0.75)).await.unwrap();
    f.reject(0.01).await;
    f.data
        .write()
        .provider_quotas()
        .unwrap()
        .recover_attempts(Some(&f.provider), 100, f.now + 900, false)
        .await
        .unwrap();
    f.reject(0.01).await;
    // Free requests are admitted even with a fully occupied budget.
    f.admit(&f.attempt(0.0)).await.unwrap();
    // Late measured usage replaces the uncertain estimate and releases the difference.
    f.settle(&a, 0.05).await;
    // An explicit upstream rejection releases its estimate; unknown consumption above does not.
    let mut rejected = f.attempt(0.20);
    f.admit(&rejected).await.unwrap();
    rejected.status = RequestCandidateStatus::Failed;
    rejected.status_code = Some(429);
    rejected.extra_data = None;
    f.admit(&rejected).await.unwrap();
    f.admit(&f.attempt(0.20)).await.unwrap();
    f.reject(0.01).await;
    // Old-cycle in-flight reservations must not consume the new period.
    let mut next = f;
    next.epoch = next.now / 60 * 60;
    next.now += 1;
    next.exec(&format!("UPDATE providers SET quota_last_reset_at={},quota_cycle_start_at={},monthly_used_usd=0 WHERE id='{}'", next.timestamp(next.epoch),next.epoch,next.provider)).await;
    next.admit(&next.attempt(1.0)).await.unwrap();
    next.exec(&format!(
        "UPDATE providers SET quota_expires_at={} WHERE id='{}'",
        next.timestamp(next.now - 1),
        next.provider
    ))
    .await;
    next.reject(0.0).await; // Zero-cost does not bypass subscription expiry.
}

async fn run(driver: DatabaseDriver, url: String) {
    cycle_case(driver, url.clone()).await;
    rolling_case(driver, url.clone()).await;
    recovery_and_free_case(driver, url).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_provider_quota_reservations() {
    let path =
        std::env::temp_dir().join(format!("aether-reservations-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    run(DatabaseDriver::Sqlite, url).await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable AETHER_TEST_POSTGRES_URL database"]
async fn postgres_provider_quota_reservations() {
    run(
        DatabaseDriver::Postgres,
        std::env::var("AETHER_TEST_POSTGRES_URL").unwrap(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable AETHER_TEST_MYSQL_URL database"]
async fn mysql_provider_quota_reservations() {
    run(
        DatabaseDriver::Mysql,
        std::env::var("AETHER_TEST_MYSQL_URL").unwrap(),
    )
    .await;
}
