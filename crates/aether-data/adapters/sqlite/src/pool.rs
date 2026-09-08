use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::{DataLayerError, DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool as SqlxSqlitePool;

pub type SqlitePool = SqlxSqlitePool;
pub type SqlitePoolConfig = SqlDatabaseConfig;

#[derive(Debug, Clone)]
pub struct SqlitePoolFactory {
    config: SqlitePoolConfig,
}

impl SqlitePoolFactory {
    pub fn new(config: SqlitePoolConfig) -> Result<Self, DataLayerError> {
        if config.driver != DatabaseDriver::Sqlite {
            return Err(DataLayerError::InvalidConfiguration(format!(
                "sqlite pool requires sqlite driver, got {}",
                config.driver
            )));
        }
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &SqlitePoolConfig {
        &self.config
    }

    pub fn connect_options(&self) -> Result<SqliteConnectOptions, DataLayerError> {
        ensure_sqlite_parent_dir(self.config.url.trim())?;
        let is_memory = is_sqlite_memory_url(self.config.url.trim());
        let cache_mb = self.config.sqlite_cache_mb();
        SqliteConnectOptions::from_str(self.config.url.trim())
            .map(|options| {
                let options = options
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .statement_cache_capacity(self.config.pool.statement_cache_capacity);
                if is_memory {
                    options
                } else {
                    options
                        .journal_mode(SqliteJournalMode::Wal)
                        // 64MB page cache by default (sqlx defaults to ~2MB) plus a 256MB
                        // read mmap; both keep hot pages resident for raw fallback paths.
                        .pragma("cache_size", format!("-{}", cache_mb * 1024))
                        .pragma("mmap_size", "268435456")
                        // Safe with WAL: at most the durability of the last committed
                        // transaction is lost on a crash, never integrity.
                        .pragma("synchronous", "NORMAL")
                }
            })
            .map_err(|err| {
                DataLayerError::InvalidConfiguration(format!("invalid sqlite database url: {err}"))
            })
    }

    pub fn connect_lazy(&self) -> Result<SqlitePool, DataLayerError> {
        let SqlPoolConfig {
            min_connections,
            max_connections,
            acquire_timeout_ms,
            idle_timeout_ms,
            max_lifetime_ms,
            ..
        } = self.config.pool;

        Ok(SqlitePoolOptions::new()
            .min_connections(min_connections)
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_millis(acquire_timeout_ms))
            .idle_timeout(Duration::from_millis(idle_timeout_ms))
            .max_lifetime(Duration::from_millis(max_lifetime_ms))
            .connect_lazy_with(self.connect_options()?))
    }
}

fn is_sqlite_memory_url(url: &str) -> bool {
    matches!(url.trim(), "sqlite::memory:" | "sqlite://:memory:")
}

fn sqlite_file_path_from_url(url: &str) -> Option<PathBuf> {
    let url = url.trim();
    if is_sqlite_memory_url(url) {
        return None;
    }
    let path = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn ensure_sqlite_parent_dir(url: &str) -> Result<(), DataLayerError> {
    let Some(path) = sqlite_file_path_from_url(url) else {
        return Ok(());
    };
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).map_err(|err| {
        DataLayerError::InvalidConfiguration(format!(
            "failed to create sqlite database parent directory '{}': {err}",
            parent.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::SqlitePoolFactory;
    use crate::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig, DEFAULT_SQLITE_CACHE_MB};
    use std::path::PathBuf;

    #[tokio::test]
    async fn factory_builds_lazy_pool_from_valid_config() {
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            url: "sqlite://./data/aether.db".to_string(),
            pool: SqlPoolConfig {
                min_connections: 1,
                max_connections: 4,
                acquire_timeout_ms: 1_000,
                idle_timeout_ms: 5_000,
                max_lifetime_ms: 30_000,
                statement_cache_capacity: 64,
                require_ssl: false,
                sqlite_cache_mb: DEFAULT_SQLITE_CACHE_MB,
            },
        };

        let factory = SqlitePoolFactory::new(config).expect("factory should build");
        let _pool = factory.connect_lazy().expect("lazy pool should build");
    }

    #[tokio::test]
    async fn file_database_connection_applies_tuning_pragmas() {
        let db_path = unique_temp_db_path();
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            url: format!("sqlite://{}", db_path.display()),
            pool: SqlPoolConfig::default(),
        };
        let factory = SqlitePoolFactory::new(config).expect("factory should build");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(factory.connect_options().expect("options should build"));
        let (cache_size,): (i64,) = sqlx::query_as("PRAGMA cache_size;")
            .fetch_one(&pool)
            .await
            .expect("cache_size should read back");
        let (mmap_size,): (i64,) = sqlx::query_as("PRAGMA mmap_size;")
            .fetch_one(&pool)
            .await
            .expect("mmap_size should read back");
        let (synchronous,): (i64,) = sqlx::query_as("PRAGMA synchronous;")
            .fetch_one(&pool)
            .await
            .expect("synchronous should read back");
        assert_eq!(cache_size, -(i64::from(DEFAULT_SQLITE_CACHE_MB)) * 1024);
        assert_eq!(mmap_size, 268_435_456);
        assert_eq!(synchronous, 1, "NORMAL maps to 1");
        let _ = std::fs::remove_dir_all(db_path.parent().expect("temp db path should have parent"));
    }

    #[tokio::test]
    async fn memory_database_skips_pragma_tuning() {
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            url: "sqlite::memory:".to_string(),
            pool: SqlPoolConfig::default(),
        };
        let factory = SqlitePoolFactory::new(config).expect("factory should build");
        let options = factory.connect_options().expect("options should build");
        // Only file-backed databases get the page-cache/mmap/synchronous tuning.
        let options_debug = format!("{options:?}");
        assert!(!options_debug.contains("cache_size"));
    }

    #[tokio::test]
    async fn factory_creates_parent_directory_for_file_database() {
        let db_path = unique_temp_db_path();
        let parent = db_path.parent().expect("temp db path should have parent");
        let _ = std::fs::remove_dir_all(parent);
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            url: format!("sqlite://{}", db_path.display()),
            pool: SqlPoolConfig::default(),
        };

        let factory = SqlitePoolFactory::new(config).expect("factory should build");
        let _pool = factory.connect_lazy().expect("lazy pool should build");

        assert!(parent.exists());
        let _ = std::fs::remove_dir_all(parent);
    }

    fn unique_temp_db_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("aether-sqlite-{}", uuid::Uuid::new_v4()))
            .join("nested")
            .join("aether.db")
    }
}
