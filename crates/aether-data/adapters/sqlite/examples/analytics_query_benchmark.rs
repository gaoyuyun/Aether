//! Compare the analytics repository against the same local SQLite snapshot.
//!
//! Run this example before and after an optimization, using separate copies of
//! the snapshot when migrations change it. Connections are read-only. Reports
//! contain usage identifiers, so keep them outside the repository.
//!
//! AETHER_ANALYTICS_BENCHMARK_DATABASE_URL=sqlite:///path/to/snapshot.db \
//!   cargo run -p aether-data-sqlite --example analytics_query_benchmark -- \
//!   /private/before.json
//! Repeat with `/private/after.json /private/before.json` to check every result.
//! Optional environment variables: AETHER_ANALYTICS_BENCHMARK_TZ (default 480),
//! AETHER_ANALYTICS_BENCHMARK_ITERATIONS (default 5).

use std::{path::PathBuf, str::FromStr, time::Instant};

use aether_data_contracts::repository::usage::{
    UsageBreakdownGroupBy, UsageBreakdownSummaryQuery, UsageCostSavingsSummaryQuery,
    UsageDailyHeatmapQuery, UsageLeaderboardGroupBy, UsageLeaderboardQuery, UsageReadRepository,
    UsageTimeSeriesGranularity, UsageTimeSeriesQuery,
};
use aether_data_sqlite::SqliteUsageReadRepository;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn assert_equivalent(expected: &Value, actual: &Value, path: &str) {
    match (expected, actual) {
        (Value::Number(left), Value::Number(right)) if left.is_f64() || right.is_f64() => {
            let left = left.as_f64().expect("numeric baseline");
            let right = right.as_f64().expect("numeric result");
            let tolerance = 1e-9_f64.max(left.abs().max(right.abs()) * 1e-12);
            assert!(
                (left - right).abs() <= tolerance,
                "numeric mismatch at {path}"
            );
        }
        (Value::Array(left), Value::Array(right)) => {
            assert_eq!(left.len(), right.len(), "array length mismatch at {path}");
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                assert_equivalent(left, right, &format!("{path}[{index}]"));
            }
        }
        (Value::Object(left), Value::Object(right)) => {
            assert_eq!(left.len(), right.len(), "field count mismatch at {path}");
            for (key, left) in left {
                assert_equivalent(
                    left,
                    right.get(key).expect("result field should exist"),
                    &format!("{path}.{key}"),
                );
            }
        }
        _ => assert!(expected == actual, "result mismatch at {path}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let output = PathBuf::from(args.next().ok_or("usage: OUTPUT_JSON [BASELINE_JSON]")?);
    let baseline: Option<Value> = args
        .next()
        .map(|path| -> Result<Value, Box<dyn std::error::Error>> {
            Ok(serde_json::from_slice(&std::fs::read(path)?)?)
        })
        .transpose()?;
    let iterations: usize = std::env::var("AETHER_ANALYTICS_BENCHMARK_ITERATIONS")
        .unwrap_or_else(|_| "5".into())
        .parse()?;
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    let tz: i32 = std::env::var("AETHER_ANALYTICS_BENCHMARK_TZ")
        .unwrap_or_else(|_| "480".into())
        .parse()?;
    let options =
        SqliteConnectOptions::from_str(&std::env::var("AETHER_ANALYTICS_BENCHMARK_DATABASE_URL")?)?
            .read_only(true)
            .pragma("cache_size", "-65536")
            .pragma("mmap_size", "268435456");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let max_secs: Option<i64> = sqlx::query_scalar("SELECT MAX(created_at_unix_ms) FROM usage")
        .fetch_one(&pool)
        .await?;
    let max_secs = max_secs.ok_or("snapshot contains no usage")?;
    let offset = i64::from(tz) * 60;
    let until = max_secs + 1;
    let from = (max_secs + offset).div_euclid(86_400) * 86_400 - offset - 29 * 86_400;
    let heatmap_from = (max_secs.div_euclid(86_400) * 86_400 - 364 * 86_400).max(0) as u64;
    let user_id: Option<String> = sqlx::query_scalar(
        "SELECT user_id FROM usage WHERE user_id IS NOT NULL AND user_id <> '' LIMIT 1",
    )
    .fetch_optional(&pool)
    .await?;
    let reader = SqliteUsageReadRepository::new(pool.clone());
    let mut report = json!({
        "from": from, "until": until, "tz_offset_minutes": tz,
        "heatmap_from": heatmap_from, "queries": {},
    });
    if let Some(baseline) = &baseline {
        for field in ["from", "until", "tz_offset_minutes", "heatmap_from"] {
            assert_equivalent(&baseline[field], &report[field], field);
        }
    }

    macro_rules! measure {
        ($name:expr, $query:expr) => {{
            let name = $name;
            let mut timings = Vec::with_capacity(iterations);
            let mut result = Value::Null;
            for _ in 0..iterations {
                let started = Instant::now();
                let rows = $query.await?;
                timings.push(started.elapsed().as_secs_f64() * 1000.0);
                result = serde_json::to_value(rows)?;
            }
            if let Some(baseline) = &baseline {
                assert_equivalent(&baseline["queries"][name]["result"], &result, name);
            }
            let first_ms = timings[0];
            timings.sort_by(f64::total_cmp);
            let median_ms = timings[timings.len() / 2];
            println!("{name:<24} first={first_ms:9.3} ms median={median_ms:9.3} ms");
            report["queries"][name] = json!({
                "first_ms": first_ms, "median_ms": median_ms, "result": result,
            });
        }};
    }

    for (name, group_by) in [
        ("api_key_leaderboard", UsageLeaderboardGroupBy::ApiKey),
        ("user_leaderboard", UsageLeaderboardGroupBy::User),
        ("model_leaderboard", UsageLeaderboardGroupBy::Model),
    ] {
        measure!(
            name,
            reader.summarize_usage_leaderboard(&UsageLeaderboardQuery {
                created_from_unix_secs: from.max(0) as u64,
                created_until_unix_secs: until as u64,
                group_by,
                user_id: None,
                provider_name: None,
                model: None,
            })
        );
    }
    measure!(
        "cost_forecast",
        reader.summarize_usage_time_series(&UsageTimeSeriesQuery {
            created_from_unix_secs: from.max(0) as u64,
            created_until_unix_secs: until as u64,
            granularity: UsageTimeSeriesGranularity::Day,
            tz_offset_minutes: tz,
            user_id: None,
            provider_name: None,
            model: None,
        })
    );
    measure!(
        "cost_savings",
        reader.summarize_usage_cost_savings(&UsageCostSavingsSummaryQuery {
            created_from_unix_secs: from.max(0) as u64,
            created_until_unix_secs: until as u64,
            user_id: None,
            provider_name: None,
            model: None,
        })
    );
    measure!(
        "provider_breakdown",
        reader.summarize_usage_breakdown(&UsageBreakdownSummaryQuery {
            created_from_unix_secs: from.max(0) as u64,
            created_until_unix_secs: until as u64,
            group_by: UsageBreakdownGroupBy::Provider,
            user_id: None,
            provider_name: None,
            model: None,
            api_format: None,
            exclude_status_codes: Vec::new(),
        })
    );
    measure!(
        "admin_heatmap",
        reader.summarize_usage_daily_heatmap(&UsageDailyHeatmapQuery {
            created_from_unix_secs: heatmap_from,
            user_id: None,
            admin_mode: true,
        })
    );
    if let Some(user_id) = user_id {
        measure!(
            "user_heatmap",
            reader.summarize_usage_daily_heatmap(&UsageDailyHeatmapQuery {
                created_from_unix_secs: heatmap_from,
                user_id: Some(user_id.clone()),
                admin_mode: false,
            })
        );
    }
    std::fs::write(output, serde_json::to_vec_pretty(&report)?)?;
    if baseline.is_some() {
        println!("All results match the baseline (integer fields exact; floats within rounding tolerance).");
    }
    pool.close().await;
    Ok(())
}
