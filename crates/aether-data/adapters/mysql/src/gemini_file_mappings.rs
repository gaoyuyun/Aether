use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, QueryBuilder, Row};

use aether_data_contracts::repository::gemini_file_mappings::{
    GeminiFileMappingListQuery, GeminiFileMappingMimeTypeCount, GeminiFileMappingReadRepository,
    GeminiFileMappingStats, GeminiFileMappingWriteRepository, StoredGeminiFileMapping,
    StoredGeminiFileMappingListPage, UpsertGeminiFileMappingRecord,
};
use aether_data_contracts::DataLayerError;
use aether_data_query::{push_ci_contains_any, push_limit_offset, SqlDialect, WhereClause};

use crate::error::SqlResultExt;
use crate::MysqlPool;

const OWNER_GUARDED_UPSERT_SQL: &str = r#"
INSERT INTO gemini_file_mappings (
  id, file_name, key_id, user_id, display_name, mime_type, source_hash,
  created_at, expires_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  display_name = IF(BINARY file_name = BINARY VALUES(file_name) AND BINARY key_id = BINARY VALUES(key_id) AND BINARY user_id <=> BINARY VALUES(user_id), VALUES(display_name), display_name),
  mime_type = IF(BINARY file_name = BINARY VALUES(file_name) AND BINARY key_id = BINARY VALUES(key_id) AND BINARY user_id <=> BINARY VALUES(user_id), VALUES(mime_type), mime_type),
  source_hash = IF(BINARY file_name = BINARY VALUES(file_name) AND BINARY key_id = BINARY VALUES(key_id) AND BINARY user_id <=> BINARY VALUES(user_id), VALUES(source_hash), source_hash),
  expires_at = IF(BINARY file_name = BINARY VALUES(file_name) AND BINARY key_id = BINARY VALUES(key_id) AND BINARY user_id <=> BINARY VALUES(user_id), VALUES(expires_at), expires_at)
"#;

#[derive(Debug, Clone)]
pub struct MysqlGeminiFileMappingRepository {
    pool: MysqlPool,
}

impl MysqlGeminiFileMappingRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }

    async fn reload_by_file_name(
        &self,
        file_name: &str,
    ) -> Result<StoredGeminiFileMapping, DataLayerError> {
        self.find_by_file_name(file_name).await?.ok_or_else(|| {
            DataLayerError::UnexpectedValue("gemini file mapping missing after write".to_string())
        })
    }
}

