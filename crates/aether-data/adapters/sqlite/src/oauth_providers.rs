use async_trait::async_trait;
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite};

use aether_data_contracts::repository::oauth_providers::{
    OAuthProviderReadRepository, OAuthProviderWriteRepository, StoredOAuthProviderConfig,
    UpsertOAuthProviderConfigOutcome, UpsertOAuthProviderConfigRecord,
};
use aether_data_contracts::DataLayerError;
use aether_data_query::{push_eq, push_limit, WhereClause};

use crate::error::SqlResultExt;
use crate::SqlitePool;

#[derive(Debug, Clone)]
pub struct SqliteOAuthProviderRepository {
    pool: SqlitePool,
}

impl SqliteOAuthProviderRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn get_provider(
        &self,
        provider_type: &str,
    ) -> Result<Option<StoredOAuthProviderConfig>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(OAUTH_PROVIDER_COLUMNS);
        let mut where_clause = WhereClause::new();
        push_eq(
            &mut builder,
            &mut where_clause,
            "provider_type",
            provider_type.to_string(),
        );
        push_limit(&mut builder, 1);
        let row = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_oauth_provider_row).transpose()
    }
}

const OAUTH_PROVIDER_COLUMNS: &str = r#"
SELECT
  provider_type,
  display_name,
  client_id,
  client_secret_encrypted,
  authorization_url_override,
  token_url_override,
  userinfo_url_override,
  scopes,
  redirect_uri,
  frontend_callback_url,
  attribute_mapping,
  extra_config,
  icon_url,
  is_enabled,
  created_at AS created_at_unix_ms,
  updated_at AS updated_at_unix_secs
FROM oauth_providers
"#;

const COUNT_LOCKED_USERS_IF_PROVIDER_DISABLED_SQL: &str = r#"
SELECT COUNT(DISTINCT users.id) AS locked_count
FROM users
JOIN user_oauth_links
  ON users.id = user_oauth_links.user_id
WHERE users.is_active = 1
  AND users.is_deleted = 0
  AND user_oauth_links.provider_type = ?
  AND (
    (
      users.auth_source = 'oauth'
      AND NOT EXISTS (
        SELECT 1
        FROM user_oauth_links other_links
        JOIN oauth_providers other_provider
          ON other_links.provider_type = other_provider.provider_type
        WHERE other_links.user_id = users.id
          AND other_links.provider_type <> ?
          AND other_provider.is_enabled = 1
      )
    ) OR (
      ? = 1
      AND users.auth_source = 'local'
      AND users.role <> 'admin'
      AND NOT EXISTS (
        SELECT 1
        FROM user_oauth_links other_links
        JOIN oauth_providers other_provider
          ON other_links.provider_type = other_provider.provider_type
        WHERE other_links.user_id = users.id
          AND other_links.provider_type <> ?
          AND other_provider.is_enabled = 1
      )
    )
  )
"#;

const COMPARE_AND_SWAP_OAUTH_PROVIDER_CLIENT_SECRET_SQL: &str = r#"
UPDATE oauth_providers
SET client_secret_encrypted = ?
WHERE provider_type = ?
  AND client_secret_encrypted = ?
"#;

#[async_trait]
impl OAuthProviderReadRepository for SqliteOAuthProviderRepository {
    async fn list_oauth_provider_configs(
        &self,
    ) -> Result<Vec<StoredOAuthProviderConfig>, DataLayerError> {
        let mut builder = QueryBuilder::<Sqlite>::new(OAUTH_PROVIDER_COLUMNS);
        builder.push(" ORDER BY provider_type ASC");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_oauth_provider_row).collect()
    }

    async fn get_oauth_provider_config(
        &self,
        provider_type: &str,
    ) -> Result<Option<StoredOAuthProviderConfig>, DataLayerError> {
        self.get_provider(provider_type).await
    }

    async fn count_locked_users_if_provider_disabled(
        &self,
        provider_type: &str,
        ldap_exclusive: bool,
    ) -> Result<usize, DataLayerError> {
        let row = sqlx::query(COUNT_LOCKED_USERS_IF_PROVIDER_DISABLED_SQL)
            .bind(provider_type)
            .bind(provider_type)
            .bind(ldap_exclusive)
            .bind(provider_type)
            .fetch_one(&self.pool)
            .await
            .map_sql_err()?;
        let locked_count = row.try_get::<i64, _>("locked_count").map_sql_err()?;
        usize::try_from(locked_count.max(0)).map_err(|_| {
            DataLayerError::UnexpectedValue(
                "oauth_providers.locked_user_count overflowed".to_string(),
            )
        })
    }
}

