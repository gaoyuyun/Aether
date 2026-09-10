use sqlx::sqlite::{SqliteOwnedBuf, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::OnceCell;

// Cache only immutable database bytes, never a pool tied to one test's Tokio
// runtime. Each caller gets its own writable database and connection worker.
static MIGRATED_DATABASE: OnceCell<Vec<u8>> = OnceCell::const_new();

async fn empty_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
        .connect("sqlite::memory:")
        .await
        .expect("test sqlite pool should connect")
}

/// An isolated in-memory database with the current migrations and seed data.
/// Migration tests and tests of file-backed/concurrent connections should
/// continue creating their databases explicitly and running real migrations.
pub(crate) async fn migrated_pool() -> SqlitePool {
    let template = MIGRATED_DATABASE
        .get_or_init(|| async {
            let pool = empty_pool().await;
            crate::run_migrations(&pool)
                .await
                .expect("test database migrations should run");
            let snapshot = {
                let mut connection = pool.acquire().await.expect("template connection");
                connection
                    .serialize(None)
                    .await
                    .expect("test database should serialize")
                    .to_vec()
            };
            pool.close().await;
            snapshot
        })
        .await;

    let pool = empty_pool().await;
    {
        let mut connection = pool.acquire().await.expect("test database connection");
        let snapshot = SqliteOwnedBuf::try_from(template.as_slice())
            .expect("test database snapshot should allocate");
        connection
            .deserialize(None, snapshot, false)
            .await
            .expect("test database snapshot should restore");
    }
    pool
}

#[tokio::test]
async fn migrated_pools_preserve_schema_and_isolate_mutations() {
    let (first, second) = tokio::join!(migrated_pool(), migrated_pool());
    assert!(crate::pending_migrations(&first).await.unwrap().is_empty());
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&first)
        .await
        .unwrap();
    assert_eq!(foreign_keys, 1);

    sqlx::query("DELETE FROM system_configs WHERE key = 'module.wallet.enabled'")
        .execute(&first)
        .await
        .unwrap();
    let first_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM system_configs WHERE key = 'module.wallet.enabled'",
    )
    .fetch_one(&first)
    .await
    .unwrap();
    assert_eq!(first_count, 0);

    // Existing siblings and future callers must retain the fresh-install seed
    // values even after another test mutates its own database.
    let third = migrated_pool().await;
    for pool in [&second, &third] {
        let enabled: String = sqlx::query_scalar(
            "SELECT value FROM system_configs WHERE key = 'module.wallet.enabled'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(enabled, "false");
    }
    tokio::join!(first.close(), second.close(), third.close());
}
