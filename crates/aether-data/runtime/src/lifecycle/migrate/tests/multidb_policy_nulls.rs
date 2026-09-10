use super::policy_nulls::{POLICY_COLUMNS, POLICY_NULL_MIGRATION_VERSION};
use aether_data_contracts::repository::{
    auth::{AuthApiKeyLookupKey, AuthApiKeyReadRepository},
    management_tokens::ManagementTokenReadRepository,
    users::UserReadRepository,
};
use serde_json::{json, Value};
use sqlx::migrate::Migrate;

const LEGACY_POLICY_FIXTURES: &str = r#"
INSERT INTO users (id, username, role, password_hash, created_at, updated_at)
VALUES ('policy-owner', 'policy-owner', 'user', 'preserved-password-hash', 1, 1),
       ('legacy-policy-user', 'legacy-policy-user', 'user', 'preserved-password-hash', 1, 1);
UPDATE users
SET allowed_providers = 'null', allowed_api_formats = '"null"', allowed_models = '""',
    allowed_models_mode = 'deny_all'
WHERE id = 'legacy-policy-user';
INSERT INTO api_keys (
    id, user_id, key_hash, allowed_providers, allowed_api_formats, allowed_models, ip_rules,
    created_at, updated_at
) VALUES (
    'legacy-policy-key', 'policy-owner',
    'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    'null', '"null"', '""', 'null', 1, 1
);
INSERT INTO user_groups (
    id, name, normalized_name, allowed_providers, allowed_api_formats, allowed_models,
    allowed_providers_mode, allowed_api_formats_mode, allowed_models_mode, created_at, updated_at
) VALUES (
    'legacy-policy-group', 'Legacy policy', 'legacy-policy', 'null', '"null"', '""',
    'deny_all', 'specific', 'inherit', 1, 1
);
INSERT INTO management_tokens (
    id, user_id, token_hash, name, allowed_ips, permissions, created_at, updated_at
) VALUES (
    'legacy-policy-token', 'policy-owner',
    'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
    'Legacy token', 'null', 'null', 1, 1
), (
    'restricted-policy-token', 'policy-owner',
    'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',
    'Restricted token', '["127.0.0.1"]', '["admin:users:read"]', 1, 1
);
"#;

async fn assert_legacy_policy_reads_fail(
    auth: &dyn AuthApiKeyReadRepository,
    users: &dyn UserReadRepository,
) {
    let key_error = auth
        .find_api_key_snapshot(AuthApiKeyLookupKey::ApiKeyId("legacy-policy-key"))
        .await
        .unwrap_err();
    assert!(key_error.to_string().contains("allowed_"), "{key_error}");
    let user_error = users
        .find_user_auth_by_id("legacy-policy-user")
        .await
        .unwrap_err();
    assert!(user_error.to_string().contains("allowed_"), "{user_error}");
    let group_error = users
        .find_user_group_by_id("legacy-policy-group")
        .await
        .unwrap_err();
    assert!(
        group_error.to_string().contains("allowed_"),
        "{group_error}"
    );
}

