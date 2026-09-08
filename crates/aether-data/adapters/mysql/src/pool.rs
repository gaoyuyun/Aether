use std::str::FromStr;
use std::time::Duration;

use crate::{DataLayerError, DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};
use sqlx::MySqlPool as SqlxMysqlPool;

pub type MysqlPool = SqlxMysqlPool;
pub type MysqlPoolConfig = SqlDatabaseConfig;

#[derive(Debug, Clone)]
pub struct MysqlPoolFactory {
    config: MysqlPoolConfig,
}

impl MysqlPoolFactory {
    pub fn new(config: MysqlPoolConfig) -> Result<Self, DataLayerError> {
        if config.driver != DatabaseDriver::Mysql {
            return Err(DataLayerError::InvalidConfiguration(format!(
                "mysql pool requires mysql driver, got {}",
                config.driver
            )));
        }
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &MysqlPoolConfig {
        &self.config
    }

    pub fn connect_options(&self) -> Result<MySqlConnectOptions, DataLayerError> {
        MySqlConnectOptions::from_str(self.config.url.trim())
            .map(|options| {
                // Preserve explicit VERIFY_CA/VERIFY_IDENTITY from the URL.
                // `require_ssl` is a minimum transport guarantee: upgrade
                // weaker modes to Required, never downgrade verification.
                let ssl_mode = if self.config.pool.require_ssl
                    && !matches!(
                        options.get_ssl_mode(),
                        MySqlSslMode::VerifyCa | MySqlSslMode::VerifyIdentity
                    ) {
                    MySqlSslMode::Required
                } else {
                    options.get_ssl_mode()
                };
                options
                    .ssl_mode(ssl_mode)
                    .statement_cache_capacity(self.config.pool.statement_cache_capacity)
            })
            .map_err(|err| {
                DataLayerError::InvalidConfiguration(format!("invalid mysql database url: {err}"))
            })
    }

    pub fn connect_lazy(&self) -> Result<MysqlPool, DataLayerError> {
        let SqlPoolConfig {
            min_connections,
            max_connections,
            acquire_timeout_ms,
            idle_timeout_ms,
            max_lifetime_ms,
            ..
        } = self.config.pool;

        Ok(MySqlPoolOptions::new()
            .min_connections(min_connections)
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_millis(acquire_timeout_ms))
            .idle_timeout(Duration::from_millis(idle_timeout_ms))
            .max_lifetime(Duration::from_millis(max_lifetime_ms))
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    sqlx::query("SET time_zone = '+00:00'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(self.connect_options()?))
    }
}

#[cfg(test)]
mod tests {
    use super::MysqlPoolFactory;
    use crate::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
    use sqlx::mysql::MySqlSslMode;

    fn ssl_mode(url: &str, require_ssl: bool) -> MySqlSslMode {
        MysqlPoolFactory::new(SqlDatabaseConfig {
            driver: DatabaseDriver::Mysql,
            url: url.to_string(),
            pool: SqlPoolConfig {
                require_ssl,
                ..SqlPoolConfig::default()
            },
        })
        .expect("mysql config should build")
        .connect_options()
        .expect("mysql options should parse")
        .get_ssl_mode()
    }

    #[test]
    fn preserves_explicit_mysql_verification_modes() {
        assert!(matches!(
            ssl_mode(
                "mysql://user:pass@localhost/aether?ssl-mode=VERIFY_IDENTITY",
                false
            ),
            MySqlSslMode::VerifyIdentity
        ));
        assert!(matches!(
            ssl_mode(
                "mysql://user:pass@localhost/aether?ssl-mode=VERIFY_CA",
                true
            ),
            MySqlSslMode::VerifyCa
        ));
    }

    #[test]
    fn require_ssl_only_upgrades_weak_mysql_modes() {
        for mode in ["DISABLED", "PREFERRED", "REQUIRED"] {
            let url = format!("mysql://user:pass@localhost/aether?ssl-mode={mode}");
            assert!(matches!(ssl_mode(&url, true), MySqlSslMode::Required));
        }
        assert!(matches!(
            ssl_mode("mysql://user:pass@localhost/aether", false),
            MySqlSslMode::Preferred
        ));
    }

    #[tokio::test]
    async fn factory_builds_lazy_pool_from_valid_config() {
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Mysql,
            url: "mysql://user:pass@localhost:3306/aether".to_string(),
            pool: SqlPoolConfig {
                min_connections: 1,
                max_connections: 4,
                acquire_timeout_ms: 1_000,
                idle_timeout_ms: 5_000,
                max_lifetime_ms: 30_000,
                statement_cache_capacity: 64,
                require_ssl: false,
                ..SqlPoolConfig::default()
            },
        };

        let factory = MysqlPoolFactory::new(config).expect("factory should build");
        let _pool = factory.connect_lazy().expect("lazy pool should build");
    }

    #[tokio::test]
    async fn factory_configures_utc_session_timezone_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!("skipping mysql timezone test because AETHER_TEST_MYSQL_URL is unset");
            return;
        };
        let config = SqlDatabaseConfig {
            driver: DatabaseDriver::Mysql,
            url: database_url,
            pool: SqlPoolConfig {
                max_connections: 1,
                ..SqlPoolConfig::default()
            },
        };

        let pool = MysqlPoolFactory::new(config)
            .expect("factory should build")
            .connect_lazy()
            .expect("lazy pool should build");
        let timezone: String = sqlx::query_scalar("SELECT @@session.time_zone")
            .fetch_one(&pool)
            .await
            .expect("mysql session timezone should load");

        assert_eq!(timezone, "+00:00");
    }
}
