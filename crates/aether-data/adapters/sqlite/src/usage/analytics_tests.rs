use super::{sqlite_usage_leaderboard_query, SqliteUsageReadRepository};
use crate::run_migrations;
use aether_data_contracts::repository::usage::{
    UsageDailyHeatmapQuery, UsageLeaderboardGroupBy, UsageLeaderboardQuery, UsageReadRepository,
};
use sqlx::{Execute, Row, SqlitePool};

const DAY: u64 = 86_400;
const START: u64 = 1_735_689_600; // 2025-01-01 00:00 UTC

async fn analytics_pool() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("analytics pool");
    run_migrations(&pool).await.expect("analytics migrations");
    sqlx::raw_sql(
        r#"
INSERT INTO users (id, username, created_at, updated_at) VALUES
  ('user-1', 'User One', 1, 1), ('user-2', 'User Two', 1, 1);
INSERT INTO api_keys (id, user_id, key_hash, name, created_at, updated_at) VALUES
  ('key-1', 'user-1', 'hash-1', 'Key One', 1, 1);
"#,
    )
    .execute(&pool)
    .await
    .expect("analytics identities");
    pool
}

async fn insert_usage(pool: &SqlitePool, id: &str, user: &str, created: u64) {
    sqlx::query(
        r#"
INSERT INTO usage (
  id, request_id, user_id, api_key_id, provider_name, model, status,
  input_tokens, output_tokens, total_cost_usd, actual_total_cost_usd,
  created_at_unix_ms
) VALUES (?, ?, ?, 'key-1', 'Provider One', 'model-1', 'completed', 10, 2, 0.5, 0.25, ?)
"#,
    )
    .bind(id)
    .bind(id)
    .bind(user)
    .bind(created as i64)
    .execute(pool)
    .await
    .expect("analytics usage");
}

#[tokio::test]
async fn heatmap_preserves_rollups_and_reads_only_uncovered_dates() {
    let pool = analytics_pool().await;
    for (id, user, created) in [
        ("before-partial-start", "user-1", START),
        ("partial-start", "user-1", START + 60),
        ("covered", "user-1", START + DAY),
        ("gap", "user-1", START + 2 * DAY),
        ("other-user", "user-2", START + 2 * DAY + 10),
        ("empty-rollup", "user-1", START + 4 * DAY),
        ("tail", "user-1", START + 5 * DAY),
        ("pending", "user-1", START + 6 * DAY),
        ("placeholder", "user-1", START + 6 * DAY),
    ] {
        insert_usage(&pool, id, user, created).await;
    }
    sqlx::raw_sql(
        "UPDATE usage SET status = 'pending' WHERE request_id = 'pending';
         UPDATE usage SET provider_name = 'unknown' WHERE request_id = 'placeholder';",
    )
    .execute(&pool)
    .await
    .expect("non-finalized fixtures");
    for (day, requests) in [(1, 9), (3, 4), (4, 0)] {
        sqlx::query(
            r#"
INSERT INTO stats_daily (
  id, date, total_requests, input_tokens, output_tokens, total_cost, actual_total_cost,
  is_complete, created_at, updated_at
) VALUES (?, ?, ?, 100, 20, 5, 4, 1, 1, 1)
"#,
        )
        .bind(format!("day-{day}"))
        .bind((START + day * DAY) as i64)
        .bind(requests)
        .execute(&pool)
        .await
        .expect("global rollup");
        sqlx::query(
            r#"
INSERT INTO stats_user_daily (
  id, user_id, date, total_requests, input_tokens, output_tokens, total_cost,
  created_at, updated_at
) VALUES (?, 'user-1', ?, ?, 40, 10, 2, 1, 1)
"#,
        )
        .bind(format!("user-day-{day}"))
        .bind((START + day * DAY) as i64)
        .bind(requests / 2)
        .execute(&pool)
        .await
        .expect("user rollup");
    }
    let reader = SqliteUsageReadRepository::new(pool.clone());
    for (user, expected_requests, rollup_tokens, rollup_cost, rollup_actual) in [
        (None, vec![1, 9, 2, 4, 1, 1], 120, 5.0, 4.0),
        (Some("user-1"), vec![1, 4, 1, 2, 1, 1], 50, 2.0, 2.0),
    ] {
        let rows = reader
            .summarize_usage_daily_heatmap(&UsageDailyHeatmapQuery {
                created_from_unix_secs: START + 60,
                user_id: user.map(str::to_owned),
                admin_mode: user.is_none(),
            })
            .await
            .expect("heatmap with gaps");
        assert_eq!(rows.len(), 6);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.date, format!("2025-01-{:02}", index + 1));
            assert_eq!(row.requests, expected_requests[index]);
            if matches!(index, 1 | 3) {
                assert_eq!(row.total_tokens, rollup_tokens);
                assert_eq!(row.total_cost_usd, rollup_cost);
                assert_eq!(row.actual_total_cost_usd, rollup_actual);
            } else {
                assert_eq!(row.total_tokens, row.requests * 12);
                assert_eq!(row.total_cost_usd, row.requests as f64 * 0.5);
                assert_eq!(row.actual_total_cost_usd, row.requests as f64 * 0.25);
            }
        }
    }

    // No rollups is a normal state during backfill, not an empty heatmap.
    sqlx::raw_sql("DELETE FROM stats_daily; DELETE FROM stats_user_daily;")
        .execute(&pool)
        .await
        .expect("remove rollups");
    let rows = reader
        .summarize_usage_daily_heatmap(&UsageDailyHeatmapQuery {
            created_from_unix_secs: START,
            user_id: Some("user-1".into()),
            admin_mode: false,
        })
        .await
        .expect("raw-only heatmap");
    assert_eq!(rows.iter().map(|row| row.requests).sum::<u64>(), 6);
    assert_eq!(rows.iter().map(|row| row.total_tokens).sum::<u64>(), 72);
    assert_eq!(rows.len(), 5);
    pool.close().await;
}