#[async_trait]
impl GeminiFileMappingReadRepository for MysqlGeminiFileMappingRepository {
    async fn find_by_file_name(
        &self,
        file_name: &str,
    ) -> Result<Option<StoredGeminiFileMapping>, DataLayerError> {
        let row = sqlx::query(
            r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
WHERE file_name = ?
LIMIT 1
"#,
        )
        .bind(file_name)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_row).transpose()
    }

    async fn find_active_by_file_name_for_user(
        &self,
        file_name: &str,
        user_id: &str,
        now_unix_secs: u64,
    ) -> Result<Option<StoredGeminiFileMapping>, DataLayerError> {
        let row = sqlx::query(
            r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
WHERE BINARY file_name = BINARY ?
  AND BINARY user_id = BINARY ?
  AND expires_at > ?
LIMIT 1
"#,
        )
        .bind(file_name)
        .bind(user_id)
        .bind(i64_from_u64(
            now_unix_secs,
            "gemini_file_mappings.owner_read_now",
        )?)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_row).transpose()
    }

    async fn find_active_by_file_name_for_owner(
        &self,
        file_name: &str,
        key_id: &str,
        user_id: &str,
        now_unix_secs: u64,
    ) -> Result<Option<StoredGeminiFileMapping>, DataLayerError> {
        let row = sqlx::query(
            r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
WHERE BINARY file_name = BINARY ?
  AND BINARY key_id = BINARY ?
  AND BINARY user_id = BINARY ?
  AND expires_at > ?
LIMIT 1
"#,
        )
        .bind(file_name)
        .bind(key_id)
        .bind(user_id)
        .bind(i64_from_u64(
            now_unix_secs,
            "gemini_file_mappings.owner_read_now",
        )?)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_row).transpose()
    }

    async fn list_mappings(
        &self,
        query: &GeminiFileMappingListQuery,
    ) -> Result<StoredGeminiFileMappingListPage, DataLayerError> {
        let total = build_list_count_query(query)
            .build_query_scalar::<i64>()
            .fetch_one(&self.pool)
            .await
            .map_sql_err()?;
        let rows = build_list_rows_query(query)
            .build()
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;
        let items = rows.iter().map(map_row).collect::<Result<Vec<_>, _>>()?;
        Ok(StoredGeminiFileMappingListPage {
            items,
            total: usize::try_from(total).unwrap_or_default(),
        })
    }

    async fn summarize_mappings(
        &self,
        now_unix_secs: u64,
    ) -> Result<GeminiFileMappingStats, DataLayerError> {
        let totals = sqlx::query(
            r#"
SELECT
  COUNT(*) AS total_mappings,
  SUM(CASE WHEN expires_at > ? THEN 1 ELSE 0 END) AS active_mappings
FROM gemini_file_mappings
"#,
        )
        .bind(now_unix_secs as i64)
        .fetch_one(&self.pool)
        .await
        .map_sql_err()?;
        let total_mappings =
            usize::try_from(totals.try_get::<i64, _>("total_mappings").map_sql_err()?)
                .unwrap_or_default();
        let active_mappings = usize::try_from(
            totals
                .try_get::<Option<i64>, _>("active_mappings")
                .map_sql_err()?
                .unwrap_or(0),
        )
        .unwrap_or_default();
        let by_mime_type_rows = sqlx::query(
            r#"
SELECT
  COALESCE(NULLIF(TRIM(mime_type), ''), 'unknown') AS mime_type,
  COUNT(*) AS count
FROM gemini_file_mappings
WHERE expires_at > ?
GROUP BY COALESCE(NULLIF(TRIM(mime_type), ''), 'unknown')
ORDER BY mime_type ASC
"#,
        )
        .bind(now_unix_secs as i64)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        let by_mime_type = by_mime_type_rows
            .iter()
            .map(|row| {
                Ok(GeminiFileMappingMimeTypeCount {
                    mime_type: row.try_get("mime_type").map_sql_err()?,
                    count: usize::try_from(row.try_get::<i64, _>("count").map_sql_err()?)
                        .unwrap_or_default(),
                })
            })
            .collect::<Result<Vec<_>, DataLayerError>>()?;
        Ok(GeminiFileMappingStats {
            total_mappings,
            active_mappings,
            expired_mappings: total_mappings.saturating_sub(active_mappings),
            by_mime_type,
        })
    }
}

