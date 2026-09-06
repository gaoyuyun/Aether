//! Rebuild `stats_hourly_model_provider` rows for a UTC hour range from raw usage.
//!
//! Usage:
//!   AETHER_HOURLY_MP_REPAIR_DATABASE_URL=sqlite:///path/aether.db \
//!     cargo run -p aether-data-sqlite --example recover_hourly_model_provider -- \
//!     --from 1784246400 --until 1788681600 [--apply]
//!
//! Defaults to a dry run; pass `--apply` to write. The `--from`/`--until`
//! bounds are Unix seconds and are rounded to hour boundaries.
use aether_data_sqlite::run_migrations;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|v| v == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let from: i64 = value("--from")
        .ok_or("--from (unix secs) is required")?
        .parse()?;
    let until: i64 = value("--until")
        .ok_or("--until (unix secs) is required")?
        .parse()?;
    if until <= from {
        return Err("--until must be greater than --from".into());
    }
    let dry_run = !args.iter().any(|v| v == "--apply");
    let url = std::env::var("AETHER_HOURLY_MP_REPAIR_DATABASE_URL")?;
    let pool = sqlx::SqlitePool::connect(&url).await?;
    run_migrations(&pool).await?;

    let aligned_from = from.div_euclid(3600) * 3600;
    let aligned_until = until.div_euclid(3600) * 3600;
    let mut hours_rebuilt = 0usize;
    let mut rows_written = 0usize;

    let mut hour = aligned_from;
    while hour < aligned_until {
        let end = hour + 3600;
        let sql = r#"
INSERT INTO stats_hourly_model_provider (
  id, hour_utc, model, provider_name, total_requests,
  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
  total_cost, settled_total_cost, response_time_sum_ms, response_time_samples,
  created_at, updated_at
)
SELECT
  lower(hex(randomblob(32))), ?, model, provider_name, COUNT(*),
  COALESCE(SUM(CASE WHEN COALESCE("usage".total_tokens, 0) > 0 THEN MAX(COALESCE("usage".total_tokens, 0), 0)
    ELSE 0 END), 0)
    + COALESCE(SUM(CASE WHEN COALESCE("usage".total_tokens, 0) <= 0 THEN (
      CASE
        WHEN (
          LOWER(COALESCE("usage".endpoint_api_format, "usage".api_format, '')) IN ('openai', 'gemini', 'google')
          OR LOWER(COALESCE("usage".endpoint_api_format, "usage".api_format, '')) LIKE 'openai:%'
          OR LOWER(COALESCE("usage".endpoint_api_format, "usage".api_format, '')) LIKE 'gemini:%'
          OR LOWER(COALESCE("usage".endpoint_api_format, "usage".api_format, '')) LIKE 'google:%'
        ) AND COALESCE("usage".input_tokens, 0) > 0 AND COALESCE("usage".cache_read_input_tokens, 0) > 0
        THEN MAX(COALESCE("usage".input_tokens, 0) - COALESCE("usage".cache_read_input_tokens, 0), 0)
        ELSE MAX(COALESCE("usage".input_tokens, 0), 0)
      END
    ) ELSE 0 END), 0),
  COALESCE(SUM(CASE WHEN COALESCE("usage".total_tokens, 0) <= 0 THEN MAX(COALESCE("usage".output_tokens, 0), 0) ELSE 0 END), 0),
  COALESCE(SUM(CASE WHEN COALESCE("usage".total_tokens, 0) <= 0 THEN (
    CASE WHEN COALESCE("usage".cache_creation_input_tokens, 0) = 0
      AND (COALESCE("usage".cache_creation_ephemeral_5m_input_tokens, 0)
         + COALESCE("usage".cache_creation_ephemeral_1h_input_tokens, 0)) > 0
    THEN COALESCE("usage".cache_creation_ephemeral_5m_input_tokens, 0)
       + COALESCE("usage".cache_creation_ephemeral_1h_input_tokens, 0)
    ELSE MAX(COALESCE("usage".cache_creation_input_tokens, 0), 0) END
  ) ELSE 0 END), 0),
  COALESCE(SUM(CASE WHEN COALESCE("usage".total_tokens, 0) <= 0 THEN MAX(COALESCE("usage".cache_read_input_tokens, 0), 0) ELSE 0 END), 0),
  CAST(COALESCE(SUM(COALESCE(settlement.billing_total_cost_usd, "usage".total_cost_usd, 0)), 0) AS REAL),
  CAST(COALESCE(SUM(CASE WHEN COALESCE(settlement.billing_status, "usage".billing_status) = 'settled' THEN COALESCE(settlement.billing_total_cost_usd, "usage".total_cost_usd, 0) ELSE 0 END), 0) AS REAL),
  COALESCE(SUM(CASE WHEN "usage".response_time_ms IS NOT NULL THEN MAX(COALESCE("usage".response_time_ms, 0), 0) ELSE 0 END), 0),
  COALESCE(SUM(CASE WHEN "usage".response_time_ms IS NOT NULL THEN 1 ELSE 0 END), 0), ?, ?
FROM "usage"
LEFT JOIN usage_settlement_snapshots AS settlement
  ON settlement.request_id = "usage".request_id
WHERE created_at_unix_ms >= ? AND created_at_unix_ms < ?
  AND model IS NOT NULL AND model <> ''
  AND status NOT IN ('pending', 'streaming')
  AND provider_name NOT IN ('unknown', 'pending')
GROUP BY model, provider_name
ON CONFLICT (hour_utc, model, provider_name) DO UPDATE SET
  total_requests = excluded.total_requests,
  input_tokens = excluded.input_tokens,
  output_tokens = excluded.output_tokens,
  cache_creation_tokens = excluded.cache_creation_tokens,
  cache_read_tokens = excluded.cache_read_tokens,
  total_cost = excluded.total_cost,
  settled_total_cost = excluded.settled_total_cost,
  response_time_sum_ms = excluded.response_time_sum_ms,
  response_time_samples = excluded.response_time_samples,
  updated_at = excluded.updated_at
"#;
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(sql)
            .bind(hour)
            .bind(now)
            .bind(now)
            .bind(hour)
            .bind(end)
            .execute(&pool)
            .await?;
        if !dry_run {
            rows_written += usize::try_from(result.rows_affected()).unwrap_or(usize::MAX);
        }
        hours_rebuilt += 1;
        hour = end;
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": dry_run,
            "hours_rebuilt": hours_rebuilt,
            "rows_written": rows_written,
        }))?
    );
    Ok(())
}
