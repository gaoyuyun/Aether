use super::*;
use crate::{SqlDatabaseConfig, SqlPoolConfig};

fn credential_fixture(driver: DatabaseDriver) -> (String, String, String, String) {
    let user_id = uuid::Uuid::new_v4().to_string();
    let key_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let input = encode_jsonl(&[
        DataExportRecord::manifest(DataExportManifest::new(
            1_788_739_200,
            Some(driver),
            vec![
                ExportDomain::Users,
                ExportDomain::ApiKeys,
                ExportDomain::Auxiliary,
            ],
        )),
        DataExportRecord::row(
            ExportDomain::Users,
            &user_id,
            json!({
                "id": user_id, "username": user_id,
                "email": format!("{user_id}@example.test"),
                "password_hash": "trusted-password-hash",
                "created_at": 1_788_739_200, "updated_at": 1_788_739_200,
            }),
        ),
        DataExportRecord::row(
            ExportDomain::ApiKeys,
            &key_id,
            json!({
                "id": key_id, "user_id": user_id, "key_hash": key_id,
                "key_encrypted": "trusted-ciphertext", "name": "Import probe",
                "is_active": true, "is_locked": false, "status": "active",
                "created_at": 1_788_739_200, "updated_at": 1_788_739_200,
            }),
        ),
        DataExportRecord::row(
            ExportDomain::Auxiliary,
            &session_id,
            json!({
                "__table": "user_sessions", "id": session_id, "user_id": user_id,
                "client_device_id": "fixture-device",
                "refresh_token_hash": "old-session-hash",
                "prev_refresh_token_hash": "older-session-hash",
                "last_seen_at": 1_788_739_200, "expires_at": 2_000_000_000,
                "revoked_at": null, "revoke_reason": null,
                "created_at": 1_788_739_200, "updated_at": 1_788_739_200,
            }),
        ),
    ])
    .unwrap();
    (input, user_id, key_id, session_id)
}

#[tokio::test]
async fn mysql_import_credential_options_preserve_keys_but_revoke_sessions() {
    let Ok(database_url) = std::env::var("AETHER_TEST_MYSQL_URL") else {
        eprintln!("skipping MySQL import credentials test: AETHER_TEST_MYSQL_URL is unset");
        return;
    };
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    run_mysql_migrations(&pool).await.unwrap();

    for preserve_credentials in [false, true] {
        let (input, user_id, key_id, session_id) = credential_fixture(DatabaseDriver::Mysql);
        let database = SqlDatabaseConfig::new(
            DatabaseDriver::Mysql,
            &database_url,
            SqlPoolConfig {
                max_connections: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            super::super::import_database_jsonl_with_options(
                database,
                &input,
                DataImportOptions {
                    preserve_credentials
                },
            )
            .await
            .unwrap(),
            3
        );
        let password: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let key: (String, Option<String>, bool) =
            sqlx::query_as("SELECT key_hash, key_encrypted, is_active FROM api_keys WHERE id = ?")
                .bind(&key_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(password == "trusted-password-hash", preserve_credentials);
        assert_eq!(key.0 == key_id, preserve_credentials);
        assert_eq!(
            key.1.as_deref(),
            preserve_credentials.then_some("trusted-ciphertext")
        );
        assert_eq!(key.2, preserve_credentials);

        let session: (String, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT refresh_token_hash, prev_refresh_token_hash, revoked_at, revoke_reason FROM user_sessions WHERE id = ?",
        ).bind(&session_id).fetch_one(&pool).await.unwrap();
        assert_ne!(session.0, "old-session-hash");
        assert!(session.1.is_none());
        assert!(
            session.2.is_some(),
            "sessions must remain revoked even when credentials are preserved"
        );
        assert_eq!(session.3.as_deref(), Some("imported_credentials_revoked"));

        sqlx::query("DELETE FROM user_sessions WHERE id = ?")
            .bind(&session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(&key_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

#[tokio::test]
async fn sqlite_import_credential_options_preserve_keys_but_revoke_sessions() {
    let path = std::env::temp_dir().join(format!(
        "aether-import-credentials-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let database_url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    run_sqlite_migrations(&pool).await.unwrap();

    for preserve_credentials in [false, true] {
        let (input, user_id, key_id, session_id) = credential_fixture(DatabaseDriver::Sqlite);
        let database = SqlDatabaseConfig::new(
            DatabaseDriver::Sqlite,
            &database_url,
            SqlPoolConfig {
                max_connections: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            super::super::import_database_jsonl_with_options(
                database,
                &input,
                DataImportOptions {
                    preserve_credentials
                },
            )
            .await
            .unwrap(),
            3
        );
        let password: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let key: (String, Option<String>, bool) =
            sqlx::query_as("SELECT key_hash, key_encrypted, is_active FROM api_keys WHERE id = ?")
                .bind(&key_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(password == "trusted-password-hash", preserve_credentials);
        assert_eq!(key.0 == key_id, preserve_credentials);
        assert_eq!(
            key.1.as_deref(),
            preserve_credentials.then_some("trusted-ciphertext")
        );
        assert_eq!(key.2, preserve_credentials);

        let session: (String, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT refresh_token_hash, prev_refresh_token_hash, revoked_at, revoke_reason FROM user_sessions WHERE id = ?",
        ).bind(&session_id).fetch_one(&pool).await.unwrap();
        assert_ne!(session.0, "old-session-hash");
        assert!(session.1.is_none());
        assert!(
            session.2.is_some(),
            "sessions must remain revoked even when credentials are preserved"
        );
        assert_eq!(session.3.as_deref(), Some("imported_credentials_revoked"));

        sqlx::query("DELETE FROM user_sessions WHERE id = ?")
            .bind(&session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(&key_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    std::fs::remove_file(path).unwrap();
}
