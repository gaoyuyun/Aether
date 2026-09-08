use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, QueryBuilder, Row};

use aether_data_contracts::repository::auth_modules::*;
use aether_data_contracts::DataLayerError;
use aether_data_query::{push_eq, WhereClause};

use crate::error::SqlResultExt;
use crate::MysqlPool;

const OAUTH_PROVIDER_COLUMNS: &str = r#"
SELECT
  provider_type,
  display_name,
  client_id,
  client_secret_encrypted,
  redirect_uri
FROM oauth_providers
"#;

const LDAP_CONFIG_COLUMNS: &str = r#"
SELECT
  server_url,
  bind_dn,
  bind_password_encrypted,
  base_dn,
  user_search_filter,
  username_attr,
  email_attr,
  display_name_attr,
  is_enabled,
  is_exclusive,
  use_starttls,
  connect_timeout
FROM ldap_configs
"#;

const UPDATE_LDAP_CONFIG_PRESERVE_PASSWORD_SQL: &str = r#"
UPDATE ldap_configs
SET
  server_url = ?,
  bind_dn = ?,
  base_dn = ?,
  user_search_filter = ?,
  username_attr = ?,
  email_attr = ?,
  display_name_attr = ?,
  is_enabled = ?,
  is_exclusive = ?,
  use_starttls = ?,
  connect_timeout = ?,
  updated_at = GREATEST(updated_at + 1, ?)
WHERE singleton_key = 1
  AND server_url <=> ?
  AND bind_dn <=> ?
  AND BINARY bind_password_encrypted <=> BINARY ?
  AND base_dn <=> ?
  AND user_search_filter <=> ?
  AND username_attr <=> ?
  AND email_attr <=> ?
  AND display_name_attr <=> ?
  AND is_enabled <=> ?
  AND is_exclusive <=> ?
  AND use_starttls <=> ?
  AND connect_timeout <=> ?
"#;

const UPDATE_LDAP_CONFIG_REPLACE_PASSWORD_SQL: &str = r#"
UPDATE ldap_configs
SET
  server_url = ?,
  bind_dn = ?,
  bind_password_encrypted = ?,
  base_dn = ?,
  user_search_filter = ?,
  username_attr = ?,
  email_attr = ?,
  display_name_attr = ?,
  is_enabled = ?,
  is_exclusive = ?,
  use_starttls = ?,
  connect_timeout = ?,
  updated_at = GREATEST(updated_at + 1, ?)
WHERE singleton_key = 1
  AND server_url <=> ?
  AND bind_dn <=> ?
  AND BINARY bind_password_encrypted <=> BINARY ?
  AND base_dn <=> ?
  AND user_search_filter <=> ?
  AND username_attr <=> ?
  AND email_attr <=> ?
  AND display_name_attr <=> ?
  AND is_enabled <=> ?
  AND is_exclusive <=> ?
  AND use_starttls <=> ?
  AND connect_timeout <=> ?
"#;

const INSERT_LDAP_CONFIG_SQL: &str = r#"
INSERT INTO ldap_configs (
  singleton_key,
  server_url,
  bind_dn,
  bind_password_encrypted,
  base_dn,
  user_search_filter,
  username_attr,
  email_attr,
  display_name_attr,
  is_enabled,
  is_exclusive,
  use_starttls,
  connect_timeout,
  created_at,
  updated_at
) VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

#[derive(Debug, Clone)]
pub struct MysqlAuthModuleReadRepository {
    pool: MysqlPool,
}

impl MysqlAuthModuleReadRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, Clone)]
pub struct MysqlAuthModuleRepository {
    pool: MysqlPool,
}

impl MysqlAuthModuleRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }
}

async fn list_enabled_oauth_providers(
    pool: &MysqlPool,
) -> Result<Vec<StoredOAuthProviderModuleConfig>, DataLayerError> {
    let mut builder = QueryBuilder::<MySql>::new(OAUTH_PROVIDER_COLUMNS);
    let mut where_clause = WhereClause::new();
    push_eq(&mut builder, &mut where_clause, "is_enabled", true);
    builder.push(" ORDER BY provider_type ASC");
    let rows = builder.build().fetch_all(pool).await.map_sql_err()?;
    rows.iter().map(map_oauth_row).collect()
}

async fn get_ldap_config(
    pool: &MysqlPool,
) -> Result<Option<StoredLdapModuleConfig>, DataLayerError> {
    let mut builder = QueryBuilder::<MySql>::new(LDAP_CONFIG_COLUMNS);
    builder.push(" WHERE singleton_key = 1");
    let row = builder.build().fetch_optional(pool).await.map_sql_err()?;
    row.as_ref().map(map_ldap_row).transpose()
}

#[async_trait]
impl AuthModuleReadRepository for MysqlAuthModuleReadRepository {
    async fn list_enabled_oauth_providers(
        &self,
    ) -> Result<Vec<StoredOAuthProviderModuleConfig>, DataLayerError> {
        list_enabled_oauth_providers(&self.pool).await
    }