async fn assert_upgraded_policy_reads(
    auth: &dyn AuthApiKeyReadRepository,
    users: &dyn UserReadRepository,
    tokens: &dyn ManagementTokenReadRepository,
) {
    let key = auth
        .find_api_key_snapshot(AuthApiKeyLookupKey::ApiKeyId("legacy-policy-key"))
        .await
        .unwrap()
        .unwrap();
    assert!(key.api_key_allowed_providers.is_none());
    assert!(key.api_key_allowed_api_formats.is_none());
    assert!(key.api_key_allowed_models.is_none());
    assert!(key.api_key_ip_rules.is_none());
    let user = users
        .find_user_auth_by_id("legacy-policy-user")
        .await
        .unwrap()
        .unwrap();
    assert!(user.allowed_providers.is_none());
    assert!(user.allowed_api_formats.is_none());
    assert!(user.allowed_models.is_none());
    assert_eq!(user.allowed_models_mode, "deny_all");
    assert_eq!(
        user.password_hash.as_deref(),
        Some("preserved-password-hash")
    );
    let group = users
        .find_user_group_by_id("legacy-policy-group")
        .await
        .unwrap()
        .unwrap();
    assert!(group.allowed_providers.is_none());
    assert!(group.allowed_api_formats.is_none());
    assert!(group.allowed_models.is_none());
    assert_eq!(group.allowed_providers_mode, "deny_all");
    assert_eq!(group.allowed_api_formats_mode, "specific");
    assert_eq!(group.allowed_models_mode, "inherit");
    let token = tokens
        .get_management_token_with_user("legacy-policy-token")
        .await
        .unwrap()
        .unwrap();
    assert!(token.token.allowed_ips.is_none());
    assert!(token.token.permissions.is_none());
    let restricted = tokens
        .get_management_token_with_user("restricted-policy-token")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restricted.token.allowed_ips, Some(json!(["127.0.0.1"])));
    assert_eq!(
        restricted.token.permissions,
        Some(json!(["admin:users:read"]))
    );
}

#[tokio::test]
async fn mysql_full_legacy_schema_upgrades_without_rewriting_history() {
    let Ok(database_url) = std::env::var("AETHER_TEST_MYSQL_URL") else {
        eprintln!("skipping MySQL upgrade test: AETHER_TEST_MYSQL_URL is unset");
        return;
    };
    let admin = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let database = format!("aether_policy_upgrade_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let options = database_url
        .parse::<sqlx::mysql::MySqlConnectOptions>()
        .unwrap()
        .database(&database);
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    connection.ensure_migrations_table().await.unwrap();
    for migration in aether_data_mysql::MIGRATOR
        .iter()
        .filter(|migration| migration.version < POLICY_NULL_MIGRATION_VERSION)
    {
        connection.apply(migration).await.unwrap();
    }
    let previous_checksums = connection
        .list_applied_migrations()
        .await
        .unwrap()
        .into_iter()
        .map(|migration| (migration.version, migration.checksum.into_owned()))
        .collect::<Vec<_>>();
    drop(connection);
    sqlx::raw_sql(LEGACY_POLICY_FIXTURES)
        .execute(&pool)
        .await
        .unwrap();
    let auth = aether_data_mysql::MysqlAuthApiKeyReadRepository::new(pool.clone());
    let users = aether_data_mysql::MysqlUserReadRepository::new(pool.clone());
    let tokens = aether_data_mysql::MysqlManagementTokenRepository::new(pool.clone());
    assert_legacy_policy_reads_fail(&auth, &users).await;

    let pending = aether_data_mysql::prepare_database_for_startup(&pool)
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![POLICY_NULL_MIGRATION_VERSION]
    );
    for _ in 0..2 {
        aether_data_mysql::run_migrations(&pool).await.unwrap();
        assert!(aether_data_mysql::prepare_database_for_startup(&pool)
            .await
            .unwrap()
            .is_empty());
        assert_upgraded_policy_reads(&auth, &users, &tokens).await;
    }
    let mut connection = pool.acquire().await.unwrap();
    let upgraded_checksums = connection
        .list_applied_migrations()
        .await
        .unwrap()
        .into_iter()
        .filter(|migration| migration.version < POLICY_NULL_MIGRATION_VERSION)
        .map(|migration| (migration.version, migration.checksum.into_owned()))
        .collect::<Vec<_>>();
    assert_eq!(upgraded_checksums, previous_checksums);
    drop(connection);
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
async fn sqlite_full_legacy_schema_upgrades_without_rewriting_history() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    connection.ensure_migrations_table().await.unwrap();
    for migration in aether_data_sqlite::MIGRATOR
        .iter()
        .filter(|migration| migration.version < POLICY_NULL_MIGRATION_VERSION)
    {
        connection.apply(migration).await.unwrap();
    }
    let previous_checksums = connection
        .list_applied_migrations()
        .await
        .unwrap()
        .into_iter()
        .map(|migration| (migration.version, migration.checksum.into_owned()))
        .collect::<Vec<_>>();
    drop(connection);
    sqlx::raw_sql(LEGACY_POLICY_FIXTURES)
        .execute(&pool)
        .await
        .unwrap();
    let auth = aether_data_sqlite::SqliteAuthApiKeyReadRepository::new(pool.clone());
    let users = aether_data_sqlite::SqliteUserReadRepository::new(pool.clone());
    let tokens = aether_data_sqlite::SqliteManagementTokenRepository::new(pool.clone());
    assert_legacy_policy_reads_fail(&auth, &users).await;

    let pending = aether_data_sqlite::prepare_database_for_startup(&pool)
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![POLICY_NULL_MIGRATION_VERSION]
    );
    for _ in 0..2 {
        aether_data_sqlite::run_migrations(&pool).await.unwrap();
        assert!(aether_data_sqlite::prepare_database_for_startup(&pool)
            .await
            .unwrap()
            .is_empty());
        assert_upgraded_policy_reads(&auth, &users, &tokens).await;
    }
    let mut connection = pool.acquire().await.unwrap();
    let upgraded_checksums = connection
        .list_applied_migrations()
        .await
        .unwrap()
        .into_iter()
        .filter(|migration| migration.version < POLICY_NULL_MIGRATION_VERSION)
        .map(|migration| (migration.version, migration.checksum.into_owned()))
        .collect::<Vec<_>>();
    assert_eq!(upgraded_checksums, previous_checksums);
    drop(connection);
    pool.close().await;
}