#[async_trait]
impl OAuthProviderWriteRepository for SqliteOAuthProviderRepository {
    async fn upsert_oauth_provider_config_guarded(
        &self,
        record: &UpsertOAuthProviderConfigRecord,
        ldap_exclusive: bool,
        force_disable: bool,
        _locked_users_snapshot: usize,
    ) -> Result<UpsertOAuthProviderConfigOutcome, DataLayerError> {
        record.validate()?;
        let now = now_unix_secs();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let existing_enabled: Option<bool> =
            sqlx::query_scalar("SELECT is_enabled FROM oauth_providers WHERE provider_type = ?")
                .bind(&record.provider_type)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if !force_disable && !record.is_enabled && existing_enabled == Some(true) {
            let row = sqlx::query(COUNT_LOCKED_USERS_IF_PROVIDER_DISABLED_SQL)
                .bind(&record.provider_type)
                .bind(&record.provider_type)
                .bind(ldap_exclusive)
                .bind(&record.provider_type)
                .fetch_one(&mut *tx)
                .await
                .map_sql_err()?;
            let affected_count =
                usize::try_from(row.try_get::<i64, _>("locked_count").map_sql_err()?.max(0))
                    .map_err(|_| {
                        DataLayerError::UnexpectedValue(
                            "oauth_providers.locked_user_count overflowed".to_string(),
                        )
                    })?;
            if affected_count > 0 {
                tx.rollback().await.map_sql_err()?;
                return Ok(
                    UpsertOAuthProviderConfigOutcome::DisableRequiresConfirmation {
                        affected_count,
                    },
                );
            }
        }
        sqlx::query(
            r#"
INSERT INTO oauth_providers (
  provider_type,
  display_name,
  client_id,
  client_secret_encrypted,
  authorization_url_override,
  token_url_override,
  userinfo_url_override,
  scopes,
  redirect_uri,
  frontend_callback_url,
  attribute_mapping,
  extra_config,
  icon_url,
  is_enabled,
  created_at,
  updated_at
) VALUES (
  ?, ?, ?,
  CASE ? WHEN 'set' THEN ? WHEN 'clear' THEN NULL ELSE NULL END,
  ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
)
ON CONFLICT(provider_type) DO UPDATE SET
  display_name = excluded.display_name,
  client_id = excluded.client_id,
  client_secret_encrypted = CASE ?
    WHEN 'set' THEN ?
    WHEN 'clear' THEN NULL
    ELSE oauth_providers.client_secret_encrypted
  END,
  authorization_url_override = excluded.authorization_url_override,
  token_url_override = excluded.token_url_override,
  userinfo_url_override = excluded.userinfo_url_override,
  scopes = excluded.scopes,
  redirect_uri = excluded.redirect_uri,
  frontend_callback_url = excluded.frontend_callback_url,
  attribute_mapping = excluded.attribute_mapping,
  extra_config = excluded.extra_config,
  icon_url = excluded.icon_url,
  is_enabled = excluded.is_enabled,
  updated_at = excluded.updated_at
"#,
        )
        .bind(&record.provider_type)
        .bind(&record.display_name)
        .bind(&record.client_id)
        .bind(record.client_secret_encrypted.mode_name())
        .bind(record.client_secret_encrypted.value())
        .bind(record.authorization_url_override.as_deref())
        .bind(record.token_url_override.as_deref())
        .bind(record.userinfo_url_override.as_deref())
        .bind(scopes_to_json_string(record.scopes.as_ref())?)
        .bind(&record.redirect_uri)
        .bind(&record.frontend_callback_url)
        .bind(json_to_string(record.attribute_mapping.as_ref())?)
        .bind(json_to_string(record.extra_config.as_ref())?)
        .bind(record.icon_url.as_deref())
        .bind(record.is_enabled)
        .bind(now as i64)
        .bind(now as i64)
        .bind(record.client_secret_encrypted.mode_name())
        .bind(record.client_secret_encrypted.value())
        .execute(&mut *tx)
        .await
        .map_sql_err()?;

        let row = sqlx::query(&format!(
            "{OAUTH_PROVIDER_COLUMNS} WHERE provider_type = ? LIMIT 1"
        ))
        .bind(&record.provider_type)
        .fetch_one(&mut *tx)
        .await
        .map_sql_err()?;
        let provider = map_oauth_provider_row(&row)?;
        tx.commit().await.map_sql_err()?;
        Ok(UpsertOAuthProviderConfigOutcome::Upserted(provider))
    }

