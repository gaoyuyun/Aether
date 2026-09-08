use crate::{DataLayerError, PostgresPoolConfig};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::str::FromStr;
use std::time::Duration;

fn connect_options(config: &PostgresPoolConfig) -> Result<PgConnectOptions, DataLayerError> {
    config.validate()?;
    let options = PgConnectOptions::from_str(config.database_url.trim()).map_err(|err| {
        DataLayerError::InvalidConfiguration(format!("invalid postgres database_url: {err}"))
    })?;

    // Preserve an explicit verification mode from the URL. `require_ssl` is
    // a minimum transport guarantee, so it may upgrade Disable/Allow/Prefer
    // to Require but must never silently weaken VerifyCa/VerifyFull.
    let ssl_mode = if config.require_ssl
        && !matches!(
            options.get_ssl_mode(),
            PgSslMode::VerifyCa | PgSslMode::VerifyFull
        ) {
        PgSslMode::Require
    } else {
        options.get_ssl_mode()
    };

    Ok(options
        .ssl_mode(ssl_mode)
        .statement_cache_capacity(config.statement_cache_capacity))
}

pub type PostgresPool = PgPool;

#[derive(Debug, Clone)]
pub struct PostgresPoolFactory {
    config: PostgresPoolConfig,
}

impl PostgresPoolFactory {
    pub fn new(config: PostgresPoolConfig) -> Result<Self, DataLayerError> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &PostgresPoolConfig {
        &self.config
    }

    pub fn connect_lazy(&self) -> Result<PostgresPool, DataLayerError> {
        let options = connect_options(&self.config)?;
        Ok(PgPoolOptions::new()
            .min_connections(self.config.min_connections)
            .max_connections(self.config.max_connections)
            .acquire_timeout(Duration::from_millis(self.config.acquire_timeout_ms))
            .idle_timeout(Duration::from_millis(self.config.idle_timeout_ms))
            .max_lifetime(Duration::from_millis(self.config.max_lifetime_ms))
            .connect_lazy_with(options))
    }
}

#[cfg(test)]
mod tests {
    use super::{connect_options, PostgresPoolFactory};
    use crate::PostgresPoolConfig;
    use sqlx::postgres::PgSslMode;

    fn ssl_mode(url: &str, require_ssl: bool) -> PgSslMode {
        connect_options(&PostgresPoolConfig {
            database_url: url.to_string(),
            require_ssl,
            ..PostgresPoolConfig::default()
        })
        .expect("postgres options should parse")
        .get_ssl_mode()
    }

    #[test]
    fn preserves_explicit_postgres_verification_modes() {
        assert!(matches!(
            ssl_mode("postgres://localhost/aether?sslmode=verify-full", false),
            PgSslMode::VerifyFull
        ));
        assert!(matches!(
            ssl_mode("postgres://localhost/aether?sslmode=verify-ca", true),
            PgSslMode::VerifyCa
        ));
    }

    #[test]
    fn require_ssl_only_upgrades_weak_postgres_modes() {
        for mode in ["disable", "allow", "prefer"] {
            let url = format!("postgres://localhost/aether?sslmode={mode}");
            assert!(matches!(ssl_mode(&url, true), PgSslMode::Require));
        }
        assert!(matches!(
            ssl_mode("postgres://localhost/aether", false),
            PgSslMode::Prefer
        ));
    }

    #[tokio::test]
    async fn factory_builds_lazy_pool_from_valid_config() {
        let config = PostgresPoolConfig {
            database_url: "postgres://localhost/aether".to_string(),
            min_connections: 1,
            max_connections: 4,
            acquire_timeout_ms: 1_000,
            idle_timeout_ms: 5_000,
            max_lifetime_ms: 30_000,
            statement_cache_capacity: 64,
            require_ssl: false,
        };

        let factory = PostgresPoolFactory::new(config).expect("factory should build");
        let _pool = factory.connect_lazy().expect("lazy pool should build");
    }
}
