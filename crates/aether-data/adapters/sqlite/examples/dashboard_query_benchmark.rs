//! Dashboard query benchmark against a real database snapshot.
//!
//! Reproduces the SQLite performance fix plan's acceptance benchmark
//! (`docs/operations/sqlite-performance-fix-plan.md`):
//!
//!   AETHER_DASHBOARD_BENCHMARK_DATABASE_URL=sqlite:///path/to/aether.db \
//!     cargo run -p aether-data-sqlite --example dashboard_query_benchmark -- \
//!     --days 52 --tz 480
//!
//! The benchmark temporarily empties `stats_hourly_model_provider` to measure
//! the pre-fix behavior, then restores it (from the backup table it creates)
//! and measures the fast path. Numeric equality between the two runs is
//! asserted before exiting.
use aether_data_contracts::repository::usage::{
    UsageDashboardDailyBreakdownQuery, UsageDashboardSummaryQuery, UsageReadRepository,
};
use aether_data_sqlite::SqliteUsageReadRepository;

fn parse_args() -> (u64, i32) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|v| v == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let days: u64 = value("--days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(52);
    let tz: i32 = value("--tz").and_then(|v| v.parse().ok()).unwrap_or(480);
    (days, tz)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (days, tz) = parse_args();
    let url = std::env::var("AETHER_DASHBOARD_BENCHMARK_DATABASE_URL")?;
    let pool = sqlx::SqlitePool::connect(&url).await?;
    let reader = SqliteUsageReadRepository::new(pool.clone());

    // Window: last N days of data present in the snapshot.
    let max_secs: i64 =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(created_at_unix_ms) FROM \"usage\"")
            .fetch_one(&pool)
            .await?
            .unwrap_or(0);
    let until = max_secs as u64 / 86_400 * 86_400;
    let from = until.saturating_sub(days * 86_400);
    eprintln!("window: {from}..{until} ({days} days), tz_offset_minutes = {tz}");

    let breakdown_query = UsageDashboardDailyBreakdownQuery {
        created_from_unix_secs: from,
        created_until_unix_secs: until,
        tz_offset_minutes: tz,
        user_id: None,
    };
    let summary_query = UsageDashboardSummaryQuery {
        created_from_unix_secs: from,
        created_until_unix_secs: until,
        user_id: None,
    };

    // Backup the hourly model-provider rows, then empty the live table to
    // force the pre-fix code path (raw merge) for the "before" measurement.
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS stats_hourly_model_provider_benchmark_backup; \
         CREATE TABLE stats_hourly_model_provider_benchmark_backup AS \
           SELECT * FROM stats_hourly_model_provider; \
         DELETE FROM stats_hourly_model_provider;",
    )
    .execute(&pool)
    .await?;

    // Warm the OS page cache with one throwaway run, then measure.
    let _ = reader.list_dashboard_daily_breakdown(&breakdown_query).await?;
    let _ = reader.summarize_dashboard_usage(&summary_query).await?;

    let t0 = std::time::Instant::now();
    let pre_rows = reader.list_dashboard_daily_breakdown(&breakdown_query).await?;
    let pre_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Restore the hourly rows and re-measure through the fast path.
    sqlx::raw_sql("INSERT INTO stats_hourly_model_provider \
                    SELECT * FROM stats_hourly_model_provider_benchmark_backup; \
                  DROP TABLE stats_hourly_model_provider_benchmark_backup;")
        .execute(&pool)
        .await?;

    let t0 = std::time::Instant::now();
    let fast_rows = reader.list_dashboard_daily_breakdown(&breakdown_query).await?;
    let fast_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let close = |left: f64, right: f64| {
        (left - right).abs() <= 1e-9_f64.max(left.abs().max(right.abs()) * 1e-12)
    };
    let equal = pre_rows.len() == fast_rows.len()
        && pre_rows.iter().zip(&fast_rows).all(|(before, after)| {
            before.date == after.date
                && before.model == after.model
                && before.provider == after.provider
                && before.requests == after.requests
                && before.total_tokens == after.total_tokens
                && before.response_time_samples == after.response_time_samples
                && close(before.total_cost_usd, after.total_cost_usd)
                && close(before.response_time_sum_ms, after.response_time_sum_ms)
        });

    let t0 = std::time::Instant::now();
    let summary = reader.summarize_dashboard_usage(&summary_query).await?;
    let summary_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "{:<44} {:>12} {:>10}",
        "query", "latency(ms)", "rows/gate"
    );
    println!(
        "{:<44} {:>12.1} {:>10}",
        "daily-stats BEFORE (raw merge, tz!=0)",
        pre_ms,
        pre_rows.len()
    );
    println!(
        "{:<44} {:>12.1} {:>10}",
        "daily-stats AFTER (hourly fast path)",
        fast_ms,
        fast_rows.len()
    );
    println!(
        "{:<44} {:>12.1} {:>10}",
        "speedup", pre_ms / fast_ms, "x"
    );
    println!(
        "{:<44} {:>12} {:>10}",
        "fast path matches raw rows",
        "-",
        if equal { "PASS" } else { "FAIL" }
    );
    println!(
        "{:<44} {:>12.1} {:>10}",
        "dashboard/stats summary", summary_ms, summary.total_requests
    );

    if !equal {
        eprintln!("FAIL: fast path rows differ from pre-fix rows");
        std::process::exit(1);
    }
    Ok(())
}