    async fn get_ldap_config(&self) -> Result<Option<StoredLdapModuleConfig>, DataLayerError> {
        get_ldap_config(&self.pool).await
    }
}

#[async_trait]
impl AuthModuleReadRepository for MysqlAuthModuleRepository {
    async fn list_enabled_oauth_providers(
        &self,
    ) -> Result<Vec<StoredOAuthProviderModuleConfig>, DataLayerError> {
        list_enabled_oauth_providers(&self.pool).await
    }

    async fn get_ldap_config(&self) -> Result<Option<StoredLdapModuleConfig>, DataLayerError> {
        get_ldap_config(&self.pool).await
    }
}

#[async_trait]
impl AuthModuleWriteRepository for MysqlAuthModuleRepository {
    async fn compare_and_swap_ldap_config(
        &self,
        expected: Option<&StoredLdapModuleConfig>,
        replacement: &StoredLdapModuleConfig,
        bind_password_update: &LdapBindPasswordUpdate,
    ) -> Result<CompareAndSwapLdapConfigResult, DataLayerError> {
        let persisted =
            ldap_config_after_password_update(expected, replacement, bind_password_update)?;
        let now = now_unix_secs();
        let Some(expected) = expected else {
            let insert = sqlx::query(INSERT_LDAP_CONFIG_SQL)
                .bind(&persisted.server_url)
                .bind(&persisted.bind_dn)
                .bind(persisted.bind_password_encrypted.as_deref())
                .bind(&persisted.base_dn)
                .bind(persisted.user_search_filter.as_deref())
                .bind(persisted.username_attr.as_deref())
                .bind(persisted.email_attr.as_deref())
                .bind(persisted.display_name_attr.as_deref())
                .bind(persisted.is_enabled)
                .bind(persisted.is_exclusive)
                .bind(persisted.use_starttls)
                .bind(persisted.connect_timeout)
                .bind(now as i64)
                .bind(now as i64)
                .execute(&self.pool)
                .await;
            return match insert {
                Ok(result) if result.rows_affected() == 1 => {
                    Ok(CompareAndSwapLdapConfigResult::Applied(persisted))
                }
                Ok(_) => Ok(CompareAndSwapLdapConfigResult::Conflict),
                Err(error)
                    if error
                        .as_database_error()
                        .is_some_and(|error| error.is_unique_violation()) =>
                {
                    Ok(CompareAndSwapLdapConfigResult::Conflict)
                }
                Err(error) => Err(DataLayerError::sql(error)),
            };
        };

        let rows_affected = match bind_password_update {
            LdapBindPasswordUpdate::Preserve => {
                sqlx::query(UPDATE_LDAP_CONFIG_PRESERVE_PASSWORD_SQL)
                    .bind(&replacement.server_url)
                    .bind(&replacement.bind_dn)
                    .bind(&replacement.base_dn)
                    .bind(replacement.user_search_filter.as_deref())
                    .bind(replacement.username_attr.as_deref())
                    .bind(replacement.email_attr.as_deref())
                    .bind(replacement.display_name_attr.as_deref())
                    .bind(replacement.is_enabled)
                    .bind(replacement.is_exclusive)
                    .bind(replacement.use_starttls)
                    .bind(replacement.connect_timeout)
                    .bind(now as i64)
                    .bind(&expected.server_url)
                    .bind(&expected.bind_dn)
                    .bind(expected.bind_password_encrypted.as_deref())
                    .bind(&expected.base_dn)
                    .bind(expected.user_search_filter.as_deref())
                    .bind(expected.username_attr.as_deref())
                    .bind(expected.email_attr.as_deref())
                    .bind(expected.display_name_attr.as_deref())
                    .bind(expected.is_enabled)
                    .bind(expected.is_exclusive)
                    .bind(expected.use_starttls)
                    .bind(expected.connect_timeout)
                    .execute(&self.pool)
                    .await
                    .map_sql_err()?
                    .rows_affected()
            }
            LdapBindPasswordUpdate::Set(_) | LdapBindPasswordUpdate::Clear => {
                sqlx::query(UPDATE_LDAP_CONFIG_REPLACE_PASSWORD_SQL)
                    .bind(&replacement.server_url)
                    .bind(&replacement.bind_dn)
                    .bind(persisted.bind_password_encrypted.as_deref())
                    .bind(&replacement.base_dn)
                    .bind(replacement.user_search_filter.as_deref())
                    .bind(replacement.username_attr.as_deref())
                    .bind(replacement.email_attr.as_deref())
                    .bind(replacement.display_name_attr.as_deref())
                    .bind(replacement.is_enabled)
                    .bind(replacement.is_exclusive)
                    .bind(replacement.use_starttls)
                    .bind(replacement.connect_timeout)
                    .bind(now as i64)
                    .bind(&expected.server_url)
                    .bind(&expected.bind_dn)
                    .bind(expected.bind_password_encrypted.as_deref())
                    .bind(&expected.base_dn)
                    .bind(expected.user_search_filter.as_deref())
                    .bind(expected.username_attr.as_deref())
                    .bind(expected.email_attr.as_deref())
                    .bind(expected.display_name_attr.as_deref())
                    .bind(expected.is_enabled)
                    .bind(expected.is_exclusive)
                    .bind(expected.use_starttls)
                    .bind(expected.connect_timeout)
                    .execute(&self.pool)
                    .await
                    .map_sql_err()?
                    .rows_affected()
            }
        };
        if rows_affected == 1 {
            Ok(CompareAndSwapLdapConfigResult::Applied(persisted))
        } else {
            Ok(CompareAndSwapLdapConfigResult::Conflict)
        }
    }