    async fn compare_and_swap_oauth_provider_client_secret(
        &self,
        provider_type: &str,
        expected: &str,
        replacement: &str,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(COMPARE_AND_SWAP_OAUTH_PROVIDER_CLIENT_SECRET_SQL)
            .bind(replacement)
            .bind(provider_type)
            .bind(expected)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn delete_oauth_provider_config_if_unlinked(
        &self,
        provider_type: &str,
        has_links_snapshot: bool,
    ) -> Result<bool, DataLayerError> {
        if has_links_snapshot {
            return Ok(false);
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_sql_err()?;
        let provider_exists: Option<String> =
            sqlx::query_scalar("SELECT provider_type FROM oauth_providers WHERE provider_type = ?")
                .bind(provider_type)
                .fetch_optional(&mut *tx)
                .await
                .map_sql_err()?;
        if provider_exists.is_none() {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let result = sqlx::query(
            "DELETE FROM oauth_providers WHERE provider_type = ? AND NOT EXISTS (SELECT 1 FROM user_oauth_links WHERE user_oauth_links.provider_type = oauth_providers.provider_type)",
        )
            .bind(provider_type)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }
}

fn now_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn optional_unix_secs(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

fn json_to_string(value: Option<&serde_json::Value>) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!("invalid OAuth provider JSON field: {err}"))
            })
        })
        .transpose()
}

fn json_from_string(
    value: Option<String>,
    field_name: &str,
) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains invalid JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn scopes_to_json_string(scopes: Option<&Vec<String>>) -> Result<Option<String>, DataLayerError> {
    json_to_string(
        scopes
            .map(|items| {
                serde_json::Value::Array(
                    items
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                )
            })
            .as_ref(),
    )
}

fn parse_scopes(value: Option<String>) -> Result<Option<Vec<String>>, DataLayerError> {
    let Some(value) = json_from_string(value, "oauth_providers.scopes")? else {
        return Ok(None);
    };
    parse_scopes_value(&value)
}

fn parse_scopes_value(value: &serde_json::Value) -> Result<Option<Vec<String>>, DataLayerError> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(items) => parse_scopes_array(items).map(Some),
        serde_json::Value::String(raw) => parse_embedded_scopes(raw),
        _ => Err(DataLayerError::UnexpectedValue(
            "oauth_providers.scopes is not a JSON array".to_string(),
        )),
    }
}

fn parse_embedded_scopes(raw: &str) -> Result<Option<Vec<String>>, DataLayerError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
        return Ok(None);
    }

    if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(raw) {
        return parse_scopes_value(&decoded);
    }

    Ok(Some(vec![raw.to_string()]))
}

fn parse_scopes_array(items: &[serde_json::Value]) -> Result<Vec<String>, DataLayerError> {
    let mut scopes = Vec::with_capacity(items.len());
    for item in items {
        let Some(scope) = item.as_str() else {
            return Err(DataLayerError::UnexpectedValue(
                "oauth_providers.scopes contains non-string value".to_string(),
            ));
        };
        let scope = scope.trim();
        if !scope.is_empty() {
            scopes.push(scope.to_string());
        }
    }
    Ok(scopes)
}

