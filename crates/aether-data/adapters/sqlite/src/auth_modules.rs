use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite};

use aether_data_contracts::repository::auth_modules::*;
use aether_data_contracts::DataLayerError;
use aether_data_query::{push_eq, WhereClause};

use crate::error::SqlResultExt;
use crate::SqlitePool;

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
  updated_at = MAX(updated_at + 1, ?)
WHERE singleton_key = 1
  AND server_url IS ?
  AND bind_dn IS ?
  AND bind_password_encrypted IS ?
  AND base_dn IS ?
  AND user_search_filter IS ?
  AND username_attr IS ?
  AND email_attr IS ?
  AND display_name_attr IS ?
  AND is_enabled IS ?
  AND is_exclusive IS ?
  AND use_starttls IS ?
  AND connect_timeout IS ?
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
  updated_at = MAX(updated_at + 1, ?)
WHERE singleton_key = 1
  AND server_url IS ?
  AND bind_dn IS ?
  AND bind_password_encrypted IS ?
  AND base_dn IS ?
  AND user_search_filter IS ?
  AND username_attr IS ?
  AND email_attr IS ?
  AND display_name_attr IS ?
  AND is_enabled IS ?
  AND is_exclusive IS ?
  AND use_starttls IS ?
  AND connect_timeout IS ?
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
pub struct SqliteAuthModuleReadRepository {
    pool: SqlitePool,
}

impl SqliteAuthModuleReadRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, Clone)]
pub struct SqliteAuthModuleRepository {
    pool: SqlitePool,
}

impl SqliteAuthModuleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

async fn list_enabled_oauth_providers(
    pool: &SqlitePool,
) -> Result<Vec<StoredOAuthProviderModuleConfig>, DataLayerError> {
    let mut builder = QueryBuilder::<Sqlite>::new(OAUTH_PROVIDER_COLUMNS);
    let mut where_clause = WhereClause::new();
    push_eq(&mut builder, &mut where_clause, "is_enabled", true);
    builder.push(" ORDER BY provider_type ASC");
    let rows = builder.build().fetch_all(pool).await.map_sql_err()?;
    rows.iter().map(map_oauth_row).collect()
}

async fn get_ldap_config(
    pool: &SqlitePool,
) -> Result<Option<StoredLdapModuleConfig>, DataLayerError> {
    let mut builder = QueryBuilder::<Sqlite>::new(LDAP_CONFIG_COLUMNS);
    builder.push(" WHERE singleton_key = 1");
    let row = builder.build().fetch_optional(pool).await.map_sql_err()?;
    row.as_ref().map(map_ldap_row).transpose()
}