#[tokio::test]
async fn leaderboard_preserves_canonical_tokens_filters_and_current_names() {
    let pool = analytics_pool().await;
    for id in [
        "effective",
        "context",
        "recorded",
        "legacy",
        "cancelled",
        "deleted",
    ] {
        insert_usage(&pool, id, "user-1", START + 100).await;
    }
    insert_usage(&pool, "before", "user-1", START - 1).await;
    insert_usage(&pool, "until", "user-1", START + DAY).await;
    for id in ["pending", "streaming", "placeholder", "blank-key"] {
        insert_usage(&pool, id, "user-1", START + 100).await;
    }
    sqlx::raw_sql(
        r#"
UPDATE usage SET total_tokens = 9999 WHERE request_id IN ('effective', 'context');
UPDATE usage SET total_tokens = 120 WHERE request_id = 'recorded';
UPDATE usage SET total_tokens = 0, input_tokens = 100, output_tokens = 10,
  cache_read_input_tokens = 80, cache_creation_input_tokens = 0,
  cache_creation_ephemeral_5m_input_tokens = 2, cache_creation_ephemeral_1h_input_tokens = 3,
  api_format = 'openai:chat' WHERE request_id = 'legacy';
UPDATE usage SET total_tokens = 7, status = 'cancelled' WHERE request_id = 'cancelled';
UPDATE usage SET total_tokens = 13, api_key_id = 'deleted-key' WHERE request_id = 'deleted';
UPDATE usage SET status = request_id WHERE request_id IN ('pending', 'streaming');
UPDATE usage SET provider_name = 'pending' WHERE request_id = 'placeholder';
UPDATE usage SET api_key_id = ' ' WHERE request_id = 'blank-key';
INSERT INTO usage_settlement_snapshots (
  request_id, billing_status, billing_effective_input_tokens, billing_output_tokens,
  billing_cache_creation_tokens, billing_cache_read_tokens, created_at, updated_at
) VALUES ('effective', 'settled', 40, 5, 7, 11, 1, 1);
INSERT INTO usage_settlement_snapshots (
  request_id, billing_status, billing_total_input_context, billing_output_tokens, created_at, updated_at
) VALUES ('context', 'settled', 80, 3, 1, 1);
"#,
    )
    .execute(&pool)
    .await
    .expect("canonical and legacy token fixtures");
    let reader = SqliteUsageReadRepository::new(pool.clone());
    // Exact half-open bounds include midnight correctly for all supported offsets.
    for offset in [0_i64, 60, 480, -300, 330, 345] {
        let from = (START as i64 - offset * 60) as u64;
        sqlx::query(
            "UPDATE usage SET created_at_unix_ms = ? WHERE request_id NOT IN ('before', 'until')",
        )
        .bind((from + 100) as i64)
        .execute(&pool)
        .await
        .expect("shift fixtures into local day");
        sqlx::query("UPDATE usage SET created_at_unix_ms = ? WHERE request_id = 'before'")
            .bind((from - 1) as i64)
            .execute(&pool)
            .await
            .expect("left boundary");
        sqlx::query("UPDATE usage SET created_at_unix_ms = ? WHERE request_id = 'until'")
            .bind((from + DAY) as i64)
            .execute(&pool)
            .await
            .expect("right boundary");
        let query = UsageLeaderboardQuery {
            created_from_unix_secs: from,
            created_until_unix_secs: from + DAY,
            group_by: UsageLeaderboardGroupBy::ApiKey,
            user_id: Some("user-1".into()),
            provider_name: Some("Provider One".into()),
            model: Some("model-1".into()),
        };
        let rows = reader
            .summarize_usage_leaderboard(&query)
            .await
            .expect("leaderboard");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].group_key, "deleted-key");
        assert_eq!(rows[0].legacy_name, None);
        assert_eq!(rows[0].total_tokens, 13);
        assert_eq!(rows[1].group_key, "key-1");
        assert_eq!(rows[1].legacy_name.as_deref(), Some("Key One"));
        assert_eq!(rows[1].request_count, 5);
        assert_eq!(rows[1].total_tokens, 388);
        assert_eq!(rows[1].total_cost_usd, 2.5);
        let absent = reader
            .summarize_usage_leaderboard(&UsageLeaderboardQuery {
                user_id: Some("user-2".into()),
                ..query
            })
            .await
            .expect("different user scope");
        assert!(absent.is_empty());
    }
    sqlx::query("UPDATE api_keys SET name = 'Renamed Key' WHERE id = 'key-1'")
        .execute(&pool)
        .await
        .expect("rename key");
    for (group_by, name) in [
        (UsageLeaderboardGroupBy::ApiKey, Some("Renamed Key")),
        (UsageLeaderboardGroupBy::User, Some("User One")),
        (UsageLeaderboardGroupBy::Model, None),
    ] {
        let rows = reader
            .summarize_usage_leaderboard(&UsageLeaderboardQuery {
                created_from_unix_secs: START - DAY,
                created_until_unix_secs: START + 2 * DAY,
                group_by,
                user_id: None,
                provider_name: None,
                model: None,
            })
            .await
            .expect("group names");
        assert_eq!(rows.last().expect("group").legacy_name.as_deref(), name);
    }
    pool.close().await;
}