#[async_trait]
impl GeminiFileMappingWriteRepository for MysqlGeminiFileMappingRepository {
    async fn upsert(
        &self,
        record: UpsertGeminiFileMappingRecord,
    ) -> Result<StoredGeminiFileMapping, DataLayerError> {
        record.validate()?;
        sqlx::query(
            r#"
INSERT INTO gemini_file_mappings (
  id, file_name, key_id, user_id, display_name, mime_type, source_hash,
  created_at, expires_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  key_id = VALUES(key_id),
  user_id = VALUES(user_id),
  display_name = VALUES(display_name),
  mime_type = VALUES(mime_type),
  source_hash = VALUES(source_hash),
  expires_at = VALUES(expires_at)
"#,
        )
        .bind(&record.id)
        .bind(&record.file_name)
        .bind(&record.key_id)
        .bind(&record.user_id)
        .bind(&record.display_name)
        .bind(&record.mime_type)
        .bind(&record.source_hash)
        .bind(current_unix_secs() as i64)
        .bind(i64_from_u64(
            record.expires_at_unix_secs,
            "gemini_file_mappings.expires_at",
        )?)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        self.reload_by_file_name(&record.file_name).await
    }

    async fn upsert_if_owner_matches(
        &self,
        record: UpsertGeminiFileMappingRecord,
    ) -> Result<Option<StoredGeminiFileMapping>, DataLayerError> {
        record.validate()?;
        let mut transaction = self.pool.begin().await.map_sql_err()?;
        sqlx::query(OWNER_GUARDED_UPSERT_SQL)
            .bind(&record.id)
            .bind(&record.file_name)
            .bind(&record.key_id)
            .bind(&record.user_id)
            .bind(&record.display_name)
            .bind(&record.mime_type)
            .bind(&record.source_hash)
            .bind(current_unix_secs() as i64)
            .bind(i64_from_u64(
                record.expires_at_unix_secs,
                "gemini_file_mappings.expires_at",
            )?)
            .execute(&mut *transaction)
            .await
            .map_sql_err()?;

        let row = sqlx::query(
            r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
WHERE file_name = ?
LIMIT 1
FOR UPDATE
"#,
        )
        .bind(&record.file_name)
        .fetch_one(&mut *transaction)
        .await
        .map_sql_err()?;
        let stored = map_row(&row)?;
        let owner_matches = stored.file_name == record.file_name
            && stored.key_id == record.key_id
            && stored.user_id == record.user_id;
        transaction.commit().await.map_sql_err()?;

        Ok(owner_matches.then_some(stored))
    }

    async fn delete_by_file_name(&self, file_name: &str) -> Result<bool, DataLayerError> {
        let rows_affected = sqlx::query("DELETE FROM gemini_file_mappings WHERE file_name = ?")
            .bind(file_name)
            .execute(&self.pool)
            .await
            .map_sql_err()?
            .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn delete_by_file_name_for_user(
        &self,
        file_name: &str,
        user_id: &str,
    ) -> Result<bool, DataLayerError> {
        let rows_affected =
            sqlx::query(
                "DELETE FROM gemini_file_mappings WHERE BINARY file_name = BINARY ? AND BINARY user_id = BINARY ?",
            )
                .bind(file_name)
                .bind(user_id)
                .execute(&self.pool)
                .await
                .map_sql_err()?
                .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn delete_by_file_name_for_owner(
        &self,
        file_name: &str,
        key_id: &str,
        user_id: &str,
    ) -> Result<bool, DataLayerError> {
        let rows_affected = sqlx::query(
            "DELETE FROM gemini_file_mappings WHERE BINARY file_name = BINARY ? AND BINARY key_id = BINARY ? AND BINARY user_id = BINARY ?",
        )
        .bind(file_name)
        .bind(key_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn delete_by_id(
        &self,
        mapping_id: &str,
    ) -> Result<Option<StoredGeminiFileMapping>, DataLayerError> {
        let existing = sqlx::query(
            r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
WHERE id = ?
LIMIT 1
"#,
        )
        .bind(mapping_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        let Some(existing) = existing else {
            return Ok(None);
        };
        sqlx::query("DELETE FROM gemini_file_mappings WHERE id = ?")
            .bind(mapping_id)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        Ok(Some(map_row(&existing)?))
    }

    async fn delete_expired_before(&self, now_unix_secs: u64) -> Result<usize, DataLayerError> {
        let rows_affected = sqlx::query("DELETE FROM gemini_file_mappings WHERE expires_at <= ?")
            .bind(now_unix_secs as i64)
            .execute(&self.pool)
            .await
            .map_sql_err()?
            .rows_affected();
        Ok(usize::try_from(rows_affected).unwrap_or_default())
    }
}

fn build_list_count_query(query: &GeminiFileMappingListQuery) -> QueryBuilder<'_, MySql> {
    let mut builder =
        QueryBuilder::<MySql>::new("SELECT COUNT(*) AS total FROM gemini_file_mappings");
    let mut where_clause = WhereClause::new();
    apply_list_filters(&mut builder, &mut where_clause, query);
    builder
}

fn build_list_rows_query(query: &GeminiFileMappingListQuery) -> QueryBuilder<'_, MySql> {
    let mut builder = QueryBuilder::<MySql>::new(
        r#"
SELECT
  id,
  file_name,
  key_id,
  user_id,
  display_name,
  mime_type,
  source_hash,
  created_at AS created_at_unix_ms,
  expires_at AS expires_at_unix_secs
FROM gemini_file_mappings
"#,
    );
    let mut where_clause = WhereClause::new();
    apply_list_filters(&mut builder, &mut where_clause, query);
    builder.push(" ORDER BY created_at DESC, file_name ASC");
    push_limit_offset(
        &mut builder,
        i64::try_from(query.limit).unwrap_or(i64::MAX),
        i64::try_from(query.offset).unwrap_or(i64::MAX),
    );
    builder
}

fn apply_list_filters(
    builder: &mut QueryBuilder<'_, MySql>,
    where_clause: &mut WhereClause,
    query: &GeminiFileMappingListQuery,
) {
    if let Some(user_id) = query.user_id.as_deref() {
        where_clause.push_next(builder);
        builder.push("BINARY user_id = BINARY ");
        builder.push_bind(user_id.to_string());
    }
    if !query.include_expired {
        where_clause.push_next(builder);
        builder.push("expires_at > ");
        builder.push_bind(query.now_unix_secs as i64);
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        push_ci_contains_any(
            builder,
            where_clause,
            SqlDialect::MySql,
            &["file_name", "COALESCE(display_name, '')"],
            search,
        );
    }
}

fn current_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn i64_from_u64(value: u64, field_name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::InvalidInput(format!("{field_name} exceeds i64: {value}")))
}

fn map_row(row: &MySqlRow) -> Result<StoredGeminiFileMapping, DataLayerError> {
    Ok(StoredGeminiFileMapping {
        id: row.try_get("id").map_sql_err()?,
        file_name: row.try_get("file_name").map_sql_err()?,
        key_id: row.try_get("key_id").map_sql_err()?,
        user_id: row.try_get("user_id").ok().flatten(),
        display_name: row.try_get("display_name").ok().flatten(),
        mime_type: row.try_get("mime_type").ok().flatten(),
        source_hash: row.try_get("source_hash").ok().flatten(),
        created_at_unix_ms: u64::try_from(
            row.try_get::<i64, _>("created_at_unix_ms").map_sql_err()?,
        )
        .map_err(|_| {
            DataLayerError::UnexpectedValue(
                "gemini_file_mappings.created_at is invalid".to_string(),
            )
        })?,
        expires_at_unix_secs: u64::try_from(
            row.try_get::<i64, _>("expires_at_unix_secs")
                .map_sql_err()?,
        )
        .map_err(|_| {
            DataLayerError::UnexpectedValue(
                "gemini_file_mappings.expires_at is invalid".to_string(),
            )
        })?,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_list_count_query, build_list_rows_query, MysqlGeminiFileMappingRepository,
        OWNER_GUARDED_UPSERT_SQL,
    };
    use aether_data_contracts::repository::gemini_file_mappings::GeminiFileMappingListQuery;
    use sqlx::Execute;

    #[test]
    fn list_query_uses_shared_mysql_filter_and_pagination_rendering() {
        let query = GeminiFileMappingListQuery {
            user_id: Some("user-1".to_string()),
            include_expired: false,
            search: Some(" Report ".to_string()),
            offset: 5,
            limit: 10,
            now_unix_secs: 123,
        };

        let mut count = build_list_count_query(&query);
        let count_sql = count.build().sql().to_string();
        assert!(count_sql.contains(
            " WHERE BINARY user_id = BINARY ? AND expires_at > ? AND (LOWER(file_name) LIKE ?"
        ));
        assert!(!count_sql.contains("WHERE 1=1"));

        let mut rows = build_list_rows_query(&query);
        let rows_sql = rows.build().sql().to_string();
        assert!(rows_sql.contains("LOWER(COALESCE(display_name, '')) LIKE ?"));
        assert!(rows_sql.contains(" ORDER BY created_at DESC, file_name ASC LIMIT ? OFFSET ?"));
    }

    #[test]
    fn owner_guarded_upsert_uses_exact_file_key_and_user_identity() {
        let identity = "BINARY file_name = BINARY VALUES(file_name) AND BINARY key_id = BINARY VALUES(key_id) AND BINARY user_id <=> BINARY VALUES(user_id)";
        assert_eq!(OWNER_GUARDED_UPSERT_SQL.matches(identity).count(), 4);
    }

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlGeminiFileMappingRepository::new(pool);
    }
}
