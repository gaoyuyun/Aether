use sqlx::{
    migrate::{Migrate, MigrateError, Migrator},
    SqlitePool,
};

use aether_data_contracts::PendingMigrationInfo;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");
const FRESH_INSTALLATION_MARKER_TABLE: &str = "_aether_fresh_installation";

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), MigrateError> {
    let fresh_installation =
        fresh_installation_marker_exists(pool).await? || !has_existing_aether_schema(pool).await?;
    if fresh_installation {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS _aether_fresh_installation (id INTEGER PRIMARY KEY)",
        )
        .execute(pool)
        .await?;
    }
    MIGRATOR.run(pool).await?;
    if fresh_installation {
        sqlx::query(
            r#"
UPDATE system_configs
SET value = 'false'
WHERE key IN ('module.wallet.enabled', 'module.billing_plans.enabled')
"#,
        )
        .execute(pool)
        .await?;
        sqlx::query("DROP TABLE IF EXISTS _aether_fresh_installation")
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn fresh_installation_marker_exists(pool: &SqlitePool) -> Result<bool, MigrateError> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(FRESH_INSTALLATION_MARKER_TABLE)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

async fn has_existing_aether_schema(pool: &SqlitePool) -> Result<bool, MigrateError> {
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)
FROM sqlite_master
WHERE type = 'table'
  AND name IN ('system_configs', 'users', 'usage')
"#,
    )
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

pub async fn pending_migrations(
    pool: &SqlitePool,
) -> Result<Vec<PendingMigrationInfo>, MigrateError> {
    let mut conn = pool.acquire().await?;
    let applied_migrations = match conn.list_applied_migrations().await {
        Ok(applied_migrations) => applied_migrations,
        Err(err) if is_missing_sqlx_migrations_table_error(&err) => Vec::new(),
        Err(err) => return Err(err),
    };
    Ok(pending_migrations_from_applied(&applied_migrations))
}

pub async fn prepare_database_for_startup(
    pool: &SqlitePool,
) -> Result<Vec<PendingMigrationInfo>, MigrateError> {
    pending_migrations(pool).await
}

fn is_missing_sqlx_migrations_table_error(err: &MigrateError) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("_sqlx_migrations")
        && (message.contains("no such table")
            || message.contains("doesn't exist")
            || message.contains("does not exist")
            || message.contains("unknown table"))
}

fn pending_migrations_from_applied(
    applied_migrations: &[sqlx::migrate::AppliedMigration],
) -> Vec<PendingMigrationInfo> {
    let applied_versions = applied_migrations
        .iter()
        .map(|migration| migration.version)
        .collect::<std::collections::HashSet<_>>();
    MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .filter(|migration| !applied_versions.contains(&migration.version))
        .map(|migration| PendingMigrationInfo {
            version: migration.version,
            description: migration.description.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{pending_migrations, run_migrations, MIGRATOR};

    #[tokio::test]
    async fn migrates_empty_database_and_clears_pending_set() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");

        let pending = pending_migrations(&pool).await.expect("pending migrations");
        assert_eq!(pending.len(), MIGRATOR.iter().count());
        assert!(!pending.is_empty());

        run_migrations(&pool).await.expect("run sqlite migrations");
        assert!(pending_migrations(&pool)
            .await
            .expect("pending migrations after run")
            .is_empty());
    }
}