    async fn delete_ldap_config_if_matches(
        &self,
        expected: &StoredLdapModuleConfig,
    ) -> Result<bool, DataLayerError> {
        let rows_affected = sqlx::query(
            r#"
DELETE FROM ldap_configs
WHERE singleton_key = 1
  AND server_url <=> ?
  AND bind_dn <=> ?
  AND BINARY bind_password_encrypted <=> BINARY ?
  AND base_dn <=> ?
  AND user_search_filter <=> ?
  AND username_attr <=> ?
  AND email_attr <=> ?
  AND display_name_attr <=> ?
  AND is_enabled <=> ?
  AND is_exclusive <=> ?
  AND use_starttls <=> ?
  AND connect_timeout <=> ?
"#,
        )
        .bind(&expected.server_url)
        .bind(&expected.bind_dn)
        .bind(expected.bind_password_encrypted.as_deref())
        .bind(&expected.base_dn)
        .bind(expected.user_search_filter.as_deref())
        .bind(expected.username_attr.as_deref())
        .bind(expected.email_attr.as_deref())
        .bind(expected.display_name_attr.as_deref())
        .bind(expected.is_enabled)
        .bind(expected.is_exclusive)
        .bind(expected.use_starttls)
        .bind(expected.connect_timeout)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(rows_affected == 1)
    }

    async fn compare_and_swap_ldap_bind_password(
        &self,
        expected: &str,
        replacement: &str,
    ) -> Result<bool, DataLayerError> {
        let rows_affected = sqlx::query(
            r#"
UPDATE ldap_configs
SET bind_password_encrypted = ?, updated_at = GREATEST(updated_at + 1, ?)
WHERE singleton_key = 1
  AND BINARY bind_password_encrypted = BINARY ?
"#,
        )
        .bind(replacement)
        .bind(now_unix_secs() as i64)
        .bind(expected)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(rows_affected == 1)
    }
}

fn ldap_config_after_password_update(
    expected: Option<&StoredLdapModuleConfig>,
    replacement: &StoredLdapModuleConfig,
    bind_password_update: &LdapBindPasswordUpdate,
) -> Result<StoredLdapModuleConfig, DataLayerError> {
    let bind_password_encrypted = match bind_password_update {
        LdapBindPasswordUpdate::Preserve => expected
            .ok_or_else(|| {
                DataLayerError::InvalidConfiguration(
                    "LDAP bind password cannot be preserved while creating the singleton"
                        .to_string(),
                )
            })?
            .bind_password_encrypted
            .clone(),
        LdapBindPasswordUpdate::Set(ciphertext) => Some(ciphertext.clone()),
        LdapBindPasswordUpdate::Clear => None,
    };
    Ok(StoredLdapModuleConfig {
        bind_password_encrypted,
        ..replacement.clone()
    })
}

fn now_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn map_oauth_row(row: &MySqlRow) -> Result<StoredOAuthProviderModuleConfig, DataLayerError> {
    StoredOAuthProviderModuleConfig::new(
        row.try_get("provider_type").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("client_id").map_sql_err()?,
        row.try_get("client_secret_encrypted").map_sql_err()?,
        row.try_get("redirect_uri").map_sql_err()?,
    )
}

fn map_ldap_row(row: &MySqlRow) -> Result<StoredLdapModuleConfig, DataLayerError> {
    Ok(StoredLdapModuleConfig {
        server_url: row.try_get("server_url").map_sql_err()?,
        bind_dn: row.try_get("bind_dn").map_sql_err()?,
        bind_password_encrypted: row.try_get("bind_password_encrypted").map_sql_err()?,
        base_dn: row.try_get("base_dn").map_sql_err()?,
        user_search_filter: row.try_get("user_search_filter").map_sql_err()?,
        username_attr: row.try_get("username_attr").map_sql_err()?,
        email_attr: row.try_get("email_attr").map_sql_err()?,
        display_name_attr: row.try_get("display_name_attr").map_sql_err()?,
        is_enabled: row.try_get("is_enabled").map_sql_err()?,
        is_exclusive: row.try_get("is_exclusive").map_sql_err()?,
        use_starttls: row.try_get("use_starttls").map_sql_err()?,
        connect_timeout: row.try_get("connect_timeout").map_sql_err()?,
    })
}

#[cfg(test)]
mod tests {
    use super::{MysqlAuthModuleReadRepository, MysqlAuthModuleRepository};

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlAuthModuleReadRepository::new(pool.clone());
        let _writable_repository = MysqlAuthModuleRepository::new(pool);
    }
}