fn policy_cases() -> Vec<Option<Value>> {
    vec![
        None,
        Some(Value::Null),
        Some(json!("null")),
        Some(json!(" NULL ")),
        Some(json!("\tNuLl\r\n")),
        Some(json!("")),
        Some(json!(" \t\r\n")),
        Some(json!([])),
        Some(json!(["provider-allowed"])),
        Some(json!(["null"])),
        Some(json!("provider-allowed")),
        Some(json!("null-provider")),
        Some(json!("nullnull")),
        Some(json!(["127.0.0.1/32"])),
        Some(json!([null])),
        Some(json!({})),
        Some(json!({"policy": null})),
        Some(json!(true)),
        Some(json!(42)),
    ]
}

#[tokio::test]
async fn mysql_policy_null_migration_preserves_explicit_policies_and_is_idempotent() {
    let Ok(database_url) = std::env::var("AETHER_TEST_MYSQL_URL") else {
        eprintln!("skipping MySQL policy migration test: AETHER_TEST_MYSQL_URL is unset");
        return;
    };
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();

    let migration = aether_data_mysql::MIGRATOR
        .iter()
        .find(|migration| migration.version == POLICY_NULL_MIGRATION_VERSION)
        .unwrap();
    let cases = policy_cases();
    for &(table, columns) in POLICY_COLUMNS {
        let definitions = columns
            .iter()
            .map(|column| format!("{column} TEXT"))
            .collect::<Vec<_>>()
            .join(", ");
        sqlx::query(&format!(
            "CREATE TEMPORARY TABLE {table} (id INTEGER PRIMARY KEY, {definitions}, metadata JSON)"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let fields = columns.join(", ");
        let placeholders = vec!["?"; columns.len()].join(", ");
        for (index, input) in cases.iter().enumerate() {
            let sql = format!(
                "INSERT INTO {table} (id, {fields}, metadata) VALUES (?, {placeholders}, 'null')"
            );
            let mut insert = sqlx::query(&sql).bind(index as i32);
            for _ in columns {
                insert = insert.bind(input.as_ref().map(Value::to_string));
            }
            insert.execute(&pool).await.unwrap();
        }
    }

    sqlx::query(
        "INSERT INTO users (id, allowed_models, metadata) VALUES (1000, 'malformed-json', 'null')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        let malformed: String =
            sqlx::query_scalar("SELECT allowed_models FROM users WHERE id = 1000")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(malformed, "malformed-json");
        for &(table, columns) in POLICY_COLUMNS {
            for column in columns {
                let values: Vec<Option<String>> = sqlx::query_scalar(&format!(
                    "SELECT CAST({column} AS CHAR) FROM {table} WHERE id < ? ORDER BY id",
                ))
                .bind(cases.len() as i32)
                .fetch_all(&pool)
                .await
                .unwrap();
                let values = values
                    .into_iter()
                    .map(|value| value.map(|value| serde_json::from_str::<Value>(&value).unwrap()))
                    .collect::<Vec<_>>();
                let expected = cases
                    .iter()
                    .map(|input| {
                        input.clone().filter(|value| {
                            if value.is_null() {
                                return false;
                            }
                            table == "management_tokens"
                                || value.as_str().is_none_or(|text| {
                                    !text.trim().is_empty()
                                        && !text.trim().eq_ignore_ascii_case("null")
                                })
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(values, expected, "{table}.{column}");
            }
            let metadata: Vec<Option<String>> =
                sqlx::query_scalar(&format!("SELECT CAST(metadata AS CHAR) FROM {table}"))
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(metadata
                .iter()
                .all(|value| value.as_deref() == Some("null")));
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn sqlite_policy_null_migration_preserves_explicit_policies_and_is_idempotent() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    let migration = aether_data_sqlite::MIGRATOR
        .iter()
        .find(|migration| migration.version == POLICY_NULL_MIGRATION_VERSION)
        .unwrap();
    let cases = policy_cases();
    for &(table, columns) in POLICY_COLUMNS {
        let definitions = columns
            .iter()
            .map(|column| format!("{column} TEXT"))
            .collect::<Vec<_>>()
            .join(", ");
        sqlx::query(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, {definitions}, metadata TEXT)"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let fields = columns.join(", ");
        let placeholders = vec!["?"; columns.len()].join(", ");
        for (index, input) in cases.iter().enumerate() {
            let sql = format!(
                "INSERT INTO {table} (id, {fields}, metadata) VALUES (?, {placeholders}, 'null')"
            );
            let mut insert = sqlx::query(&sql).bind(index as i32);
            for _ in columns {
                insert = insert.bind(input.as_ref().map(Value::to_string));
            }
            insert.execute(&pool).await.unwrap();
        }
    }
    sqlx::query(
        "INSERT INTO users (id, allowed_models, metadata) VALUES (1000, 'malformed-json', 'null')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        for &(table, columns) in POLICY_COLUMNS {
            for column in columns {
                let values: Vec<Option<String>> = sqlx::query_scalar(&format!(
                    "SELECT {column} FROM {table} WHERE id < ? ORDER BY id",
                ))
                .bind(cases.len() as i32)
                .fetch_all(&pool)
                .await
                .unwrap();
                let values = values
                    .into_iter()
                    .map(|value| value.map(|value| serde_json::from_str::<Value>(&value).unwrap()))
                    .collect::<Vec<_>>();
                let expected = cases
                    .iter()
                    .map(|input| {
                        input.clone().filter(|value| {
                            if value.is_null() {
                                return false;
                            }
                            table == "management_tokens"
                                || value.as_str().is_none_or(|text| {
                                    !text.trim().is_empty()
                                        && !text.trim().eq_ignore_ascii_case("null")
                                })
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(values, expected, "{table}.{column}");
            }
            let metadata: Vec<Option<String>> =
                sqlx::query_scalar(&format!("SELECT metadata FROM {table}"))
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(metadata
                .iter()
                .all(|value| value.as_deref() == Some("null")));
        }
        let malformed: String =
            sqlx::query_scalar("SELECT allowed_models FROM users WHERE id = 1000")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(malformed, "malformed-json");
    }
    pool.close().await;
}