#[tokio::test]
async fn leaderboard_query_plan_avoids_usage_and_settlement_payloads() {
    let pool = analytics_pool().await;
    for group_by in [
        UsageLeaderboardGroupBy::ApiKey,
        UsageLeaderboardGroupBy::User,
        UsageLeaderboardGroupBy::Model,
    ] {
        let mut builder = sqlite_usage_leaderboard_query(&UsageLeaderboardQuery {
            created_from_unix_secs: START,
            created_until_unix_secs: START + 30 * DAY,
            group_by,
            user_id: None,
            provider_name: Some("Provider One".into()),
            model: None,
        });
        let mut query = builder.build();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql());
        let arguments = query
            .take_arguments()
            .expect("query arguments")
            .expect("bound range");
        let rows = sqlx::query_with(&sql, arguments)
            .fetch_all(&pool)
            .await
            .expect("explain production query");
        let plan: Vec<String> = rows.iter().map(|row| row.get("detail")).collect();
        for index in [
            "idx_usage_analytics_covering",
            "idx_usage_settlement_analytics_covering",
        ] {
            assert!(
                plan.iter()
                    .any(|step| step.contains(&format!("USING COVERING INDEX {index}"))),
                "analytics must not fetch payload rows: {plan:?}"
            );
        }
    }
    pool.close().await;
}