fn map_oauth_provider_row(row: &SqliteRow) -> Result<StoredOAuthProviderConfig, DataLayerError> {
    Ok(StoredOAuthProviderConfig::new(
        row.try_get("provider_type").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("client_id").map_sql_err()?,
        row.try_get("redirect_uri").map_sql_err()?,
        row.try_get("frontend_callback_url").map_sql_err()?,
    )?
    .with_config_fields(
        row.try_get("client_secret_encrypted").map_sql_err()?,
        row.try_get("authorization_url_override").map_sql_err()?,
        row.try_get("token_url_override").map_sql_err()?,
        row.try_get("userinfo_url_override").map_sql_err()?,
        parse_scopes(row.try_get("scopes").map_sql_err()?)?,
        json_from_string(
            row.try_get("attribute_mapping").map_sql_err()?,
            "oauth_providers.attribute_mapping",
        )?,
        json_from_string(
            row.try_get("extra_config").map_sql_err()?,
            "oauth_providers.extra_config",
        )?,
        row.try_get("icon_url").map_sql_err()?,
        row.try_get("is_enabled").map_sql_err()?,
    )
    .with_timestamps(
        optional_unix_secs(row.try_get("created_at_unix_ms").map_sql_err()?),
        optional_unix_secs(row.try_get("updated_at_unix_secs").map_sql_err()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::SqliteOAuthProviderRepository;
    use crate::run_migrations;
    use aether_data_contracts::repository::oauth_providers::{
        EncryptedSecretUpdate, OAuthProviderReadRepository, OAuthProviderWriteRepository,
        UpsertOAuthProviderConfigOutcome, UpsertOAuthProviderConfigRecord,
    };

    fn sample_upsert(provider_type: &str) -> UpsertOAuthProviderConfigRecord {
        UpsertOAuthProviderConfigRecord {
            provider_type: provider_type.to_string(),
            display_name: format!("{provider_type} display"),
            client_id: format!("{provider_type}-client"),
            client_secret_encrypted: EncryptedSecretUpdate::Preserve,
            authorization_url_override: Some(
                "https://connect.linux.do/oauth2/authorize".to_string(),
            ),
            token_url_override: Some("https://connect.linux.do/oauth2/token".to_string()),
            userinfo_url_override: None,
            scopes: Some(vec!["openid".to_string(), "profile".to_string()]),
            redirect_uri: format!("https://{provider_type}.example.com/redirect"),
            frontend_callback_url: "https://frontend.example.com/auth/callback".to_string(),
            attribute_mapping: Some(serde_json::json!({"email": "email"})),
            extra_config: Some(serde_json::json!({"team": true})),
            icon_url: None,
            is_enabled: true,
        }
    }

    #[tokio::test]
    async fn sqlite_repository_round_trips_oauth_provider_configs() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_migrations(&pool)
            .await
            .expect("sqlite migrations should run");

        let repository = SqliteOAuthProviderRepository::new(pool.clone());
        let created = repository
            .upsert_oauth_provider_config(&UpsertOAuthProviderConfigRecord {
                client_secret_encrypted: EncryptedSecretUpdate::Set("secret-1".to_string()),
                ..sample_upsert("linuxdo")
            })
            .await
            .expect("provider should upsert");
        assert_eq!(created.client_secret_encrypted.as_deref(), Some("secret-1"));
        assert_eq!(
            created.scopes,
            Some(vec!["openid".to_string(), "profile".to_string()])
        );

        let updated = repository
            .upsert_oauth_provider_config(&UpsertOAuthProviderConfigRecord {
                client_secret_encrypted: EncryptedSecretUpdate::Preserve,
                display_name: "Linux.do".to_string(),
                ..sample_upsert("linuxdo")
            })
            .await
            .expect("provider should update");
        assert_eq!(updated.display_name, "Linux.do");
        assert_eq!(updated.client_secret_encrypted.as_deref(), Some("secret-1"));
        assert!(
            repository
                .compare_and_swap_oauth_provider_client_secret(
                    "linuxdo",
                    "secret-1",
                    "record-bound-v2",
                )
                .await
                .expect("client secret CAS should execute")
        );
        let migrated = repository
            .get_oauth_provider_config("linuxdo")
            .await
            .expect("provider should fetch")
            .expect("provider should exist");
        assert_eq!(migrated.display_name, "Linux.do");
        assert_eq!(migrated.updated_at_unix_secs, updated.updated_at_unix_secs);
        assert_eq!(
            migrated.client_secret_encrypted.as_deref(),
            Some("record-bound-v2")
        );
        assert!(!repository
            .compare_and_swap_oauth_provider_client_secret("linuxdo", "secret-1", "must-not-win",)
            .await
            .expect("stale client secret CAS should execute"));

        let listed = repository
            .list_oauth_provider_configs()
            .await
            .expect("providers should list");
        assert_eq!(listed.len(), 1);

        let fetched = repository
            .get_oauth_provider_config("linuxdo")
            .await
            .expect("provider should fetch")
            .expect("provider should exist");
        assert_eq!(
            fetched.attribute_mapping,
            Some(serde_json::json!({"email": "email"}))
        );

        sqlx::query(
            r#"
INSERT INTO users (
  id, email, username, role, auth_source, is_active, is_deleted, created_at, updated_at
) VALUES
  ('user-oauth', 'oauth@example.com', 'oauth-user', 'user', 'oauth', 1, 0, 1, 1),
  ('user-local', 'local@example.com', 'local-user', 'user', 'local', 1, 0, 1, 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("users should seed");
        sqlx::query(
            r#"
INSERT INTO user_oauth_links (
  id, user_id, provider_type, provider_user_id, linked_at
) VALUES
  ('link-1', 'user-oauth', 'linuxdo', 'linuxdo-1', 1),
  ('link-2', 'user-local', 'linuxdo', 'linuxdo-2', 1)
"#,
        )
        .execute(&pool)
        .await
        .expect("oauth links should seed");
        assert_eq!(
            repository
                .count_locked_users_if_provider_disabled("linuxdo", false)
                .await
                .expect("locked users should count"),
            1
        );
        assert_eq!(
            repository
                .count_locked_users_if_provider_disabled("linuxdo", true)
                .await
                .expect("locked users should count"),
            2
        );

        assert_eq!(
            repository
                .upsert_oauth_provider_config_guarded(
                    &UpsertOAuthProviderConfigRecord {
                        is_enabled: false,
                        ..sample_upsert("linuxdo")
                    },
                    false,
                    false,
                    0,
                )
                .await
                .expect("guarded disable should resolve"),
            UpsertOAuthProviderConfigOutcome::DisableRequiresConfirmation { affected_count: 1 }
        );
        let forced = repository
            .upsert_oauth_provider_config_guarded(
                &UpsertOAuthProviderConfigRecord {
                    is_enabled: false,
                    ..sample_upsert("linuxdo")
                },
                false,
                true,
                0,
            )
            .await
            .expect("forced provider disable should succeed");
        assert!(matches!(
            forced,
            UpsertOAuthProviderConfigOutcome::Upserted(provider) if !provider.is_enabled
        ));

        assert!(!repository
            .delete_oauth_provider_config_if_unlinked("linuxdo", false)
            .await
            .expect("linked provider deletion should resolve"));
        assert!(repository
            .get_oauth_provider_config("linuxdo")
            .await
            .expect("provider should fetch")
            .is_some());

        sqlx::query("DELETE FROM user_oauth_links WHERE provider_type = 'linuxdo'")
            .execute(&pool)
            .await
            .expect("links should delete");
        assert!(repository
            .delete_oauth_provider_config_if_unlinked("linuxdo", false)
            .await
            .expect("unlinked provider should delete"));
    }
}