#[async_trait]
impl AuthModuleReadRepository for SqliteAuthModuleReadRepository {
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
impl AuthModuleReadRepository for SqliteAuthModuleRepository {
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
impl AuthModuleWriteRepository for SqliteAuthModuleRepository {
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
  AND server_url IS ?
  AND bind_dn IS ?
  AND bind_password_encrypted IS ?
  AND base_dn IS ?
  AND user_search_filter IS ?
  AND username_attr IS ?
  AND email_attr IS ?
  AND display_name_attr IS ?
  AND is_enabled IS ?
  AND is_exclusive IS ?
  AND use_starttls IS ?
  AND connect_timeout IS ?
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
SET bind_password_encrypted = ?, updated_at = MAX(updated_at + 1, ?)
WHERE singleton_key = 1
  AND bind_password_encrypted = ?
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

fn map_oauth_row(row: &SqliteRow) -> Result<StoredOAuthProviderModuleConfig, DataLayerError> {
    StoredOAuthProviderModuleConfig::new(
        row.try_get("provider_type").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("client_id").map_sql_err()?,
        row.try_get("client_secret_encrypted").map_sql_err()?,
        row.try_get("redirect_uri").map_sql_err()?,
    )
}

fn map_ldap_row(row: &SqliteRow) -> Result<StoredLdapModuleConfig, DataLayerError> {
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
    use super::SqliteAuthModuleRepository;
    use aether_data_contracts::repository::auth_modules::{
        AuthModuleReadRepository, AuthModuleWriteRepository, CompareAndSwapLdapConfigResult,
        LdapBindPasswordUpdate, StoredLdapModuleConfig,
    };

    use crate::run_migrations;

    #[tokio::test]
    async fn sqlite_repository_reads_and_writes_auth_module_configs() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type, display_name, client_id, redirect_uri, frontend_callback_url,
  is_enabled, created_at, updated_at
) VALUES
  ('github', 'GitHub', 'github-client', 'https://github.example.com/callback',
   'https://frontend.example.com/callback', 1, 1, 1),
  ('disabled', 'Disabled', 'disabled-client', 'https://disabled.example.com/callback',
   'https://frontend.example.com/callback', 0, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("oauth providers should seed");

        let repository = SqliteAuthModuleRepository::new(pool);
        let oauth = repository
            .list_enabled_oauth_providers()
            .await
            .expect("oauth providers should load");
        assert_eq!(oauth.len(), 1);
        assert_eq!(oauth[0].provider_type, "github");

        let ldap = StoredLdapModuleConfig {
            server_url: "ldaps://ldap.example.com".to_string(),
            bind_dn: "cn=admin,dc=example,dc=com".to_string(),
            bind_password_encrypted: None,
            base_dn: "dc=example,dc=com".to_string(),
            user_search_filter: Some("(uid={username})".to_string()),
            username_attr: Some("uid".to_string()),
            email_attr: Some("mail".to_string()),
            display_name_attr: Some("displayName".to_string()),
            is_enabled: true,
            is_exclusive: false,
            use_starttls: true,
            connect_timeout: Some(10),
        };
        let stored = repository
            .compare_and_swap_ldap_config(
                None,
                &ldap,
                &LdapBindPasswordUpdate::Set("encrypted-password".to_string()),
            )
            .await
            .expect("ldap create CAS should execute");
        let CompareAndSwapLdapConfigResult::Applied(stored) = stored else {
            panic!("initial LDAP create should apply");
        };
        assert_eq!(stored.server_url, "ldaps://ldap.example.com");
        assert_eq!(
            stored.bind_password_encrypted.as_deref(),
            Some("encrypted-password")
        );

        let competing_create = repository
            .compare_and_swap_ldap_config(
                None,
                &ldap,
                &LdapBindPasswordUpdate::Set("competing-password".to_string()),
            )
            .await
            .expect("competing LDAP create should execute");
        assert_eq!(competing_create, CompareAndSwapLdapConfigResult::Conflict);

        let preserve_replacement = StoredLdapModuleConfig {
            server_url: "ldap://ldap.example.com".to_string(),
            bind_password_encrypted: Some("stale-password-must-not-be-written".to_string()),
            ..stored.clone()
        };
        let updated = repository
            .compare_and_swap_ldap_config(
                Some(&stored),
                &preserve_replacement,
                &LdapBindPasswordUpdate::Preserve,
            )
            .await
            .expect("LDAP preserve CAS should execute");
        let CompareAndSwapLdapConfigResult::Applied(updated) = updated else {
            panic!("fresh LDAP snapshot should update");
        };
        assert_eq!(updated.server_url, "ldap://ldap.example.com");
        assert_eq!(
            updated.bind_password_encrypted.as_deref(),
            Some("encrypted-password")
        );

        assert!(repository
            .compare_and_swap_ldap_bind_password("encrypted-password", "rotated-password")
            .await
            .expect("LDAP password rotation should execute"));
        let stale = repository
            .compare_and_swap_ldap_config(
                Some(&updated),
                &updated,
                &LdapBindPasswordUpdate::Preserve,
            )
            .await
            .expect("stale LDAP CAS should execute");
        assert_eq!(stale, CompareAndSwapLdapConfigResult::Conflict);
        let rotated = repository
            .get_ldap_config()
            .await
            .expect("rotated LDAP config should load")
            .expect("rotated LDAP config should exist");
        assert_eq!(
            rotated.bind_password_encrypted.as_deref(),
            Some("rotated-password")
        );

        let set = repository
            .compare_and_swap_ldap_config(
                Some(&rotated),
                &rotated,
                &LdapBindPasswordUpdate::Set("replacement-password".to_string()),
            )
            .await
            .expect("LDAP password set CAS should execute");
        let CompareAndSwapLdapConfigResult::Applied(set) = set else {
            panic!("fresh LDAP password set should apply");
        };
        let cleared = repository
            .compare_and_swap_ldap_config(Some(&set), &set, &LdapBindPasswordUpdate::Clear)
            .await
            .expect("LDAP password clear CAS should execute");
        let CompareAndSwapLdapConfigResult::Applied(cleared) = cleared else {
            panic!("fresh LDAP password clear should apply");
        };
        assert!(cleared.bind_password_encrypted.is_none());

        let mismatched = StoredLdapModuleConfig {
            base_dn: "dc=changed,dc=example".to_string(),
            ..cleared.clone()
        };
        assert!(!repository
            .delete_ldap_config_if_matches(&mismatched)
            .await
            .expect("mismatched LDAP delete should execute"));
        assert!(repository
            .get_ldap_config()
            .await
            .expect("LDAP config should remain readable")
            .is_some());
        assert!(repository
            .delete_ldap_config_if_matches(&cleared)
            .await
            .expect("matching LDAP delete should execute"));
        assert!(repository
            .get_ldap_config()
            .await
            .expect("LDAP config should remain readable")
            .is_none());
    }
}
